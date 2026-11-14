//! SMB1 for legcon (docs/PLAN.md §4.1): file sharing with Windows for Workgroups, 95 / 98,
//! NT 3.5x / 4, OS/2 and Samba, over NetBIOS on port 139.
//!
//! No Rust crate speaks SMB1, so this is written here from MS-CIFS. Choices that keep 1990s
//! servers happy:
//! - **Dialects** LANMAN1.0 to NT LM 0.12; everything after the negotiation uses commands both
//!   ends have had since LANMAN 1.0 (`OPEN_ANDX`, `READ_ANDX`, `WRITE_ANDX`, `TRANS2 FIND`
//!   level 1), so Windows 9x and NT take the same path.
//! - **No Unicode**: names travel in the machine's OEM character set.
//! - **Logins**: share-level (Windows 9x, a password per share) and user-level, with LM and
//!   NTLM responses, then NTLMv2 if the server refuses those (newer Samba).
//! - **Shares** are the top level of the path (`\C\WINDOWS`), listed with RAP `NetShareEnum`.
//! - **Disconnect** disconnects every tree and logs off, so Windows does not ask its user
//!   about a connected client at shutdown.

pub mod auth;
pub mod netbios;
pub mod wire;

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use lc_core::{
    Charset, ConnState, Connection, Entry, EntryKind, Error, FileCaps, FileSession, PathStyle,
    RemotePath, Result, TimePrecision, Timestamp, WriteMode,
};
use wire::{Buf, Message};

/// The default port (NetBIOS session service).
pub const PORT: u16 = 139;
/// SMB straight over TCP, without NetBIOS framing; Windows 2000 and later only.
pub const DIRECT_PORT: u16 = 445;

/// The dialects offered, oldest first.
pub const DIALECTS: [&str; 5] = [
    "PC NETWORK PROGRAM 1.0",
    "LANMAN1.0",
    "LM1.2X002",
    "LANMAN2.1",
    "NT LM 0.12",
];

#[derive(Debug, Clone)]
pub struct Options {
    pub charset: Charset,
    pub timeout: Duration,
    /// The name this computer gives in the session request.
    pub calling_name: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: Charset::Windows1252,
            timeout: Duration::from_secs(20),
            calling_name: "LEGCON".into(),
        }
    }
}

/// What the server agreed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub dialect: String,
    /// User-level security (a login); false means share-level (a password per share).
    pub user_level: bool,
    /// Passwords go as challenge responses, not in the clear.
    pub encrypt: bool,
    pub challenge: [u8; 8],
    pub max_buffer: usize,
    pub domain: String,
}

impl Negotiated {
    pub fn is_nt(&self) -> bool {
        self.dialect == "NT LM 0.12"
    }
}

pub struct SmbSession {
    stream: TcpStream,
    opts: Options,
    server: String,
    neg: Negotiated,
    uid: u16,
    mid: u16,
    password: String,
    trees: HashMap<String, u16>,
    state: ConnState,
    /// The login was accepted as the guest account.
    pub guest: bool,
}

fn io(e: std::io::Error, timeout: Duration) -> Error {
    match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => Error::Timeout(timeout),
        _ => Error::Io(e),
    }
}

fn open_tcp(addr: SocketAddr, opts: &Options) -> Result<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, opts.timeout).map_err(|e| io(e, opts.timeout))?;
    s.set_read_timeout(Some(opts.timeout))?;
    s.set_nodelay(true)?;
    Ok(s)
}

/// Connect, negotiate and log in. `user` may be `DOMAIN\user`; empty logs in anonymously.
pub fn connect(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    opts: &Options,
) -> Result<SmbSession> {
    let mut s = start(host, port, password, opts)?;
    s.login(user, password)?;
    s.state = ConnState::Connected;
    Ok(s)
}

/// Connect and negotiate only, without logging in: the server's name and what it offers.
pub fn probe(host: &str, port: u16, opts: &Options) -> Result<(String, Negotiated)> {
    let s = start(host, port, "", opts)?;
    Ok((s.server.clone(), s.neg.clone()))
}

