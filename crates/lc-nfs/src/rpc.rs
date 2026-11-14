//! ONC RPC version 2 (RFC 1057) client over UDP or TCP, with AUTH_UNIX credentials.
//!
//! UDP is what most 1990s NFS servers speak (SunOS 4, IRIX, HP-UX, Ultrix are often UDP only):
//! each call is retransmitted with a growing wait until it is answered. TCP uses record marking.
//! Some servers only answer clients on a reserved source port (`nfs_portmon`, Linux `secure`), so
//! the socket can be bound to a port below 1024 first; Windows allows that for any user.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use lc_core::{Error, Result};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::xdr::{Dec, Enc};

pub const PMAP_PROG: u32 = 100000;
pub const PMAP_PORT: u16 = 111;
pub const MOUNT_PROG: u32 = 100005;
pub const NFS_PROG: u32 = 100003;
pub const IPPROTO_TCP: u32 = 6;
pub const IPPROTO_UDP: u32 = 17;

/// AUTH_UNIX: who the server should take us for. NFS has no passwords; the server trusts this.
#[derive(Debug, Clone)]
pub struct Cred {
    pub machine: String,
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

impl Cred {
    fn encode(&self, e: &mut Enc) {
        let mut body = Enc::new();
        body.u32(0)
            .opaque(self.machine.as_bytes())
            .u32(self.uid)
            .u32(self.gid);
        body.u32(self.gids.len() as u32);
        for g in &self.gids {
            body.u32(*g);
        }
        e.u32(1).opaque(body.bytes());
    }
}

enum Link {
    Udp(UdpSocket),
    Tcp(TcpStream),
}

/// A client for one program and version at one address.
pub struct Client {
    link: Link,
    pub prog: u32,
    pub vers: u32,
    cred: Cred,
    xid: u32,
    timeout: Duration,
    /// The source port is below 1024.
    pub reserved: bool,
}

/// Bind a socket to a reserved port (1023 downwards, as `bindresvport` does), or any port.
fn bind(sock: &Socket, addr: &SocketAddr, reserved: bool) -> bool {
    let any: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    if reserved {
        for port in (600..1024).rev() {
            let mut a = any;
            a.set_port(port);
            if sock.bind(&SockAddr::from(a)).is_ok() {
                return true;
            }
        }
    }
    let _ = sock.bind(&SockAddr::from(any));
    false
}

/// A TCP connection, from a reserved port when asked. A port whose last connection to this
/// server is still in TIME_WAIT binds but cannot connect, so the next port is tried.
fn connect_tcp(
    domain: Domain,
    addr: SocketAddr,
    reserved: bool,
    timeout: Duration,
) -> Result<(Socket, bool)> {
    if reserved {
        let mut any: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        for port in (600..1024).rev() {
            let s = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
            any.set_port(port);
            if s.bind(&SockAddr::from(any)).is_err() {
                continue;
            }
            match s.connect_timeout(&SockAddr::from(addr), timeout) {
                Ok(()) => return Ok((s, true)),
                Err(e)
                    if matches!(e.kind(), ErrorKind::AddrInUse | ErrorKind::AddrNotAvailable) =>
                {
                    continue
                }
                Err(e) => return Err(io_err(e, timeout)),
            }
        }
    }
    let s = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    s.connect_timeout(&SockAddr::from(addr), timeout)
        .map_err(|e| io_err(e, timeout))?;
    Ok((s, false))
}

fn io_err(e: std::io::Error, timeout: Duration) -> Error {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        Error::Timeout(timeout)
    } else {
        Error::Io(e)
    }
}

impl Client {
    pub fn connect(
        addr: SocketAddr,
        tcp: bool,
        reserved: bool,
        prog: u32,
        vers: u32,
        cred: Cred,
        timeout: Duration,
    ) -> Result<Client> {
        let domain = Domain::for_address(addr);
        let (link, got_reserved) = if tcp {
            let (s, r) = connect_tcp(domain, addr, reserved, timeout)?;
            let s: TcpStream = s.into();
            s.set_nodelay(true)?;
            s.set_read_timeout(Some(timeout))?;
            (Link::Tcp(s), r)
        } else {
            let s = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
            let r = bind(&s, &addr, reserved);
            let s: UdpSocket = s.into();
            s.connect(addr)?;
            (Link::Udp(s), r)
        };
        // Start somewhere unpredictable so a restarted client is not mistaken for an old one.
        let xid = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
            .unwrap_or(1);
        Ok(Client {
            link,
            prog,
            vers,
            cred,
            xid,
            timeout,
            reserved: got_reserved,
        })
    }

    /// The same server and credentials, another program or version on another port.
    pub fn sibling(&self, addr: SocketAddr, prog: u32, vers: u32) -> Result<Client> {
        Client::connect(
            addr,
            matches!(self.link, Link::Tcp(_)),
            self.reserved,
            prog,
            vers,
            self.cred.clone(),
            self.timeout,
        )
    }

    pub fn is_tcp(&self) -> bool {
        matches!(self.link, Link::Tcp(_))
    }

    /// Call `procedure` with XDR `args`; the reply's result body.
    pub fn call(&mut self, procedure: u32, args: &[u8]) -> Result<Vec<u8>> {
        self.xid = self.xid.wrapping_add(1);
        let xid = self.xid;
        let mut e = Enc::new();
        e.u32(xid)
            .u32(0)
            .u32(2)
            .u32(self.prog)
            .u32(self.vers)
            .u32(procedure);
        self.cred.encode(&mut e);
        e.u32(0).u32(0); // AUTH_NULL verifier
        e.0.extend_from_slice(args);
        let reply = match &mut self.link {
            Link::Udp(s) => udp_call(s, &e.0, xid, self.timeout)?,
            Link::Tcp(s) => tcp_call(s, &e.0, xid, self.timeout)?,
        };
        self.result(&reply)
    }

