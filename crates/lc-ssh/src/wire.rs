//! SSH-2's data types (RFC 4251 §5): reading a packet's fields and writing them.

use lc_core::{Error, Result};

pub struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(Error::Protocol(
                "a message from the server is cut short".into(),
            ));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub fn name_list(&mut self) -> Result<Vec<String>> {
        let s = self.string()?;
        Ok(String::from_utf8_lossy(s)
            .split(',')
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect())
    }

    pub fn rest(&self) -> &'a [u8] {
        self.b
    }
}

pub fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

pub fn put_name_list(out: &mut Vec<u8>, names: &[&str]) {
    put_string(out, names.join(",").as_bytes());
}

/// An unsigned big-endian number as an mpint: no leading zeros, a zero byte before a high bit.
pub fn put_mpint(out: &mut Vec<u8>, n: &[u8]) {
    let n = &n[n.iter().position(|b| *b != 0).unwrap_or(n.len())..];
    if n.first().is_some_and(|b| b & 0x80 != 0) {
        put_u32(out, n.len() as u32 + 1);
        out.push(0);
        out.extend_from_slice(n);
    } else {
        put_string(out, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpints_as_rfc_4251_shows_them() {
        let enc = |n: &[u8]| {
            let mut v = Vec::new();
            put_mpint(&mut v, n);
            v
        };
        assert_eq!(enc(&[0, 0]), [0, 0, 0, 0]);
        assert_eq!(enc(&[0x80]), [0, 0, 0, 2, 0, 0x80]);
        assert_eq!(enc(&[0, 0x12, 0x34]), [0, 0, 0, 2, 0x12, 0x34]);
    }
}