fn start(host: &str, port: u16, password: &str, opts: &Options) -> Result<SmbSession> {
    let addr: SocketAddr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let mut stream = open_tcp(addr, opts)?;
    let mut server = netbios::ANY_SERVER.to_string();
    // Port 445 is SMB without NetBIOS (Windows 2000 on); everything older wants a session request.
    let netbios_session = port != DIRECT_PORT;
    if netbios_session && !netbios::session_request(&mut stream, &server, &opts.calling_name)? {
        // Windows 9x wants its own name: ask it, or fall back to the host name's first label.
        let name = netbios::server_name(addr.ip(), Duration::from_secs(2))
            .unwrap_or_else(|| host.split('.').next().unwrap_or(host).to_ascii_uppercase());
        stream = open_tcp(addr, opts)?;
        if !netbios::session_request(&mut stream, &name, &opts.calling_name)? {
            return Err(Error::Protocol(format!(
                "{host} refused a NetBIOS session as {name}"
            )));
        }
        server = name;
    }
    if server == netbios::ANY_SERVER {
        // Shares are named \\server\share; an address works there as well as a name does.
        server = if host.parse::<std::net::IpAddr>().is_ok() {
            host.to_string()
        } else {
            host.split('.').next().unwrap_or(host).to_ascii_uppercase()
        };
    }
    let mut s = SmbSession {
        stream,
        opts: opts.clone(),
        server,
        neg: Negotiated {
            dialect: String::new(),
            user_level: true,
            encrypt: true,
            challenge: [0; 8],
            max_buffer: 4096,
            domain: String::new(),
        },
        uid: 0,
        mid: 1,
        password: password.to_string(),
        trees: HashMap::new(),
        state: ConnState::Connecting,
        guest: false,
    };
    s.negotiate()?;
    Ok(s)
}

/// An open file; dropping it closes the file on the server.
struct Handle<'a> {
    s: &'a mut SmbSession,
    tid: u16,
    fid: u16,
    offset: u64,
}

impl Read for Handle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = buf.len().min(self.s.chunk()).min(0xffff);
        let words = Buf::default()
            .no_andx()
            .u16(self.fid)
            .u32(self.offset as u32)
            .u16(want as u16)
            .u16(want as u16)
            .u32(0)
            .u16(0)
            .0;
        let r = self
            .s
            .call(wire::READ_ANDX, self.tid, words, Vec::new())
            .map_err(to_io)?;
        let (len, at) = (usize::from(r.word(5)), usize::from(r.word(6)));
        let start = at.saturating_sub(r.bytes_offset());
        let data = r
            .bytes
            .get(start..start + len)
            .ok_or_else(|| to_io(Error::Protocol("short read".into())))?;
        buf[..len].copy_from_slice(data);
        self.offset += len as u64;
        Ok(len)
    }
}

impl Write for Handle<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.s.chunk()).min(0xffff);
        // The data follows one pad byte after the 12 parameter words.
        let data_offset = wire::HEADER + 1 + 24 + 2 + 1;
        let words = Buf::default()
            .no_andx()
            .u16(self.fid)
            .u32(self.offset as u32)
            .u32(0)
            .u16(0)
            .u16(0)
            .u16(0)
            .u16(n as u16)
            .u16(data_offset as u16)
            .0;
        let bytes = Buf::default().u8(0).raw(&buf[..n]).0;
        let r = self
            .s
            .call(wire::WRITE_ANDX, self.tid, words, bytes)
            .map_err(to_io)?;
        let wrote = usize::from(r.word(2));
        if wrote == 0 && n > 0 {
            return Err(std::io::Error::new(
                ErrorKind::WriteZero,
                "the server wrote nothing (disk full?)",
            ));
        }
        self.offset += wrote as u64;
        Ok(wrote)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Handle<'_> {
    fn drop(&mut self) {
        let words = Buf::default().u16(self.fid).u32(0xffff_ffff).0;
        let _ = self.s.call(wire::CLOSE, self.tid, words, Vec::new());
    }
}

fn to_io(e: Error) -> std::io::Error {
    match e {
        Error::Io(e) => e,
        other => std::io::Error::other(other.to_string()),
    }
}

/// The domain in a negotiate reply: NT sends it in UTF-16 even to a client that asked for none.
fn domain_name(b: &[u8], cs: Charset) -> String {
    if b.len() >= 2 && b[1] == 0 && b[0] != 0 {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        return String::from_utf16_lossy(&units);
    }
    cs.decode(wire::cstr(b, 0).0)
}

/// The server said there is nothing (more) to list.
fn no_more_files(m: &Message) -> bool {
    m.status == 0x8000_0006 || m.status == (1 | (18 << 16))
}

