//! SSH-2 for legcon (docs/PLAN.md §4.1, D19): shells, single commands and SFTP v3.
//!
//! Pure Rust (D19, revised 2026-11-15): the transport, login and channels are our own code
//! ([`transport`]) over RustCrypto ([`crypto`]), so nothing links OpenSSL or any C library. The
//! pre-2005 algorithms (`diffie-hellman-group1-sha1`, `ssh-dss`, `ssh-rsa`, `3des-cbc`,
//! `arcfour`, `hmac-md5`, `hmac-sha1`) are offered after the modern ones, so an old algorithm is
//! only used when the server offers nothing newer; [`SshSession::negotiated`] says which won.
//! SFTP is our own code ([`sftp`]) so file names stay bytes in the machine's character set.

pub mod agent;
pub mod crypto;
pub mod keyfile;
pub mod keys;
mod scp;
pub mod sftp;
pub mod ssh1;
pub mod transport;
pub mod wire;

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::{
    Charset, ConnState, Connection, Entry, EntryKind, Error, ExecResult, FileCaps, FileSession,
    PathStyle, Pty, PtyWriter, RemotePath, Result, ShellCaps, ShellSession, Timestamp, WriteMode,
};
use sha2::Digest as _;
use transport::{lock, Channel, Link, Shared};

use sftp::{flag, Attrs, Sftp, CHUNK};

pub const PORT: u16 = 22;

#[derive(Debug, Clone)]
pub struct Options {
    pub charset: Charset,
    pub connect_timeout: Duration,
    /// How long one blocking SSH call may take.
    pub timeout: Duration,
    /// How long a terminal read waits before returning `TimedOut`, so a reader can check for cancel.
    pub poll: Duration,
    /// Use SFTP when the server has it; off, files always go through SCP and shell commands.
    pub sftp: bool,
    /// Where each step (key exchange, login, SFTP or SCP) is noted for the Log tab.
    pub trace: Option<lc_core::TraceLog>,
}

/// A key pair on disk: traditional PEM private key, and its OpenSSH `.pub` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFiles {
    pub private: PathBuf,
    pub public: PathBuf,
}

/// How to log in: the machine's key first when the server takes keys, then (as `ssh` does) the
/// keys in the user's ssh-agent and `user_keys`, then the password.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    pub password: Option<String>,
    pub key: Option<KeyFiles>,
    /// Try the keys the user's ssh-agent holds.
    pub agent: bool,
    /// Unencrypted private keys to try, such as `~/.ssh/id_ed25519`.
    pub user_keys: Vec<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: Charset::default(),
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(60),
            poll: Duration::from_millis(250),
            sftp: true,
            trace: None,
        }
    }
}

/// The server's host key, for the caller to check against what it saw before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    /// `ssh-rsa`, `ssh-dss`, ...
    pub algorithm: String,
    pub blob: Vec<u8>,
    /// `12:ab:...`, as old servers' admins see it.
    pub md5: String,
    /// `SHA256:...` (unpadded Base64), as OpenSSH shows it today.
    pub sha256: String,
}

/// What the two sides agreed on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Negotiated {
    /// The server's version line, such as `SSH-2.0-OpenSSH_3.9p1`.
    pub banner: String,
    pub kex: String,
    pub host_key: String,
    pub cipher: String,
    pub mac: String,
    /// Strict key exchange (the Terrapin countermeasure) is on.
    pub strict: bool,
}

impl Negotiated {
    /// Whether any algorithm in use is one a modern client would refuse.
    pub fn weak(&self) -> bool {
        self.kex.ends_with("group1-sha1")
            || self.host_key == "ssh-dss"
            || [
                "3des-cbc",
                "arcfour",
                "arcfour128",
                "blowfish-cbc",
                "cast128-cbc",
            ]
            .contains(&self.cipher.as_str())
            || self.mac.starts_with("hmac-md5")
    }
}

fn hex_md5(h: &[u8]) -> String {
    h.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            s.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    s
}

/// A logged-in SSH connection: SFTP for files, channels for commands and a terminal.
pub struct SshSession {
    link: Shared,
    sftp: Option<Sftp<Channel>>,
    /// The server has no SFTP (or it is turned off): files go through SCP and the shell.
    shell_files: bool,
    opts: Options,
    negotiated: Negotiated,
    closed: bool,
    breaker: lc_core::net::Breaker,
}

