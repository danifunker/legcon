//! SSH-2, our own code (D19, revised 2026-11-15): the transport (RFC 4253), user authentication
//! (RFC 4252) and the connection protocol's session channels (RFC 4254), over the algorithms in
//! [`crate::crypto`]. One [`Link`] per TCP connection, shared by its channels behind a mutex.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lc_core::{Error, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::BigUint;

use crate::crypto::{self, Cipher, Curve, Dh, Hash, Mac, Signer};
use crate::wire::{put_bool, put_mpint, put_name_list, put_string, put_u32, put_u8, Reader};
use crate::{HostKey, Negotiated};

/// Message numbers (RFC 4250 §4.1).
pub mod msg {
    pub const DISCONNECT: u8 = 1;
    pub const IGNORE: u8 = 2;
    pub const UNIMPLEMENTED: u8 = 3;
    pub const DEBUG: u8 = 4;
    pub const SERVICE_REQUEST: u8 = 5;
    pub const SERVICE_ACCEPT: u8 = 6;
    pub const EXT_INFO: u8 = 7;
    pub const KEXINIT: u8 = 20;
    pub const NEWKEYS: u8 = 21;
    pub const KEX_INIT: u8 = 30;
    pub const KEX_REPLY: u8 = 31;
    pub const GEX_GROUP: u8 = 31;
    pub const GEX_INIT: u8 = 32;
    pub const GEX_REPLY: u8 = 33;
    pub const GEX_REQUEST: u8 = 34;
    pub const USERAUTH_REQUEST: u8 = 50;
    pub const USERAUTH_FAILURE: u8 = 51;
    pub const USERAUTH_SUCCESS: u8 = 52;
    pub const USERAUTH_BANNER: u8 = 53;
    pub const USERAUTH_INFO_REQUEST: u8 = 60;
    /// The same number answers a public key query (RFC 4252 7).
    pub const USERAUTH_PK_OK: u8 = 60;
    pub const USERAUTH_INFO_RESPONSE: u8 = 61;
    pub const GLOBAL_REQUEST: u8 = 80;
    pub const REQUEST_FAILURE: u8 = 82;
    pub const CHANNEL_OPEN: u8 = 90;
    pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
    pub const CHANNEL_OPEN_FAILURE: u8 = 92;
    pub const CHANNEL_WINDOW_ADJUST: u8 = 93;
    pub const CHANNEL_DATA: u8 = 94;
    pub const CHANNEL_EXTENDED_DATA: u8 = 95;
    pub const CHANNEL_EOF: u8 = 96;
    pub const CHANNEL_CLOSE: u8 = 97;
    pub const CHANNEL_REQUEST: u8 = 98;
    pub const CHANNEL_SUCCESS: u8 = 99;
    pub const CHANNEL_FAILURE: u8 = 100;
}

const VERSION: &str = "SSH-2.0-legcon_0.1";
/// Our receive window per channel, and the largest data packet we take.
const WINDOW: u32 = 2 * 1024 * 1024;
const MAX_PACKET: u32 = 32768;
/// Longer than any packet a server sends (RFC 4253 asks for at least 35000).
const MAX_LEN: usize = 256 * 1024;

fn bad(what: impl Into<String>) -> Error {
    Error::Protocol(what.into())
}

fn io_err(e: std::io::Error, timeout: Duration) -> Error {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => Error::Timeout(timeout),
        _ => Error::Io(e),
    }
}

/// One direction of the packet layer.
struct Dir {
    cipher: Cipher,
    mac: Option<Mac>,
    /// chacha20-poly1305, which replaces both.
    aead: Option<crypto::ChaChaPoly>,
    block: usize,
    seq: u32,
}

impl Dir {
    fn plain() -> Dir {
        Dir {
            cipher: Cipher::None,
            mac: None,
            aead: None,
            block: 8,
            seq: 0,
        }
    }
}

/// A channel's state on our side.
#[derive(Default)]
struct Chan {
    remote: u32,
    /// What we may still send, and the largest packet the server takes.
    out_window: u32,
    out_max: u32,
    /// Bytes read by the caller since we last widened the server's window.
    consumed: u32,
    data: VecDeque<u8>,
    ext: VecDeque<u8>,
    opened: Option<std::result::Result<(), String>>,
    replies: VecDeque<bool>,
    eof: bool,
    closed: bool,
    close_sent: bool,
    exit_status: Option<u32>,
}

/// The algorithms a key exchange settled on.
struct Algs {
    kex: String,
    host_key: String,
    cipher_out: String,
    cipher_in: String,
    mac_out: String,
    mac_in: String,
}

/// One SSH-2 connection: the packet layer, the keys, and its channels.
pub struct Link {
    tcp: TcpStream,
    timeout: Duration,
    out: Dir,
    inp: Dir,
    rbuf: Vec<u8>,
    v_c: Vec<u8>,
    v_s: Vec<u8>,
    session_id: Option<Vec<u8>>,
    pub negotiated: Negotiated,
    pub host_key: Option<HostKey>,
    server_sig_algs: Vec<String>,
    channels: HashMap<u32, Chan>,
    next_id: u32,
    closed: bool,
    /// Strict key exchange (OpenSSH's Terrapin countermeasure): both sides offered it.
    strict: bool,
    /// Inside the first key exchange, where strict mode allows no stray messages.
    first_kex: bool,
}