impl SmbSession {
    pub fn negotiated(&self) -> &Negotiated {
        &self.neg
    }

    /// The server's NetBIOS name.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// How much file data one message may carry.
    fn chunk(&self) -> usize {
        self.neg
            .max_buffer
            .saturating_sub(128)
            .clamp(512, 60 * 1024)
    }

    fn send(&mut self, m: &Message) -> Result<()> {
        netbios::send(&mut self.stream, netbios::SESSION_MESSAGE, &m.encode()).map_err(
            |e| match e {
                Error::Io(e) => io(e, self.opts.timeout),
                other => other,
            },
        )
    }

    fn recv(&mut self) -> Result<(Message, Vec<u8>)> {
        let (_, raw) = netbios::recv(&mut self.stream).map_err(|e| match e {
            Error::Io(e) => io(e, self.opts.timeout),
            other => other,
        })?;
        Ok((Message::decode(&raw)?, raw))
    }

    /// Send one request and read its reply; a failed status comes back as the reply, not an error.
    fn exchange(
        &mut self,
        command: u8,
        tid: u16,
        words: Vec<u8>,
        bytes: Vec<u8>,
    ) -> Result<(Message, Vec<u8>)> {
        let mut m = Message::new(command);
        m.tid = tid;
        m.uid = self.uid;
        m.mid = self.mid;
        self.mid = self.mid.wrapping_add(1).max(1);
        m.words = words;
        m.bytes = bytes;
        self.send(&m)?;
        loop {
            let (r, raw) = self.recv()?;
            if r.mid == m.mid {
                return Ok((r, raw));
            }
        }
    }

    /// Like [`Self::exchange`], with a failed status as an error.
    fn call(&mut self, command: u8, tid: u16, words: Vec<u8>, bytes: Vec<u8>) -> Result<Message> {
        let (r, _) = self.exchange(command, tid, words, bytes)?;
        if r.is_error() {
            return Err(r.error(&format!("SMB command {command:#04x}")));
        }
        Ok(r)
    }

    fn oem(&self, s: &str) -> Result<Vec<u8>> {
        self.opts.charset.encode(s).ok_or_else(|| {
            Error::Unsupported(format!(
                "{s:?} has characters the machine's character set lacks"
            ))
        })
    }

    fn negotiate(&mut self) -> Result<()> {
        let mut bytes = Vec::new();
        for d in DIALECTS {
            bytes.push(0x02);
            bytes.extend(d.as_bytes());
            bytes.push(0);
        }
        let (r, _) = self.exchange(wire::NEGOTIATE, 0, Vec::new(), bytes)?;
        if r.is_error() {
            return Err(r.error("negotiate"));
        }
        let index = usize::from(r.word(0));
        let dialect = DIALECTS.get(index).ok_or_else(|| {
            Error::Unsupported("the server took none of the SMB dialects offered".into())
        })?;
        let mut neg = Negotiated {
            dialect: dialect.to_string(),
            user_level: true,
            encrypt: true,
            challenge: [0; 8],
            max_buffer: 4096,
            domain: String::new(),
        };
        match r.words.len() / 2 {
            17 => {
                let w = &r.words;
                neg.user_level = w[2] & 1 != 0;
                neg.encrypt = w[2] & 2 != 0;
                neg.max_buffer = u32::from_le_bytes([w[7], w[8], w[9], w[10]]) as usize;
                let clen = usize::from(w[33]);
                if clen >= 8 {
                    neg.challenge.copy_from_slice(&r.bytes[..8]);
                }
                neg.domain = domain_name(&r.bytes[clen.min(r.bytes.len())..], self.opts.charset);
            }
            13 => {
                neg.user_level = r.word(1) & 1 != 0;
                neg.encrypt = r.word(1) & 2 != 0;
                neg.max_buffer = usize::from(r.word(2));
                let clen = usize::from(r.word(11));
                if clen >= 8 {
                    neg.challenge.copy_from_slice(&r.bytes[..8]);
                }
                neg.domain = self.opts.charset.decode(wire::cstr(&r.bytes, clen).0);
            }
            // Core protocol: no logins at all, only share passwords in the clear.
            _ => {
                neg.user_level = false;
                neg.encrypt = false;
            }
        }
        self.neg = neg;
        Ok(())
    }

