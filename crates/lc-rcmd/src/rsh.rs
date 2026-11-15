//! rsh (port 514), rlogin (513) and rcp over rsh.
//!
//! No password: the server trusts us because of `.rhosts` / `hosts.equiv` and because the
//! connection comes from a source port below 1024, which on 1990s Unix only root could bind
//! (docs/PLAN.md §4.2). Windows lets anyone bind one; Linux needs `CAP_NET_BIND_SERVICE`.
//! rsh is sent `0` as its stderr port, so errors come back on the same connection and the
//! server never connects back to us (D26). rsh reports no exit status, so file commands echo
//! theirs at the end.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lc_core::rcp::{self, shell_error};
use lc_core::{
    Charset, ConnState, Connection, Entry, Error, ExecResult, FileCaps, FileSession, PathStyle,
    Pty, PtyWriter, RemotePath, Result, ShellCaps, ShellSession, WriteMode,
};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::Options;

pub const RSH_PORT: u16 = 514;
pub const RLOGIN_PORT: u16 = 513;

/// What this computer says when it will not hand out a port below 1024.
pub const NO_LOW_PORT: &str = "this computer will not let legcon use a source port below 1024, \
     which rsh and rlogin need (on Linux: setcap cap_net_bind_service=+ep on legcon)";

/// Why rsh would not let us in, as the user can act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Untrusted {
    /// This computer would not give legcon a source port below 1024.
    NoLowPort,
    /// The server refused; `local` is the address and port we connected from.
    Refused { local: SocketAddr, said: String },
}

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Untrusted::NoLowPort => f.write_str(NO_LOW_PORT),
            Untrusted::Refused { local, said } => {
                let said = if said.is_empty() {
                    "the server closed the connection".to_string()
                } else {
                    said.clone()
                };
                write!(
                    f,
                    "{said}. rsh trusts .rhosts / hosts.equiv on the server; this computer came from {} port {}",
                    local.ip(),
                    local.port()
                )
            }
        }
    }
}

/// Connect from a reserved port, trying 1023 downwards as `rresvport()` does.
pub fn dial_reserved(host: &str, port: u16, opts: &Options) -> Result<TcpStream> {
    let addr: SocketAddr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let mut last = None;
    for local in (512..1024u16).rev() {
        let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        let mut any: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        any.set_port(local);
        match s.bind(&SockAddr::from(any)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                return Err(Error::Denied(NO_LOW_PORT.into()))
            }
            Err(e) => {
                last = Some(e);
                continue;
            }
        }
        match s.connect_timeout(&SockAddr::from(addr), opts.connect_timeout) {
            Ok(()) => {
                let s: TcpStream = s.into();
                s.set_nodelay(true)?;
                s.set_read_timeout(Some(opts.read_timeout))?;
                return Ok(s);
            }
            // A port still in TIME_WAIT towards this server: try the next one.
            Err(e) if e.kind() == ErrorKind::AddrInUse => last = Some(e),
            Err(e) if e.kind() == ErrorKind::TimedOut => {
                return Err(Error::Timeout(opts.connect_timeout))
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::Io(last.unwrap_or_else(|| {
        std::io::Error::other("no reserved port free")
    })))
}

/// Read the 0 that means "accepted"; a refusal line after a 1, or a close, is `Untrusted`.
fn accepted(s: &mut TcpStream) -> Result<std::result::Result<(), Untrusted>> {
    let local = s.local_addr()?;
    let mut b = [0u8; 1];
    let refused = |said: String| Ok(Err(Untrusted::Refused { local, said }));
    match s.read(&mut b) {
        Ok(0) => return refused(String::new()),
        Err(e) if e.kind() == ErrorKind::ConnectionReset => return refused(String::new()),
        Err(e) => return Err(e.into()),
        Ok(_) if b[0] == 0 => return Ok(Ok(())),
        Ok(_) => {}
    }
    let mut line = Vec::new();
    while s.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
        line.push(b[0]);
    }
    refused(String::from_utf8_lossy(&line).trim().to_string())
}

