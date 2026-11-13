//! NetBIOS over TCP (RFC 1001 / 1002): the session service SMB rides on, and a name lookup.
//!
//! Port 139 wants a session request naming the server. `*SMBSERVER` works for NT and Samba;
//! Windows 9x insists on its real name, which a node-status query (UDP 137) finds. The query
//! is a request and its reply, so nothing listens on this computer.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use lc_core::{Error, Result};

pub const SESSION_MESSAGE: u8 = 0x00;
pub const SESSION_REQUEST: u8 = 0x81;
pub const POSITIVE_RESPONSE: u8 = 0x82;
pub const NEGATIVE_RESPONSE: u8 = 0x83;
pub const KEEPALIVE: u8 = 0x85;

/// The name any NT or Samba server answers to on port 139.
pub const ANY_SERVER: &str = "*SMBSERVER";

/// A NetBIOS name in "first level" encoding: padded to 15 with spaces, a suffix byte, then each
/// half-byte as a letter from 'A'.
pub fn encode_name(name: &str, suffix: u8) -> Vec<u8> {
    let mut raw = [b' '; 16];
    for (i, b) in name.bytes().take(15).enumerate() {
        raw[i] = b.to_ascii_uppercase();
    }
    // The wildcard name pads with NULs, not spaces.
    if name == "*" {
        raw = [0; 16];
        raw[0] = b'*';
    }
    raw[15] = suffix;
    let mut out = Vec::with_capacity(34);
    out.push(32);
    for b in raw {
        out.push(b'A' + (b >> 4));
        out.push(b'A' + (b & 0x0f));
    }
    out.push(0);
    out
}

/// Write one session-service frame.
pub fn send(stream: &mut TcpStream, kind: u8, payload: &[u8]) -> Result<()> {
    let len = payload.len();
    if len > 0x1_ffff {
        return Err(Error::Protocol(format!("NetBIOS frame too long: {len}")));
    }
    let mut frame = vec![kind, ((len >> 16) & 1) as u8, (len >> 8) as u8, len as u8];
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    Ok(())
}

/// Read one frame; keepalives are skipped.
pub fn recv(stream: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    loop {
        let mut head = [0u8; 4];
        stream.read_exact(&mut head)?;
        let len =
            (usize::from(head[1] & 1) << 16) | (usize::from(head[2]) << 8) | usize::from(head[3]);
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload)?;
        if head[0] != KEEPALIVE {
            return Ok((head[0], payload));
        }
    }
}

/// Open the session on an already connected stream; `Ok(false)` when the server refused the name.
pub fn session_request(stream: &mut TcpStream, called: &str, calling: &str) -> Result<bool> {
    let mut payload = encode_name(called, 0x20);
    payload.extend(encode_name(calling, 0x00));
    send(stream, SESSION_REQUEST, &payload)?;
    match recv(stream)? {
        (POSITIVE_RESPONSE, _) => Ok(true),
        (NEGATIVE_RESPONSE, _) => Ok(false),
        (k, _) => Err(Error::Protocol(format!(
            "NetBIOS session: unexpected frame type {k:#x}"
        ))),
    }
}

/// Ask a machine for its NetBIOS names (node status, UDP 137); the file server name, if it has one.
pub fn server_name(ip: std::net::IpAddr, timeout: Duration) -> Option<String> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    let mut q = vec![0x4c, 0x45, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    q.extend(encode_name("*", 0x00));
    q.extend([0x00, 0x21, 0x00, 0x01]);
    sock.send_to(&q, SocketAddr::new(ip, 137)).ok()?;
    let mut buf = [0u8; 1024];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    parse_node_status(&buf[..n])
}

/// The first unique name with the server suffix (0x20) in a node-status reply.
pub fn parse_node_status(reply: &[u8]) -> Option<String> {
    // Header (12) + the echoed name (34) + type, class, TTL, length (10), then the name count.
    let at = 12 + 34 + 10;
    let count = usize::from(*reply.get(at)?);
    for i in 0..count {
        let e = reply.get(at + 1 + i * 18..at + 1 + (i + 1) * 18)?;
        let (name, suffix, flags) = (&e[..15], e[15], e[16]);
        let group = flags & 0x80 != 0;
        if suffix == 0x20 && !group {
            return Some(String::from_utf8_lossy(name).trim_end().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_encode_as_rfc1001() {
        let e = encode_name("WIN95", 0x20);
        assert_eq!(e.len(), 34);
        assert_eq!(&e[1..5], b"FHEJ");
        assert_eq!(&e[31..33], b"CA");
        assert_eq!(&encode_name("*", 0)[1..3], b"CK");
    }

    #[test]
    fn node_status_picks_the_server_name() {
        let mut r = vec![0u8; 12];
        r.extend(encode_name("*", 0));
        r.extend([0, 0x21, 0, 1, 0, 0, 0, 0, 0, 0]);
        r.push(2);
        let mut group = b"WORKGROUP      ".to_vec();
        group.extend([0x00, 0x84, 0x00]);
        let mut server = b"WIN95          ".to_vec();
        server.extend([0x20, 0x04, 0x00]);
        r.extend(group);
        r.extend(server);
        assert_eq!(parse_node_status(&r).as_deref(), Some("WIN95"));
    }
}
