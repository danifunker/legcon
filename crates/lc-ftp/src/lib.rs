//! FTP for legcon (docs/PLAN.md §4.1): a blocking RFC 959 client for 1990s servers.
//!
//! Written here rather than wrapping suppaftp: that crate decodes listings with
//! `from_utf8_lossy` and sends paths as UTF-8, which mangles the Latin-1 and MacRoman names old
//! servers use. Here every name goes through the machine's [`Charset`] both ways.
//!
//! **Passive mode only** (PLAN.md D26): the client never listens, so nothing has to be open on
//! the user's computer. The data connection goes to the control connection's address, not the
//! one in the `227` reply, which old servers behind NAT get wrong. Listings are taken with
//! `CWD` + bare `LIST`, the form DOS, VMS and NetWare servers all accept.

pub mod list;

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::{
    Charset, ConnState, Connection, Entry, Error, FileCaps, FileSession, PathStyle, RemotePath,
    Result, WriteMode,
};

/// The default port.
pub const PORT: u16 = 21;

#[derive(Debug, Clone)]
pub struct Options {
    pub charset: Charset,
    pub timeout: Duration,
    /// Seconds since 1970 to place listing dates without a year; `None` uses the clock.
    pub now: Option<i64>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: Charset::Latin1,
            timeout: Duration::from_secs(20),
            now: None,
        }
    }
}

/// One line of the protocol trace: what was sent (passwords masked) or received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    Sent(String),
    Received(String),
    Note(String),
}

/// A server reply: its code and its lines (the last carries the code too).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn class(&self) -> u16 {
        self.code / 100
    }
}

struct Control {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    charset: Charset,
    trace: Arc<Mutex<Vec<Trace>>>,
}

impl Control {
    fn note(&self, t: Trace) {
        if let Ok(mut v) = self.trace.lock() {
            // Keep the trace bounded; the Log tab shows the recent end.
            if v.len() >= 2000 {
                v.drain(..500);
            }
            v.push(t);
        }
    }

    fn send(&mut self, verb: &str, arg: Option<&str>) -> Result<()> {
        let mut line = verb.as_bytes().to_vec();
        if let Some(a) = arg {
            line.push(b' ');
            line.extend(self.charset.encode(a).ok_or_else(|| {
                Error::Unsupported(format!(
                    "{a:?} has characters the machine's character set lacks"
                ))
            })?);
        }
        let shown = if verb == "PASS" {
            "PASS ********".to_string()
        } else {
            self.charset.decode(&line)
        };
        self.note(Trace::Sent(shown));
        line.extend(b"\r\n");
        self.writer.write_all(&line)?;
        Ok(())
    }

    fn line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = self
            .reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                    Error::Timeout(Duration::from_secs(0))
                }
                _ => Error::Io(e),
            })?;
        if n == 0 {
            return Err(Error::Io(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "the server closed the connection",
            )));
        }
        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }
        Ok(self.charset.decode(&buf))
    }

    /// Read one reply, multi-line ones included (`123-...` up to `123 ...`).
    fn reply(&mut self) -> Result<Reply> {
        let first = self.line()?;
        self.note(Trace::Received(first.clone()));
        let code: u16 = first
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| Error::Protocol(format!("not a reply: {first:?}")))?;
        let mut lines = vec![first.clone()];
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let l = self.line()?;
                self.note(Trace::Received(l.clone()));
                let done = l.starts_with(&end) || l == code.to_string();
                lines.push(l);
                if done {
                    break;
                }
            }
        }
        Ok(Reply { code, lines })
    }

    fn command(&mut self, verb: &str, arg: Option<&str>) -> Result<Reply> {
        self.send(verb, arg)?;
        self.reply()
    }
}

fn refuse(r: &Reply, what: &str) -> Error {
    let msg = format!("{what}: {}", r.text());
    match r.code {
        530 | 532 => Error::Auth(msg),
        550 if what.starts_with("open") || what.starts_with("list") => Error::NotFound(msg),
        550 | 553 => Error::Denied(msg),
        500 | 502 | 504 => Error::Unsupported(msg),
        _ => Error::Protocol(msg),
    }
}

/// The port from a `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)` reply.
pub fn pasv_port(text: &str) -> Option<u16> {
    let start = text.find(|c: char| c.is_ascii_digit() && text.contains(','))?;
    let nums: Vec<u16> = text[start..]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    // The reply's own code may come first when the parser started at it.
    let nums = if nums.len() >= 7 && nums[0] == 227 {
        &nums[1..]
    } else {
        &nums[..]
    };
    (nums.len() >= 6 && nums[..6].iter().all(|n| *n <= 255)).then(|| nums[4] * 256 + nums[5])
}

/// A logged-in FTP session.
pub struct FtpSession {
    ctrl: Control,
    peer: IpAddr,
    opts: Options,
    state: ConnState,
    cwd: Option<String>,
    trace: Arc<Mutex<Vec<Trace>>>,
}