    fn setup(&mut self, account: &str, domain: &str, lm: &[u8], nt: &[u8]) -> Result<Message> {
        let (account, domain) = (self.oem(account)?, self.oem(domain)?);
        let max = self.neg.max_buffer.min(0xffff) as u16;
        let (words, bytes) = if self.neg.is_nt() {
            let words = Buf::default()
                .no_andx()
                .u16(max)
                .u16(2)
                .u16(0)
                .u32(0)
                .u16(lm.len() as u16)
                .u16(nt.len() as u16)
                .u32(0)
                .u32(0)
                .0;
            (
                words,
                Buf::default()
                    .raw(lm)
                    .raw(nt)
                    .sz(&account)
                    .sz(&domain)
                    .sz(b"legcon")
                    .sz(b"legcon")
                    .0,
            )
        } else {
            let words = Buf::default()
                .no_andx()
                .u16(max)
                .u16(2)
                .u16(0)
                .u32(0)
                .u16(lm.len() as u16)
                .u32(0)
                .0;
            (
                words,
                Buf::default()
                    .raw(lm)
                    .sz(&account)
                    .sz(&domain)
                    .sz(b"legcon")
                    .sz(b"legcon")
                    .0,
            )
        };
        let (r, _) = self.exchange(wire::SESSION_SETUP_ANDX, 0, words, bytes)?;
        Ok(r)
    }

    fn login(&mut self, user: &str, password: &str) -> Result<()> {
        if self.neg.dialect == DIALECTS[0] {
            return Ok(());
        }
        let (domain, account) = match user.split_once('\\') {
            Some((d, u)) => (d.to_string(), u.to_string()),
            None => (self.neg.domain.clone(), user.to_string()),
        };
        // Share-level servers check passwords per share; the session itself needs none.
        if !self.neg.user_level || account.is_empty() {
            let r = self.setup(&account, &domain, &[], &[])?;
            if r.is_error() {
                return Err(r.error("login"));
            }
            self.uid = r.uid;
            return Ok(());
        }
        let ch = self.neg.challenge;
        let r = if self.neg.encrypt {
            let lm = auth::v1_response(&auth::lm_hash(&self.oem(password)?), &ch);
            let nt = if self.neg.is_nt() {
                auth::v1_response(&auth::nt_hash(password), &ch).to_vec()
            } else {
                Vec::new()
            };
            let r = self.setup(&account, &domain, &lm, &nt)?;
            if r.is_error() && self.neg.is_nt() && matches!(r.error(""), Error::Auth(_)) {
                // Newer Samba refuses v1 answers; try NTLMv2 before giving up.
                let client: [u8; 8] = std::array::from_fn(|i| {
                    (std::process::id() as u8)
                        .wrapping_mul(31)
                        .wrapping_add(i as u8 * 17)
                });
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let filetime = (now + 11_644_473_600) * 10_000_000;
                let (lm2, nt2) =
                    auth::v2_responses(password, &account, &domain, &ch, &client, filetime);
                self.setup(&account, &domain, &lm2, &nt2)?
            } else {
                r
            }
        } else {
            let mut plain = self.oem(password)?;
            plain.push(0);
            self.setup(&account, &domain, &plain, &[])?
        };
        if r.is_error() {
            return Err(r.error(&format!("login as {account}")));
        }
        self.uid = r.uid;
        self.guest = r.word(2) & 1 != 0;
        Ok(())
    }

    /// The tree id for a share, connecting to it the first time.
    fn tree(&mut self, share: &str) -> Result<u16> {
        let key = share.to_ascii_uppercase();
        if let Some(t) = self.trees.get(&key) {
            return Ok(*t);
        }
        let password: Vec<u8> = if self.neg.user_level {
            vec![0]
        } else if self.neg.encrypt {
            let pw = self.oem(&self.password.clone())?;
            auth::v1_response(&auth::lm_hash(&pw), &self.neg.challenge).to_vec()
        } else {
            let mut p = self.oem(&self.password.clone())?;
            p.push(0);
            p
        };
        let path = self.oem(&format!("\\\\{}\\{}", self.server, key))?;
        let words = Buf::default().no_andx().u16(0).u16(password.len() as u16).0;
        let bytes = Buf::default().raw(&password).sz(&path).sz(b"?????").0;
        let (r, _) = self.exchange(wire::TREE_CONNECT_ANDX, 0, words, bytes)?;
        if r.is_error() {
            return Err(r.error(&format!("open share {share}")));
        }
        self.trees.insert(key, r.tid);
        Ok(r.tid)
    }

