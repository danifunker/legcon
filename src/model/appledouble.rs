//! AppleDouble sidecars (`._name`): a classic Mac file's Finder info and resource fork, kept next
//! to its data fork on a computer whose files have no forks (D31).
//!
//! Version 2 layout: magic, version, 16 bytes of filler, the entry count, then (id, offset,
//! length) descriptors. Written with Finder Info (id 9) and Resource Fork (id 2), as macOS does.
//! Our own small code: rusty-backup's `fs-mac` has the same, but brings a GPL-2.0-only crate.

use lc_core::MacExtras;

const MAGIC: u32 = 0x0005_1607;
const VERSION: u32 = 0x0002_0000;
const RESOURCE_FORK: u32 = 2;
const FINDER_INFO: u32 = 9;

/// The sidecar's name for a file: `._Read Me` for `Read Me`.
pub fn sidecar_name(name: &str) -> String {
    format!("._{name}")
}

/// Whether a name is itself a sidecar.
pub fn is_sidecar(name: &str) -> bool {
    name.starts_with("._")
}

pub fn build(x: &MacExtras) -> Vec<u8> {
    let header = 26 + 2 * 12;
    let finder_at = header as u32;
    let rsrc_at = finder_at + 32;
    let mut v = Vec::with_capacity(header + 32 + x.resource_fork.len());
    v.extend_from_slice(&MAGIC.to_be_bytes());
    v.extend_from_slice(&VERSION.to_be_bytes());
    v.extend_from_slice(&[0; 16]);
    v.extend_from_slice(&2u16.to_be_bytes());
    for (id, at, len) in [
        (FINDER_INFO, finder_at, 32u32),
        (RESOURCE_FORK, rsrc_at, x.resource_fork.len() as u32),
    ] {
        v.extend_from_slice(&id.to_be_bytes());
        v.extend_from_slice(&at.to_be_bytes());
        v.extend_from_slice(&len.to_be_bytes());
    }
    v.extend_from_slice(&x.finder_info);
    v.extend_from_slice(&x.resource_fork);
    v
}

/// Read a sidecar back; `None` if it is not AppleDouble.
pub fn parse(data: &[u8]) -> Option<MacExtras> {
    let u32_at = |at: usize| {
        data.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    if u32_at(0)? != MAGIC {
        return None;
    }
    let count = u16::from_be_bytes(data.get(24..26)?.try_into().ok()?) as usize;
    let mut x = MacExtras {
        finder_info: [0; 32],
        resource_fork: Vec::new(),
    };
    for i in 0..count {
        let d = 26 + i * 12;
        let (id, at, len) = (u32_at(d)?, u32_at(d + 4)? as usize, u32_at(d + 8)? as usize);
        let body = data.get(at..at + len)?;
        match id {
            FINDER_INFO => {
                let n = len.min(32);
                x.finder_info[..n].copy_from_slice(&body[..n]);
            }
            RESOURCE_FORK => x.resource_fork = body.to_vec(),
            _ => {}
        }
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut fi = [0u8; 32];
        fi[..8].copy_from_slice(b"APPLttxt");
        let x = MacExtras {
            finder_info: fi,
            resource_fork: b"rsrc".to_vec(),
        };
        let b = build(&x);
        assert_eq!(&b[..4], &[0, 5, 0x16, 7]);
        assert_eq!(b.len(), 50 + 32 + 4);
        assert_eq!(parse(&b), Some(x));
        assert_eq!(parse(b"not appledouble"), None);
        assert!(is_sidecar(&sidecar_name("Read Me")));
    }
}