/// Connect and log in; an empty `user` logs in as `anonymous`.
pub fn connect(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    opts: &Options,
) -> Result<FtpSession> {
    let addr: SocketAddr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let stream = TcpStream::connect_timeout(&addr, opts.timeout).map_err(|e| match e.kind() {
        ErrorKind::TimedOut => Error::Timeout(opts.timeout),
        _ => Error::Io(e),
    })?;
    stream.set_read_timeout(Some(opts.timeout))?;
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut ctrl = Control {
        reader: BufReader::new(stream.try_clone()?),
        writer: stream,
        charset: opts.charset,
        trace: Arc::clone(&trace),
    };
    let mut hello = ctrl.reply()?;
    // 120: "ready in a few minutes"; the real greeting follows.
    while hello.code == 120 {
        hello = ctrl.reply()?;
    }
    if hello.code != 220 {
        return Err(refuse(&hello, "greeting"));
    }
    let (user, password) = if user.is_empty() {
        (
            "anonymous",
            if password.is_empty() {
                "legcon@"
            } else {
                password
            },
        )
    } else {
        (user, password)
    };
    let r = ctrl.command("USER", Some(user))?;
    let r = match r.code {
        230 => r,
        331 | 332 => ctrl.command("PASS", Some(password))?,
        _ => return Err(refuse(&r, "login")),
    };
    if r.code != 230 && r.code != 202 {
        return Err(refuse(&r, "login"));
    }
    let r = ctrl.command("TYPE", Some("I"))?;
    if r.class() != 2 {
        return Err(refuse(&r, "binary mode"));
    }
    Ok(FtpSession {
        ctrl,
        peer: addr.ip(),
        opts: opts.clone(),
        state: ConnState::Connected,
        cwd: None,
        trace,
    })
}

/// A transfer's data stream; dropping it reads the transfer's final reply.
struct Transfer<'a> {
    data: Option<TcpStream>,
    ctrl: &'a mut Control,
    upload: bool,
}

impl Read for Transfer<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.data.as_mut() {
            Some(d) => d.read(buf),
            None => Ok(0),
        }
    }
}

impl Write for Transfer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.data.as_mut() {
            Some(d) => d.write(buf),
            None => Err(std::io::Error::new(
                ErrorKind::BrokenPipe,
                "transfer finished",
            )),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.data.as_mut().map_or(Ok(()), Write::flush)
    }
}

impl Drop for Transfer<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.data.take() {
            if self.upload {
                let _ = d.shutdown(Shutdown::Write);
            }
            drop(d);
        }
        // 226 / 250 when done, 426 when aborted; either way the control channel is in step again.
        let _ = self.ctrl.reply();
    }
}

impl FtpSession {
    /// The protocol trace so far (commands and replies, passwords masked).
    pub fn trace(&self) -> Arc<Mutex<Vec<Trace>>> {
        Arc::clone(&self.trace)
    }

    /// Send a raw command (the Console tab); its reply.
    pub fn quote(&mut self, line: &str) -> Result<Reply> {
        let (verb, arg) = match line.trim().split_once(' ') {
            Some((v, a)) => (v.to_ascii_uppercase(), Some(a.to_string())),
            None => (line.trim().to_ascii_uppercase(), None),
        };
        if matches!(verb.as_str(), "PORT" | "EPRT") {
            return Err(Error::Unsupported(
                "active mode: legcon never opens a port on this computer".into(),
            ));
        }
        self.ctrl.command(&verb, arg.as_deref())
    }

    fn live(&self) -> Result<()> {
        match self.state {
            ConnState::Connected | ConnState::Busy => Ok(()),
            _ => Err(Error::NotConnected),
        }
    }

    fn now(&self) -> i64 {
        self.opts.now.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
    }

    /// Open a passive data connection, to the control connection's address.
    fn data(&mut self) -> Result<TcpStream> {
        let r = self.ctrl.command("PASV", None)?;
        if r.code != 227 {
            return Err(refuse(&r, "passive mode"));
        }
        let port = pasv_port(&r.text())
            .ok_or_else(|| Error::Protocol(format!("bad PASV reply: {}", r.text())))?;
        let addr = SocketAddr::new(self.peer, port);
        self.ctrl
            .note(Trace::Note(format!("data connection to {addr}")));
        let s = TcpStream::connect_timeout(&addr, self.opts.timeout)?;
        s.set_read_timeout(Some(self.opts.timeout))?;
        Ok(s)
    }

    fn cd(&mut self, dir: &RemotePath) -> Result<()> {
        let d = dir.to_string();
        if self.cwd.as_deref() == Some(d.as_str()) {
            return Ok(());
        }
        let r = self.ctrl.command("CWD", Some(&d))?;
        if r.class() != 2 {
            self.cwd = None;
            return Err(refuse(&r, &format!("list {d}")));
        }
        self.cwd = Some(d);
        Ok(())
    }

