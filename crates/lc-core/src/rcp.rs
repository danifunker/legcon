//! The rcp protocol, which SCP kept: `rcp -f` / `scp -f` sends a file, `-t` takes one.
//!
//! The source sends `C<mode> <size> <name>\n`, then the bytes, then a 0; each step is answered
//! with a 0 byte, or 1 / 2 and a line of text for an error. It runs over anything that reads and
//! writes: an SSH exec channel (`lc-ssh`) or an rsh connection (`lc-rcmd`).

use std::io::{Read, Write};

use crate::{Error, Result};

/// The connection a copy runs over, and how to end it.
pub trait Link: Read + Write + Send {
    /// The copy is complete: say we are done sending and wait for the other side to go.
    fn end(&mut self) {}
    /// The copy was cut short.
    fn abort(&mut self) {}
    /// No more input for the command (end of file), while its output can still be read.
    fn close_write(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a failed command or copy said, as the error the user sees.
pub fn shell_error(what: &str, said: &[u8]) -> Error {
    let text = String::from_utf8_lossy(said).trim().to_string();
    let lower = text.to_lowercase();
    let msg = if text.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {text}")
    };
    if lower.contains("no such file") || lower.contains("not found") {
        Error::NotFound(msg)
    } else if lower.contains("permission denied") || lower.contains("not permitted") {
        Error::Denied(msg)
    } else {
        Error::Protocol(msg)
    }
}

/// Read the acknowledgement byte; an error's text follows 1 or 2 up to the newline.
pub fn ack(s: &mut impl Read, what: &str) -> Result<()> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b)
        .map_err(|_| Error::Protocol(format!("{what}: the copy ended without a reply")))?;
    if b[0] == 0 {
        return Ok(());
    }
    let mut line = Vec::new();
    while s.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
        line.push(b[0]);
    }
    Err(shell_error(what, &line))
}

/// Receive one file from a source started with `-f`; `at` bytes are skipped (rcp cannot seek).
pub fn receive<S: Link>(mut s: S, what: &str, at: u64) -> Result<Receiver<S>> {
    s.write_all(&[0])?;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        s.read_exact(&mut b)
            .map_err(|_| Error::Protocol(format!("{what}: the copy sent nothing")))?;
        if line.is_empty() && (b[0] == 1 || b[0] == 2) {
            let mut msg = Vec::new();
            while s.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
                msg.push(b[0]);
            }
            s.abort();
            return Err(shell_error(what, &msg));
        }
        if b[0] == b'\n' {
            // `T` lines (times) come only with -p; skip one if a server sends it anyway.
            if line.first() == Some(&b'T') {
                s.write_all(&[0])?;
                line.clear();
                continue;
            }
            break;
        }
        line.push(b[0]);
    }
    let text = String::from_utf8_lossy(&line).into_owned();
    let size: u64 = match (text.starts_with('C'), text.split(' ').nth(1)) {
        (true, Some(n)) => n
            .parse()
            .map_err(|_| Error::Protocol(format!("{what}: the copy said {text:?}")))?,
        _ => {
            s.abort();
            return Err(Error::Unsupported(format!(
                "{what} is not a plain file ({text})"
            )));
        }
    };
    s.write_all(&[0])?;
    let mut r = Receiver {
        s,
        left: size,
        done: false,
    };
    std::io::copy(&mut (&mut r).take(at), &mut std::io::sink())?;
    Ok(r)
}

/// Send one file of `size` bytes named `name` to a sink started with `-t`.
pub fn send<S: Link>(mut s: S, name: &[u8], size: u64, what: &str) -> Result<Sender<S>> {
    ack(&mut s, what)?;
    let mut header = format!("C0644 {size} ").into_bytes();
    header.extend_from_slice(name);
    header.push(b'\n');
    s.write_all(&header)?;
    if let Err(e) = ack(&mut s, what) {
        s.abort();
        return Err(e);
    }
    let mut w = Sender {
        s,
        left: size,
        what: what.to_string(),
        finished: false,
    };
    if size == 0 {
        w.finish()?;
    }
    Ok(w)
}

pub struct Receiver<S: Link> {
    s: S,
    left: u64,
    done: bool,
}

impl<S: Link> Receiver<S> {
    /// The bytes still to come.
    pub fn left(&self) -> u64 {
        self.left
    }
}

impl<S: Link> Read for Receiver<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            if !self.done {
                self.done = true;
                // The sender's closing 0, then ours.
                let mut b = [0u8; 1];
                self.s.read_exact(&mut b)?;
                self.s.write_all(&[0])?;
            }
            return Ok(0);
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.s.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

impl<S: Link> Drop for Receiver<S> {
    fn drop(&mut self) {
        if self.done {
            self.s.end();
        } else {
            self.s.abort();
        }
    }
}

pub struct Sender<S: Link> {
    s: S,
    left: u64,
    what: String,
    finished: bool,
}

impl<S: Link> Sender<S> {
    /// Our closing 0, and the other side's word that the file is complete.
    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        self.s.write_all(&[0])?;
        ack(&mut self.s, &self.what)?;
        self.s.end();
        Ok(())
    }
}

impl<S: Link> Write for Sender<S> {
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

impl<S: Link> Drop for Sender<S> {
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

    /// Replays what a source says and records what we answer.
    struct Script {
        input: Cursor<Vec<u8>>,
        out: Vec<u8>,
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

    impl Link for Script {}

    #[test]
    fn receives_a_file_and_skips_a_resume_offset() {
        let s = Script {
            input: Cursor::new(b"C0644 11 hello.txt\nhello world\0".to_vec()),
            out: Vec::new(),
        };
        let mut r = receive(s, "hello.txt", 6).unwrap();
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "world");
        assert_eq!(r.s.out, [0, 0, 0]);
    }

    #[test]
    fn an_error_line_becomes_the_error() {
        let s = Script {
            input: Cursor::new(b"\x01rcp: /nope: No such file or directory\n".to_vec()),
            out: Vec::new(),
        };
        let e = receive(s, "/nope", 0).err().unwrap();
        assert!(
            matches!(e, Error::NotFound(ref m) if m.contains("No such file")),
            "{e}"
        );
    }

    #[test]
    fn sends_header_data_and_trailer() {
        let s = Script {
            input: Cursor::new(vec![0, 0, 0]),
            out: Vec::new(),
        };
        let mut w = send(s, b"a.txt", 3, "a.txt").unwrap();
        w.write_all(b"abc").unwrap();
        assert_eq!(w.s.out, b"C0644 3 a.txt\nabc\0");
    }
}