/// Connect, check the host key with `verify`, and log in with `auth`.
///
/// `verify` sees the key before any password is sent; an error from it ends the connection.
pub fn connect(
    host: &str,
    port: u16,
    user: &str,
    auth: &Auth,
    opts: &Options,
    verify: &mut dyn FnMut(&HostKey) -> Result<()>,
) -> Result<SshSession> {
    let note = |t: String| lc_core::trace::note(opts.trace.as_ref(), t);
    note(format!("Connecting to {host} port {port} (SSH)"));
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
    lc_core::net::tune(&tcp, opts.timeout)?;
    let breaker = lc_core::net::Breaker::new(&tcp);
    let mut link = Link::handshake(tcp, opts.timeout)?;
    let key = link
        .host_key
        .clone()
        .ok_or_else(|| Error::Protocol("the server sent no host key".into()))?;
    let negotiated = link.negotiated.clone();
    note(format!("Server: {}", negotiated.banner));
    note(format!(
        "Key exchange {}, host key {} {}, cipher {}, MAC {}, strict key exchange {}",
        negotiated.kex,
        key.algorithm,
        key.sha256,
        negotiated.cipher,
        negotiated.mac,
        if negotiated.strict { "on" } else { "off" }
    ));
    if negotiated.weak() {
        note("Old algorithms in use: the server offers nothing newer".into());
    }
    if let Err(e) = verify(&key) {
        note(format!("Host key not accepted: {e}"));
        let _ = link.disconnect("legcon: host key not accepted");
        return Err(e);
    }
    note("Host key accepted".into());
    let (mut done, methods) = link.auth_methods(user)?;
    note(format!(
        "Logging in as {user}; the server takes: {}",
        if done {
            "no login (none needed)".to_string()
        } else {
            methods.join(", ")
        }
    ));
    let has = |m: &str| methods.iter().any(|x| x == m);
    let mut key_error = None;
    if let (false, Some(k), true) = (done, &auth.key, has("publickey")) {
        note(format!("Trying the key {}", k.private.display()));
        match keyfile::signer_file(&k.private).and_then(|s| link.auth_key(user, &s)) {
            Ok(true) => done = true,
            Ok(false) => key_error = Some("the server did not take the key".to_string()),
            Err(e) => key_error = Some(e.to_string()),
        }
        note(match &key_error {
            None => "The key was accepted".to_string(),
            Some(e) => format!("The key was refused: {e}"),
        });
    }
    if !done && has("publickey") {
        done = try_user_keys(&mut link, user, auth, &note)?;
    }
    if !done {
        let result = match &auth.password {
            Some(password) => {
                let mut ok = false;
                if has("password") {
                    note("Trying the password".into());
                    ok = link.auth_password(user, password)?;
                }
                if !ok && has("keyboard-interactive") {
                    note("Trying keyboard-interactive, answering with the password".into());
                    ok = link.auth_keyboard(user, password)?;
                }
                note(
                    if ok {
                        "The password was accepted"
                    } else {
                        "The password was refused"
                    }
                    .into(),
                );
                if ok {
                    Ok(())
                } else {
                    Err("the password was not accepted".to_string())
                }
            }
            // The wording callers look for to ask for a password and try again.
            None => Err(match &key_error {
                Some(k) => format!("no password and no key to log in with ({k})"),
                None => "no password and no key to log in with".into(),
            }),
        };
        if let Err(e) = result {
            let e = match key_error {
                Some(k) if auth.password.is_some() => format!("{e}; the key was refused too ({k})"),
                _ => e,
            };
            note(format!("Login failed: {e}"));
            let _ = link.disconnect("legcon: login failed");
            return Err(Error::Auth(if methods.is_empty() {
                e
            } else {
                format!("{e} (the server offers: {})", methods.join(","))
            }));
        }
    }
    note(format!("Logged in as {user}"));
    Ok(SshSession {
        link: Arc::new(Mutex::new(link)),
        sftp: None,
        shell_files: !opts.sftp,
        opts: opts.clone(),
        negotiated,
        closed: false,
        breaker,
    })
}