    /// Start a transfer command on a fresh data connection; its stream.
    fn transfer(&mut self, verb: &str, arg: Option<&str>, upload: bool) -> Result<Transfer<'_>> {
        let data = self.data()?;
        let r = self.ctrl.command(verb, arg)?;
        if r.code != 125 && r.code != 150 {
            return Err(refuse(&r, &format!("open {}", arg.unwrap_or(""))));
        }
        Ok(Transfer {
            data: Some(data),
            ctrl: &mut self.ctrl,
            upload,
        })
    }
}

impl Connection for FtpSession {
    fn state(&self) -> ConnState {
        self.state
    }

    fn disconnect(&mut self, timeout: Duration) -> Result<()> {
        if !matches!(self.state, ConnState::Connected | ConnState::Busy) {
            return Ok(());
        }
        self.state = ConnState::Disconnecting;
        let _ = self.ctrl.writer.set_read_timeout(Some(timeout));
        let result = self.ctrl.command("QUIT", None).map(|_| ());
        let _ = self.ctrl.writer.shutdown(Shutdown::Both);
        self.state = ConnState::Disconnected;
        result
    }
}

impl FileSession for FtpSession {
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: true,
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
        self.live()?;
        let r = self.ctrl.command("PWD", None)?;
        let text = r.text();
        // 257 "/export/home/dani" is current directory; a quote inside is doubled.
        let path = match (text.find('"'), text.rfind('"')) {
            (Some(a), Some(b)) if b > a => text[a + 1..b].replace("\"\"", "\""),
            _ => text.get(4..).unwrap_or("/").trim().to_string(),
        };
        let style = if path.contains('\\') || path.get(1..2) == Some(":") {
            PathStyle::Dos
        } else {
            PathStyle::Unix
        };
        let home = RemotePath::parse(style, &path);
        self.cwd = Some(home.to_string());
        Ok(home)
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        self.live()?;
        self.cd(dir)?;
        let cs = self.ctrl.charset;
        let now = self.now();
        let mut raw = Vec::new();
        self.transfer("LIST", None, false)?.read_to_end(&mut raw)?;
        Ok(list::parse(&raw, cs, now))
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        self.list(&parent)?
            .into_iter()
            .find(|e| e.name == path.name())
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        self.live()?;
        if at > 0 {
            let r = self.ctrl.command("REST", Some(&at.to_string()))?;
            if r.code != 350 {
                return Err(refuse(&r, "resume"));
            }
        }
        let p = path.to_string();
        Ok(Box::new(self.transfer("RETR", Some(&p), false)?))
    }

    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        self.live()?;
        let p = path.to_string();
        let verb = match mode {
            WriteMode::Create | WriteMode::Overwrite => "STOR",
            WriteMode::Resume(_) => return Err(Error::Unsupported("resumed uploads".into())),
        };
        Ok(Box::new(self.transfer(verb, Some(&p), true)?))
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        self.live()?;
        let r = self.ctrl.command("MKD", Some(&path.to_string()))?;
        if r.class() == 2 {
            Ok(())
        } else {
            Err(refuse(&r, "new folder"))
        }
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        self.live()?;
        let p = path.to_string();
        let r = self.ctrl.command("DELE", Some(&p))?;
        if r.class() == 2 {
            return Ok(());
        }
        // DELE refuses folders; RMD takes an empty one.
        let r = self.ctrl.command("RMD", Some(&p))?;
        if r.class() == 2 {
            Ok(())
        } else {
            Err(refuse(&r, "delete"))
        }
    }

    fn quote(&mut self, dir: &RemotePath, line: &str) -> Result<String> {
        self.live()?;
        // The server's folder moves with every listing and copy; the user means the pane's.
        self.cd(dir)?;
        // A command that opens a data connection would leave the control channel out of step.
        let verb = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(
            verb.as_str(),
            "LIST" | "NLST" | "RETR" | "STOR" | "APPE" | "STOU" | "PASV" | "EPSV"
        ) {
            return Err(Error::Unsupported(format!(
                "{verb} needs a data connection; use the panes for that"
            )));
        }
        if verb == "QUIT" {
            return Err(Error::Unsupported("use Disconnect to log off".into()));
        }
        let r = FtpSession::quote(self, line)?;
        self.cwd = None;
        Ok(r.text())
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.live()?;
        let r = self.ctrl.command("RNFR", Some(&from.to_string()))?;
        if r.code != 350 {
            return Err(refuse(&r, "rename"));
        }
        let r = self.ctrl.command("RNTO", Some(&to.to_string()))?;
        if r.class() == 2 {
            Ok(())
        } else {
            Err(refuse(&r, "rename"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pasv_port;

    #[test]
    fn pasv_replies() {
        assert_eq!(
            pasv_port("227 Entering Passive Mode (192,168,1,40,129,44)"),
            Some(129 * 256 + 44)
        );
        assert_eq!(
            pasv_port("227 Entering passive mode. 10,0,0,5,4,1"),
            Some(1025)
        );
        assert_eq!(pasv_port("227 nonsense"), None);
    }
}