    /// A transaction (TRANS or TRANS2); its reply parameters and data, put together.
    fn transact(
        &mut self,
        command: u8,
        tid: u16,
        setup: &[u16],
        name: &[u8],
        params: &[u8],
        data: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let wc = 14 + setup.len();
        let bytes_at = wire::HEADER + 1 + wc * 2 + 2;
        let pad = |at: usize| (4 - at % 4) % 4;
        let param_at = bytes_at + name.len() + pad(bytes_at + name.len());
        let data_at = param_at + params.len() + pad(param_at + params.len());
        let max_data = self.neg.max_buffer.min(0xffff).saturating_sub(100) as u16;
        let mut words = Buf::default()
            .u16(params.len() as u16)
            .u16(data.len() as u16)
            .u16(1024)
            .u16(max_data)
            .u8(0)
            .u8(0)
            .u16(0)
            .u32(0)
            .u16(0)
            .u16(params.len() as u16)
            .u16(param_at as u16)
            .u16(data.len() as u16)
            .u16(if data.is_empty() { 0 } else { data_at as u16 })
            .u8(setup.len() as u8)
            .u8(0);
        for s in setup {
            words = words.u16(*s);
        }
        let mut bytes = name.to_vec();
        bytes.resize(param_at - bytes_at, 0);
        bytes.extend(params);
        bytes.resize(data_at - bytes_at, 0);
        bytes.extend(data);
        let (mut r, mut raw) = self.exchange(command, tid, words.0, bytes)?;
        let (mut p, mut d) = (Vec::new(), Vec::new());
        loop {
            if r.is_error() && !no_more_files(&r) {
                return Err(r.error("transaction"));
            }
            if r.is_error() {
                return Ok((Vec::new(), Vec::new()));
            }
            let total = (usize::from(r.word(0)), usize::from(r.word(1)));
            let take = |count: usize, off: usize| raw.get(off..off + count).map(<[u8]>::to_vec);
            p.extend(take(usize::from(r.word(3)), usize::from(r.word(4))).unwrap_or_default());
            d.extend(take(usize::from(r.word(6)), usize::from(r.word(7))).unwrap_or_default());
            if p.len() >= total.0 && d.len() >= total.1 {
                return Ok((p, d));
            }
            // The rest arrives in further replies with the same message id.
            let (nr, nraw) = self.recv()?;
            r = nr;
            raw = nraw;
        }
    }

    /// The disk shares, as folders at the top of the path.
    fn shares(&mut self) -> Result<Vec<Entry>> {
        let tid = self.tree("IPC$")?;
        let params = Buf::default()
            .u16(0)
            .sz(b"WrLeh")
            .sz(b"B13BWz")
            .u16(1)
            .u16(0x1000)
            .0;
        let (p, d) = self.transact(
            wire::TRANSACTION,
            tid,
            &[],
            b"\\PIPE\\LANMAN\0",
            &params,
            &[],
        )?;
        if p.len() < 8 {
            return Err(Error::Protocol("short share list".into()));
        }
        let status = u16::from_le_bytes([p[0], p[1]]);
        if status != 0 {
            return Err(Error::Denied(format!(
                "share list refused (RAP status {status})"
            )));
        }
        let count = usize::from(u16::from_le_bytes([p[4], p[5]]));
        let mut out = Vec::new();
        for i in 0..count {
            let Some(e) = d.get(i * 20..i * 20 + 20) else {
                break;
            };
            let kind = u16::from_le_bytes([e[14], e[15]]) & 0x7fff;
            // Disk shares only: printers and IPC$ hold no files.
            if kind != 0 {
                continue;
            }
            let raw = wire::cstr(&e[..13], 0).0;
            let mut entry = Entry::new(self.opts.charset.decode(raw), EntryKind::Folder);
            entry.raw_name = raw.to_vec();
            out.push(entry);
        }
        Ok(out)
    }

    /// Split a path into its share and the path within it (`\dir\file`).
    fn split(&self, path: &RemotePath) -> Result<(String, String)> {
        let parts = path.parts();
        let share = parts.first().ok_or_else(|| {
            Error::Unsupported("that is the list of shares, not a folder on one".into())
        })?;
        Ok((share.clone(), format!("\\{}", parts[1..].join("\\"))))
    }

