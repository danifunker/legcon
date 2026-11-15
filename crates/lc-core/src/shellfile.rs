//! Resuming over a Unix shell where rcp / SCP cannot (`dd` from an offset, `cat >>`), for rsh and
//! SSH without SFTP; commands run under `/bin/sh` on one line, so a csh login shell is fine.

use std::io::{Read, Write};

use crate::rcp::{shell_error, Link};
use crate::{Error, Result};

/// `dd` reads whole blocks of this size; the rest of the offset is skipped here.
pub const BLOCK: u64 = 512;

const OK: &[u8] = b"@legcon-ok";
const APPENDED: &[u8] = b"@legcon-appended";

/// Single-quote for the Bourne shell.
pub fn sq(b: &[u8]) -> Vec<u8> {
    let mut v = b"'".to_vec();
    for &c in b {
        if c == b'\'' {
            v.extend_from_slice(b"'\\''");
        } else {
            v.push(c);
        }
    }
    v.push(b'\'');
    v
}

/// `exec /bin/sh -c '<inner>'`, for any login shell.
pub fn under_sh(inner: &[u8]) -> Vec<u8> {
    let mut v = b"exec /bin/sh -c ".to_vec();
    v.extend_from_slice(&sq(inner));
    v
}

/// The command that sends `path` (bytes, unquoted) from byte `at` on.
pub fn tail_command(path: &[u8], at: u64) -> Vec<u8> {
    let q = sq(path);
    let mut inner = b"if test -f ".to_vec();
    for part in [
        &q[..],
        b" && test -r ",
        &q,
        b"; then echo ",
        OK,
        b"; exec dd if=",
        &q,
        format!(" bs={BLOCK} skip={} 2>/dev/null; else ls -ld ", at / BLOCK).as_bytes(),
        &q,
        b" 2>&1; fi",
    ] {
        inner.extend_from_slice(part);
    }
    under_sh(&inner)
}

/// The command that appends what it is sent to `path`.
pub fn append_command(path: &[u8]) -> Vec<u8> {
    let mut inner = b"cat >> ".to_vec();
    inner.extend_from_slice(&sq(path));
    inner.extend_from_slice(b" && echo ");
    inner.extend_from_slice(APPENDED);
    under_sh(&inner)
}

/// The file's bytes from the offset, after the command started with [`tail_command`].
pub struct Tail<L: Link> {
    s: L,
    done: bool,
}

/// Read the command's first line; then skip what `dd` sent before `at`.
pub fn tail<L: Link>(mut s: L, what: &str, at: u64) -> Result<Tail<L>> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while s.read(&mut b)? == 1 && b[0] != b'\n' {
        line.push(b[0]);
    }
    if line.trim_ascii_end() != OK {
        let mut rest = line;
        let _ = s.read_to_end(&mut rest);
        s.abort();
        let said = String::from_utf8_lossy(&rest).trim().to_string();
        return Err(if said.is_empty() || said.contains("No such") {
            Error::NotFound(format!("{what}: no such file"))
        } else {
            shell_error(what, format!("not a readable file ({said})").as_bytes())
        });
    }
    let mut t = Tail { s, done: false };
    std::io::copy(&mut (&mut t).take(at % BLOCK), &mut std::io::sink())?;
    Ok(t)
}

impl<L: Link> Read for Tail<L> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.s.read(buf)?;
        if n == 0 {
            self.done = true;
        }
        Ok(n)
    }
}

impl<L: Link> Drop for Tail<L> {
    fn drop(&mut self) {
        if self.done {
            self.s.end();
        } else {
            self.s.abort();
        }
    }
}

/// Appends `left` bytes, after the command started with [`append_command`].
pub struct Append<L: Link> {
    s: L,
    left: u64,
    what: String,
    finished: bool,
}

pub fn append<L: Link>(s: L, size: u64, what: &str) -> Result<Append<L>> {
    let mut a = Append {
        s,
        left: size,
        what: what.to_string(),
        finished: false,
    };
    if size == 0 {
        a.finish()?;
    }
    Ok(a)
}

impl<L: Link> Append<L> {
    /// End the input, then wait for the command's word that it all went in.
    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        self.s.close_write()?;
        let mut out = Vec::new();
        let _ = self.s.read_to_end(&mut out);
        self.s.end();
        if out.windows(APPENDED.len()).any(|w| w == APPENDED) {
            Ok(())
        } else {
            Err(shell_error(&self.what, &out))
        }
    }
}

impl<L: Link> Write for Append<L> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other(format!(
                "{}: more bytes than announced",
                self.what
            )));
        }
        let n = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.s.write(&buf[..n])?;
        self.left -= n as u64;
        if self.left == 0 {
            self.finish().map_err(std::io::Error::other)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<L: Link> Drop for Append<L> {
    fn drop(&mut self) {
        if !self.finished {
            self.s.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Script {
        input: Cursor<Vec<u8>>,
        out: Vec<u8>,
        closed: bool,
    }

    impl Read for Script {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(b)
        }
    }

    impl Write for Script {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.out.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Link for Script {
        fn close_write(&mut self) -> std::io::Result<()> {
            self.closed = true;
            Ok(())
        }
    }

    fn script(input: &[u8]) -> Script {
        Script {
            input: Cursor::new(input.to_vec()),
            out: Vec::new(),
            closed: false,
        }
    }

    #[test]
    fn commands_read_whole_blocks_and_quote_names() {
        let c = String::from_utf8(tail_command(b"/tmp/it's", 1030)).unwrap();
        assert!(
            c.starts_with("exec /bin/sh -c 'if test -f '\\''/tmp/it'\\''\\'"),
            "{c}"
        );
        assert!(c.contains("bs=512 skip=2 2>"), "{c}");
        let a = String::from_utf8(append_command(b"/tmp/a b")).unwrap();
        assert_eq!(
            a,
            "exec /bin/sh -c 'cat >> '\\''/tmp/a b'\\'' && echo @legcon-appended'"
        );
    }

    #[test]
    fn tail_skips_inside_the_first_block() {
        let mut input = b"@legcon-ok\n".to_vec();
        input.extend_from_slice(b"0123456789");
        let mut t = tail(script(&input), "f", 512 + 4).unwrap();
        let mut got = String::new();
        t.read_to_string(&mut got).unwrap();
        assert_eq!(got, "456789");
    }

    #[test]
    fn tail_of_a_missing_file_is_not_found() {
        let e = tail(script(b"ls: /x: No such file or directory\n"), "/x", 10)
            .err()
            .unwrap();
        assert!(matches!(e, Error::NotFound(_)), "{e}");
    }

    #[test]
    fn append_ends_input_and_checks_the_word() {
        let mut a = append(script(b"@legcon-appended\n"), 3, "f").unwrap();
        a.write_all(b"abc").unwrap();
        assert!(a.s.closed && a.s.out == b"abc");
        let mut a = append(script(b"cat: f: Permission denied\n"), 1, "f").unwrap();
        let e = a.write_all(b"x").unwrap_err();
        assert!(e.to_string().contains("Permission denied"), "{e}");
    }
}
