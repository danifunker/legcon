//! SSH-1 (protocol 1.5), our own code: SSH 1.2.x, F-Secure 1.x and OpenSSH with `Protocol 1`.
//! One command per connection, 3DES in SSH-1's three-CBC mode, password or RSA key login.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use des::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use lc_core::rcp::{self, shell_error};
use lc_core::{
    ConnState, Connection, Entry, Error, ExecResult, FileCaps, FileSession, PathStyle, Pty,
    PtyWriter, RemotePath, Result, WriteMode,
};
use md5::Digest as _;
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};

use crate::{Auth, HostKey, Options};

/// Message numbers (the SSH 1.5 protocol document).
pub mod msg {
    pub const DISCONNECT: u8 = 1;
    pub const PUBLIC_KEY: u8 = 2;
    pub const SESSION_KEY: u8 = 3;
    pub const USER: u8 = 4;
    pub const AUTH_RSA: u8 = 6;
    pub const AUTH_RSA_CHALLENGE: u8 = 7;
    pub const AUTH_RSA_RESPONSE: u8 = 8;
    pub const AUTH_PASSWORD: u8 = 9;
    pub const REQUEST_PTY: u8 = 10;
    pub const WINDOW_SIZE: u8 = 11;
    pub const EXEC_SHELL: u8 = 12;
    pub const EXEC_CMD: u8 = 13;
    pub const SUCCESS: u8 = 14;
    pub const FAILURE: u8 = 15;
    pub const STDIN_DATA: u8 = 16;
    pub const STDOUT_DATA: u8 = 17;
    pub const STDERR_DATA: u8 = 18;
    pub const EOF: u8 = 19;
    pub const EXITSTATUS: u8 = 20;
    pub const IGNORE: u8 = 32;
    pub const EXIT_CONFIRMATION: u8 = 33;
    pub const DEBUG: u8 = 36;
}

pub const CIPHER_3DES: u8 = 3;
pub const AUTH_RSA: u32 = 2;
pub const AUTH_PASSWORD: u32 = 3;
const MAX_PACKET: usize = 256 * 1024;

/// A server that speaks only SSH-1 (`SSH-1.99` speaks both and gets SSH-2).
pub fn is_ssh1(banner: &str) -> bool {
    banner.starts_with("SSH-1.") && !banner.starts_with("SSH-1.99")
}

/// The CRC-32 SSH-1 seals packets with (initial 0, no final inversion).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    data.iter().fold(0u32, |c, &b| {
        t[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8)
    })
}

/// SSH-1's 3DES: three separate DES-CBC chains (encrypt, decrypt, encrypt), not EDE-CBC.
pub struct Des3 {
    k: [des::Des; 3],
    iv: [[u8; 8]; 3],
}

impl Des3 {
    /// From the first 24 bytes of the session key.
    pub fn new(key: &[u8]) -> Des3 {
        let k = |i: usize| des::Des::new(GenericArray::from_slice(&key[i * 8..i * 8 + 8]));
        Des3 {
            k: [k(0), k(1), k(2)],
            iv: [[0; 8]; 3],
        }
    }

    fn enc(&mut self, i: usize, b: &mut [u8]) {
        for (x, v) in b.iter_mut().zip(self.iv[i]) {
            *x ^= v;
        }
        self.k[i].encrypt_block(GenericArray::from_mut_slice(b));
        self.iv[i].copy_from_slice(b);
    }

    fn dec(&mut self, i: usize, b: &mut [u8]) {
        let c: [u8; 8] = b.try_into().unwrap_or([0; 8]);
        self.k[i].decrypt_block(GenericArray::from_mut_slice(b));
        for (x, v) in b.iter_mut().zip(self.iv[i]) {
            *x ^= v;
        }
        self.iv[i] = c;
    }

    pub fn encrypt(&mut self, data: &mut [u8]) {
        for b in data.chunks_exact_mut(8) {
            self.enc(0, b);
            self.dec(1, b);
            self.enc(2, b);
        }
    }