    fn entries(&self, data: &[u8], count: usize, out: &mut Vec<Entry>) -> Option<String> {
        let mut at = 0;
        let mut last = None;
        for _ in 0..count {
            let e = data.get(at..)?;
            if e.len() < 23 {
                break;
            }
            let w = |i: usize| u16::from_le_bytes([e[i], e[i + 1]]);
            let size = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
            let attrs = w(20);
            let len = usize::from(e[22]);
            let raw = e.get(23..23 + len)?;
            // The name is followed by a NUL on every server seen; skip it only when it is there.
            at += 23 + len + usize::from(e.get(23 + len) == Some(&0));
            let name = self.opts.charset.decode(raw);
            last = Some(name.clone());
            if name == "." || name == ".." {
                continue;
            }
            let dir = attrs & 0x10 != 0;
            let mut entry = Entry::new(
                name,
                if dir {
                    EntryKind::Folder
                } else {
                    EntryKind::File
                },
            );
            entry.raw_name = raw.to_vec();
            entry.size = (!dir).then_some(u64::from(size));
            entry.dos_attributes = Some(attrs as u8);
            entry.modified = wire::dos_time(w(8), w(10)).map(|unix| Timestamp {
                unix,
                precision: TimePrecision::LocalUnknownZone,
            });
            out.push(entry);
        }
        last
    }

    fn find(&mut self, tid: u16, dir: &str) -> Result<Vec<Entry>> {
        let pattern = if dir == "\\" {
            "\\*".to_string()
        } else {
            format!("{dir}\\*")
        };
        let pattern = self.oem(&pattern)?;
        let params = Buf::default()
            .u16(0x16)
            .u16(200)
            .u16(0x0002)
            .u16(1)
            .u32(0)
            .sz(&pattern)
            .0;
        let (p, d) = self.transact(wire::TRANSACTION2, tid, &[0x0001], &[0], &params, &[])?;
        let mut out = Vec::new();
        if p.len() < 10 {
            return Ok(out);
        }
        let sid = u16::from_le_bytes([p[0], p[1]]);
        let mut count = usize::from(u16::from_le_bytes([p[2], p[3]]));
        let mut end = u16::from_le_bytes([p[4], p[5]]) != 0;
        let mut last = self.entries(&d, count, &mut out);
        while !end && count > 0 {
            let Some(name) = last.take() else { break };
            let name = self.oem(&name)?;
            let params = Buf::default()
                .u16(sid)
                .u16(200)
                .u16(1)
                .u32(0)
                .u16(0x000a)
                .sz(&name)
                .0;
            let (p, d) = self.transact(wire::TRANSACTION2, tid, &[0x0002], &[0], &params, &[])?;
            if p.len() < 8 {
                break;
            }
            count = usize::from(u16::from_le_bytes([p[0], p[1]]));
            end = u16::from_le_bytes([p[2], p[3]]) != 0;
            last = self.entries(&d, count, &mut out);
        }
        if !end {
            let _ = self.call(
                wire::FIND_CLOSE2,
                tid,
                Buf::default().u16(sid).0,
                Vec::new(),
            );
        }
        Ok(out)
    }

    fn open(&mut self, path: &RemotePath, access: u16, open_mode: u16) -> Result<(u16, u16)> {
        let (share, p) = self.split(path)?;
        let tid = self.tree(&share)?;
        let name = self.oem(&p)?;
        let words = Buf::default()
            .no_andx()
            .u16(0)
            .u16(access)
            .u16(0x16)
            .u16(0)
            .u32(0)
            .u16(open_mode)
            .u32(0)
            .u32(0)
            .u32(0)
            .0;
        let (r, _) = self.exchange(wire::OPEN_ANDX, tid, words, Buf::default().sz(&name).0)?;
        if r.is_error() {
            return Err(r.error(&format!("open {path}")));
        }
        Ok((tid, r.word(2)))
    }

    fn simple(&mut self, command: u8, path: &RemotePath, words: Vec<u8>, what: &str) -> Result<()> {
        let (share, p) = self.split(path)?;
        let tid = self.tree(&share)?;
        let name = self.oem(&p)?;
        let (r, _) = self.exchange(command, tid, words, Buf::default().u8(4).sz(&name).0)?;
        if r.is_error() {
            return Err(r.error(&format!("{what} {path}")));
        }
        Ok(())
    }

