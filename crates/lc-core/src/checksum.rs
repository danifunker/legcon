//! Checksums a machine can compute on its own side, and the same sums over local bytes, so a copy
//! can be verified without reading it back.
//!
//! Two kinds are common on old machines: POSIX `cksum` (POSIX.2, 1992: IRIX, Solaris, HP-UX, AIX,
//! Linux, the BSDs), which any shell protocol can run, and the zlib CRC-32 some FTP servers give
//! for `XCRC`.

use std::io::Read;

/// A sum the machine reported for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checksum {
    /// POSIX `cksum`: its CRC and the length it covered.
    Cksum { crc: u32, len: u64 },
    /// The zlib / PKZIP CRC-32 (FTP `XCRC`).
    Crc32(u32),
}

impl Checksum {
    /// The same kind of sum over local bytes.
    pub fn of(&self, r: &mut dyn Read) -> std::io::Result<Checksum> {
        match self {
            Checksum::Cksum { .. } => {
                let (crc, len) = cksum(r)?;
                Ok(Checksum::Cksum { crc, len })
            }
            Checksum::Crc32(_) => Ok(Checksum::Crc32(crc32(r)?)),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Checksum::Cksum { .. } => "cksum",
            Checksum::Crc32(_) => "CRC-32",
        }
    }
}

impl std::fmt::Display for Checksum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Checksum::Cksum { crc, len } => write!(f, "cksum {crc} {len}"),
            Checksum::Crc32(c) => write!(f, "CRC-32 {c:08X}"),
        }
    }
}

fn table(poly: u32, reflected: bool) -> [u32; 256] {
    let mut t = [0u32; 256];
    for (i, slot) in t.iter_mut().enumerate() {
        let mut c = if reflected {
            i as u32
        } else {
            (i as u32) << 24
        };
        for _ in 0..8 {
            c = if reflected {
                if c & 1 != 0 {
                    (c >> 1) ^ poly
                } else {
                    c >> 1
                }
            } else if c & 0x8000_0000 != 0 {
                (c << 1) ^ poly
            } else {
                c << 1
            };
        }
        *slot = c;
    }
    t
}

fn each_block(r: &mut dyn Read, mut f: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => f(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// POSIX `cksum`: CRC over the bytes, then over the length's octets, inverted; and the length.
pub fn cksum(r: &mut dyn Read) -> std::io::Result<(u32, u64)> {
    let t = table(0x04C1_1DB7, false);
    let step = |crc: u32, b: u8| (crc << 8) ^ t[((crc >> 24) as u8 ^ b) as usize];
    let mut crc = 0u32;
    let mut len = 0u64;
    each_block(r, |b| {
        len += b.len() as u64;
        for &x in b {
            crc = step(crc, x);
        }
    })?;
    let mut n = len;
    while n != 0 {
        crc = step(crc, n as u8);
        n >>= 8;
    }
    Ok((!crc, len))
}

/// The zlib CRC-32.
pub fn crc32(r: &mut dyn Read) -> std::io::Result<u32> {
    let t = table(0xEDB8_8320, true);
    let mut crc = !0u32;
    each_block(r, |b| {
        for &x in b {
            crc = (crc >> 8) ^ t[((crc as u8) ^ x) as usize];
        }
    })?;
    Ok(!crc)
}

/// The shell command that prints a file's `cksum` (read from stdin, so no name is echoed).
pub fn cksum_command(path: &[u8]) -> Vec<u8> {
    let mut c = b"cksum < ".to_vec();
    c.extend_from_slice(&crate::shellfile::sq(path));
    c
}

/// `cksum` output: "CRC LENGTH" (a name may follow).
pub fn parse_cksum(out: &str) -> Option<Checksum> {
    out.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        let crc = w.next()?.parse().ok()?;
        let len = w.next()?.parse().ok()?;
        Some(Checksum::Cksum { crc, len })
    })
}

/// An `XCRC` reply's text: the CRC is its first word of eight hex digits.
pub fn parse_xcrc(text: &str) -> Option<Checksum> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .find(|w| w.len() == 8 && w.chars().all(|c| c.is_ascii_hexdigit()))
        .and_then(|w| u32::from_str_radix(w, 16).ok())
        .map(Checksum::Crc32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // `printf 'hello\n' | cksum` prints "3015617425 6".
        assert_eq!(cksum(&mut &b"hello\n"[..]).unwrap(), (3015617425, 6));
        assert_eq!(cksum(&mut &b""[..]).unwrap(), (4294967295, 0));
        assert_eq!(crc32(&mut &b"123456789"[..]).unwrap(), 0xCBF4_3926);
    }

    #[test]
    fn replies() {
        assert_eq!(
            parse_cksum("3015617425 6\n"),
            Some(Checksum::Cksum {
                crc: 3015617425,
                len: 6
            })
        );
        assert_eq!(parse_cksum("cksum: not found"), None);
        assert_eq!(
            parse_xcrc("250 CBF43926"),
            Some(Checksum::Crc32(0xCBF43926))
        );
        assert_eq!(
            parse_xcrc("250 CRC32 cbf43926 for file.txt"),
            Some(Checksum::Crc32(0xCBF43926))
        );
        assert_eq!(parse_xcrc("500 Unknown command"), None);
    }
}