/// Start `command` under rsh, telling a trust problem apart from any other failure.
pub fn try_start(
    host: &str,
    port: u16,
    local_user: &[u8],
    remote_user: &[u8],
    command: &[u8],
    opts: &Options,
) -> Result<std::result::Result<TcpStream, Untrusted>> {
    let mut s = match dial_reserved(host, port, opts) {
        Err(Error::Denied(m)) if m == NO_LOW_PORT => return Ok(Err(Untrusted::NoLowPort)),
        r => r?,
    };
    let mut req = b"0\0".to_vec();
    for part in [local_user, remote_user, command] {
        req.extend_from_slice(part);
        req.push(0);
    }
    s.write_all(&req)?;
    Ok(accepted(&mut s)?.map(|()| s))
}

/// Start `command` under rsh; the stream gives its output, errors included.
pub fn start(
    host: &str,
    port: u16,
    local_user: &[u8],
    remote_user: &[u8],
    command: &[u8],
    opts: &Options,
) -> Result<TcpStream> {
    try_start(host, port, local_user, remote_user, command, opts)?
        .map_err(|u| Error::Auth(format!("rsh: {u}")))
}

pub use lc_core::shellfile::sq;
use lc_core::shellfile::{self, under_sh};

/// The end-of-command marker file commands print with their exit status.
const MARK: &[u8] = b"@legcon-status:";
/// Where a command's errors start, in [`Rsh::exec_bytes`]'s output.
const ERRORS: &[u8] = b"@legcon-errors:\n";

/// An rsh connection as the link an rcp copy runs over.
pub struct RshLink(TcpStream);

impl Read for RshLink {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(b)
    }
}