/// How many of the user's own keys are offered at most.
const MAX_OWN_KEYS: usize = 4;

/// Log in with the user's own keys as `ssh` would: the agent's, then `user_keys`.
fn try_user_keys(link: &mut Link, user: &str, auth: &Auth, note: &dyn Fn(String)) -> Result<bool> {
    let mut tried: Vec<Vec<u8>> = Vec::new();
    if auth.agent {
        match agent::Agent::connect() {
            None => note("No ssh-agent running".into()),
            Some(mut a) => {
                let mut ids = a.identities().unwrap_or_default();
                // OpenSSH drops the connection after 6 failures (MaxAuthTries): leave the
                // password a turn.
                ids.truncate(MAX_OWN_KEYS);
                note(format!("ssh-agent holds {} key(s)", ids.len()));
                for id in ids {
                    let (alg, flags) = id.algorithm(link.server_sig_algs());
                    note(format!("Trying the agent's key {} ({alg})", id.comment));
                    tried.push(id.blob.clone());
                    let blob = id.blob.clone();
                    let ok = link.auth_publickey(user, &alg, &id.blob, &mut |data| {
                        a.sign(&blob, data, flags)
                    })?;
                    if ok {
                        note(format!("The agent's key {} was accepted", id.comment));
                        return Ok(true);
                    }
                }
            }
        }
    }
    for path in auth
        .user_keys
        .iter()
        .take(MAX_OWN_KEYS.saturating_sub(tried.len()))
    {
        let key = match keyfile::signer_file(path) {
            Ok(k) => k,
            Err(e) => {
                note(format!("Skipping {}: {e}", path.display()));
                continue;
            }
        };
        if tried.contains(&key.blob()) {
            continue;
        }
        note(format!("Trying the key {}", path.display()));
        if link.auth_key(user, &key)? {
            note(format!("The key {} was accepted", path.display()));
            return Ok(true);
        }
    }
    Ok(false)
}

/// The fingerprints of a host key blob.
pub(crate) fn host_key_of(blob: &[u8]) -> HostKey {
    HostKey {
        algorithm: sftp::Cursor::new(blob)
            .bytes()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default(),
        md5: hex_md5(&md5::Md5::digest(blob)),
        sha256: format!("SHA256:{}", b64(&sha2::Sha256::digest(blob))),
        blob: blob.to_vec(),
    }
}

impl SshSession {
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    fn enc(&self, text: &str) -> Vec<u8> {
        self.opts
            .charset
            .encode(text)
            .unwrap_or_else(|| text.as_bytes().to_vec())
    }

    fn path(&self, p: &RemotePath) -> Vec<u8> {
        self.enc(&p.to_string())
    }