    pub fn decrypt(&mut self, data: &mut [u8]) {
        for b in data.chunks_exact_mut(8) {
            self.dec(2, b);
            self.enc(1, b);
            self.dec(0, b);
        }
    }
}

/// Reading SSH-1 message fields.
pub struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Fields(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Error::Protocol("a short SSH-1 message".into()));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    /// A multiple-precision integer: its size in bits, then the bytes.
    pub fn mp(&mut self) -> Result<BigUint> {
        let b = self.take(2)?;
        let bits = u16::from_be_bytes([b[0], b[1]]) as usize;
        Ok(BigUint::from_bytes_be(self.take(bits.div_ceil(8))?))
    }
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}
pub fn put_mp(out: &mut Vec<u8>, n: &BigUint) {
    out.extend_from_slice(&(n.bits() as u16).to_be_bytes());
    if n.bits() > 0 {
        out.extend_from_slice(&n.to_bytes_be());
    }
}

/// The sending half of a connection.
pub struct Sender {
    tcp: TcpStream,
    cipher: Option<Des3>,
}

impl Sender {
    pub fn new(tcp: TcpStream) -> Self {
        Sender { tcp, cipher: None }
    }
    pub fn set_cipher(&mut self, c: Des3) {
        self.cipher = Some(c);
    }
    pub fn send(&mut self, t: u8, data: &[u8]) -> Result<()> {
        let len = 1 + data.len() + 4;
        let pad = 8 - (len % 8);
        let mut body = vec![0u8; pad];
        if self.cipher.is_some() {
            OsRng.fill_bytes(&mut body);
        }
        body.push(t);
        body.extend_from_slice(data);
        let crc = crc32(&body);
        body.extend_from_slice(&crc.to_be_bytes());
        if let Some(c) = self.cipher.as_mut() {
            c.encrypt(&mut body);
        }
        let mut out = (len as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        self.tcp.write_all(&out)?;
        Ok(())
    }
    pub fn shutdown(&self) {
        let _ = self.tcp.shutdown(Shutdown::Both);
    }
}

/// The receiving half: whole packets out of the byte stream, kept across read timeouts.
pub struct Receiver {
    tcp: TcpStream,
    cipher: Option<Des3>,
    buf: Vec<u8>,
}

impl Receiver {
    pub fn new(tcp: TcpStream) -> Self {
        Receiver {
            tcp,
            cipher: None,
            buf: Vec::new(),
        }
    }
    pub fn set_cipher(&mut self, c: Des3) {
        self.cipher = Some(c);
    }

    fn fill(&mut self, n: usize) -> std::io::Result<()> {
        let mut chunk = [0u8; 16384];
        while self.buf.len() < n {
            let got = self.tcp.read(&mut chunk)?;
            if got == 0 {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            self.buf.extend_from_slice(&chunk[..got]);
        }
        Ok(())
    }

    /// The next message; a read timeout comes back as an `Io` error and loses nothing.
    pub fn recv_raw(&mut self) -> Result<(u8, Vec<u8>)> {
        self.fill(4)?;
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if !(5..=MAX_PACKET).contains(&len) {
            return Err(Error::Protocol(format!("an SSH-1 packet of {len} bytes")));
        }
        let pad = 8 - (len % 8);
        let total = len + pad;
        self.fill(4 + total)?;
        let mut body: Vec<u8> = self.buf.drain(..4 + total).skip(4).collect();
        if let Some(c) = self.cipher.as_mut() {
            c.decrypt(&mut body);
        }
        let (sealed, crc) = body.split_at(total - 4);
        if crc32(sealed) != u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]) {
            return Err(Error::Protocol(
                "an SSH-1 packet failed its check (wrong key or a damaged connection)".into(),
            ));
        }
        Ok((sealed[pad], sealed[pad + 1..].to_vec()))
    }

