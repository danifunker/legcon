//! SSH-2 for legcon (docs/PLAN.md §4.1, D19): shells, single commands and SFTP v3.
//!
//! The transport is libssh2 through the `ssh2` crate, built with the pre-2005 algorithms switched
//! on (`.cargo/config.toml`): `diffie-hellman-group1-sha1`, `ssh-dss`, `ssh-rsa`, `3des-cbc`,
//! `arcfour`, `hmac-md5`, `hmac-sha1`. libssh2 prefers the modern ones, so an old algorithm is
//! only used when the server offers nothing newer; [`SshSession::negotiated`] says which won.
//! SFTP is our own code ([`sftp`]) so file names stay bytes in the machine's character set.

pub mod keys;
mod scp;
pub mod sftp;

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::{
    Charset, ConnState, Connection, Entry, EntryKind, Error, ExecResult, FileCaps, FileSession,
    PathStyle, Pty, PtyWriter, RemotePath, Result, ShellCaps, ShellSession, Timestamp, WriteMode,
};
use ssh2::{Channel, ErrorCode, HashType, MethodType, Session};

use sftp::{flag, Attrs, Sftp, CHUNK};

pub const PORT: u16 = 22;

/// libssh2's "would block" code, returned by every call on a non-blocking session.
const EAGAIN: i32 = -37;

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
}

/// A key pair on disk: traditional PEM private key, and its OpenSSH `.pub` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFiles {
    pub private: PathBuf,
    pub public: PathBuf,
}

/// How to log in: a key is tried first when the server takes keys, then the password.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    pub password: Option<String>,
    pub key: Option<KeyFiles>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: Charset::default(),
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(60),
            poll: Duration::from_millis(250),
            sftp: true,
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

fn ssh_err(e: ssh2::Error) -> Error {
    match e.code() {
        ErrorCode::Session(-9) => Error::Timeout(Duration::from_secs(0)),
        ErrorCode::Session(-18) => Error::Auth(e.message().to_string()),
        ErrorCode::Session(-8) | ErrorCode::Session(-5) => Error::Protocol(format!(
            "no algorithm in common with the server ({})",
            e.message()
        )),
        _ => Error::Protocol(e.message().to_string()),
    }
}

