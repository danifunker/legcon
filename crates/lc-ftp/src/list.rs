//! Parsers for `LIST` output, which RFC 959 never specified: each server family has its own.
//!
//! Lines arrive as bytes; the fixed fields are ASCII and the name is decoded with the machine's
//! character set, keeping the raw bytes for addressing the file again. Recognised so far: Unix
//! `ls -l` (with or without a group column, device files, symlinks) and DOS / IIS. A line no
//! parser takes is returned as a name-only entry rather than dropped.

use lc_core::{Charset, Entry, EntryKind, TimePrecision, Timestamp};

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Days since 1970-01-01 for a civil date (Howard Hinnant's days-from-civil).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil year of a time in seconds since 1970.
pub fn year_of(secs: i64) -> i64 {
    let mut y = 1970 + secs.div_euclid(31_556_952);
    while days_from_civil(y, 1, 1) * 86_400 > secs {
        y -= 1;
    }
    while days_from_civil(y + 1, 1, 1) * 86_400 <= secs {
        y += 1;
    }
    y
}

fn month(tok: &[u8]) -> Option<u32> {
    let t = std::str::from_utf8(tok).ok()?.to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == t).map(|i| i as u32 + 1)
}

fn num(tok: &[u8]) -> Option<u64> {
    std::str::from_utf8(tok).ok()?.parse().ok()
}

/// The whitespace-separated fields of a line with where each starts.
fn fields(line: &[u8]) -> Vec<(usize, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        while i < line.len() && line[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < line.len() && !line[i].is_ascii_whitespace() {
            i += 1;
        }
        if start < i {
            out.push((start, &line[start..i]));
        }
    }
    out
}

fn named(raw: &[u8], cs: Charset, kind: EntryKind) -> Entry {
    let mut e = Entry::new(cs.decode(raw), kind);
    e.raw_name = raw.to_vec();
    e
}

fn unix_mode(perm: &[u8]) -> Option<(EntryKind, u32)> {
    if perm.len() < 10 {
        return None;
    }
    let kind = match perm[0] {
        b'd' => EntryKind::Folder,
        b'-' => EntryKind::File,
        b'l' => EntryKind::Link,
        b'c' | b'b' | b'p' | b's' => EntryKind::Other,
        _ => return None,
    };
    let mut mode = 0u32;
    for (i, &c) in perm[1..10].iter().enumerate() {
        let on = !matches!(c, b'-');
        if !b"rwxrwxrwx".contains(&c) && !b"sStTl-".contains(&c) {
            return None;
        }
        if on && !matches!(c, b'S' | b'T' | b'l') {
            mode |= 1 << (8 - i);
        }
    }
    Some((kind, mode))
}

/// `drwxr-xr-x   2 dani  staff   512 Mar 16  1999 gcc-2.95.3`, with the variants old Unixes print.
pub fn parse_unix(line: &[u8], cs: Charset, now: i64) -> Option<Entry> {
    let f = fields(line);
    let (kind, mode) = unix_mode(f.first()?.1)?;
    // The date is month, day, then a year or a time; everything after it is the name.
    let at = (1..f.len().saturating_sub(3)).find(|&i| {
        month(f[i].1).is_some() && num(f[i + 1].1).is_some_and(|d| (1..=31).contains(&d))
    })?;
    let (mon, day, yt) = (month(f[at].1)?, num(f[at + 1].1)? as u32, f[at + 2].1);
    let size = num(f[at - 1].1);
    let yt_s = std::str::from_utf8(yt).ok()?;
    let (unix, precision) = if let Some((h, m)) = yt_s.split_once(':') {
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        let mut y = year_of(now);
        // A date with a time but no year is within the last six months; a future one is last year's.
        if days_from_civil(y, mon, day) * 86_400 > now + 86_400 {
            y -= 1;
        }
        (
            days_from_civil(y, mon, day) * 86_400 + h * 3600 + m * 60,
            TimePrecision::YearGuessed,
        )
    } else {
        let y: i64 = yt_s.parse().ok()?;
        (
            days_from_civil(y, mon, day) * 86_400,
            TimePrecision::DateOnly,
        )
    };
    let name_at = f.get(at + 3)?.0;
    let mut raw = &line[name_at..];
    let mut target = None;
    if kind == EntryKind::Link {
        if let Some(p) = raw.windows(4).position(|w| w == b" -> ") {
            target = Some(cs.decode(&raw[p + 4..]));
            raw = &raw[..p];
        }
    }
    let mut e = named(raw, cs, kind);
    e.size = if kind == EntryKind::Folder {
        None
    } else {
        size
    };
    e.mode = Some(mode);
    e.modified = Some(Timestamp { unix, precision });
    e.link_target = target;
    if at >= 4 {
        e.owner = Some(cs.decode(f[2].1));
        if at >= 5 {
            e.group = Some(cs.decode(f[3].1));
        }
    }
    Some(e)
}