impl Write for RshLink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl rcp::Link for RshLink {
    fn close_write(&mut self) -> std::io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
    fn end(&mut self) {
        let _ = self.0.shutdown(Shutdown::Write);
        let _ = self.0.read_to_end(&mut Vec::new());
    }
    fn abort(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// A machine's rsh service: files through rcp and shell commands, commands one at a time.
pub struct Rsh {
    host: String,
    port: u16,
    local_user: Vec<u8>,
    remote_user: Vec<u8>,
    charset: Charset,
    opts: Options,
    closed: bool,
}

impl Rsh {
    /// Check that the server lets us in, with a harmless command.
    pub fn connect(
        host: &str,
        port: u16,
        local_user: &str,
        remote_user: &str,
        charset: Charset,
        opts: &Options,
    ) -> Result<Rsh> {
        Self::check(host, port, local_user, remote_user, charset, opts)?
            .map_err(|u| Error::Auth(format!("rsh: {u}")))
    }

    /// Like [`Rsh::connect`], with a trust problem kept apart from other failures.
    pub fn check(
        host: &str,
        port: u16,
        local_user: &str,
        remote_user: &str,
        charset: Charset,
        opts: &Options,
    ) -> Result<std::result::Result<Rsh, Untrusted>> {
        let enc = |t: &str| charset.encode(t).unwrap_or_else(|| t.as_bytes().to_vec());
        let r = Rsh {
            host: host.to_string(),
            port,
            local_user: enc(local_user),
            remote_user: enc(remote_user),
            charset,
            opts: opts.clone(),
            closed: false,
        };
        let mut s = match try_start(
            &r.host,
            r.port,
            &r.local_user,
            &r.remote_user,
            b"true",
            &r.opts,
        )? {
            Ok(s) => s,
            Err(u) => return Ok(Err(u)),
        };
        let _ = s.read_to_end(&mut Vec::new());
        Ok(Ok(r))
    }

    fn stream(&self, command: &[u8]) -> Result<TcpStream> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        start(
            &self.host,
            self.port,
            &self.local_user,
            &self.remote_user,
            command,
            &self.opts,
        )
    }

    /// Run `command` and collect its output (errors merged in).
    pub fn run(&self, command: &[u8]) -> Result<Vec<u8>> {
        // Not half-closed: some servers and NAT relays then drop the reply (seen with rexecd).
        let mut s = self.stream(command)?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).map_err(|e| {
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                Error::Timeout(self.opts.read_timeout)
            } else {
                Error::Io(e)
            }
        })?;
        Ok(out)
    }

    /// Run a Bourne shell command whatever the login shell is (csh is common); its output, or
    /// an error made from it when it failed.
    fn sh(&self, what: &str, cmd: &[u8]) -> Result<Vec<u8>> {
        let mut inner = b"LC_ALL=C; export LC_ALL; ".to_vec();
        inner.extend_from_slice(cmd);
        inner.extend_from_slice(b" 2>&1; echo \"");
        inner.extend_from_slice(MARK);
        inner.extend_from_slice(b"$?\"");
        let out = self.run(&under_sh(&inner))?;
        let at = out
            .windows(MARK.len())
            .rposition(|w| w == MARK)
            .ok_or_else(|| shell_error(what, &out))?;
        let status = String::from_utf8_lossy(&out[at + MARK.len()..]);
        let body = out[..at].to_vec();
        if status.trim() == "0" {
            Ok(body)
        } else {
            Err(shell_error(what, &body))
        }
    }

    /// Run a command with its errors kept apart: they go to a file on the server and come back
    /// after the output, so the server never connects back to us; the exit status follows.
    pub fn exec_bytes(&self, command: &[u8]) -> Result<ExecResult> {
        let mut inner = b"e=/tmp/.legcon-err.$$; ( ".to_vec();
        inner.extend_from_slice(command);
        inner.extend_from_slice(b" ) 2>$e; s=$?; echo; echo '");
        inner.extend_from_slice(ERRORS.trim_ascii_end());
        inner.extend_from_slice(b"'; cat $e 2>/dev/null; rm -f $e; echo \"");
        inner.extend_from_slice(MARK);
        inner.extend_from_slice(b"$s\"");
        Ok(split_exec(&self.run(&under_sh(&inner))?))
    }

    fn path(&self, p: &RemotePath) -> Vec<u8> {
        let t = p.to_string();
        self.charset.encode(&t).unwrap_or_else(|| t.into_bytes())
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Take [`Rsh::exec_bytes`]'s output apart: output, errors, exit status.
fn split_exec(out: &[u8]) -> ExecResult {
    let Some(at) = out.windows(MARK.len()).rposition(|w| w == MARK) else {
        return ExecResult {
            output: out.to_vec(),
            errors: Vec::new(),
            exit_status: None,
        };
    };
    let status = String::from_utf8_lossy(&out[at + MARK.len()..])
        .trim()
        .parse()
        .ok();
    let body = &out[..at];
    let (output, errors) = match body.windows(ERRORS.len()).rposition(|w| w == ERRORS) {
        // The echo before the marker added a newline to the output.
        Some(e) => (&body[..e.saturating_sub(1)], &body[e + ERRORS.len()..]),
        None => (body, &[][..]),
    };
    ExecResult {
        output: output.to_vec(),
        errors: errors.to_vec(),
        exit_status: status,
    }
}

impl Connection for Rsh {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    /// Nothing is held open between commands.
    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}

impl ShellSession for Rsh {
    fn caps(&self) -> ShellCaps {
        ShellCaps {
            pty: false,
            exec: true,
            exit_status: false,
            window_size: false,
        }
    }

    fn exec(&mut self, command: &str) -> Result<ExecResult> {
        let c = self
            .charset
            .encode(command)
            .unwrap_or_else(|| command.as_bytes().to_vec());
        self.exec_bytes(&c)
    }

    fn open_pty(&mut self, _term: &str, _cols: u16, _rows: u16) -> Result<Pty> {
        Err(Error::Unsupported("use rlogin for a terminal".into()))
    }
}

impl FileSession for Rsh {
    fn charset(&self) -> lc_core::Charset {
        self.charset
    }
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: false,
            random_write: false,
            resume_upload: true,
            rename: true,
            delete: true,
            mkdir: true,
            set_times: false,
            set_mode: false,
        }
    }

    fn home(&mut self) -> Result<RemotePath> {
        let out = self.sh("pwd", b"pwd")?;
        let dir = self.charset.decode(out.trim_ascii());
        Ok(RemotePath::parse(PathStyle::Unix, &dir))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        let mut cmd = b"ls -laL ".to_vec();
        cmd.extend_from_slice(&sq(&self.path(dir)));
        let out = self.sh(&dir.to_string(), &cmd)?;
        // Links that point nowhere make ls complain on the same stream; those lines parse as nothing.
        Ok(lc_core::listing::parse(&out, self.charset, Self::now())
            .into_iter()
            .filter(|e| e.kind != lc_core::EntryKind::Other || e.size.is_some())
            .collect())
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let mut cmd = b"ls -ldL ".to_vec();
        cmd.extend_from_slice(&sq(&self.path(path)));
        let out = self.sh(&path.to_string(), &cmd)?;
        let line = out.split(|&b| b == b'\n').next().unwrap_or(&[]);
        let mut e = lc_core::listing::parse_unix(line, self.charset, Self::now())
            .ok_or_else(|| Error::Protocol(format!("cannot read ls output for {path}")))?;
        e.name = path.name().to_string();
        e.raw_name = self
            .path(path)
            .rsplit(|&b| b == b'/')
            .next()
            .unwrap_or(&[])
            .to_vec();
        Ok(e)
    }

    /// rcp from the start; from an offset, `dd` (rcp would send the skipped bytes too).
    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        if at > 0 {
            let s = self.stream(&shellfile::tail_command(&self.path(path), at))?;
            return Ok(Box::new(shellfile::tail(
                RshLink(s),
                &path.to_string(),
                at,
            )?));
        }
        let mut cmd = b"rcp -f ".to_vec();
        cmd.extend_from_slice(&sq(&self.path(path)));
        let s = self.stream(&cmd)?;
        Ok(Box::new(rcp::receive(RshLink(s), &path.to_string(), at)?))
    }

    fn open_write(
        &mut self,
        _path: &RemotePath,
        _mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        Err(Error::Unsupported(
            "rcp needs the size before the data".into(),
        ))
    }

    fn open_write_sized(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
        size: u64,
    ) -> Result<Box<dyn Write + Send + '_>> {
        if matches!(mode, WriteMode::Resume(n) if n > 0) {
            // rcp cannot append; `cat >>` can. `size` is what is left to send.
            let s = self.stream(&shellfile::append_command(&self.path(path)))?;
            return Ok(Box::new(shellfile::append(
                RshLink(s),
                size,
                &path.to_string(),
            )?));
        }
        let parent = path
            .parent()
            .unwrap_or_else(|| RemotePath::root(path.style));
        let mut cmd = b"rcp -t ".to_vec();
        cmd.extend_from_slice(&sq(&self.path(&parent)));
        let s = self.stream(&cmd)?;
        let name = self
            .charset
            .encode(path.name())
            .unwrap_or_else(|| path.name().as_bytes().to_vec());
        Ok(Box::new(rcp::send(
            RshLink(s),
            &name,
            size,
            &path.to_string(),
        )?))
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        let mut cmd = b"mkdir ".to_vec();
        cmd.extend_from_slice(&sq(&self.path(path)));
        self.sh(&path.to_string(), &cmd).map(drop)
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        let q = sq(&self.path(path));
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
        cmd.extend_from_slice(&sq(&self.path(from)));
        cmd.push(b' ');
        cmd.extend_from_slice(&sq(&self.path(to)));
        self.sh(&from.to_string(), &cmd).map(drop)
    }

    /// POSIX `cksum` on the machine; `None` where it has none.
    fn checksum(&mut self, path: &RemotePath) -> Result<Option<lc_core::Checksum>> {
        let cmd = lc_core::checksum::cksum_command(&self.path(path));
        Ok(self
            .sh(&path.to_string(), &cmd)
            .ok()
            .and_then(|out| lc_core::checksum::parse_cksum(&String::from_utf8_lossy(&out))))
    }

    /// A shell command run in `dir`, as typed in the Console tab.
    fn quote(&mut self, dir: &RemotePath, line: &str) -> Result<String> {
        let mut cmd = b"cd ".to_vec();
        cmd.extend_from_slice(&sq(&self.path(dir)));
        cmd.extend_from_slice(b" && ");
        cmd.extend_from_slice(
            &self
                .charset
                .encode(line)
                .unwrap_or_else(|| line.as_bytes().to_vec()),
        );
        let r = self.exec_bytes(&cmd)?;
        let mut text = self.charset.decode(&r.output);
        text.push_str(&self.charset.decode(&r.errors));
        let mut text = text.trim_end().to_string();
        if let Some(n) = r.exit_status.filter(|&n| n != 0) {
            text.push_str(&format!("\n[exit status {n}]"));
        }
        Ok(text)
    }
}