    /// The next message that matters: IGNORE and DEBUG are skipped, DISCONNECT is an error.
    pub fn recv(&mut self) -> Result<(u8, Vec<u8>)> {
        loop {
            let (t, d) = self.recv_raw()?;
            match t {
                msg::IGNORE | msg::DEBUG => continue,
                msg::DISCONNECT => {
                    let why = Fields::new(&d).string().unwrap_or(b"").to_vec();
                    return Err(Error::Protocol(format!(
                        "the server ended the connection: {}",
                        String::from_utf8_lossy(&why)
                    )));
                }
                _ => return Ok((t, d)),
            }
        }
    }

    pub fn set_timeout(&self, t: Option<Duration>) -> Result<()> {
        self.tcp.set_read_timeout(t)?;
        Ok(())
    }
}

fn expect(r: &mut Receiver, want: u8, what: &str) -> Result<Vec<u8>> {
    let (t, d) = r.recv()?;
    if t != want {
        return Err(Error::Protocol(format!(
            "{what}: unexpected SSH-1 message {t}"
        )));
    }
    Ok(d)
}

/// What a connection agreed on, for the log.
#[derive(Debug, Clone)]
pub struct Ssh1Info {
    pub banner: String,
    pub host_bits: usize,
    pub server_bits: usize,
}

/// A logged-in SSH-1 connection, ready for its one command or shell.
pub struct Conn {
    tx: Sender,
    rx: Receiver,
    pub info: Ssh1Info,
}

fn read_banner(tcp: &mut TcpStream) -> Result<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if tcp.read(&mut b)? == 0 {
            return Err(Error::Protocol(
                "the server closed before saying its version".into(),
            ));
        }
        if b[0] == b'\n' {
            let text = String::from_utf8_lossy(&line).trim_end().to_string();
            if text.starts_with("SSH-") {
                return Ok(text);
            }
            line.clear();
        } else if line.len() < 255 {
            line.push(b[0]);
        }
    }
}

fn dial(host: &str, port: u16, opts: &Options) -> Result<TcpStream> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let tcp = TcpStream::connect_timeout(&addr, opts.connect_timeout).map_err(|e| {
        if e.kind() == ErrorKind::TimedOut {
            Error::Timeout(opts.connect_timeout)
        } else {
            Error::Io(e)
        }
    })?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(opts.timeout))?;
    Ok(tcp)
}

/// The version line a server sends, read without logging in.
pub fn banner(host: &str, port: u16, opts: &Options) -> Result<String> {
    let mut tcp = dial(host, port, opts)?;
    let b = read_banner(&mut tcp);
    let _ = tcp.shutdown(Shutdown::Both);
    b
}

fn md5_hex(data: &[u8]) -> String {
    md5::Md5::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The host key as legcon keeps it: `rsa1` with e and n as SSH-2 style mpints.
pub fn host_key(e: &BigUint, n: &BigUint) -> HostKey {
    let mut blob = Vec::new();
    for v in [e, n] {
        crate::keys::put_mpint(&mut blob, &v.to_bytes_be());
    }
    let mut both = n.to_bytes_be();
    both.extend_from_slice(&e.to_bytes_be());
    HostKey {
        algorithm: "rsa1".into(),
        md5: md5_hex(&both),
        sha256: format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(&both))
        ),
        blob,
    }
}

fn strip(b: Vec<u8>) -> Vec<u8> {
    let z = b.iter().take_while(|&&x| x == 0).count();
    b[z..].to_vec()
}

fn rsa_err(e: rsa::Error) -> Error {
    Error::Protocol(format!("RSA: {e}"))
}