pub type Shared = Arc<Mutex<Link>>;

pub fn lock(m: &Mutex<Link>) -> MutexGuard<'_, Link> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Read the server's version line (lines before it are allowed, RFC 4253 §4.2).
fn read_version(tcp: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>> {
    let end = Instant::now() + timeout;
    loop {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            if Instant::now() > end {
                return Err(Error::Timeout(timeout));
            }
            match tcp.read(&mut b) {
                Ok(0) => {
                    return Err(bad(
                        "the server closed the connection before its version line",
                    ))
                }
                Ok(_) => {}
                Err(e) => return Err(io_err(e, timeout)),
            }
            if b[0] == b'\n' {
                break;
            }
            if line.len() > 1024 {
                return Err(bad(
                    "the server sent an overlong line instead of its version",
                ));
            }
            line.push(b[0]);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.starts_with(b"SSH-") {
            return Ok(line);
        }
    }
}

impl Link {
    /// Exchange versions and keys on `tcp`; the host key is left for the caller to check.
    pub fn handshake(mut tcp: TcpStream, timeout: Duration) -> Result<Link> {
        tcp.set_read_timeout(Some(timeout))?;
        tcp.write_all(format!("{VERSION}\r\n").as_bytes())?;
        let v_s = read_version(&mut tcp, timeout)?;
        let banner = String::from_utf8_lossy(&v_s).into_owned();
        if !(banner.starts_with("SSH-2.") || banner.starts_with("SSH-1.99")) {
            return Err(bad(format!("{banner} speaks only SSH-1")));
        }
        let mut link = Link {
            tcp,
            timeout,
            out: Dir::plain(),
            inp: Dir::plain(),
            rbuf: Vec::new(),
            v_c: VERSION.as_bytes().to_vec(),
            v_s,
            session_id: None,
            negotiated: Negotiated {
                banner,
                ..Negotiated::default()
            },
            host_key: None,
            server_sig_algs: Vec::new(),
            channels: HashMap::new(),
            next_id: 0,
            closed: false,
            strict: false,
            first_kex: true,
        };
        link.kex(None)?;
        Ok(link)
    }

    // The packet layer.

    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let bs = self.out.block.max(8);
        // An AEAD leaves the length out of the padded part.
        let counted = if self.out.aead.is_some() { 1 } else { 5 };
        let mut pad = bs - (counted + payload.len()) % bs;
        if pad < 4 {
            pad += bs;
        }
        let mut pkt = Vec::with_capacity(5 + payload.len() + pad + 64);
        put_u32(&mut pkt, (1 + payload.len() + pad) as u32);
        pkt.push(pad as u8);
        pkt.extend_from_slice(payload);
        let at = pkt.len();
        pkt.resize(at + pad, 0);
        // A user-space CSPRNG: a system call per packet slowed bulk transfers.
        rand::thread_rng().fill_bytes(&mut pkt[at..]);
        if let Some(a) = &self.out.aead {
            a.seal(self.out.seq, &mut pkt);
            self.tcp.write_all(&pkt)?;
            self.out.seq = self.out.seq.wrapping_add(1);
            return Ok(());
        }
        let mac = self.out.mac.as_ref().map(|m| m.sign(self.out.seq, &pkt));
        self.out.cipher.apply(&mut pkt);
        if let Some(m) = mac {
            pkt.extend_from_slice(&m);
        }
        self.tcp.write_all(&pkt)?;
        self.out.seq = self.out.seq.wrapping_add(1);
        Ok(())
    }

    fn fill(&mut self, n: usize) -> Result<()> {
        let mut buf = [0u8; 65536];
        while self.rbuf.len() < n {
            match self.tcp.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    return Err(bad("the server closed the connection"));
                }
                Ok(k) => self.rbuf.extend_from_slice(&buf[..k]),
                Err(e) => return Err(io_err(e, self.timeout)),
            }
        }
        Ok(())
    }

    /// Whether a packet's bytes are already here, so reading one will not wait on the socket.
    pub fn buffered(&self) -> bool {
        !self.rbuf.is_empty()
    }

    /// The next packet's payload, as it came (transport messages included).
    fn recv_raw(&mut self) -> Result<Vec<u8>> {
        self.tcp.set_read_timeout(Some(self.timeout))?;
        if self.inp.aead.is_some() {
            return self.recv_aead();
        }
        let bs = self.inp.block.max(8);
        self.fill(bs)?;
        let mut first = self.rbuf[..bs].to_vec();
        self.inp.cipher.apply(&mut first);
        let len = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if !(5..=MAX_LEN).contains(&len)
            || !(4 + len).is_multiple_of(bs) && !matches!(self.inp.cipher, Cipher::None)
        {
            return Err(bad(
                "a packet from the server is malformed (wrong keys or a broken connection)",
            ));
        }
        let mac_len = self.inp.mac.as_ref().map_or(0, Mac::size);
        let total = 4 + len;
        self.fill(total + mac_len)?;
        let mut pkt = first;
        let mut rest = self.rbuf[bs.min(total)..total].to_vec();
        self.inp.cipher.apply(&mut rest);
        pkt.extend_from_slice(&rest);
        if let Some(m) = &self.inp.mac {
            use subtle::ConstantTimeEq;
            let want = m.sign(self.inp.seq, &pkt);
            if !bool::from(want.ct_eq(&self.rbuf[total..total + mac_len])) {
                return Err(bad("a packet from the server failed its integrity check"));
            }
        }
        self.rbuf.drain(..total + mac_len);
        self.inp.seq = self.inp.seq.wrapping_add(1);
        let pad = pkt[4] as usize;
        if pad + 1 > len {
            return Err(bad("a packet from the server has bad padding"));
        }
        Ok(pkt[5..4 + len - pad].to_vec())
    }

    fn recv_aead(&mut self) -> Result<Vec<u8>> {
        self.fill(4)?;
        let a = self.inp.aead.as_ref().expect("an AEAD");
        let len = a.length(self.inp.seq, &self.rbuf[..4]) as usize;
        if !(5..=MAX_LEN).contains(&len) || !len.is_multiple_of(8) {
            return Err(bad(
                "a packet from the server is malformed (wrong keys or a broken connection)",
            ));
        }
        self.fill(4 + len + 16)?;
        let a = self.inp.aead.as_ref().expect("an AEAD");
        let (pkt, tag) = self.rbuf[..4 + len + 16].split_at_mut(4 + len);
        if !a.open(self.inp.seq, pkt, tag) {
            return Err(bad("a packet from the server failed its integrity check"));
        }
        self.inp.seq = self.inp.seq.wrapping_add(1);
        let pad = self.rbuf[4] as usize;
        if pad + 1 > len {
            return Err(bad("a packet from the server has bad padding"));
        }
        let payload = self.rbuf[5..4 + len - pad].to_vec();
        self.rbuf.drain(..4 + len + 16);
        Ok(payload)
    }

    /// The next packet that is not transport chatter; a rekey runs on the way.
    fn recv(&mut self) -> Result<Vec<u8>> {
        loop {
            let p = self.recv_raw()?;
            match p.first().copied() {
                Some(msg::IGNORE | msg::DEBUG | msg::UNIMPLEMENTED) => {}
                Some(msg::DISCONNECT) => {
                    self.closed = true;
                    let mut r = Reader::new(&p[1..]);
                    let _ = r.u32();
                    let why = r.string().map(|s| String::from_utf8_lossy(s).into_owned());
                    return Err(Error::Protocol(format!(
                        "the server disconnected: {}",
                        why.unwrap_or_default()
                    )));
                }
                Some(msg::KEXINIT) if self.session_id.is_some() => self.kex(Some(p))?,
                Some(msg::EXT_INFO) => self.ext_info(&p)?,
                Some(_) => return Ok(p),
                None => return Err(bad("an empty packet from the server")),
            }
        }
    }

    fn ext_info(&mut self, p: &[u8]) -> Result<()> {
        let mut r = Reader::new(&p[1..]);
        for _ in 0..r.u32()? {
            let name = r.string()?;
            let value = r.string()?;
            if name == b"server-sig-algs" {
                self.server_sig_algs = String::from_utf8_lossy(value)
                    .split(',')
                    .map(str::to_string)
                    .collect();
            }
        }
        Ok(())
    }

    // Key exchange.

    fn kexinit(first: bool) -> Vec<u8> {
        let mut p = vec![msg::KEXINIT];
        let mut cookie = [0u8; 16];
        OsRng.fill_bytes(&mut cookie);
        p.extend_from_slice(&cookie);
        let mut kex = crypto::KEX.to_vec();
        if first {
            // Only in the first KEXINIT, as both markers are defined.
            kex.push("ext-info-c");
            kex.push("kex-strict-c-v00@openssh.com");
        }
        put_name_list(&mut p, &kex);
        put_name_list(&mut p, crypto::HOST_KEY);
        for _ in 0..2 {
            put_name_list(&mut p, crypto::CIPHER);
        }
        for _ in 0..2 {
            put_name_list(&mut p, crypto::MAC);
        }
        for _ in 0..2 {
            put_name_list(&mut p, &["none"]);
        }
        for _ in 0..2 {
            put_name_list(&mut p, &[]);
        }
        put_bool(&mut p, false);
        put_u32(&mut p, 0);
        p
    }

    fn pick(ours: &[&str], theirs: &[String], what: &str) -> Result<String> {
        ours.iter()
            .find(|a| theirs.iter().any(|t| t == *a))
            .map(|a| a.to_string())
            .ok_or_else(|| {
                bad(format!(
                    "no {what} in common with the server (it offers {})",
                    theirs.join(", ")
                ))
            })
    }

    /// Run a key exchange; `theirs` is the server's KEXINIT when it started one.
    fn kex(&mut self, theirs: Option<Vec<u8>>) -> Result<()> {
        let first = self.session_id.is_none();
        let i_c = Self::kexinit(first);
        self.send(&i_c)?;
        let mut stray = false;
        let i_s = match theirs {
            Some(p) => p,
            None => loop {
                let p = self.recv_raw()?;
                match p.first().copied() {
                    Some(msg::KEXINIT) => break p,
                    Some(msg::IGNORE | msg::DEBUG) => stray = true,
                    _ => return Err(bad("the server did not start the key exchange")),
                }
            },
        };
        if i_s.len() < 17 {
            return Err(bad("the server's key exchange offer is cut short"));
        }
        let mut r = Reader::new(&i_s[17..]);
        let kex = r.name_list()?;
        if first {
            self.strict = kex.iter().any(|k| k == "kex-strict-s-v00@openssh.com");
            if self.strict && stray {
                return Err(bad(
                    "strict key exchange: the server sent a message before its offer",
                ));
            }
        }
        let host = r.name_list()?;
        let (c_cs, c_sc, m_cs, m_sc) = (
            r.name_list()?,
            r.name_list()?,
            r.name_list()?,
            r.name_list()?,
        );
        let (z_cs, z_sc) = (r.name_list()?, r.name_list()?);
        let _ = (r.name_list()?, r.name_list()?);
        let guessed = r.bool()?;
        if !z_cs.iter().any(|z| z == "none") || !z_sc.iter().any(|z| z == "none") {
            return Err(bad(
                "the server insists on compression, which legcon does not do",
            ));
        }
        let algs = Algs {
            kex: Self::pick(crypto::KEX, &kex, "key exchange")?,
            host_key: Self::pick(crypto::HOST_KEY, &host, "host key type")?,
            cipher_out: Self::pick(crypto::CIPHER, &c_cs, "cipher")?,
            cipher_in: Self::pick(crypto::CIPHER, &c_sc, "cipher")?,
            mac_out: Self::pick(crypto::MAC, &m_cs, "MAC")?,
            mac_in: Self::pick(crypto::MAC, &m_sc, "MAC")?,
        };
        // A guessed first packet is ours to drop when the guess was wrong.
        if guessed && (kex.first() != Some(&algs.kex) || host.first() != Some(&algs.host_key)) {
            self.recv_raw()?;
        }
        let hash = if algs.kex.ends_with("sha1") {
            Hash::Sha1
        } else {
            Hash::Sha256
        };
        let mut h = Vec::new();
        put_string(&mut h, &self.v_c);
        put_string(&mut h, &self.v_s);
        put_string(&mut h, &i_c);
        put_string(&mut h, &i_s);
        let (k_s, k, sig) = if algs.kex.starts_with("curve25519") {
            let c = Curve::new();
            let mut p = vec![msg::KEX_INIT];
            put_string(&mut p, &c.public);
            self.send(&p)?;
            let reply = self.expect(msg::KEX_REPLY, "the key exchange")?;
            let mut r = Reader::new(&reply[1..]);
            let (k_s, q_s, sig) = (
                r.string()?.to_vec(),
                r.string()?.to_vec(),
                r.string()?.to_vec(),
            );
            let k = c.shared(&q_s)?;
            put_string(&mut h, &k_s);
            put_string(&mut h, &c.public);
            put_string(&mut h, &q_s);
            (k_s, k, sig)
        } else if algs.kex.contains("group-exchange") {
            let mut p = vec![msg::GEX_REQUEST];
            for v in [1024u32, 2048, 8192] {
                put_u32(&mut p, v);
            }
            self.send(&p)?;
            let group = self.expect(msg::GEX_GROUP, "the key exchange")?;
            let mut r = Reader::new(&group[1..]);
            let (pp, g) = (r.string()?.to_vec(), r.string()?.to_vec());
            let (p_n, g_n) = (BigUint::from_bytes_be(&pp), BigUint::from_bytes_be(&g));
            // The sizes asked for: a smaller group is weak, a far larger one a stall.
            if !(1024..=8192).contains(&p_n.bits()) || g_n <= BigUint::from(1u8) || g_n >= p_n {
                return Err(bad(format!(
                    "the server offered an unusable Diffie-Hellman group ({} bits)",
                    p_n.bits()
                )));
            }
            let dh = Dh::new(p_n, &g_n);
            let mut p = vec![msg::GEX_INIT];
            put_mpint(&mut p, &dh.e.to_bytes_be());
            self.send(&p)?;
            let reply = self.expect(msg::GEX_REPLY, "the key exchange")?;
            let mut r = Reader::new(&reply[1..]);
            let (k_s, f, sig) = (
                r.string()?.to_vec(),
                r.string()?.to_vec(),
                r.string()?.to_vec(),
            );
            let k = dh.shared(&BigUint::from_bytes_be(&f))?;
            put_string(&mut h, &k_s);
            for v in [1024u32, 2048, 8192] {
                put_u32(&mut h, v);
            }
            put_mpint(&mut h, &pp);
            put_mpint(&mut h, &g);
            put_mpint(&mut h, &dh.e.to_bytes_be());
            put_mpint(&mut h, &f);
            (k_s, k, sig)
        } else {
            let group = if algs.kex.contains("group1-") {
                crypto::group1()
            } else {
                crypto::group14()
            };
            let dh = Dh::new(group, &BigUint::from(2u8));
            let mut p = vec![msg::KEX_INIT];
            put_mpint(&mut p, &dh.e.to_bytes_be());
            self.send(&p)?;
            let reply = self.expect(msg::KEX_REPLY, "the key exchange")?;
            let mut r = Reader::new(&reply[1..]);
            let (k_s, f, sig) = (
                r.string()?.to_vec(),
                r.string()?.to_vec(),
                r.string()?.to_vec(),
            );
            let k = dh.shared(&BigUint::from_bytes_be(&f))?;
            put_string(&mut h, &k_s);
            put_mpint(&mut h, &dh.e.to_bytes_be());
            put_mpint(&mut h, &f);
            (k_s, k, sig)
        };
        let mut k_enc = Vec::new();
        put_mpint(&mut k_enc, &k.to_bytes_be());
        h.extend_from_slice(&k_enc);
        let hh = hash.digest(&h);
        crypto::verify_host(&k_s, &sig, &hh)?;
        match &self.host_key {
            Some(known) if known.blob != k_s => {
                return Err(bad("the server's host key changed during the connection"))
            }
            Some(_) => {}
            None => self.host_key = Some(crate::host_key_of(&k_s)),
        }
        let session_id = self.session_id.get_or_insert_with(|| hh.clone()).clone();
        self.send(&[msg::NEWKEYS])?;
        let derive = |letter: u8, len: usize| -> Vec<u8> {
            let mut buf = k_enc.clone();
            buf.extend_from_slice(&hh);
            buf.push(letter);
            buf.extend_from_slice(&session_id);
            let mut key = hash.digest(&buf);
            while key.len() < len {
                let mut more = k_enc.clone();
                more.extend_from_slice(&hh);
                more.extend_from_slice(&key);
                key.extend_from_slice(&hash.digest(&more));
            }
            key.truncate(len);
            key
        };
        let (kl_o, il_o, bs_o) = crypto::cipher_sizes(&algs.cipher_out);
        let (kl_i, il_i, bs_i) = crypto::cipher_sizes(&algs.cipher_in);
        let out = Dir {
            cipher: if algs.cipher_out == crypto::CHACHA {
                Cipher::None
            } else {
                Cipher::new(
                    &algs.cipher_out,
                    &derive(b'C', kl_o),
                    &derive(b'A', il_o),
                    true,
                )?
            },
            mac: if algs.cipher_out == crypto::CHACHA {
                None
            } else {
                Some(Mac::new(
                    &algs.mac_out,
                    &derive(b'E', crypto::mac_key_len(&algs.mac_out)),
                )?)
            },
            aead: if algs.cipher_out == crypto::CHACHA {
                Some(crypto::ChaChaPoly::new(&derive(b'C', 64))?)
            } else {
                None
            },
            block: bs_o,
            // Strict mode restarts the counts at each NEWKEYS, so no message can be slipped in.
            seq: if self.strict { 0 } else { self.out.seq },
        };
        self.out = out;
        self.expect(msg::NEWKEYS, "the key exchange")?;
        let inp = Dir {
            cipher: if algs.cipher_in == crypto::CHACHA {
                Cipher::None
            } else {
                Cipher::new(
                    &algs.cipher_in,
                    &derive(b'D', kl_i),
                    &derive(b'B', il_i),
                    false,
                )?
            },
            mac: if algs.cipher_in == crypto::CHACHA {
                None
            } else {
                Some(Mac::new(
                    &algs.mac_in,
                    &derive(b'F', crypto::mac_key_len(&algs.mac_in)),
                )?)
            },
            aead: if algs.cipher_in == crypto::CHACHA {
                Some(crypto::ChaChaPoly::new(&derive(b'D', 64))?)
            } else {
                None
            },
            block: bs_i,
            seq: if self.strict { 0 } else { self.inp.seq },
        };
        self.inp = inp;
        self.first_kex = false;
        self.negotiated.kex = algs.kex;
        self.negotiated.host_key = algs.host_key;
        self.negotiated.mac = if algs.cipher_out == crypto::CHACHA {
            "built into the cipher".into()
        } else {
            algs.mac_out
        };
        self.negotiated.cipher = algs.cipher_out;
        self.negotiated.strict = self.strict;
        Ok(())
    }

    /// The next key exchange or authentication message, which must be `want`.
    fn expect(&mut self, want: u8, during: &str) -> Result<Vec<u8>> {
        loop {
            let p = self.recv_raw()?;
            match p.first().copied() {
                Some(t) if t == want => return Ok(p),
                Some(msg::IGNORE | msg::DEBUG) if self.strict && self.first_kex => {
                    return Err(bad(format!(
                        "strict key exchange: the server sent a stray message during {during}"
                    )))
                }
                Some(msg::IGNORE | msg::DEBUG) => {}
                Some(msg::EXT_INFO) => self.ext_info(&p)?,
                Some(msg::DISCONNECT) => {
                    self.closed = true;
                    return Err(bad(format!("the server disconnected during {during}")));
                }
                Some(t) => {
                    return Err(bad(format!(
                        "the server sent message {t} during {during}, expecting {want}"
                    )))
                }
                None => return Err(bad("an empty packet from the server")),
            }
        }
    }

    // User authentication.

    /// Ask for the user-auth service, then which methods the server takes for `user`.
    pub fn auth_methods(&mut self, user: &str) -> Result<(bool, Vec<String>)> {
        let mut p = vec![msg::SERVICE_REQUEST];
        put_string(&mut p, b"ssh-userauth");
        self.send(&p)?;
        self.expect(msg::SERVICE_ACCEPT, "login")?;
        self.auth_request(user, "none", &[])?;
        self.auth_reply()
    }

    fn auth_request(&mut self, user: &str, method: &str, rest: &[u8]) -> Result<()> {
        let mut p = vec![msg::USERAUTH_REQUEST];
        put_string(&mut p, user.as_bytes());
        put_string(&mut p, b"ssh-connection");
        put_string(&mut p, method.as_bytes());
        p.extend_from_slice(rest);
        self.send(&p)
    }

    /// Success, or failure with the methods that can continue.
    fn auth_reply(&mut self) -> Result<(bool, Vec<String>)> {
        loop {
            let p = self.recv()?;
            match p[0] {
                msg::USERAUTH_SUCCESS => return Ok((true, Vec::new())),
                msg::USERAUTH_FAILURE => {
                    let mut r = Reader::new(&p[1..]);
                    return Ok((false, r.name_list()?));
                }
                msg::USERAUTH_BANNER => {}
                msg::USERAUTH_INFO_REQUEST => {
                    return Err(Error::Auth(
                        "the server wants the password changed first; change it on the machine"
                            .into(),
                    ))
                }
                t => return Err(bad(format!("unexpected message {t} during login"))),
            }
        }
    }

    pub fn auth_password(&mut self, user: &str, password: &str) -> Result<bool> {
        let mut rest = Vec::new();
        put_bool(&mut rest, false);
        put_string(&mut rest, password.as_bytes());
        self.auth_request(user, "password", &rest)?;
        Ok(self.auth_reply()?.0)
    }

    /// keyboard-interactive, answering every prompt with the password (PAM's "Password:").
    pub fn auth_keyboard(&mut self, user: &str, password: &str) -> Result<bool> {
        let mut rest = Vec::new();
        put_string(&mut rest, b"");
        put_string(&mut rest, b"");
        self.auth_request(user, "keyboard-interactive", &rest)?;
        for _ in 0..8 {
            let p = self.recv()?;
            match p[0] {
                msg::USERAUTH_SUCCESS => return Ok(true),
                msg::USERAUTH_FAILURE => return Ok(false),
                msg::USERAUTH_BANNER => {}
                msg::USERAUTH_INFO_REQUEST => {
                    let mut r = Reader::new(&p[1..]);
                    let _ = (r.string()?, r.string()?, r.string()?);
                    let n = r.u32()?;
                    let mut reply = vec![msg::USERAUTH_INFO_RESPONSE];
                    put_u32(&mut reply, n);
                    for _ in 0..n {
                        let _ = (r.string()?, r.bool()?);
                        put_string(&mut reply, password.as_bytes());
                    }
                    self.send(&reply)?;
                }
                t => return Err(bad(format!("unexpected message {t} during login"))),
            }
        }
        Ok(false)
    }

    pub fn auth_key(&mut self, user: &str, key: &Signer) -> Result<bool> {
        let algorithm = key.algorithm(&self.server_sig_algs);
        self.auth_publickey(user, algorithm, &key.blob(), &mut |data| {
            key.sign(algorithm, data)
        })
    }

    /// The algorithms the server says it verifies signatures with (`server-sig-algs`).
    pub fn server_sig_algs(&self) -> &[String] {
        &self.server_sig_algs
    }

    /// Public key login as OpenSSH does it: ask whether the server takes the key, and only
    /// then have `sign` sign (an agent may prompt for that).
    pub fn auth_publickey(
        &mut self,
        user: &str,
        algorithm: &str,
        blob: &[u8],
        sign: &mut dyn FnMut(&[u8]) -> Result<Vec<u8>>,
    ) -> Result<bool> {
        let mut query = Vec::new();
        put_bool(&mut query, false);
        put_string(&mut query, algorithm.as_bytes());
        put_string(&mut query, blob);
        self.auth_request(user, "publickey", &query)?;
        loop {
            let p = self.recv()?;
            match p[0] {
                msg::USERAUTH_PK_OK => break,
                msg::USERAUTH_FAILURE => return Ok(false),
                msg::USERAUTH_SUCCESS => return Ok(true),
                msg::USERAUTH_BANNER => {}
                t => return Err(bad(format!("unexpected message {t} during key login"))),
            }
        }
        let mut rest = Vec::new();
        put_bool(&mut rest, true);
        put_string(&mut rest, algorithm.as_bytes());
        put_string(&mut rest, blob);
        let mut signed = Vec::new();
        put_string(&mut signed, self.session_id.as_deref().unwrap_or_default());
        put_u8(&mut signed, msg::USERAUTH_REQUEST);
        put_string(&mut signed, user.as_bytes());
        put_string(&mut signed, b"ssh-connection");
        put_string(&mut signed, b"publickey");
        signed.extend_from_slice(&rest);
        put_string(&mut rest, &sign(&signed)?);
        self.auth_request(user, "publickey", &rest)?;
        Ok(self.auth_reply()?.0)
    }

    // Channels.

    /// Read one packet and act on it: data into its channel, replies noted, requests refused.
    pub fn pump(&mut self) -> Result<()> {
        let p = self.recv()?;
        let mut r = Reader::new(&p[1..]);
        match p[0] {
            msg::GLOBAL_REQUEST => {
                let _ = r.string()?;
                if r.bool()? {
                    self.send(&[msg::REQUEST_FAILURE])?;
                }
            }
            msg::CHANNEL_OPEN => {
                // X11 or agent forwarding, which legcon never asks for.
                let _ = r.string()?;
                let theirs = r.u32()?;
                let mut f = vec![msg::CHANNEL_OPEN_FAILURE];
                put_u32(&mut f, theirs);
                put_u32(&mut f, 1);
                put_string(&mut f, b"not offered");
                put_string(&mut f, b"");
                self.send(&f)?;
            }
            t @ msg::CHANNEL_OPEN_CONFIRMATION..=msg::CHANNEL_FAILURE => {
                let id = r.u32()?;
                let mut reply = None;
                let Some(c) = self.channels.get_mut(&id) else {
                    return Ok(());
                };
                match t {
                    msg::CHANNEL_OPEN_CONFIRMATION => {
                        c.remote = r.u32()?;
                        c.out_window = r.u32()?;
                        c.out_max = r.u32()?;
                        c.opened = Some(Ok(()));
                    }
                    msg::CHANNEL_OPEN_FAILURE => {
                        let _ = r.u32()?;
                        let why = String::from_utf8_lossy(r.string()?).into_owned();
                        c.opened = Some(Err(why));
                    }
                    msg::CHANNEL_WINDOW_ADJUST => {
                        c.out_window = c.out_window.saturating_add(r.u32()?)
                    }
                    msg::CHANNEL_DATA => c.data.extend(r.string()?),
                    msg::CHANNEL_EXTENDED_DATA => {
                        if r.u32()? == 1 {
                            c.ext.extend(r.string()?);
                        }
                    }
                    msg::CHANNEL_EOF => c.eof = true,
                    msg::CHANNEL_CLOSE => {
                        c.eof = true;
                        c.closed = true;
                        if !c.close_sent {
                            c.close_sent = true;
                            let mut m = vec![msg::CHANNEL_CLOSE];
                            put_u32(&mut m, c.remote);
                            reply = Some(m);
                        }
                    }
                    msg::CHANNEL_REQUEST => {
                        let name = r.string()?.to_vec();
                        let want = r.bool()?;
                        if name == b"exit-status" {
                            c.exit_status = Some(r.u32()?);
                        }
                        if want {
                            let mut m = vec![msg::CHANNEL_FAILURE];
                            put_u32(&mut m, c.remote);
                            reply = Some(m);
                        }
                    }
                    msg::CHANNEL_SUCCESS => c.replies.push_back(true),
                    _ => c.replies.push_back(false),
                }
                if let Some(m) = reply {
                    self.send(&m)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn chan(&mut self, id: u32) -> Result<&mut Chan> {
        self.channels.get_mut(&id).ok_or(Error::NotConnected)
    }

    /// Open a session channel; its id.
    pub fn open_session(&mut self) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        self.channels.insert(id, Chan::default());
        let mut p = vec![msg::CHANNEL_OPEN];
        put_string(&mut p, b"session");
        put_u32(&mut p, id);
        put_u32(&mut p, WINDOW);
        put_u32(&mut p, MAX_PACKET);
        self.send(&p)?;
        loop {
            if let Some(r) = self.chan(id)?.opened.take() {
                return r.map(|()| id).map_err(|why| {
                    self.channels.remove(&id);
                    bad(format!("the server would not open a channel: {why}"))
                });
            }
            self.pump()?;
        }
    }

    /// A channel request (`pty-req`, `shell`, `exec`, `subsystem`, `window-change`).
    pub fn request(&mut self, id: u32, name: &str, data: &[u8], want_reply: bool) -> Result<bool> {
        let remote = self.chan(id)?.remote;
        let mut p = vec![msg::CHANNEL_REQUEST];
        put_u32(&mut p, remote);
        put_string(&mut p, name.as_bytes());
        put_bool(&mut p, want_reply);
        p.extend_from_slice(data);
        self.send(&p)?;
        if !want_reply {
            return Ok(true);
        }
        loop {
            let c = self.chan(id)?;
            if let Some(ok) = c.replies.pop_front() {
                return Ok(ok);
            }
            if c.closed {
                return Ok(false);
            }
            self.pump()?;
        }
    }

    /// Send all of `data`, waiting for the server to widen the window when it is full.
    pub fn write(&mut self, id: u32, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let c = self.chan(id)?;
            if c.closed {
                return Err(Error::Io(ErrorKind::BrokenPipe.into()));
            }
            let room = c.out_window.min(c.out_max.saturating_sub(64).max(1024)) as usize;
            if room == 0 || c.out_window == 0 {
                self.pump()?;
                continue;
            }
            let n = room.min(data.len());
            c.out_window -= n as u32;
            let mut p = vec![msg::CHANNEL_DATA];
            put_u32(&mut p, c.remote);
            put_string(&mut p, &data[..n]);
            self.send(&p)?;
            data = &data[n..];
        }
        Ok(())
    }

    /// Take what is waiting (output, or errors with `ext`); `None` when nothing is yet.
    pub fn take(&mut self, id: u32, buf: &mut [u8], ext: bool) -> Result<Option<usize>> {
        let c = self.chan(id)?;
        let q = if ext { &mut c.ext } else { &mut c.data };
        if q.is_empty() {
            return Ok(if c.eof { Some(0) } else { None });
        }
        let n = buf.len().min(q.len());
        let (a, b) = q.as_slices();
        let k = a.len().min(n);
        buf[..k].copy_from_slice(&a[..k]);
        buf[k..n].copy_from_slice(&b[..n - k]);
        q.drain(..n);
        c.consumed += n as u32;
        if c.consumed >= WINDOW / 2 && !c.closed {
            let mut p = vec![msg::CHANNEL_WINDOW_ADJUST];
            put_u32(&mut p, c.remote);
            put_u32(&mut p, c.consumed);
            c.consumed = 0;
            self.send(&p)?;
        }
        Ok(Some(n))
    }

    /// Read from a channel, waiting for the server.
    pub fn read(&mut self, id: u32, buf: &mut [u8], ext: bool) -> Result<usize> {
        loop {
            if let Some(n) = self.take(id, buf, ext)? {
                return Ok(n);
            }
            self.pump()?;
        }
    }

    pub fn eof(&mut self, id: u32) -> Result<bool> {
        Ok(self.chan(id)?.eof)
    }

    pub fn send_eof(&mut self, id: u32) -> Result<()> {
        let c = self.chan(id)?;
        if c.closed {
            return Ok(());
        }
        let mut p = vec![msg::CHANNEL_EOF];
        put_u32(&mut p, c.remote);
        self.send(&p)
    }

    /// Close a channel and wait (up to `limit`) for the server's close; then forget it.
    pub fn close(&mut self, id: u32, limit: Duration) -> Result<Option<u32>> {
        let end = Instant::now() + limit;
        let c = self.chan(id)?;
        if !c.close_sent {
            c.close_sent = true;
            let mut p = vec![msg::CHANNEL_CLOSE];
            put_u32(&mut p, c.remote);
            self.send(&p)?;
        }
        while !self.chan(id)?.closed && Instant::now() < end {
            if let Err(e) = self.pump() {
                self.channels.remove(&id);
                return Err(e);
            }
        }
        Ok(self.channels.remove(&id).and_then(|c| c.exit_status))
    }

    /// Wait for the command on a channel to end; its exit status.
    pub fn wait_close(&mut self, id: u32) -> Result<Option<u32>> {
        while !self.chan(id)?.closed {
            self.pump()?;
        }
        self.close(id, Duration::from_secs(1))
    }

    /// Log off: a polite DISCONNECT, then the socket goes.
    pub fn disconnect(&mut self, why: &str) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut p = vec![msg::DISCONNECT];
        put_u32(&mut p, 11);
        put_string(&mut p, why.as_bytes());
        put_string(&mut p, b"");
        let r = self.send(&p);
        self.closed = true;
        let _ = self.tcp.shutdown(Shutdown::Both);
        r
    }

    pub fn tcp(&self) -> Result<TcpStream> {
        Ok(self.tcp.try_clone()?)
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }
}

