//! XDR (RFC 4506): big-endian 32-bit words, opaque data padded to a multiple of 4.

use lc_core::{Error, Result};

#[derive(Debug, Default, Clone)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn new() -> Self {
        Enc(Vec::new())
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(v as u32)
    }

    /// Fixed-length opaque data (no length word).
    pub fn fixed(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self.pad(b.len())
    }

    /// Variable-length opaque data or a string.
    pub fn opaque(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.fixed(b)
    }

    fn pad(&mut self, n: usize) -> &mut Self {
        self.0.resize(self.0.len() + (4 - n % 4) % 4, 0);
        self
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

pub struct Dec<'a> {
    b: &'a [u8],
}

fn short() -> Error {
    Error::Protocol("RPC reply ends early".into())
}

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec { b }
    }

    pub fn rest(&self) -> &'a [u8] {
        self.b
    }

    pub fn u32(&mut self) -> Result<u32> {
        let v = self.b.get(..4).ok_or_else(short)?;
        self.b = &self.b[4..];
        Ok(u32::from_be_bytes(v.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok((u64::from(self.u32()?) << 32) | u64::from(self.u32()?))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u32()? != 0)
    }

    pub fn fixed(&mut self, n: usize) -> Result<&'a [u8]> {
        let padded = n + (4 - n % 4) % 4;
        let v = self.b.get(..n).ok_or_else(short)?;
        self.b = self.b.get(padded..).ok_or_else(short)?;
        Ok(v)
    }

    pub fn opaque(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        if n > self.b.len() {
            return Err(short());
        }
        self.fixed(n)
    }

    pub fn skip(&mut self, words: usize) -> Result<()> {
        self.b = self.b.get(words * 4..).ok_or_else(short)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_is_padded_and_round_trips() {
        let mut e = Enc::new();
        e.opaque(b"abcde").u32(7).u64(1 << 40);
        assert_eq!(e.0.len(), 4 + 8 + 4 + 8);
        let mut d = Dec::new(&e.0);
        assert_eq!(d.opaque().unwrap(), b"abcde");
        assert_eq!(d.u32().unwrap(), 7);
        assert_eq!(d.u64().unwrap(), 1 << 40);
        assert!(d.u32().is_err());
    }
}