    /// Whether files go through SFTP; the first call finds out, falling back to SCP and the shell.
    fn use_sftp(&mut self) -> Result<bool> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        if self.sftp.is_none() && !self.shell_files {
            let id = lock(&self.link).open_session()?;
            let mut name = Vec::new();
            wire::put_string(&mut name, b"sftp");
            let ch = Channel::new(Arc::clone(&self.link), id);
            let accepted = lock(&self.link).request(id, "subsystem", &name, true)?;
            // Accepted is not enough: an sshd naming a missing sftp-server closes it at once.
            match accepted.then(|| Sftp::start(ch)) {
                Some(Ok(sftp)) => {
                    lc_core::trace::note(self.opts.trace.as_ref(), "Files over SFTP");
                    self.sftp = Some(sftp);
                }
                _ => {
                    let _ = lock(&self.link).close(id, Duration::from_secs(1));
                    lc_core::trace::note(
                        self.opts.trace.as_ref(),
                        "No SFTP here (refused, or no sftp-server): files over SCP and shell commands",
                    );
                    self.shell_files = true;
                }
            }
        }
        Ok(!self.shell_files)
    }

    /// Whether files go through SCP and shell commands because the server has no SFTP.
    pub fn uses_scp(&self) -> bool {
        self.shell_files
    }

    fn sftp(&mut self) -> Result<&mut Sftp<Channel>> {
        self.sftp.as_mut().ok_or(Error::NotConnected)
    }

    fn entry(&self, name: &[u8], long: &[u8], a: &Attrs) -> Entry {
        let mut e = Entry::new(
            self.opts.charset.decode(name),
            if a.is_dir() {
                EntryKind::Folder
            } else if a.is_link() {
                EntryKind::Link
            } else if a.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            },
        );
        e.raw_name = name.to_vec();
        e.size = a.size;
        e.mode = a.permissions.map(|p| p & 0o7777);
        e.modified = a.atime_mtime.map(|(_, m)| Timestamp {
            unix: i64::from(m),
            precision: lc_core::TimePrecision::Exact,
        });
        // The `ls -l` style long name carries owner and group names.
        let fields: Vec<&[u8]> = long
            .split(|&b| b == b' ')
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() >= 8 && fields[0].len() == 10 {
            e.owner = Some(self.opts.charset.decode(fields[2]));
            e.group = Some(self.opts.charset.decode(fields[3]));
        } else if let Some((u, g)) = a.uid_gid {
            e.owner = Some(u.to_string());
            e.group = Some(g.to_string());
        }
        e
    }

    /// Run `command` on its own channel; output and errors kept apart, with the exit status.
    pub fn run(&mut self, command: &[u8]) -> Result<ExecResult> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let mut ch = self.exec_channel(command)?;
        let mut output = Vec::new();
        let mut errors = Vec::new();
        ch.read_to_end(&mut output)?;
        let mut buf = [0u8; 4096];
        loop {
            match ch.read_stderr(&mut buf)? {
                0 => break,
                n => errors.extend_from_slice(&buf[..n]),
            }
        }
        let exit_status = ch.wait_close()?;
        Ok(ExecResult {
            output,
            errors,
            exit_status: exit_status.map(|s| s as i32),
        })
    }

    /// A channel running `command` (bytes as they are: names stay in the machine's charset).
    pub(crate) fn exec_channel(&mut self, command: &[u8]) -> Result<Channel> {
        let mut link = lock(&self.link);
        let id = link.open_session()?;
        let mut data = Vec::new();
        wire::put_string(&mut data, command);
        if !link.request(id, "exec", &data, true)? {
            let _ = link.close(id, Duration::from_secs(1));
            return Err(Error::Protocol(
                "the server would not run the command".into(),
            ));
        }
        drop(link);
        Ok(Channel::new(Arc::clone(&self.link), id))
    }

    /// Open an interactive terminal; the connection then belongs to it.
    pub fn into_pty(self, term: &str, cols: u16, rows: u16) -> Result<Pty> {
        let mut link = lock(&self.link);
        let id = link.open_session()?;
        let mut pty = Vec::new();
        wire::put_string(&mut pty, term.as_bytes());
        for v in [u32::from(cols), u32::from(rows), 0, 0] {
            wire::put_u32(&mut pty, v);
        }
        wire::put_string(&mut pty, &[0]);
        if !link.request(id, "pty-req", &pty, true)? {
            return Err(Error::Protocol(
                "the server would not give a terminal".into(),
            ));
        }
        if !link.request(id, "shell", &[], true)? {
            return Err(Error::Protocol("the server would not start a shell".into()));
        }
        let tcp = link.tcp()?;
        drop(link);
        lc_core::trace::note(
            self.opts.trace.as_ref(),
            format!("Terminal {term}, {cols} x {rows}: shell started"),
        );
        Ok(Pty {
            reader: Box::new(PtyReader {
                link: Arc::clone(&self.link),
                id,
                tcp,
                poll: self.opts.poll,
            }),
            writer: Box::new(PtyWriterImpl {
                link: Arc::clone(&self.link),
                id,
                closed: false,
            }),
        })
    }

    fn log_off(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if let Some(s) = self.sftp.take() {
            let mut ch = s.into_inner();
            let _ = ch.send_eof();
            let _ = ch.close();
        }
        lock(&self.link).disconnect("legcon: logged off")
    }
}