/// A channel of a shared link, as a byte stream: SFTP and SCP run over it.
pub struct Channel {
    link: Shared,
    id: u32,
}

impl Channel {
    pub fn new(link: Shared, id: u32) -> Channel {
        Channel { link, id }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn link(&self) -> &Shared {
        &self.link
    }

    pub fn read_stderr(&mut self, buf: &mut [u8]) -> Result<usize> {
        lock(&self.link).read(self.id, buf, true)
    }

    pub fn send_eof(&mut self) -> Result<()> {
        lock(&self.link).send_eof(self.id)
    }

    pub fn close(&mut self) -> Result<Option<u32>> {
        lock(&self.link).close(self.id, Duration::from_secs(2))
    }

    pub fn wait_close(&mut self) -> Result<Option<u32>> {
        lock(&self.link).wait_close(self.id)
    }
}

fn to_io(e: Error) -> std::io::Error {
    match e {
        Error::Io(e) => e,
        Error::Timeout(_) => ErrorKind::TimedOut.into(),
        e => std::io::Error::other(e.to_string()),
    }
}

impl Read for Channel {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        lock(&self.link).read(self.id, buf, false).map_err(to_io)
    }
}

impl Write for Channel {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        lock(&self.link).write(self.id, buf).map_err(to_io)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