    fn live(&self) -> Result<()> {
        match self.state {
            ConnState::Connected | ConnState::Busy => Ok(()),
            _ => Err(Error::NotConnected),
        }
    }
}

impl Connection for SmbSession {
    fn state(&self) -> ConnState {
        self.state
    }

    fn disconnect(&mut self, timeout: Duration) -> Result<()> {
        if !matches!(self.state, ConnState::Connected | ConnState::Busy) {
            return Ok(());
        }
        self.state = ConnState::Disconnecting;
        let _ = self.stream.set_read_timeout(Some(timeout));
        let mut first_error = None;
        let trees: Vec<u16> = self.trees.drain().map(|(_, t)| t).collect();
        for tid in trees {
            if let Err(e) = self.call(wire::TREE_DISCONNECT, tid, Vec::new(), Vec::new()) {
                first_error.get_or_insert(e);
            }
        }
        if self.uid != 0 || self.neg.user_level {
            if let Err(e) = self.call(wire::LOGOFF_ANDX, 0, Buf::default().no_andx().0, Vec::new())
            {
                first_error.get_or_insert(e);
            }
        }
        let _ = self.stream.shutdown(Shutdown::Both);
        self.state = ConnState::Disconnected;
        first_error.map_or(Ok(()), Err)
    }
}

impl FileSession for SmbSession {
    fn charset(&self) -> lc_core::Charset {
        self.opts.charset
    }
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: true,
            random_write: true,
            resume_upload: false,
            rename: true,
            delete: true,
            mkdir: true,
            set_times: false,
            set_mode: false,
        }
    }

    /// The list of shares; a share is the first part of every path.
    fn home(&mut self) -> Result<RemotePath> {
        self.live()?;
        Ok(RemotePath::root(PathStyle::Dos))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        self.live()?;
        if dir.is_root() {
            return self.shares();
        }
        let (share, p) = self.split(dir)?;
        let tid = self.tree(&share)?;
        self.find(tid, &p)
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        self.list(&parent)?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(path.name()))
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        self.live()?;
        // Read access, sharing deny-none; open only if it exists.
        let (tid, fid) = self.open(path, 0x0040, 0x0001)?;
        Ok(Box::new(Handle {
            s: self,
            tid,
            fid,
            offset: at,
        }))
    }

    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        self.live()?;
        let (open_mode, offset) = match mode {
            WriteMode::Create => (0x0010, 0),
            WriteMode::Overwrite => (0x0012, 0),
            WriteMode::Resume(at) => (0x0011, at),
        };
        let (tid, fid) = self.open(path, 0x0041, open_mode)?;
        Ok(Box::new(Handle {
            s: self,
            tid,
            fid,
            offset,
        }))
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        self.live()?;
        self.simple(wire::CREATE_DIRECTORY, path, Vec::new(), "new folder")
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        self.live()?;
        match self.simple(wire::DELETE, path, Buf::default().u16(0x06).0, "delete") {
            Ok(()) => Ok(()),
            // DELETE refuses folders; DELETE_DIRECTORY takes an empty one.
            Err(first) => self
                .simple(wire::DELETE_DIRECTORY, path, Vec::new(), "delete")
                .map_err(|_| first),
        }
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.live()?;
        let ((s1, p1), (s2, p2)) = (self.split(from)?, self.split(to)?);
        if !s1.eq_ignore_ascii_case(&s2) {
            return Err(Error::Unsupported(
                "a rename cannot move a file to another share".into(),
            ));
        }
        let tid = self.tree(&s1)?;
        let (a, b) = (self.oem(&p1)?, self.oem(&p2)?);
        let bytes = Buf::default().u8(4).sz(&a).u8(4).sz(&b).0;
        let (r, _) = self.exchange(wire::RENAME, tid, Buf::default().u16(0x16).0, bytes)?;
        if r.is_error() {
            return Err(r.error(&format!("rename {from}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn domain_names_in_either_form() {
        let utf16: Vec<u8> = "WORKGROUP\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            super::domain_name(&utf16, lc_core::Charset::Latin1),
            "WORKGROUP"
        );
        assert_eq!(
            super::domain_name(b"LANGROUP\0", lc_core::Charset::Latin1),
            "LANGROUP"
        );
    }
}
