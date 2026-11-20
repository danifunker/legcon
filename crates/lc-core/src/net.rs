//! Socket set-up every protocol shares: timeouts both ways, TCP keepalive, and a breaker.
//!
//! TCP keepalive finds a link that died without a word (a NAT that forgot it, a machine
//! switched off) within a couple of minutes, where a blocked read would wait for its timeout.

use std::io::ErrorKind;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Error, Result};

/// How long a link may be quiet before TCP asks the other end whether it is there.
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Write timeout and TCP keepalive on a connected socket (reads keep the timeout they have).
pub fn tune(s: &TcpStream, timeout: Duration) -> std::io::Result<()> {
    s.set_write_timeout(Some(timeout))?;
    let ka = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    // Some systems refuse the interval; keepalive with their defaults is still worth having.
    let sock = socket2::SockRef::from(s);
    if sock.set_tcp_keepalive(&ka).is_err() {
        sock.set_keepalive(true)?;
    }
    Ok(())
}

/// Connect within `connect`, then read and write with `timeout`, keepalive on.
pub fn dial(addr: &SocketAddr, connect: Duration, timeout: Duration) -> Result<TcpStream> {
    let s = TcpStream::connect_timeout(addr, connect).map_err(|e| {
        if e.kind() == ErrorKind::TimedOut {
            Error::Timeout(connect)
        } else {
            Error::Io(e)
        }
    })?;
    s.set_read_timeout(Some(timeout))?;
    tune(&s, timeout)?;
    Ok(s)
}

/// Shuts a session's socket from another thread; whatever waits on it returns at once.
#[derive(Clone, Default)]
pub struct Breaker(Arc<Mutex<Socks>>);

/// The sockets a breaker shuts, by the key [`Breaker::add`] gave them.
#[derive(Default)]
struct Socks {
    next: u64,
    list: Vec<(u64, TcpStream)>,
}

impl Breaker {
    /// A breaker for `s` (a clone of the socket, so the session keeps its own).
    pub fn new(s: &TcpStream) -> Breaker {
        let b = Breaker::default();
        b.add(s);
        b
    }

    /// Another socket of the same session (NFS's MOUNT, an FTP data connection); its key.
    pub fn add(&self, s: &TcpStream) -> u64 {
        let mut g = self.0.lock().unwrap_or_else(|p| p.into_inner());
        g.next += 1;
        let key = g.next;
        if let Ok(c) = s.try_clone() {
            g.list.push((key, c));
        }
        key
    }

    /// Stop holding a socket that closes before the session does (an FTP data connection).
    pub fn remove(&self, key: u64) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .list
            .retain(|(k, _)| *k != key);
    }

    /// Close the link both ways; the session's next read or write fails.
    pub fn trip(&self) {
        for (_, s) in self.0.lock().unwrap_or_else(|p| p.into_inner()).list.iter() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Let go of the socket clones, so the session's own close is the last one.
    pub fn release(&self) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .list
            .clear();
    }
}

impl std::fmt::Debug for Breaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Breaker")
    }
}

// Windows does not wake a read blocked on another handle of the socket; the session
// manager leaves such a call to end on its own (model::session::log_off).
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn a_breaker_ends_a_blocked_read() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let mut c = dial(&addr, Duration::from_secs(2), Duration::from_secs(30)).unwrap();
        let _server = l.accept().unwrap();
        let b = Breaker::new(&c);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            b.trip();
        });
        let start = std::time::Instant::now();
        let n = c.read(&mut [0u8; 8]).unwrap_or(0);
        assert_eq!(n, 0);
        assert!(start.elapsed() < Duration::from_secs(5));
        t.join().unwrap();
    }
}
