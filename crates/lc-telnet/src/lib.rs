//! Telnet for legcon (docs/PLAN.md §4.1): a client that negotiates options with 1990s
//! `telnetd`s and hands back a split [`Pty`].
//!
//! The option engine is [`nvt::Nvt`], written here rather than wrapping libtelnet-rs: it is a
//! few hundred lines, needs exactly the policy legcon wants (BINARY, ECHO, SGA, TTYPE, NAWS),
//! and keeps the crate free of dependencies beyond `lc-core`. Only outgoing connections are made.

pub mod nvt;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::{Error, Pty, PtyWriter, Result};

pub use nvt::Nvt;

/// The default port.
pub const PORT: u16 = 23;

#[derive(Debug, Clone)]
pub struct Options {
    /// The terminal type sent when asked (`vt100`, `vt220`, `ansi`).
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    pub connect_timeout: Duration,
    /// How long a read waits before returning `TimedOut`, so a reader can check for cancel.
    pub poll: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            term: "vt100".into(),
            cols: 80,
            rows: 24,
            connect_timeout: Duration::from_secs(10),
            poll: Duration::from_millis(250),
        }
    }
}

fn lock(n: &Mutex<Nvt>) -> std::sync::MutexGuard<'_, Nvt> {
    n.lock().unwrap_or_else(|p| p.into_inner())
}

struct Reader {
    stream: TcpStream,
    replies: TcpStream,
    nvt: Arc<Mutex<Nvt>>,
    pending: Vec<u8>,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut raw = [0u8; 4096];
        while self.pending.is_empty() {
            let n = self.stream.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            let got = lock(&self.nvt).receive(&raw[..n]);
            if !got.reply.is_empty() {
                self.replies.write_all(&got.reply)?;
            }
            self.pending = got.data;
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

struct Writer {
    stream: TcpStream,
    nvt: Arc<Mutex<Nvt>>,
}

impl PtyWriter for Writer {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let wire = lock(&self.nvt).encode(bytes);
        self.stream.write_all(&wire)?;
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let wire = lock(&self.nvt).resize(cols, rows);
        if !wire.is_empty() {
            self.stream.write_all(&wire)?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        match self.stream.shutdown(Shutdown::Both) {
            Err(e) if e.kind() != ErrorKind::NotConnected => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// Connect to `host:port` and start negotiating; the session's terminal is ready to use.
pub fn connect(host: &str, port: u16, opts: &Options) -> Result<Pty> {
    connect_with_local(host, port, opts).map(|(pty, _)| pty)
}

/// [`connect`], also giving our end of the connection (address and port).
pub fn connect_with_local(
    host: &str,
    port: u16,
    opts: &Options,
) -> Result<(Pty, std::net::SocketAddr)> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let stream = TcpStream::connect_timeout(&addr, opts.connect_timeout).map_err(|e| {
        if e.kind() == ErrorKind::TimedOut {
            Error::Timeout(opts.connect_timeout)
        } else {
            Error::Io(e)
        }
    })?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(opts.poll))?;
    let local = stream.local_addr()?;
    let mut nvt = Nvt::new(&opts.term, opts.cols, opts.rows);
    let hello = nvt.greeting();
    let nvt = Arc::new(Mutex::new(nvt));
    let mut writer = Writer {
        stream: stream.try_clone()?,
        nvt: Arc::clone(&nvt),
    };
    writer.stream.write_all(&hello)?;
    let reader = Reader {
        replies: stream.try_clone()?,
        stream,
        nvt,
        pending: Vec::new(),
    };
    Ok((
        Pty {
            reader: Box::new(reader),
            writer: Box::new(writer),
        },
        local,
    ))
}
