//! DSI, the Data Stream Interface: how AFP rides on TCP (port 548).
//!
//! Every message is a 16-byte header (flags, command, request id, error code or write offset,
//! data length, reserved) and its data. The server may send tickles (keep-alives) and attention
//! messages between replies; tickles are answered in kind, attention is acknowledged.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use lc_core::{Error, Result};

pub const CLOSE_SESSION: u8 = 1;
pub const COMMAND: u8 = 2;
pub const GET_STATUS: u8 = 3;
pub const OPEN_SESSION: u8 = 4;
pub const TICKLE: u8 = 5;
pub const WRITE: u8 = 6;
pub const ATTENTION: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// 0 for a request, 1 for a reply.
    pub flags: u8,
    pub command: u8,
    pub id: u16,
    /// The AFP result in a reply; the write offset in a DSIWrite request.
    pub code: i32,
    pub len: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; 16] {
        let mut h = [0u8; 16];
        h[0] = self.flags;
        h[1] = self.command;
        h[2..4].copy_from_slice(&self.id.to_be_bytes());
        h[4..8].copy_from_slice(&self.code.to_be_bytes());
        h[8..12].copy_from_slice(&self.len.to_be_bytes());
        h
    }

    pub fn decode(h: &[u8; 16]) -> Header {
        Header {
            flags: h[0],
            command: h[1],
            id: u16::from_be_bytes([h[2], h[3]]),
            code: i32::from_be_bytes([h[4], h[5], h[6], h[7]]),
            len: u32::from_be_bytes([h[8], h[9], h[10], h[11]]),
        }
    }
}

/// Read one message.
pub fn read_message(s: &mut impl Read) -> Result<(Header, Vec<u8>)> {
    let mut h = [0u8; 16];
    s.read_exact(&mut h)?;
    let h = Header::decode(&h);
    if h.len > 64 << 20 {
        return Err(Error::Protocol(format!("DSI message of {} bytes", h.len)));
    }
    let mut data = vec![0u8; h.len as usize];
    s.read_exact(&mut data)?;
    Ok((h, data))
}

pub fn write_message(s: &mut impl Write, h: Header, data: &[u8]) -> Result<()> {
    let mut m = Vec::with_capacity(16 + data.len());
    m.extend_from_slice(&h.encode());
    m.extend_from_slice(data);
    s.write_all(&m)?;
    Ok(())
}

/// A DSI session: requests out, replies matched by id.
pub struct Dsi {
    pub stream: TcpStream,
    next: u16,
    timeout: Duration,
}

impl Dsi {
    pub fn new(stream: TcpStream, timeout: Duration) -> Result<Dsi> {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        lc_core::net::tune(&stream, timeout)?;
        Ok(Dsi {
            stream,
            next: 0,
            timeout,
        })
    }

    /// Wait at most `t` for each reply from now on.
    pub fn set_timeout(&mut self, t: Duration) -> Result<()> {
        self.timeout = t;
        self.stream.set_read_timeout(Some(t))?;
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Send a request and wait for its reply: the AFP result code and the reply data.
    pub fn request(
        &mut self,
        command: u8,
        data: &[u8],
        write_offset: u32,
    ) -> Result<(i32, Vec<u8>)> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let h = Header {
            flags: 0,
            command,
            id,
            code: write_offset as i32,
            len: data.len() as u32,
        };
        write_message(&mut self.stream, h, data)?;
        loop {
            let (r, body) = read_message(&mut self.stream).map_err(|e| match e {
                Error::Io(io)
                    if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    Error::Timeout(self.timeout)
                }
                other => other,
            })?;
            if r.flags == 0 {
                match r.command {
                    TICKLE => {
                        let t = Header {
                            flags: 0,
                            command: TICKLE,
                            id: self.next,
                            code: 0,
                            len: 0,
                        };
                        self.next = self.next.wrapping_add(1);
                        write_message(&mut self.stream, t, &[])?;
                    }
                    ATTENTION => {
                        let ack = Header {
                            flags: 1,
                            len: 0,
                            code: 0,
                            ..r
                        };
                        write_message(&mut self.stream, ack, &[])?;
                    }
                    CLOSE_SESSION => {
                        return Err(Error::Lost("the server closed the session".into()))
                    }
                    _ => {}
                }
                continue;
            }
            if r.id == id {
                return Ok((r.code, body));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = Header {
            flags: 1,
            command: COMMAND,
            id: 7,
            code: -5018,
            len: 3,
        };
        assert_eq!(Header::decode(&h.encode()), h);
        assert_eq!(&h.encode()[4..8], &(-5018i32).to_be_bytes());
    }
}