impl Conn {
    /// Connect, agree on keys, check the host key with `verify`, and log in.
    pub fn open(
        host: &str,
        port: u16,
        user: &str,
        auth: &Auth,
        opts: &Options,
        verify: &mut dyn FnMut(&HostKey) -> Result<()>,
    ) -> Result<Conn> {
        let mut tcp = dial(host, port, opts)?;
        let banner = read_banner(&mut tcp)?;
        if !banner.starts_with("SSH-1.") {
            return Err(Error::Unsupported(format!(
                "{banner} is not an SSH-1 server"
            )));
        }
        tcp.write_all(b"SSH-1.5-legcon\n")?;
        let mut tx = Sender::new(tcp.try_clone()?);
        let mut rx = Receiver::new(tcp);

        let d = expect(&mut rx, msg::PUBLIC_KEY, "key exchange")?;
        let mut f = Fields::new(&d);
        let cookie = f.bytes(8)?.to_vec();
        let _ = f.u32()?;
        let (server_e, server_n) = (f.mp()?, f.mp()?);
        let _ = f.u32()?;
        let (host_e, host_n) = (f.mp()?, f.mp()?);
        let _flags = f.u32()?;
        let ciphers = f.u32()?;
        let auths = f.u32()?;
        let info = Ssh1Info {
            banner: banner.clone(),
            host_bits: host_n.bits(),
            server_bits: server_n.bits(),
        };
        if let Err(e) = verify(&host_key(&host_e, &host_n)) {
            tx.shutdown();
            return Err(e);
        }
        if ciphers & (1 << CIPHER_3DES) == 0 {
            tx.shutdown();
            return Err(Error::Unsupported(format!(
                "{banner} does not offer 3DES (cipher mask {ciphers:#x})"
            )));
        }
        let mut id_src = host_n.to_bytes_be();
        id_src.extend_from_slice(&server_n.to_bytes_be());
        id_src.extend_from_slice(&cookie);
        let session_id: [u8; 16] = md5::Md5::digest(&id_src).into();

        let mut session_key = [0u8; 32];
        OsRng.fill_bytes(&mut session_key);
        let mut sealed = session_key.to_vec();
        for (b, s) in sealed.iter_mut().zip(session_id) {
            *b ^= s;
        }
        // Encrypt with the smaller key first, then the larger.
        let server = RsaPublicKey::new(server_n, server_e).map_err(rsa_err)?;
        let host_k = RsaPublicKey::new(host_n, host_e).map_err(rsa_err)?;
        let (small, large) = if rsa::traits::PublicKeyParts::n(&server).bits()
            < rsa::traits::PublicKeyParts::n(&host_k).bits()
        {
            (&server, &host_k)
        } else {
            (&host_k, &server)
        };
        let once = strip(
            small
                .encrypt(&mut OsRng, Pkcs1v15Encrypt, &strip(sealed))
                .map_err(rsa_err)?,
        );
        let twice = large
            .encrypt(&mut OsRng, Pkcs1v15Encrypt, &once)
            .map_err(rsa_err)?;
        let mut m = vec![CIPHER_3DES];
        m.extend_from_slice(&cookie);
        put_mp(&mut m, &BigUint::from_bytes_be(&twice));
        put_u32(&mut m, 0);
        tx.send(msg::SESSION_KEY, &m)?;
        tx.set_cipher(Des3::new(&session_key));
        rx.set_cipher(Des3::new(&session_key));
        expect(&mut rx, msg::SUCCESS, "key exchange")?;

        let mut conn = Conn { tx, rx, info };
        conn.log_in(user, auth, auths, &session_id)?;
        Ok(conn)
    }

    fn log_in(&mut self, user: &str, auth: &Auth, auths: u32, session_id: &[u8; 16]) -> Result<()> {
        let mut m = Vec::new();
        put_string(&mut m, user.as_bytes());
        self.tx.send(msg::USER, &m)?;
        if self.rx.recv()?.0 == msg::SUCCESS {
            return Ok(());
        }
        let mut key_error = None;
        if let (Some(k), true) = (&auth.key, auths & (1 << AUTH_RSA) != 0) {
            match self.rsa_login(&k.private, session_id) {
                Ok(true) => return Ok(()),
                Ok(false) => key_error = Some("the key was refused".to_string()),
                Err(e) => key_error = Some(e.to_string()),
            }
        }
        if let (Some(pw), true) = (&auth.password, auths & (1 << AUTH_PASSWORD) != 0) {
            let mut m = Vec::new();
            put_string(&mut m, pw.as_bytes());
            self.tx.send(msg::AUTH_PASSWORD, &m)?;
            if self.rx.recv()?.0 == msg::SUCCESS {
                return Ok(());
            }
            self.tx.shutdown();
            return Err(Error::Auth(match key_error {
                Some(k) => format!("password refused; {k}"),
                None => "password refused".into(),
            }));
        }
        self.tx.shutdown();
        Err(Error::Auth(key_error.unwrap_or_else(|| {
            "no password, and no SSH-1 key the server takes".into()
        })))
    }