struct PtyReader {
    link: Shared,
    id: u32,
    tcp: TcpStream,
    poll: Duration,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let other = |e: Error| match e {
            Error::Io(e) => e,
            e => std::io::Error::other(e.to_string()),
        };
        loop {
            {
                let mut link = lock(&self.link);
                for ext in [false, true] {
                    match link.take(self.id, buf, ext).map_err(other)? {
                        Some(0) | None => {}
                        Some(n) => return Ok(n),
                    }
                }
                if link.eof(self.id).map_err(other)? {
                    return Ok(0);
                }
                if link.buffered() {
                    link.pump().map_err(other)?;
                    continue;
                }
            }
            // Wait for the socket without holding the link, so the writer can send meanwhile.
            self.tcp.set_read_timeout(Some(self.poll))?;
            match self.tcp.peek(&mut [0u8; 1]) {
                Ok(0) => return Ok(0),
                Ok(_) => lock(&self.link).pump().map_err(other)?,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Err(ErrorKind::TimedOut.into())
                }
                Err(e) => return Err(e),
            }
        }
    }
}

struct PtyWriterImpl {
    link: Shared,
    id: u32,
    closed: bool,
}

impl PtyWriter for PtyWriterImpl {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        lock(&self.link).write(self.id, bytes)
    }

    fn keepalive(&mut self) -> Result<()> {
        lock(&self.link).ignore()
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let mut data = Vec::new();
        for v in [u32::from(cols), u32::from(rows), 0, 0] {
            wire::put_u32(&mut data, v);
        }
        lock(&self.link)
            .request(self.id, "window-change", &data, false)
            .map(drop)
    }

    fn close(&mut self) -> Result<()> {
        if std::mem::replace(&mut self.closed, true) {
            return Ok(());
        }
        let mut link = lock(&self.link);
        // Dropbear sometimes never answers the channel close; log off regardless.
        let _ = link.close(self.id, Duration::from_secs(2));
        link.disconnect("legcon: logged off")
    }
}

/// Requests kept in flight at once, as OpenSSH does: 64 of 32 KB fill a 2 MB window.
const DEPTH: usize = 64;

/// Reads a file through SFTP from an offset, reading ahead; closes the handle when dropped.
struct FileReader<'a> {
    sftp: &'a mut Sftp<Channel>,
    handle: Vec<u8>,
    /// Offset of the next read to ask for.
    next: u64,
    /// Reads asked for: id, offset, length. Starts at one and grows, so small files ask little.
    inflight: VecDeque<(u32, u64, u32)>,
    depth: usize,
    got: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<'a> FileReader<'a> {
    fn new(sftp: &'a mut Sftp<Channel>, handle: Vec<u8>, at: u64) -> Self {
        FileReader {
            sftp,
            handle,
            next: at,
            inflight: VecDeque::new(),
            depth: 1,
            got: Vec::new(),
            pos: 0,
            eof: false,
        }
    }

    /// Collect and drop the replies still owed, so the channel is clean for the next request.
    fn settle(&mut self) {
        while let Some((id, ..)) = self.inflight.pop_front() {
            let _ = self.sftp.read_data(id);
        }
    }

    fn next_block(&mut self) -> Result<()> {
        while self.inflight.len() < self.depth {
            let id = self.sftp.send_read(&self.handle, self.next, CHUNK)?;
            self.inflight.push_back((id, self.next, CHUNK));
            self.next += u64::from(CHUNK);
        }
        let (id, at, len) = self.inflight.pop_front().expect("a read in flight");
        let data = self.sftp.read_data(id)?;
        self.depth = (self.depth * 2).min(DEPTH);
        if data.is_empty() {
            self.eof = true;
            self.settle();
        } else if data.len() < len as usize {
            // A short read leaves a gap before the reads already asked for: ask again from it.
            self.settle();
            self.next = at + data.len() as u64;
        }
        self.got = data;
        self.pos = 0;
        Ok(())
    }
}

impl Read for FileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.got.len() {
            if self.eof {
                return Ok(0);
            }
            self.next_block().map_err(std::io::Error::other)?;
        }
        let n = buf.len().min(self.got.len() - self.pos);
        buf[..n].copy_from_slice(&self.got[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Drop for FileReader<'_> {
    fn drop(&mut self) {
        self.settle();
        let _ = self.sftp.close(&self.handle);
    }
}

/// Writes a file through SFTP in whole, aligned 32 KB writes, not waiting for each one's
/// answer; `flush` sends the rest and collects them.
struct FileWriter<'a> {
    sftp: &'a mut Sftp<Channel>,
    handle: Vec<u8>,
    at: u64,
    buf: Vec<u8>,
    inflight: VecDeque<u32>,
}

