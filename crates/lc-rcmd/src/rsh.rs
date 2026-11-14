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
                return Err(Error::Denied(
                    "this computer will not let legcon use a source port below 1024, which rsh \
                     and rlogin need (on Linux: setcap cap_net_bind_service=+ep on legcon)"
                        .into(),
                ))
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

/// Read the 0 that means "accepted", or the refusal line after a 1.
fn accepted(s: &mut TcpStream, what: &str) -> Result<()> {
    let mut b = [0u8; 1];
    if s.read(&mut b)? == 0 {
        return Err(Error::Protocol(format!(
            "{what}: the server closed the connection (is this computer in its .rhosts?)"
        )));
    }
    if b[0] == 0 {
        return Ok(());
    }
    let mut line = Vec::new();
    while s.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
        line.push(b[0]);
    }
    let text = String::from_utf8_lossy(&line).trim().to_string();
    Err(Error::Auth(format!(
        "{text} (rsh trusts .rhosts / hosts.equiv on the server, and this computer's name)"
    )))
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
    let mut s = dial_reserved(host, port, opts)?;
    let mut req = b"0\0".to_vec();
    for part in [local_user, remote_user, command] {
        req.extend_from_slice(part);
        req.push(0);
    }
    s.write_all(&req)?;
    accepted(&mut s, "rsh")?;
    Ok(s)
}

/// Single-quote for the Bourne shell.
pub fn sq(b: &[u8]) -> Vec<u8> {
    let mut v = b"'".to_vec();
    for &c in b {
        if c == b'\'' {
            v.extend_from_slice(b"'\\''");
        } else {
            v.push(c);
        }
    }
    v.push(b'\'');
    v
}

/// The end-of-command marker file commands print with their exit status.
const MARK: &[u8] = b"@legcon-status:";

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
        r.run(b"true")?;
        Ok(r)
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
        let mut full = b"exec /bin/sh -c ".to_vec();
        full.extend_from_slice(&sq(&inner));
        let out = self.run(&full)?;
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
        Ok(ExecResult {
            output: self.run(&c)?,
            errors: Vec::new(),
            exit_status: None,
        })
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

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
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
            return Err(Error::Unsupported("rcp cannot continue an upload".into()));
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
        let mut full = b"exec /bin/sh -c ".to_vec();
        full.extend_from_slice(&sq(&cmd));
        let out = self.run(&full)?;
        Ok(self.charset.decode(&out).trim_end().to_string())
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
}