struct RloginReader(TcpStream);

impl Read for RloginReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

struct RloginWriter(TcpStream);

/// The window-size message rlogind looks for in the input: a magic cookie, then rows, cols, x, y.
pub fn window_size(cols: u16, rows: u16) -> [u8; 12] {
    let mut m = [0xff, 0xff, b's', b's', 0, 0, 0, 0, 0, 0, 0, 0];
    m[4..6].copy_from_slice(&rows.to_be_bytes());
    m[6..8].copy_from_slice(&cols.to_be_bytes());
    m
}

impl PtyWriter for RloginWriter {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.write_all(bytes)?;
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.0.write_all(&window_size(cols, rows))?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        match self.0.shutdown(Shutdown::Both) {
            Err(e) if e.kind() != ErrorKind::NotConnected => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// Open an rlogin terminal. Without a `.rhosts` entry the server asks for the password on screen.
#[allow(clippy::too_many_arguments)]
pub fn rlogin(
    host: &str,
    port: u16,
    local_user: &[u8],
    remote_user: &[u8],
    term: &str,
    cols: u16,
    rows: u16,
    poll: Duration,
    opts: &Options,
) -> Result<Pty> {
    let mut s = dial_reserved(host, port, opts)?;
    let mut req = vec![0u8];
    for part in [local_user, remote_user, format!("{term}/38400").as_bytes()] {
        req.extend_from_slice(part);
        req.push(0);
    }
    s.write_all(&req)?;
    let mut b = [0u8; 1];
    if s.read(&mut b)? == 0 || b[0] != 0 {
        return Err(Error::Auth("rlogin was refused".into()));
    }
    // rlogind asks for the window size at once; sending it unasked is what clients do.
    s.write_all(&window_size(cols, rows))?;
    s.set_read_timeout(Some(poll))?;
    Ok(Pty {
        reader: Box::new(RloginReader(s.try_clone()?)),
        writer: Box::new(RloginWriter(s)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_size_message() {
        assert_eq!(
            window_size(80, 24),
            [0xff, 0xff, b's', b's', 0, 24, 0, 80, 0, 0, 0, 0]
        );
        assert_eq!(sq(b"it's"), b"'it'\\''s'");
    }

    #[test]
    fn exec_output_errors_and_status_come_apart() {
        let r = split_exec(b"out1\nout2\n\n@legcon-errors:\noops\n@legcon-status:3\n");
        assert_eq!(r.output, b"out1\nout2\n");
        assert_eq!(r.errors, b"oops\n");
        assert_eq!(r.exit_status, Some(3));
        let r = split_exec(b"no newline\n@legcon-errors:\n@legcon-status:0\n");
        assert_eq!(
            (r.output.as_slice(), r.exit_status),
            (&b"no newline"[..], Some(0))
        );
    }
}