    /// RSA login with an SSH-1 identity file; false when the server will not take the key.
    fn rsa_login(&mut self, identity: &std::path::Path, session_id: &[u8; 16]) -> Result<bool> {
        let data = std::fs::read(identity)?;
        let key = crate::keyfile::ssh1_private(&data)?;
        let n = rsa::traits::PublicKeyParts::n(&key).clone();
        let mut m = Vec::new();
        put_mp(&mut m, &n);
        self.tx.send(msg::AUTH_RSA, &m)?;
        let (t, d) = self.rx.recv()?;
        if t != msg::AUTH_RSA_CHALLENGE {
            return Ok(false);
        }
        let challenge = Fields::new(&d).mp()?;
        let k = n.bits().div_ceil(8);
        let mut ct = challenge.to_bytes_be();
        while ct.len() < k {
            ct.insert(0, 0);
        }
        let plain = key.decrypt(Pkcs1v15Encrypt, &ct).map_err(rsa_err)?;
        let mut block = vec![0u8; 32usize.saturating_sub(plain.len())];
        block.extend_from_slice(&plain);
        block.extend_from_slice(session_id);
        let answer: [u8; 16] = md5::Md5::digest(&block).into();
        self.tx.send(msg::AUTH_RSA_RESPONSE, &answer)?;
        Ok(self.rx.recv()?.0 == msg::SUCCESS)
    }

    /// Run one command; the connection ends with it.
    pub fn exec(mut self, command: &[u8], stdin: &[u8]) -> Result<ExecResult> {
        let mut m = Vec::new();
        put_string(&mut m, command);
        self.tx.send(msg::EXEC_CMD, &m)?;
        for chunk in stdin.chunks(16384) {
            let mut m = Vec::new();
            put_string(&mut m, chunk);
            self.tx.send(msg::STDIN_DATA, &m)?;
        }
        self.tx.send(msg::EOF, &[])?;
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        loop {
            let (t, d) = self.rx.recv()?;
            match t {
                msg::STDOUT_DATA => output.extend_from_slice(Fields::new(&d).string()?),
                msg::STDERR_DATA => errors.extend_from_slice(Fields::new(&d).string()?),
                msg::EXITSTATUS => {
                    let status = Fields::new(&d).u32()?;
                    let _ = self.tx.send(msg::EXIT_CONFIRMATION, &[]);
                    self.tx.shutdown();
                    return Ok(ExecResult {
                        output,
                        errors,
                        exit_status: Some(status as i32),
                    });
                }
                _ => {}
            }
        }
    }

    /// Start a command whose input and output are streamed (`scp -f`, `scp -t`).
    pub fn into_link(mut self, command: &[u8]) -> Result<ExecLink> {
        let mut m = Vec::new();
        put_string(&mut m, command);
        self.tx.send(msg::EXEC_CMD, &m)?;
        Ok(ExecLink {
            conn: self,
            pending: Vec::new(),
            errors: Vec::new(),
            done: false,
        })
    }

    /// Open an interactive terminal; the connection then belongs to it.
    pub fn into_pty(mut self, term: &str, cols: u16, rows: u16, poll: Duration) -> Result<Pty> {
        let mut m = Vec::new();
        put_string(&mut m, term.as_bytes());
        for v in [u32::from(rows), u32::from(cols), 0, 0] {
            put_u32(&mut m, v);
        }
        m.push(0); // no terminal modes
        self.tx.send(msg::REQUEST_PTY, &m)?;
        let _ = self.rx.recv()?;
        self.tx.send(msg::EXEC_SHELL, &[])?;
        self.rx.set_timeout(Some(poll))?;
        let tx = Arc::new(Mutex::new(self.tx));
        Ok(Pty {
            reader: Box::new(PtyReader {
                rx: self.rx,
                tx: Arc::clone(&tx),
                pending: Vec::new(),
                ended: false,
            }),
            writer: Box::new(PtyWriterImpl { tx, closed: false }),
        })
    }
}