impl<'a> FileWriter<'a> {
    fn new(sftp: &'a mut Sftp<Channel>, handle: Vec<u8>, at: u64) -> Self {
        FileWriter {
            sftp,
            handle,
            at,
            buf: Vec::with_capacity(CHUNK as usize),
            inflight: VecDeque::new(),
        }
    }

    fn send(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let id = self.sftp.send_write(&self.handle, self.at, &self.buf)?;
        self.inflight.push_back(id);
        self.at += self.buf.len() as u64;
        self.buf.clear();
        self.collect(DEPTH - 1)
    }

    fn collect(&mut self, keep: usize) -> Result<()> {
        while self.inflight.len() > keep {
            let id = self.inflight.pop_front().expect("a write in flight");
            if let Err(e) = self.sftp.write_done(id) {
                self.settle();
                return Err(e);
            }
        }
        Ok(())
    }

    fn settle(&mut self) {
        while let Some(id) = self.inflight.pop_front() {
            let _ = self.sftp.write_done(id);
        }
    }
}

impl Write for FileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(CHUNK as usize - self.buf.len());
        self.buf.extend_from_slice(&buf[..n]);
        if self.buf.len() == CHUNK as usize {
            self.send().map_err(std::io::Error::other)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
            .and_then(|()| self.collect(0))
            .map_err(std::io::Error::other)
    }
}

impl Drop for FileWriter<'_> {
    fn drop(&mut self) {
        let _ = self.send();
        self.settle();
        let _ = self.sftp.close(&self.handle);
    }
}

impl Connection for SshSession {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else if lock(&self.link).is_closed() {
            ConnState::Lost
        } else {
            ConnState::Connected
        }
    }

    fn disconnect(&mut self, timeout: Duration) -> Result<()> {
        lock(&self.link).set_timeout(timeout);
        self.log_off()
    }

    fn ping(&mut self, timeout: Duration) -> Result<()> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        lock(&self.link).keepalive(timeout)
    }

    fn breaker(&self) -> Option<lc_core::net::Breaker> {
        Some(self.breaker.clone())
    }
}

impl FileSession for SshSession {
    fn charset(&self) -> lc_core::Charset {
        self.opts.charset
    }
    fn caps(&self) -> FileCaps {
        let sftp = !self.shell_files;
        FileCaps {
            random_read: sftp,
            random_write: sftp,
            resume_upload: true,
            rename: true,
            delete: true,
            mkdir: true,
            set_times: false,
            set_mode: false,
        }
    }