    fn result(&self, reply: &[u8]) -> Result<Vec<u8>> {
        let mut d = Dec::new(reply);
        d.u32()?; // xid, already matched
        if d.u32()? != 1 {
            return Err(Error::Protocol("RPC answer is not a reply".into()));
        }
        let what = format!("program {} version {}", self.prog, self.vers);
        match d.u32()? {
            0 => {
                d.u32()?;
                d.opaque()?; // verifier
                match d.u32()? {
                    0 => Ok(d.rest().to_vec()),
                    1 => Err(Error::Unsupported(format!(
                        "the server does not run {what}"
                    ))),
                    2 => {
                        let (lo, hi) = (d.u32()?, d.u32()?);
                        Err(Error::Unsupported(format!(
                            "{what} is not offered (versions {lo} to {hi} are)"
                        )))
                    }
                    3 => Err(Error::Unsupported(format!("{what} lacks that procedure"))),
                    n => Err(Error::Protocol(format!("RPC call refused ({n})"))),
                }
            }
            _ => match (d.u32()?, d.u32().unwrap_or(0)) {
                (0, _) => Err(Error::Unsupported(
                    "the server wants another RPC version".into(),
                )),
                (_, why) => Err(Error::Denied(format!(
                    "the server refused our credentials (auth error {why}){}",
                    if self.reserved {
                        ""
                    } else {
                        "; it may want a reserved source port"
                    }
                ))),
            },
        }
    }
}

fn udp_call(s: &UdpSocket, msg: &[u8], xid: u32, timeout: Duration) -> Result<Vec<u8>> {
    let end = Instant::now() + timeout;
    let mut wait = Duration::from_millis(700);
    let mut buf = vec![0u8; 65536];
    loop {
        s.send(msg)?;
        let resend = Instant::now() + wait;
        loop {
            let now = Instant::now();
            if now >= end {
                return Err(Error::Timeout(timeout));
            }
            s.set_read_timeout(Some(
                resend
                    .min(end)
                    .saturating_duration_since(now)
                    .max(Duration::from_millis(1)),
            ))?;
            match s.recv(&mut buf) {
                Ok(n) if n >= 4 && buf[..4] == xid.to_be_bytes() => return Ok(buf[..n].to_vec()),
                // A late answer to an earlier retransmit, or something else: keep waiting.
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                // Windows reports an ICMP port-unreachable as a reset on the next receive.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                    return Err(Error::Unsupported(
                        "nothing listens on that UDP port".into(),
                    ))
                }
                Err(e) => return Err(Error::Io(e)),
            }
            if Instant::now() >= resend {
                break;
            }
        }
        wait = (wait * 2).min(Duration::from_secs(4));
    }
}

fn tcp_call(s: &mut TcpStream, msg: &[u8], xid: u32, timeout: Duration) -> Result<Vec<u8>> {
    let mut rec = Vec::with_capacity(msg.len() + 4);
    rec.extend_from_slice(&(0x8000_0000 | msg.len() as u32).to_be_bytes());
    rec.extend_from_slice(msg);
    s.write_all(&rec)?;
    loop {
        let mut reply = Vec::new();
        loop {
            let mut h = [0u8; 4];
            s.read_exact(&mut h).map_err(|e| io_err(e, timeout))?;
            let h = u32::from_be_bytes(h);
            let start = reply.len();
            reply.resize(start + (h & 0x7fff_ffff) as usize, 0);
            s.read_exact(&mut reply[start..])
                .map_err(|e| io_err(e, timeout))?;
            if h & 0x8000_0000 != 0 {
                break;
            }
        }
        if reply.len() >= 4 && reply[..4] == xid.to_be_bytes() {
            return Ok(reply);
        }
    }
}

/// Ask the portmapper where `prog` / `vers` listens; 0 means it is not registered.
pub fn getport(pmap: &mut Client, prog: u32, vers: u32, tcp: bool) -> Result<u16> {
    let mut e = Enc::new();
    e.u32(prog)
        .u32(vers)
        .u32(if tcp { IPPROTO_TCP } else { IPPROTO_UDP })
        .u32(0);
    let r = pmap.call(3, &e.0)?;
    Ok(Dec::new(&r).u32()? as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The reply comes back in two fragments; the client must join them.
    #[test]
    fn tcp_records_in_fragments() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut h = [0u8; 4];
            s.read_exact(&mut h).unwrap();
            let mut call = vec![0u8; (u32::from_be_bytes(h) & 0x7fff_ffff) as usize];
            s.read_exact(&mut call).unwrap();
            let mut r = Enc::new();
            r.fixed(&call[..4])
                .u32(1)
                .u32(0)
                .u32(0)
                .u32(0)
                .u32(0)
                .u32(42);
            let (a, b) = r.0.split_at(8);
            s.write_all(&(a.len() as u32).to_be_bytes()).unwrap();
            s.write_all(a).unwrap();
            s.write_all(&(0x8000_0000 | b.len() as u32).to_be_bytes())
                .unwrap();
            s.write_all(b).unwrap();
        });
        let cred = Cred {
            machine: "t".into(),
            uid: 1,
            gid: 1,
            gids: vec![],
        };
        let mut c = Client::connect(addr, true, false, 1, 1, cred, Duration::from_secs(2)).unwrap();
        assert_eq!(c.call(0, &[]).unwrap(), 42u32.to_be_bytes());
    }
}
