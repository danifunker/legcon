//! The SMB1 message: a 32-byte header, parameter words and data bytes (MS-CIFS 2.2.3).
//!
//! legcon speaks it without Unicode (Flags2 bit 15 clear) so every name is in the machine's
//! OEM character set, as Windows 9x needs, and asks for DOS error codes, which every server
//! from LANMAN 1.0 on understands; NT status codes in replies are read too.

use lc_core::{Error, Result};

pub const NEGOTIATE: u8 = 0x72;
pub const SESSION_SETUP_ANDX: u8 = 0x73;
pub const LOGOFF_ANDX: u8 = 0x74;
pub const TREE_CONNECT_ANDX: u8 = 0x75;
pub const TREE_DISCONNECT: u8 = 0x71;
pub const TRANSACTION: u8 = 0x25;
pub const TRANSACTION2: u8 = 0x32;
pub const FIND_CLOSE2: u8 = 0x34;
pub const OPEN_ANDX: u8 = 0x2d;
pub const READ_ANDX: u8 = 0x2e;
pub const WRITE_ANDX: u8 = 0x2f;
pub const CLOSE: u8 = 0x04;
pub const DELETE: u8 = 0x06;
pub const RENAME: u8 = 0x07;
pub const CREATE_DIRECTORY: u8 = 0x00;
pub const DELETE_DIRECTORY: u8 = 0x01;
pub const ECHO: u8 = 0x2b;

/// No AndX command follows.
pub const NO_ANDX: u8 = 0xff;
pub const HEADER: usize = 32;

/// Flags: path names are caseless and canonical.
pub const FLAGS: u8 = 0x18;
/// Flags2: long names are understood. No Unicode, no NT status, no signing.
pub const FLAGS2: u16 = 0x0001;
const FLAGS2_NT_STATUS: u16 = 0x4000;
pub const FLAGS_REPLY: u8 = 0x80;

/// One SMB message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub command: u8,
    /// The status as the server sent it: an NT status, or DOS class in the low byte and code above.
    pub status: u32,
    pub flags: u8,
    pub flags2: u16,
    pub tid: u16,
    pub pid: u16,
    pub uid: u16,
    pub mid: u16,
    pub words: Vec<u8>,
    pub bytes: Vec<u8>,
}

impl Message {
    pub fn new(command: u8) -> Message {
        Message {
            command,
            flags: FLAGS,
            flags2: FLAGS2,
            pid: 0x4c43,
            ..Message::default()
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + 3 + self.words.len() + self.bytes.len());
        out.extend(b"\xffSMB");
        out.push(self.command);
        out.extend(self.status.to_le_bytes());
        out.push(self.flags);
        out.extend(self.flags2.to_le_bytes());
        out.extend([0u8; 12]);
        out.extend(self.tid.to_le_bytes());
        out.extend(self.pid.to_le_bytes());
        out.extend(self.uid.to_le_bytes());
        out.extend(self.mid.to_le_bytes());
        out.push((self.words.len() / 2) as u8);
        out.extend(&self.words);
        out.extend((self.bytes.len() as u16).to_le_bytes());
        out.extend(&self.bytes);
        out
    }

    pub fn decode(raw: &[u8]) -> Result<Message> {
        if raw.len() < HEADER + 3 || &raw[..4] != b"\xffSMB" {
            return Err(Error::Protocol("not an SMB message".into()));
        }
        let u16_at = |i: usize| u16::from_le_bytes([raw[i], raw[i + 1]]);
        let wc = usize::from(raw[HEADER]);
        let words_end = HEADER + 1 + wc * 2;
        let bc = usize::from(u16::from_le_bytes([
            *raw.get(words_end).ok_or_else(short)?,
            *raw.get(words_end + 1).ok_or_else(short)?,
        ]));
        let bytes = raw
            .get(words_end + 2..words_end + 2 + bc)
            .ok_or_else(short)?;
        Ok(Message {
            command: raw[4],
            status: u32::from_le_bytes([raw[5], raw[6], raw[7], raw[8]]),
            flags: raw[9],
            flags2: u16_at(10),
            tid: u16_at(24),
            pid: u16_at(26),
            uid: u16_at(28),
            mid: u16_at(30),
            words: raw[HEADER + 1..words_end].to_vec(),
            bytes: bytes.to_vec(),
        })
    }

    /// Where the bytes section starts in the encoded message, for offsets that point into it.
    pub fn bytes_offset(&self) -> usize {
        HEADER + 1 + self.words.len() + 2
    }

    pub fn word(&self, i: usize) -> u16 {
        self.words
            .get(i * 2..i * 2 + 2)
            .map_or(0, |w| u16::from_le_bytes([w[0], w[1]]))
    }

    pub fn dword(&self, i: usize) -> u32 {
        u32::from(self.word(i)) | (u32::from(self.word(i + 1)) << 16)
    }

    pub fn is_error(&self) -> bool {
        self.status != 0
    }

