//! AppleDouble sidecars (`._name`): a classic Mac file's Finder info and resource fork, kept next
//! to its data fork on a computer whose files have no forks (D31).
//!
//! Version 2 layout: magic, version, 16 bytes of filler, the entry count, then (id, offset,
//! length) descriptors. Written with Finder Info (id 9), File Dates (id 8) when the date is
//! known, and Resource Fork (id 2), as macOS does.
//! Our own small code: rusty-backup's `fs-mac` has the same, but brings a GPL-2.0-only crate.

use lc_core::MacExtras;

const MAGIC: u32 = 0x0005_1607;
const VERSION: u32 = 0x0002_0000;
const RESOURCE_FORK: u32 = 2;
const FILE_DATES: u32 = 8;
const FINDER_INFO: u32 = 9;
/// AppleDouble dates count seconds from 2000-01-01 GMT; this one means "unknown".
const NO_DATE: i32 = i32::MIN;
const EPOCH_2000: i64 = 946_684_800;

/// The sidecar's name for a file: `._Read Me` for `Read Me`.
pub fn sidecar_name(name: &str) -> String {
    format!("._{name}")
}

/// Whether a name is itself a sidecar.
pub fn is_sidecar(name: &str) -> bool {
    name.starts_with("._")
}

/// The sidecar for `x`; `modified` (seconds since 1970) goes into a File Dates entry.
pub fn build(x: &MacExtras, modified: Option<i64>) -> Vec<u8> {
    let count = if modified.is_some() { 3 } else { 2 };
    let header = 26 + count * 12;
    let finder_at = header as u32;
    let dates_at = finder_at + 32;
    let rsrc_at = dates_at + if modified.is_some() { 16 } else { 0 };
    let mut v = Vec::with_capacity(rsrc_at as usize + x.resource_fork.len());
    v.extend_from_slice(&MAGIC.to_be_bytes());
    v.extend_from_slice(&VERSION.to_be_bytes());
    v.extend_from_slice(&[0; 16]);
    v.extend_from_slice(&(count as u16).to_be_bytes());
    let mut entries = vec![(FINDER_INFO, finder_at, 32u32)];
    if modified.is_some() {
        entries.push((FILE_DATES, dates_at, 16));
    }
    entries.push((RESOURCE_FORK, rsrc_at, x.resource_fork.len() as u32));
    for (id, at, len) in entries {
        v.extend_from_slice(&id.to_be_bytes());
        v.extend_from_slice(&at.to_be_bytes());
        v.extend_from_slice(&len.to_be_bytes());
    }
    v.extend_from_slice(&x.finder_info);
    if let Some(m) = modified {
        let m = (m - EPOCH_2000).clamp(i64::from(i32::MIN) + 1, i64::from(i32::MAX)) as i32;
        // Created, modified, backed up, accessed: only the modification time is known.
        for d in [NO_DATE, m, NO_DATE, NO_DATE] {
            v.extend_from_slice(&d.to_be_bytes());
        }
    }
    v.extend_from_slice(&x.resource_fork);
    v
}

/// The modification time (seconds since 1970) a sidecar records, if any.
pub fn modified(data: &[u8]) -> Option<i64> {
    let u32_at = |at: usize| {
        data.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    let count = u16::from_be_bytes(data.get(24..26)?.try_into().ok()?) as usize;
    for i in 0..count {
        let d = 26 + i * 12;
        if u32_at(d)? == FILE_DATES {
            let m = u32_at(u32_at(d + 4)? as usize + 4)? as i32;
            return (m != NO_DATE).then_some(EPOCH_2000 + i64::from(m));
        }
    }
    None
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
        let b = build(&x, None);
        assert_eq!(&b[..4], &[0, 5, 0x16, 7]);
        assert_eq!(b.len(), 50 + 32 + 4);
        assert_eq!(parse(&b), Some(x.clone()));
        assert_eq!(modified(&b), None);
        let dated = build(&x, Some(879_415_440));
        assert_eq!(parse(&dated), Some(x));
        assert_eq!(modified(&dated), Some(879_415_440));
        assert_eq!(parse(b"not appledouble"), None);
        assert!(is_sidecar(&sidecar_name("Read Me")));
    }
}
