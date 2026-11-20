//! HTTP/1.0 and Gopher for legcon (docs/PLAN.md §4.3, D33): read-only browsing.
//!
//! A web server's folder is what its index page links to (Apache, NCSA and CERN `Index of`
//! pages, and MacHTTP / WebSTAR folder lists); a Gopher hole's folders are its menus. Both are
//! our own code, a few hundred lines, so names stay bytes in the machine's character set.

pub mod gopher;
pub mod http;

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use lc_core::{Error, Result};

pub use gopher::GopherSession;
pub use http::HttpSession;

#[derive(Debug, Clone)]
pub struct Options {
    pub charset: lc_core::Charset,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: lc_core::Charset::default(),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
        }
    }
}

pub(crate) fn dial(host: &str, port: u16, opts: &Options) -> Result<TcpStream> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let s = TcpStream::connect_timeout(&addr, opts.connect_timeout).map_err(|e| {
        if e.kind() == ErrorKind::TimedOut {
            Error::Timeout(opts.connect_timeout)
        } else {
            Error::Io(e)
        }
    })?;
    s.set_read_timeout(Some(opts.read_timeout))?;
    s.set_nodelay(true)?;
    lc_core::net::tune(&s, opts.read_timeout)?;
    Ok(s)
}

pub(crate) fn read_only() -> Error {
    Error::Unsupported("this service is read-only".into())
}

/// A name made safe for a path part: a `/` inside a name becomes `-`.
pub(crate) fn part_name(s: &str) -> String {
    s.replace('/', "-")
}
