//! Names crossing between a remote machine and this computer.
//!
//! Windows refuses `\ / : * ? " < > |`, control characters, a trailing dot or space, and device
//! names (`CON`, `NUL`, `COM1` ...), all common on Macs and Unix. Downloads turn them into
//! look-alikes: the fullwidth form (`?` -> `？`), control pictures (CR -> `␍`), and a fullwidth
//! last letter for a device name (`CON` -> `COＮ`). Uploads turn look-alikes back unless the
//! machine's character set holds them as real letters (Shift-JIS), where they are the name.

use lc_core::{Charset, PathStyle};

const FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
const DEVICES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn fullwidth(c: char) -> char {
    char::from_u32(c as u32 + 0xFEE0).unwrap_or(c)
}

/// The ASCII character a look-alike stands for, if it is one.
fn plain(c: char) -> Option<char> {
    let u = c as u32;
    match u {
        0xFF01..=0xFF5E => char::from_u32(u - 0xFEE0),
        0x2400..=0x241F => char::from_u32(u - 0x2400),
        0x3000 => Some(' '),
        _ => None,
    }
}

/// A remote name as this computer can store it.
pub fn to_local(name: &str) -> String {
    let mut chars: Vec<char> = name
        .chars()
        .map(|c| match c {
            c if FORBIDDEN.contains(&c) => fullwidth(c),
            c if (c as u32) < 0x20 => char::from_u32(0x2400 + c as u32).unwrap(),
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces.
    for c in chars.iter_mut().rev() {
        match *c {
            '.' => *c = fullwidth('.'),
            ' ' => *c = '\u{3000}',
            _ => break,
        }
    }
    let stem_len = chars.iter().position(|&c| c == '.').unwrap_or(chars.len());
    let stem: String = chars[..stem_len].iter().collect();
    if stem_len > 0 && DEVICES.iter().any(|d| d.eq_ignore_ascii_case(&stem)) {
        chars[stem_len - 1] = fullwidth(chars[stem_len - 1]);
    }
    chars.into_iter().collect()
}

/// The separator, and anything else, a machine of this style cannot have in a name.
fn refused_by(style: PathStyle, c: char) -> bool {
    match style {
        PathStyle::Unix => c == '/' || c == '\0',
        PathStyle::Mac => c == ':',
        PathStyle::Dos => FORBIDDEN.contains(&c) || (c as u32) < 0x20,
    }
}

/// A local name as it goes to the machine: look-alikes become plain again where the machine
/// allows the plain character, except on Shift-JIS machines that use fullwidth letters for real.
pub fn to_remote(name: &str, charset: Charset, style: PathStyle) -> String {
    name.chars()
        .map(|c| match plain(c) {
            Some(p)
                if (charset == Charset::Utf8
                    || charset.encode(c.encode_utf8(&mut [0; 4])).is_none())
                    && !refused_by(style, p) =>
            {
                p
            }
            _ => c,
        })
        .collect()
}

/// Why a name cannot go to the machine as it is, if it cannot.
pub fn remote_problem(name: &str, charset: Charset, max_len: Option<usize>) -> Option<String> {
    let Some(bytes) = charset.encode(name) else {
        let bad: String = name
            .chars()
            .filter(|c| charset.encode(c.encode_utf8(&mut [0; 4])).is_none())
            .take(3)
            .collect();
        return Some(format!(
            "the name has characters {} cannot hold ({bad})",
            charset.label()
        ));
    };
    match max_len {
        Some(max) if bytes.len() > max => Some(format!(
            "the name is {} characters long; this machine allows {max}",
            bytes.len()
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_safe_and_back() {
        for (remote, local, style) in [
            ("What?", "What\u{ff1f}", PathStyle::Unix),
            ("Q1/Q2", "Q1\u{ff0f}Q2", PathStyle::Mac),
            ("a: b", "a\u{ff1a} b", PathStyle::Unix),
            ("Icon\r", "Icon\u{240d}", PathStyle::Mac),
            ("draft. ", "draft\u{ff0e}\u{3000}", PathStyle::Unix),
            ("CON", "CO\u{ff2e}", PathStyle::Unix),
            ("nul.txt", "nu\u{ff4c}.txt", PathStyle::Unix),
            ("Read Me", "Read Me", PathStyle::Mac),
        ] {
            assert_eq!(to_local(remote), local, "{remote:?}");
            assert_eq!(
                to_remote(local, Charset::MacRoman, style),
                remote,
                "{local:?}"
            );
        }
        // A DOS machine cannot have `/` or `:`, so those stay look-alikes going there.
        let local = to_local("Q1/Q2: notes");
        assert_eq!(
            to_remote(&local, Charset::Windows1252, PathStyle::Dos),
            local
        );
    }

    #[test]
    fn real_fullwidth_names_and_separators_stay() {
        // Shift-JIS holds fullwidth letters, so they are the name, not look-alikes.
        assert_eq!(
            to_remote("\u{ff21}BC", Charset::ShiftJis, PathStyle::Unix),
            "\u{ff21}BC"
        );
        // A Mac name cannot have ':' and a Unix name cannot have '/'.
        assert_eq!(
            to_remote("a\u{ff1a}b", Charset::MacRoman, PathStyle::Mac),
            "a\u{ff1a}b"
        );
        assert_eq!(
            to_remote("a\u{ff0f}b", Charset::Latin1, PathStyle::Unix),
            "a\u{ff0f}b"
        );
        assert_eq!(
            to_remote("a\u{ff0f}b", Charset::MacRoman, PathStyle::Mac),
            "a/b"
        );
    }

    #[test]
    fn problems_are_said_plainly() {
        assert_eq!(remote_problem("Read Me", Charset::MacRoman, Some(31)), None);
        let e = remote_problem("\u{4e2d}\u{6587}.txt", Charset::MacRoman, None).unwrap();
        assert!(e.contains("MacRoman cannot hold"), "{e}");
        let e = remote_problem(&"x".repeat(40), Charset::MacRoman, Some(31)).unwrap();
        assert!(e.contains("allows 31"), "{e}");
    }
}