/// Retry a call on a non-blocking session until it stops saying "would block".
fn again<T>(mut f: impl FnMut() -> std::result::Result<T, ssh2::Error>) -> Result<T> {
    loop {
        match f() {
            Err(e) if e.code() == ErrorCode::Session(EAGAIN) => {
                std::thread::sleep(Duration::from_millis(2))
            }
            r => return r.map_err(ssh_err),
        }
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

struct Answer(String);

impl ssh2::KeyboardInteractivePrompt for Answer {
    fn prompt<'a>(
        &mut self,
        _user: &str,
        _instructions: &str,
        prompts: &[ssh2::Prompt<'a>],
    ) -> Vec<String> {
        prompts.iter().map(|_| self.0.clone()).collect()
    }
}

/// A logged-in SSH connection: SFTP for files, channels for commands and a terminal.
pub struct SshSession {
    session: Session,
    tcp: TcpStream,
    sftp: Option<Sftp<Channel>>,
    /// The server has no SFTP (or it is turned off): files go through SCP and the shell.
    shell_files: bool,
    opts: Options,
    negotiated: Negotiated,
    closed: bool,
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
    let mut session = Session::new().map_err(ssh_err)?;
    session.set_timeout(opts.timeout.as_millis() as u32);
    session.set_tcp_stream(tcp.try_clone()?);
    session.handshake().map_err(|e| match e.code() {
        ErrorCode::Session(-9) => Error::Timeout(opts.timeout),
        ErrorCode::Session(-13) | ErrorCode::Session(-43) => Error::Protocol(
            "the server closed the connection during the handshake (is it SSH-1 only?)".into(),
        ),
        _ => ssh_err(e),
    })?;
    let (blob, _) = session
        .host_key()
        .ok_or_else(|| Error::Protocol("the server sent no host key".into()))?;
    let blob = blob.to_vec();
    let key = HostKey {
        algorithm: sftp::Cursor::new(&blob)
            .bytes()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default(),
        md5: session
            .host_key_hash(HashType::Md5)
            .map(hex_md5)
            .unwrap_or_default(),
        sha256: session
            .host_key_hash(HashType::Sha256)
            .map(|h| format!("SHA256:{}", b64(h)))
            .unwrap_or_default(),
        blob,
    };
    let method = |t| session.methods(t).unwrap_or("").to_string();
    let negotiated = Negotiated {
        banner: session.banner().unwrap_or("").to_string(),
        kex: method(MethodType::Kex),
        host_key: method(MethodType::HostKey),
        cipher: method(MethodType::CryptCs),
        mac: method(MethodType::MacCs),
    };
    let bye = |s: &Session| {
        let _ = s.disconnect(None, "legcon: host key not accepted", None);
    };
    if let Err(e) = verify(&key) {
        bye(&session);
        return Err(e);
    }
    let methods = session.auth_methods(user).unwrap_or("").to_string();
    let mut key_error = None;
    if let (Some(k), true) = (&auth.key, methods.contains("publickey")) {
        if let Err(e) = session.userauth_pubkey_file(user, Some(&k.public), &k.private, None) {
            key_error = Some(e.message().to_string());
        }
    }
    if !session.authenticated() {
        let result = match &auth.password {
            Some(password) => {
                let mut r = session.userauth_password(user, password);
                if r.is_err() && methods.contains("keyboard-interactive") {
                    r = session.userauth_keyboard_interactive(user, &mut Answer(password.clone()));
                }
                r.map_err(|e| e.message().to_string())
            }
            None => Err(key_error
                .clone()
                .unwrap_or_else(|| "no password and no key to log in with".into())),
        };
        if let Err(e) = result {
            let e = match key_error {
                Some(k) if auth.password.is_some() => format!("{e}; the key was refused too ({k})"),
                _ => e,
            };
            bye(&session);
            return Err(Error::Auth(if methods.is_empty() {
                e
            } else {
                format!("{e} (the server offers: {methods})")
            }));
        }
    }
    Ok(SshSession {
        session,
        tcp,
        sftp: None,
        shell_files: !opts.sftp,
        opts: opts.clone(),
        negotiated,
        closed: false,
    })
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
            let mut ch = self.session.channel_session().map_err(ssh_err)?;
            match ch.subsystem("sftp") {
                Ok(()) => self.sftp = Some(Sftp::start(ch)?),
                Err(_) => {
                    let _ = ch.close();
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
        let mut ch = self.session.channel_session().map_err(ssh_err)?;
        // libssh2's exec takes a &str, so a command in Latin-1 or MacRoman cannot be sent as is.
        let cmd = std::str::from_utf8(command).map_err(|_| {
            Error::Unsupported("this command has characters SSH exec cannot send here".into())
        })?;
        ch.exec(cmd).map_err(ssh_err)?;
        let mut output = Vec::new();
        let mut errors = Vec::new();
        ch.read_to_end(&mut output)?;
        ch.stderr().read_to_end(&mut errors)?;
        let _ = ch.wait_close();
        Ok(ExecResult {
            output,
            errors,
            exit_status: ch.exit_status().ok(),
        })
    }

    /// Open an interactive terminal; the connection then belongs to it.
    pub fn into_pty(self, term: &str, cols: u16, rows: u16) -> Result<Pty> {
        let mut ch = self.session.channel_session().map_err(ssh_err)?;
        ch.request_pty(term, None, Some((cols.into(), rows.into(), 0, 0)))
            .map_err(ssh_err)?;
        ch.shell().map_err(ssh_err)?;
        self.session.set_blocking(false);
        let shared = Arc::new(Mutex::new(ch));
        Ok(Pty {
            reader: Box::new(PtyReader {
                ch: Arc::clone(&shared),
                tcp: self.tcp.try_clone()?,
                poll: self.opts.poll,
            }),
            writer: Box::new(PtyWriterImpl {
                ch: shared,
                session: self.session.clone(),
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
        self.session
            .disconnect(
                Some(ssh2::DisconnectCode::ByApplication),
                "legcon: logged off",
                None,
            )
            .map_err(ssh_err)
    }
}

fn lock(m: &Mutex<Channel>) -> std::sync::MutexGuard<'_, Channel> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct PtyReader {
    ch: Arc<Mutex<Channel>>,
    tcp: TcpStream,
    poll: Duration,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut spun = false;
        loop {
            {
                let ch = lock(&self.ch);
                for stream in [ch.stream(0), ch.stderr()].iter_mut() {
                    match stream.read(buf) {
                        Ok(0) => {}
                        Ok(n) => return Ok(n),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                    }
                }
                if ch.eof() {
                    return Ok(0);
                }
            }
            // Nothing decrypted yet: wait for the socket rather than spinning.
            if spun {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.tcp.set_read_timeout(Some(self.poll))?;
            match self.tcp.peek(&mut [0u8; 1]) {
                Ok(0) => return Ok(0),
                Ok(_) => spun = true,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Err(ErrorKind::TimedOut.into())
                }
                Err(e) => return Err(e),
            }
        }
    }
}

struct PtyWriterImpl {
    ch: Arc<Mutex<Channel>>,
    session: Session,
    closed: bool,
}

impl PtyWriter for PtyWriterImpl {
    fn send(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            match lock(&self.ch).write(bytes) {
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if !bytes.is_empty() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        again(|| lock(&self.ch).request_pty_size(cols.into(), rows.into(), None, None))
    }

    fn close(&mut self) -> Result<()> {
        if std::mem::replace(&mut self.closed, true) {
            return Ok(());
        }
        let _ = again(|| lock(&self.ch).close());
        again(|| {
            self.session.disconnect(
                Some(ssh2::DisconnectCode::ByApplication),
                "legcon: logged off",
                None,
            )
        })
    }
}

/// Reads a file through SFTP from an offset; closes the handle when dropped.
struct FileReader<'a> {
    sftp: &'a mut Sftp<Channel>,
    handle: Vec<u8>,
    at: u64,
}

impl Read for FileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let want = (buf.len() as u32).min(CHUNK);
        let data = self
            .sftp
            .read(&self.handle, self.at, want)
            .map_err(std::io::Error::other)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.at += n as u64;
        Ok(n)
    }
}

impl Drop for FileReader<'_> {
    fn drop(&mut self) {
        let _ = self.sftp.close(&self.handle);
    }
}

struct FileWriter<'a> {
    sftp: &'a mut Sftp<Channel>,
    handle: Vec<u8>,
    at: u64,
}

impl Write for FileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(CHUNK as usize);
        self.sftp
            .write(&self.handle, self.at, &buf[..n])
            .map_err(std::io::Error::other)?;
        self.at += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for FileWriter<'_> {
    fn drop(&mut self) {
        let _ = self.sftp.close(&self.handle);
    }
}

impl Connection for SshSession {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    fn disconnect(&mut self, timeout: Duration) -> Result<()> {
        self.session.set_timeout(timeout.as_millis() as u32);
        self.log_off()
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
            resume_upload: sftp,
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
            return Ok(Box::new(self.scp_read(path, at)?));
        }
        let raw = self.path(path);
        let sftp = self.sftp()?;
        let handle = sftp.open(&raw, flag::READ)?;
        Ok(Box::new(FileReader { sftp, handle, at }))
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
        Ok(Box::new(FileWriter { sftp, handle, at }))
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
            return Err(Error::Unsupported("SCP cannot continue an upload".into()));
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