fn lock(m: &Mutex<Sender>) -> std::sync::MutexGuard<'_, Sender> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct PtyReader {
    rx: Receiver,
    tx: Arc<Mutex<Sender>>,
    pending: Vec<u8>,
    ended: bool,
}

impl Read for PtyReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            if self.ended {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok((msg::STDOUT_DATA | msg::STDERR_DATA, d)) => {
                    self.pending = Fields::new(&d).string().unwrap_or(b"").to_vec();
                }
                Ok((msg::EXITSTATUS, _)) => {
                    let _ = lock(&self.tx).send(msg::EXIT_CONFIRMATION, &[]);
                    self.ended = true;
                }
                Ok(_) => {}
                Err(Error::Io(e)) => return Err(e),
                Err(_) => self.ended = true,
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

struct PtyWriterImpl {
    tx: Arc<Mutex<Sender>>,
    closed: bool,
}

impl PtyWriter for PtyWriterImpl {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let mut m = Vec::new();
        put_string(&mut m, bytes);
        lock(&self.tx).send(msg::STDIN_DATA, &m)
    }
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let mut m = Vec::new();
        for v in [u32::from(rows), u32::from(cols), 0, 0] {
            put_u32(&mut m, v);
        }
        lock(&self.tx).send(msg::WINDOW_SIZE, &m)
    }
    fn close(&mut self) -> Result<()> {
        if !self.closed {
            self.closed = true;
            let mut t = lock(&self.tx);
            let mut m = Vec::new();
            put_string(&mut m, b"legcon: logged off");
            let _ = t.send(msg::DISCONNECT, &m);
            t.shutdown();
        }
        Ok(())
    }
}

/// A streamed command (an SCP copy) as the link rcp runs over.
pub struct ExecLink {
    conn: Conn,
    pending: Vec<u8>,
    errors: Vec<u8>,
    done: bool,
}

