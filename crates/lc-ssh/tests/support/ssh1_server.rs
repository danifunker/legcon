//! A pretend SSH 1.2.27 server for tests and the `fake_sshd1` example: password `secret`, one
//! authorized SSH-1 key (learned from an install command), canned `pwd` / `ls`, SCP both ways.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use lc_ssh::ssh1::{
    msg, put_mp, put_string, put_u32, Des3, Fields, Receiver, Sender, AUTH_PASSWORD, AUTH_RSA,
    CIPHER_3DES,
};
use md5::Digest as _;
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

thread_local! {
    /// The port the next `start` listens on; 0 picks a free one.
    pub static PORT: RefCell<u16> = const { RefCell::new(0) };
}

pub struct Server {
    host: RsaPrivateKey,
    server: RsaPrivateKey,
    /// The modulus of the one SSH-1 key the user has authorized.
    pub authorized: Mutex<Option<BigUint>>,
    pub uploaded: Arc<Mutex<Vec<u8>>>,
    pub commands: Arc<Mutex<Vec<String>>>,
}

fn pad_to(mut v: Vec<u8>, k: usize) -> Vec<u8> {
    while v.len() < k {
        v.insert(0, 0);
    }
    v
}

/// One client's session: the stdin bytes it sent so far, read by the pretend commands.
struct Session {
    tx: Sender,
    rx: Receiver,
    stdin: Vec<u8>,
    eof: bool,
}

impl Session {
    fn more(&mut self) -> bool {
        while !self.eof {
            match self.rx.recv() {
                Ok((msg::STDIN_DATA, d)) => {
                    self.stdin
                        .extend_from_slice(Fields::new(&d).string().unwrap());
                    return true;
                }
                Ok((msg::EOF, _)) | Err(_) => self.eof = true,
                Ok(_) => {}
            }
        }
        false
    }
    fn take(&mut self, n: usize) -> Vec<u8> {
        while self.stdin.len() < n && self.more() {}
        let n = n.min(self.stdin.len());
        self.stdin.drain(..n).collect()
    }
    fn line(&mut self) -> Vec<u8> {
        loop {
            if let Some(i) = self.stdin.iter().position(|&b| b == b'\n') {
                return self.stdin.drain(..=i).collect();
            }
            if !self.more() {
                return std::mem::take(&mut self.stdin);
            }
        }
    }
    fn out(&mut self, b: &[u8]) {
        let mut m = Vec::new();
        put_string(&mut m, b);
        self.tx.send(msg::STDOUT_DATA, &m).unwrap();
    }
    fn exit(&mut self, status: u32) {
        let mut m = Vec::new();
        put_u32(&mut m, status);
        self.tx.send(msg::EXITSTATUS, &m).unwrap();
        let _ = self.rx.recv();
    }
}