    /// The failure as a legcon error; `what` says what was being done.
    pub fn error(&self, what: &str) -> Error {
        let msg = format!("{what}: {}", self.status_text());
        if self.flags2 & FLAGS2_NT_STATUS != 0 {
            return match self.status {
                0xc000_006d | 0xc000_0072 | 0xc000_006e => Error::Auth(msg),
                0xc000_0034 | 0xc000_003a | 0xc000_00cc => Error::NotFound(msg),
                0xc000_0022 | 0xc000_0043 | 0xc000_0035 => Error::Denied(msg),
                0xc000_0002 | 0xc000_00bb => Error::Unsupported(msg),
                _ => Error::Protocol(msg),
            };
        }
        let (class, code) = (self.status & 0xff, self.status >> 16);
        match (class, code) {
            (1, 2) | (1, 3) | (1, 18) | (2, 6) => Error::NotFound(msg),
            (1, 5) | (1, 32) | (1, 80) | (2, 4) => Error::Denied(msg),
            (2, 2) | (2, 91) => Error::Auth(msg),
            (1, 1) | (2, 22) | (2, 0xffff) => Error::Unsupported(msg),
            _ => Error::Protocol(msg),
        }
    }

    fn status_text(&self) -> String {
        if self.flags2 & FLAGS2_NT_STATUS != 0 {
            let name = match self.status {
                0xc000_006d => "logon failure (wrong user name or password)",
                0xc000_0034 => "no such file",
                0xc000_003a => "no such folder",
                0xc000_00cc => "no such share",
                0xc000_0022 => "access denied",
                0xc000_0035 => "it already exists",
                0xc000_0043 => "it is in use (sharing violation)",
                0xc000_0101 => "the folder is not empty",
                _ => "",
            };
            return format!("NT status {:#010x} {name}", self.status)
                .trim_end()
                .to_string();
        }
        let (class, code) = (self.status & 0xff, self.status >> 16);
        let name = match (class, code) {
            (1, 2) => "no such file",
            (1, 3) => "no such folder",
            (1, 5) => "access denied",
            (1, 18) => "no more files",
            (1, 32) => "it is in use (sharing violation)",
            (1, 80) => "it already exists",
            (2, 2) => "bad password",
            (2, 4) => "access denied by the server",
            (2, 6) => "no such share",
            _ => "",
        };
        format!("DOS error class {class} code {code} {name}")
            .trim_end()
            .to_string()
    }
}

fn short() -> Error {
    Error::Protocol("truncated SMB message".into())
}

/// Little-endian builders for parameter words and data bytes.
#[derive(Debug, Default, Clone)]
pub struct Buf(pub Vec<u8>);

impl Buf {
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u16(mut self, v: u16) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    pub fn raw(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self
    }
    /// A NUL-terminated string.
    pub fn sz(mut self, v: &[u8]) -> Self {
        self.0.extend_from_slice(v);
        self.0.push(0);
        self
    }
    /// The AndX block: no further command.
    pub fn no_andx(self) -> Self {
        self.u8(NO_ANDX).u8(0).u16(0)
    }
}

/// A NUL-terminated string starting at `at` in `b`, and where the next field starts.
pub fn cstr(b: &[u8], at: usize) -> (&[u8], usize) {
    let rest = b.get(at..).unwrap_or(&[]);
    match rest.iter().position(|&c| c == 0) {
        Some(n) => (&rest[..n], at + n + 1),
        None => (rest, b.len()),
    }
}

/// Seconds since 1970 from a DOS date and time (local time; the server's zone is not known).
pub fn dos_time(date: u16, time: u16) -> Option<i64> {
    let (y, m, d) = (
        1980 + i64::from(date >> 9),
        u32::from((date >> 5) & 0x0f),
        u32::from(date & 0x1f),
    );
    if !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    let (h, mi, s) = (
        i64::from(time >> 11),
        i64::from((time >> 5) & 0x3f),
        i64::from(time & 0x1f) * 2,
    );
    let days = {
        let yy = if m <= 2 { y - 1 } else { y };
        let era = yy.div_euclid(400);
        let yoe = yy - era * 400;
        let mp = (i64::from(m) + 9) % 12;
        let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
        era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
    };
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_fields() {
        let mut m = Message::new(TREE_CONNECT_ANDX);
        m.tid = 7;
        m.uid = 100;
        m.words = Buf::default().no_andx().u16(0).u16(1).0;
        m.bytes = Buf::default().u8(0).sz(b"\\\\WIN95\\C").sz(b"?????").0;
        let back = Message::decode(&m.encode()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.word(3), 1);
        assert_eq!(back.bytes_offset(), 32 + 1 + 8 + 2);
    }

    #[test]
    fn dos_errors_and_times() {
        let m = Message {
            status: 1 | (2 << 16),
            ..Message::new(OPEN_ANDX)
        };
        assert!(matches!(m.error("open"), Error::NotFound(_)));
        let m = Message {
            status: 0xc000_006d,
            flags2: 0x4001,
            ..Message::new(SESSION_SETUP_ANDX)
        };
        assert!(matches!(m.error("login"), Error::Auth(_)));
        // 1997-11-13 10:04:00
        let date = ((1997 - 1980) << 9) | (11 << 5) | 13;
        let time = (10 << 11) | (4 << 5);
        assert_eq!(dos_time(date, time), Some(879_415_440));
    }
}