    fn home(&mut self) -> Result<RemotePath> {
        if !self.use_sftp()? {
            return self.sh_home();
        }
        let raw = self.sftp()?.realpath(b".")?;
        Ok(RemotePath::parse(
            PathStyle::Unix,
            &self.opts.charset.decode(&raw),
        ))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        if !self.use_sftp()? {
            return self.sh_list(dir);
        }
        let raw = self.path(dir);
        let names = self.sftp()?.list(&raw)?;
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let mut e = self.entry(&n.name, &n.long, &n.attrs);
            if e.kind == EntryKind::Link {
                // Follow links so a linked folder opens like any other.
                let mut full = raw.clone();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&n.name);
                if let Ok(t) = self.sftp()?.stat(&full) {
                    if t.is_dir() {
                        e.kind = EntryKind::Folder;
                    } else {
                        e.size = t.size;
                    }
                }
            }
            out.push(e);
        }
        Ok(out)
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        if !self.use_sftp()? {
            return self.sh_stat(path);
        }
        let raw = self.path(path);
        let a = self.sftp()?.stat(&raw)?;
        let name = self.enc(path.name());
        Ok(self.entry(&name, b"", &a))
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        if !self.use_sftp()? {
            if at > 0 {
                return Ok(Box::new(self.sh_tail(path, at)?));
            }
            return Ok(Box::new(self.scp_read(path)?));
        }
        let raw = self.path(path);
        let sftp = self.sftp()?;
        let handle = sftp.open(&raw, flag::READ)?;
        Ok(Box::new(FileReader::new(sftp, handle, at)))
    }

    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        if !self.use_sftp()? {
            return Err(Error::Unsupported(
                "this server has no SFTP, and SCP needs the size before the data".into(),
            ));
        }
        let raw = self.path(path);
        let (flags, at) = match mode {
            WriteMode::Create => (flag::WRITE | flag::CREAT | flag::EXCL, 0),
            WriteMode::Overwrite => (flag::WRITE | flag::CREAT | flag::TRUNC, 0),
            WriteMode::Resume(n) => (flag::WRITE | flag::CREAT, n),
        };
        let sftp = self.sftp()?;
        let handle = sftp.open(&raw, flags)?;
        Ok(Box::new(FileWriter::new(sftp, handle, at)))
    }

    fn open_write_sized(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
        size: u64,
    ) -> Result<Box<dyn Write + Send + '_>> {
        if self.use_sftp()? {
            return self.open_write(path, mode);
        }
        if matches!(mode, WriteMode::Resume(n) if n > 0) {
            return Ok(Box::new(self.sh_append(path, size)?));
        }
        Ok(Box::new(self.scp_write(path, size)?))
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        if !self.use_sftp()? {
            return self.sh_simple(&path.to_string(), b"mkdir", &[path]);
        }
        let raw = self.path(path);
        self.sftp()?.mkdir(&raw)
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        if !self.use_sftp()? {
            return self.sh_remove(path);
        }
        let raw = self.path(path);
        let sftp = self.sftp()?;
        if sftp.lstat(&raw)?.is_dir() {
            sftp.rmdir(&raw)
        } else {
            sftp.remove(&raw)
        }
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        if !self.use_sftp()? {
            return self.sh_simple(&from.to_string(), b"mv", &[from, to]);
        }
        let (f, t) = (self.path(from), self.path(to));
        self.sftp()?.rename(&f, &t)
    }

    /// POSIX `cksum` on the machine; `None` where it has none.
    fn checksum(&mut self, path: &RemotePath) -> Result<Option<lc_core::Checksum>> {
        let cmd = lc_core::checksum::cksum_command(&self.path(path));
        Ok(self
            .run(&cmd)
            .ok()
            .filter(|r| r.exit_status.unwrap_or(0) == 0)
            .and_then(|r| lc_core::checksum::parse_cksum(&String::from_utf8_lossy(&r.output))))
    }

    /// A shell command, run in `dir`; output and errors together, then the exit status if not 0.
    fn quote(&mut self, dir: &RemotePath, line: &str) -> Result<String> {
        let mut cmd = b"cd '".to_vec();
        for &b in &self.path(dir) {
            if b == b'\'' {
                cmd.extend_from_slice(b"'\\''");
            } else {
                cmd.push(b);
            }
        }
        cmd.extend_from_slice(b"' && ");
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

impl ShellSession for SshSession {
    fn caps(&self) -> ShellCaps {
        ShellCaps {
            pty: true,
            exec: true,
            exit_status: true,
            window_size: true,
        }
    }

    fn exec(&mut self, command: &str) -> Result<ExecResult> {
        let c = self.enc(command);
        self.run(&c)
    }

    fn open_pty(&mut self, _term: &str, _cols: u16, _rows: u16) -> Result<Pty> {
        Err(Error::Unsupported(
            "use SshSession::into_pty, which hands the connection to the terminal".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_without_padding() {
        assert_eq!(b64(b"f"), "Zg");
        assert_eq!(b64(b"fo"), "Zm8");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg");
    }

    #[test]
    fn weak_algorithms_are_flagged() {
        let mut n = Negotiated {
            kex: "diffie-hellman-group14-sha256".into(),
            host_key: "rsa-sha2-256".into(),
            cipher: "aes128-ctr".into(),
            mac: "hmac-sha2-256".into(),
            ..Negotiated::default()
        };
        assert!(!n.weak());
        n.cipher = "3des-cbc".into();
        assert!(n.weak());
    }
}