/// `11-13-97  10:04AM       <DIR>          src` (DOS, Windows NT / IIS).
pub fn parse_dos(line: &[u8], cs: Charset) -> Option<Entry> {
    let f = fields(line);
    if f.len() < 4 {
        return None;
    }
    let date = std::str::from_utf8(f[0].1).ok()?;
    let mut d = date.split(['-', '/']);
    let (m, day, y): (u32, u32, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Two-digit years from the 1990s DOS / NT servers: 70-99 are 19xx, the rest 20xx.
    let y = if y < 100 {
        if y >= 70 {
            1900 + y
        } else {
            2000 + y
        }
    } else {
        y
    };
    let t = std::str::from_utf8(f[1].1).ok()?.to_ascii_uppercase();
    let (hm, pm) = match t.strip_suffix("PM") {
        Some(x) => (x, true),
        None => (t.strip_suffix("AM").unwrap_or(&t), false),
    };
    let (h, mi) = hm.split_once(':')?;
    let (mut h, mi): (i64, i64) = (h.parse().ok()?, mi.parse().ok()?);
    if t.ends_with('M') {
        h %= 12;
        if pm {
            h += 12;
        }
    }
    let dir = f[2].1.eq_ignore_ascii_case(b"<DIR>");
    let size = if dir { None } else { Some(num(f[2].1)?) };
    let mut e = named(
        &line[f[3].0..],
        cs,
        if dir {
            EntryKind::Folder
        } else {
            EntryKind::File
        },
    );
    e.size = size;
    e.modified = Some(Timestamp {
        unix: days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60,
        precision: TimePrecision::LocalUnknownZone,
    });
    Some(e)
}

/// Parse a whole listing; `now` (seconds since 1970) places dates that carry no year.
pub fn parse(listing: &[u8], cs: Charset, now: i64) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in listing.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() || trimmed.starts_with(b"total ") {
            continue;
        }
        let e = parse_unix(line, cs, now)
            .or_else(|| parse_dos(line, cs))
            .unwrap_or_else(|| named(trimmed, cs, EntryKind::Other));
        if e.name != "." && e.name != ".." {
            out.push(e);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-13 12:00 UTC.
    const NOW: i64 = 1_794_571_200;

    #[test]
    fn calendar_helpers() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(year_of(NOW), 2026);
        assert_eq!(
            year_of(days_from_civil(1999, 12, 31) * 86_400 + 86_399),
            1999
        );
    }

    #[test]
    fn sunos_ls_with_year_time_group_and_links() {
        let l = parse(
            b"total 9620\r\n\
drwxr-xr-x   2 dani     staff        512 Mar 16  1999 gcc-2.95.3\r\n\
-rw-r--r--   1 dani     staff     831744 Nov 13 08:09 bash-2.05b.tar.gz\r\n\
lrwxrwxrwx   1 root     root           7 Jan  2  2001 lib -> usr/lib\r\n\
-rw-------   1 dani     staff    4194304 Jul  4  2001 my core file\r\n\
crw-rw-rw-   1 root     sys      13,  2 Dec 30 23:59 null\r\n",
            Charset::Latin1,
            NOW,
        );
        let names: Vec<_> = l.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "gcc-2.95.3",
                "bash-2.05b.tar.gz",
                "lib",
                "my core file",
                "null"
            ]
        );
        assert_eq!(l[0].kind, EntryKind::Folder);
        assert_eq!(l[0].mode, Some(0o755));
        assert_eq!(l[0].group.as_deref(), Some("staff"));
        assert_eq!(l[1].size, Some(831_744));
        let t = l[1].modified.unwrap();
        assert_eq!(t.precision, TimePrecision::YearGuessed);
        assert_eq!(
            t.unix,
            days_from_civil(2026, 11, 13) * 86_400 + 8 * 3600 + 9 * 60
        );
        assert_eq!(l[2].link_target.as_deref(), Some("usr/lib"));
        // Dec 30 with a time, seen on Nov 13: last year's.
        assert_eq!(year_of(l[4].modified.unwrap().unix), 2025);
        assert_eq!(l[4].kind, EntryKind::Other);
    }

    #[test]
    fn no_group_column_and_latin1_names() {
        let l = parse(
            b"-rw-r--r--  1 dani  1024 Feb  1  1997 r\xe9sum\xe9.txt\n",
            Charset::Latin1,
            NOW,
        );
        assert_eq!(l[0].name, "r\u{e9}sum\u{e9}.txt");
        assert_eq!(l[0].raw_name, b"r\xe9sum\xe9.txt");
        assert_eq!(l[0].owner.as_deref(), Some("dani"));
        assert_eq!(l[0].group, None);
        assert_eq!(l[0].size, Some(1024));
    }

    #[test]
    fn dos_and_iis_listings() {
        let l = parse(
            b"11-13-97  10:04AM       <DIR>          My Documents\r\n\
03-16-99  09:12PM                 1234 AUTOEXEC.BAT\r\n\
12-31-2001  11:59PM                   0 empty.txt\r\n",
            Charset::Windows1252,
            NOW,
        );
        assert_eq!(l[0].name, "My Documents");
        assert!(l[0].is_folder());
        assert_eq!(l[1].size, Some(1234));
        assert_eq!(
            l[1].modified.unwrap().unix,
            days_from_civil(1999, 3, 16) * 86_400 + 21 * 3600 + 12 * 60
        );
        assert_eq!(year_of(l[2].modified.unwrap().unix), 2001);
    }

    #[test]
    fn unknown_lines_are_kept_as_names() {
        let l = parse(
            b"WEIRD.DAT;1   3  13-NOV-1997 10:04 [DANI] (RWED,RWED,,)\n",
            Charset::Latin1,
            NOW,
        );
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].kind, EntryKind::Other);
    }
}