impl Read for ExecLink {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            if self.done {
                return Ok(0);
            }
            match self.conn.rx.recv() {
                Ok((msg::STDOUT_DATA, d)) => {
                    self.pending = Fields::new(&d).string().unwrap_or(b"").to_vec();
                }
                Ok((msg::STDERR_DATA, d)) => {
                    self.errors
                        .extend_from_slice(Fields::new(&d).string().unwrap_or(b""));
                }
                Ok((msg::EXITSTATUS, _)) => {
                    let _ = self.conn.tx.send(msg::EXIT_CONFIRMATION, &[]);
                    self.done = true;
                }
                Ok(_) => {}
                Err(Error::Io(e)) => return Err(e),
                Err(e) => return Err(std::io::Error::other(e.to_string())),
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl Write for ExecLink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = b.len().min(16384);
        let mut m = Vec::new();
        put_string(&mut m, &b[..n]);
        self.conn
            .tx
            .send(msg::STDIN_DATA, &m)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl rcp::Link for ExecLink {
    fn end(&mut self) {
        let _ = self.conn.tx.send(msg::EOF, &[]);
        let mut sink = [0u8; 4096];
        while !self.done && self.read(&mut sink).unwrap_or(0) > 0 {}
        self.conn.tx.shutdown();
    }
    fn abort(&mut self) {
        self.conn.tx.shutdown();
    }
}

/// Files on an SSH-1 machine: each operation is its own connection (SSH-1 runs one command per
/// connection), with SCP for contents and Bourne shell commands for the rest.
pub struct Ssh1Files {
    host: String,
    port: u16,
    user: String,
    auth: Auth,
    opts: Options,
    /// The host key seen when it was opened; later connections must show the same.
    key: HostKey,
    pub info: Ssh1Info,
    closed: bool,
}

/// Single-quote for the Bourne shell.
fn sq(b: &[u8]) -> Vec<u8> {
    crate::scp::sq(b)
}

impl Ssh1Files {
    /// Log in once (checking the host key with `verify`) to prove the machine takes us.
    pub fn open(
        host: &str,
        port: u16,
        user: &str,
        auth: &Auth,
        opts: &Options,
        verify: &mut dyn FnMut(&HostKey) -> Result<()>,
    ) -> Result<Ssh1Files> {
        let mut seen = None;
        let conn = Conn::open(host, port, user, auth, opts, &mut |k: &HostKey| {
            seen = Some(k.clone());
            verify(k)
        })?;
        let info = conn.info.clone();
        drop(conn.exec(b"true", b""));
        Ok(Ssh1Files {
            host: host.into(),
            port,
            user: user.into(),
            auth: auth.clone(),
            opts: opts.clone(),
            key: seen.ok_or_else(|| Error::Protocol("no host key".into()))?,
            info,
            closed: false,
        })
    }

    fn conn(&self) -> Result<Conn> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let want = self.key.blob.clone();
        Conn::open(
            &self.host,
            self.port,
            &self.user,
            &self.auth,
            &self.opts,
            &mut |k: &HostKey| {
                if k.blob == want {
                    Ok(())
                } else {
                    Err(Error::Denied(
                        "the machine's host key changed since it was opened".into(),
                    ))
                }
            },
        )
    }

    fn enc(&self, text: &str) -> Vec<u8> {
        self.opts
            .charset
            .encode(text)
            .unwrap_or_else(|| text.as_bytes().to_vec())
    }

    fn qpath(&self, p: &RemotePath) -> Vec<u8> {
        sq(&self.enc(&p.to_string()))
    }

    /// Run a Bourne shell command, whatever the login shell is.
    pub fn run(&self, cmd: &[u8]) -> Result<ExecResult> {
        let mut inner = b"LC_ALL=C; export LC_ALL; ".to_vec();
        inner.extend_from_slice(cmd);
        let mut full = b"/bin/sh -c ".to_vec();
        full.extend_from_slice(&sq(&inner));
        self.conn()?.exec(&full, b"")
    }

    fn sh(&self, what: &str, cmd: &[u8]) -> Result<Vec<u8>> {
        let r = self.run(cmd)?;
        match r.exit_status {
            Some(0) | None => Ok(r.output),
            Some(_) => Err(shell_error(what, &r.errors)),
        }
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn link(&self, verb: &[u8], path: &RemotePath) -> Result<ExecLink> {
        let mut cmd = verb.to_vec();
        cmd.extend_from_slice(&self.qpath(path));
        self.conn()?.into_link(&cmd)
    }
}

impl Connection for Ssh1Files {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }
    /// Every operation already logged off when it ended; nothing stays open.
    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}

impl FileSession for Ssh1Files {
    fn charset(&self) -> lc_core::Charset {
        self.opts.charset
    }
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: false,
            random_write: false,
            resume_upload: false,
            rename: true,
            delete: true,
            mkdir: true,
            set_times: false,
            set_mode: false,
        }
    }
    fn home(&mut self) -> Result<RemotePath> {
        let out = self.sh("pwd", b"pwd")?;
        let dir = self.opts.charset.decode(out.trim_ascii());
        Ok(RemotePath::parse(PathStyle::Unix, &dir))
    }
    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        let mut cmd = b"ls -laL ".to_vec();
        cmd.extend_from_slice(&self.qpath(dir));
        let r = self.run(&cmd)?;
        if r.output.is_empty() && r.exit_status.unwrap_or(0) != 0 {
            return Err(shell_error(&dir.to_string(), &r.errors));
        }
        Ok(lc_core::listing::parse(
            &r.output,
            self.opts.charset,
            Self::now(),
        ))
    }
    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let mut cmd = b"ls -ldL ".to_vec();
        cmd.extend_from_slice(&self.qpath(path));
        let out = self.sh(&path.to_string(), &cmd)?;
        let line = out.split(|&b| b == b'\n').next().unwrap_or(&[]);
        let mut e = lc_core::listing::parse_unix(line, self.opts.charset, Self::now())
            .ok_or_else(|| Error::Protocol(format!("cannot read ls output for {path}")))?;
        e.name = path.name().to_string();
        e.raw_name = self.enc(path.name());
        Ok(e)
    }
    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        let link = self.link(b"scp -f ", path)?;
        Ok(Box::new(rcp::receive(link, &path.to_string(), at)?))
    }
    fn open_write(&mut self, _: &RemotePath, _: WriteMode) -> Result<Box<dyn Write + Send + '_>> {
        Err(Error::Unsupported(
            "SSH-1 copies with SCP, which needs the size before the data".into(),
        ))
    }
    fn open_write_sized(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
        size: u64,
    ) -> Result<Box<dyn Write + Send + '_>> {
        if matches!(mode, WriteMode::Resume(n) if n > 0) {
            return Err(Error::Unsupported("SCP cannot continue an upload".into()));
        }
        let parent = path
            .parent()
            .unwrap_or_else(|| RemotePath::root(path.style));
        let link = self.link(b"scp -t ", &parent)?;
        let name = self.enc(path.name());
        Ok(Box::new(rcp::send(link, &name, size, &path.to_string())?))
    }
    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        let mut cmd = b"mkdir ".to_vec();
        cmd.extend_from_slice(&self.qpath(path));
        self.sh(&path.to_string(), &cmd).map(drop)
    }
    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        let q = self.qpath(path);
        let mut cmd = b"if [ -d ".to_vec();
        for part in [
            &q[..],
            b" ]; then rmdir ",
            &q,
            b"; else rm -f ",
            &q,
            b"; fi",
        ] {
            cmd.extend_from_slice(part);
        }
        self.sh(&path.to_string(), &cmd).map(drop)
    }
    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let mut cmd = b"mv ".to_vec();
        cmd.extend_from_slice(&self.qpath(from));
        cmd.push(b' ');
        cmd.extend_from_slice(&self.qpath(to));
        self.sh(&from.to_string(), &cmd).map(drop)
    }
    fn quote(&mut self, dir: &RemotePath, line: &str) -> Result<String> {
        let mut cmd = b"cd ".to_vec();
        cmd.extend_from_slice(&self.qpath(dir));
        cmd.extend_from_slice(b" && ");
        cmd.extend_from_slice(&self.enc(line));
        let r = self.run(&cmd)?;
        let mut text = self.opts.charset.decode(&r.output);
        text.push_str(&self.opts.charset.decode(&r.errors));
        match r.exit_status {
            Some(0) | None => {}
            Some(n) => text.push_str(&format!("(exit status {n})")),
        }
        Ok(text.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_ssh1() {
        // ssh_crc32 starts at 0 and does not invert: "123456789" gives 0x2DFD2D88.
        assert_eq!(crc32(b"123456789"), 0x2DFD_2D88);
    }

    #[test]
    fn des3_matches_an_independent_implementation() {
        // From pycryptodome: CBC-encrypt k1, CBC-decrypt k2, CBC-encrypt k3, zero IVs.
        let key: Vec<u8> = (0u8..24).collect();
        let mut data: Vec<u8> = (0u8..32).chain([7u8; 16]).collect();
        Des3::new(&key).encrypt(&mut data);
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "58ed248f77f6b19e51ff0c4724da7ade29862c71ec9d76cbafa297b564d601b934fee89b51e46450983a0b4d51eea642"
        );
    }

    #[test]
    fn des3_round_trips_and_chains() {
        let key: Vec<u8> = (0u8..24).collect();
        let mut a = Des3::new(&key);
        let mut b = Des3::new(&key);
        let plain: Vec<u8> = (0u8..32).collect();
        let mut data = plain.clone();
        a.encrypt(&mut data);
        assert_ne!(data, plain);
        // Identical blocks encrypt differently: the chains carry on.
        let mut twice = [7u8; 16];
        a.encrypt(&mut twice);
        assert_ne!(twice[..8], twice[8..]);
        b.decrypt(&mut data);
        assert_eq!(data, plain);
        b.decrypt(&mut twice);
        assert_eq!(twice, [7u8; 16]);
    }
}