impl Server {
    fn serve(&self, mut tcp: TcpStream) {
        // A client that only reads the version line (legcon checking SSH-1 or 2) just goes.
        if tcp.write_all(b"SSH-1.5-1.2.27\n").is_err() {
            return;
        }
        let mut b = [0u8; 1];
        loop {
            match tcp.read(&mut b) {
                Ok(1) if b[0] != b'\n' => {}
                Ok(1) => break,
                _ => return,
            }
        }
        let mut tx = Sender::new(tcp.try_clone().unwrap());
        let mut rx = Receiver::new(tcp);
        let mut cookie = [0u8; 8];
        OsRng.fill_bytes(&mut cookie);
        let mut m = cookie.to_vec();
        for k in [&self.server, &self.host] {
            put_u32(&mut m, k.n().bits() as u32);
            put_mp(&mut m, k.e());
            put_mp(&mut m, k.n());
        }
        put_u32(&mut m, 0);
        put_u32(&mut m, 1 << CIPHER_3DES);
        put_u32(&mut m, (1 << AUTH_RSA) | (1 << AUTH_PASSWORD));
        tx.send(msg::PUBLIC_KEY, &m).unwrap();

        let Ok((t, d)) = rx.recv() else { return };
        assert_eq!(t, msg::SESSION_KEY);
        let mut f = Fields::new(&d);
        assert_eq!(f.u8().unwrap(), CIPHER_3DES);
        assert_eq!(f.bytes(8).unwrap(), cookie);
        let sealed = f.mp().unwrap();
        // The client sealed with the smaller key first, so open with the larger first.
        let k_host = self.host.size();
        let once = self
            .host
            .decrypt(Pkcs1v15Encrypt, &pad_to(sealed.to_bytes_be(), k_host))
            .unwrap();
        let k_server = self.server.size();
        let key_int = self
            .server
            .decrypt(Pkcs1v15Encrypt, &pad_to(once, k_server))
            .unwrap();
        let mut id_src = self.host.n().to_bytes_be();
        id_src.extend_from_slice(&self.server.n().to_bytes_be());
        id_src.extend_from_slice(&cookie);
        let session_id: [u8; 16] = md5::Md5::digest(&id_src).into();
        let mut key = pad_to(key_int, 32);
        for (b, s) in key.iter_mut().zip(session_id) {
            *b ^= s;
        }
        tx.set_cipher(Des3::new(&key));
        rx.set_cipher(Des3::new(&key));
        tx.send(msg::SUCCESS, &[]).unwrap();

        let (t, _) = rx.recv().unwrap();
        assert_eq!(t, msg::USER);
        tx.send(msg::FAILURE, &[]).unwrap();
        loop {
            let Ok((t, d)) = rx.recv() else { return };
            let ok = match t {
                msg::AUTH_PASSWORD => Fields::new(&d).string().unwrap() == b"secret",
                msg::AUTH_RSA => {
                    let n = Fields::new(&d).mp().unwrap();
                    if Some(&n) != self.authorized.lock().unwrap().as_ref() {
                        false
                    } else {
                        let public = RsaPublicKey::new(n, BigUint::from(65537u32)).unwrap();
                        let mut chal = [0u8; 32];
                        OsRng.fill_bytes(&mut chal);
                        chal[0] |= 1;
                        let ct = public.encrypt(&mut OsRng, Pkcs1v15Encrypt, &chal).unwrap();
                        let mut m = Vec::new();
                        put_mp(&mut m, &BigUint::from_bytes_be(&ct));
                        tx.send(msg::AUTH_RSA_CHALLENGE, &m).unwrap();
                        let (t, d) = rx.recv().unwrap();
                        assert_eq!(t, msg::AUTH_RSA_RESPONSE);
                        let mut block = chal.to_vec();
                        block.extend_from_slice(&session_id);
                        let want: [u8; 16] = md5::Md5::digest(&block).into();
                        d == want
                    }
                }
                _ => false,
            };
            tx.send(if ok { msg::SUCCESS } else { msg::FAILURE }, &[])
                .unwrap();
            if ok {
                break;
            }
        }

        let mut s = Session {
            tx,
            rx,
            stdin: Vec::new(),
            eof: false,
        };
        loop {
            let Ok((t, d)) = s.rx.recv() else { return };
            match t {
                msg::REQUEST_PTY => s.tx.send(msg::SUCCESS, &[]).unwrap(),
                msg::EXEC_SHELL => {
                    // Echo what is typed until `exit`.
                    loop {
                        match s.rx.recv() {
                            Ok((msg::STDIN_DATA, d)) => {
                                let b = Fields::new(&d).string().unwrap().to_vec();
                                if b.starts_with(b"exit") {
                                    s.exit(0);
                                    return;
                                }
                                s.out(&b);
                            }
                            Ok((msg::WINDOW_SIZE, _)) => s.out(b"[resized]"),
                            Ok(_) => {}
                            Err(_) => return,
                        }
                    }
                }
                msg::EXEC_CMD => {
                    let cmd =
                        String::from_utf8_lossy(Fields::new(&d).string().unwrap()).into_owned();
                    self.commands.lock().unwrap().push(cmd.clone());
                    if cmd.contains("scp -f") {
                        assert_eq!(s.take(1), [0]);
                        s.out(b"C0644 5 hello.txt\n");
                        assert_eq!(s.take(1), [0]);
                        s.out(b"hello\0");
                        let _ = s.take(1);
                        s.exit(0);
                    } else if cmd.contains("scp -t") {
                        s.out(&[0]);
                        let header = s.line();
                        let size: usize = String::from_utf8_lossy(&header)
                            .split(' ')
                            .nth(1)
                            .unwrap()
                            .parse()
                            .unwrap();
                        s.out(&[0]);
                        let data = s.take(size);
                        let _ = s.take(1);
                        *self.uploaded.lock().unwrap() = data;
                        s.out(&[0]);
                        while s.more() {}
                        s.exit(0);
                    } else if cmd.contains("authorized_keys") {
                        // Learn the key an install command adds: `bits e n comment`.
                        let n = cmd
                            .split("echo ")
                            .nth(1)
                            .and_then(|l| l.split_whitespace().nth(2))
                            .and_then(|t| {
                                BigUint::parse_bytes(t.trim_matches('\'').as_bytes(), 10)
                            });
                        *self.authorized.lock().unwrap() = n;
                        s.exit(0);
                    } else if cmd.contains("ls -laL") {
                        s.out(b"total 2\ndrwxr-xr-x  3 dani  staff  512 Nov 14 10:00 .\n-rw-r--r--  1 dani  staff    5 Nov 14 10:00 hello.txt\ndrwxr-xr-x  2 dani  staff  512 Nov 14 10:00 src\n");
                        s.exit(0);
                    } else if cmd.contains("pwd") {
                        s.out(b"/home/dani\n");
                        s.exit(0);
                    } else if cmd.contains("false") {
                        let mut m = Vec::new();
                        put_string(&mut m, b"sh: nope: not found\n");
                        s.tx.send(msg::STDERR_DATA, &m).unwrap();
                        s.exit(1);
                    } else {
                        s.exit(0);
                    }
                    return;
                }
                _ => {}
            }
        }
    }
}

pub fn start(authorized: Option<BigUint>) -> (u16, Arc<Server>) {
    let server = Arc::new(Server {
        host: RsaPrivateKey::new(&mut OsRng, 1024).unwrap(),
        server: RsaPrivateKey::new(&mut OsRng, 768).unwrap(),
        authorized: Mutex::new(authorized),
        uploaded: Arc::default(),
        commands: Arc::default(),
    });
    let l = TcpListener::bind(("127.0.0.1", PORT.with(|p| *p.borrow()))).unwrap();
    let port = l.local_addr().unwrap().port();
    let s = Arc::clone(&server);
    thread::spawn(move || {
        for c in l.incoming().flatten() {
            let s = Arc::clone(&s);
            thread::spawn(move || s.serve(c));
        }
    });
    (port, server)
}
