//! Files over SSH without SFTP: SCP for contents, shell commands (`ls -laL`, `mkdir`, `mv`, `rm`)
//! for the rest. Servers before OpenSSH 2.3, Dropbear without `sftp-server`, and some SSH.com
//! installs have no SFTP subsystem but always have `scp` and a Bourne shell.
//!
//! SCP (the rcp protocol over exec): `scp -f` sends `C<mode> <size> <name>\n` then the bytes;
//! `scp -t` takes the same. Each step is acknowledged with a 0 byte; 1 or 2 and a line is an error.

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use lc_core::{Entry, Error, RemotePath, Result};
use ssh2::Channel;

use crate::{ssh_err, SshSession};

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

/// What a failed command said, as the error the user sees.
fn shell_error(what: &str, stderr: &[u8]) -> Error {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
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
fn ack(ch: &mut Channel, what: &str) -> Result<()> {
    let mut b = [0u8; 1];
    ch.read_exact(&mut b)
        .map_err(|_| Error::Protocol(format!("{what}: scp ended without a reply")))?;
    if b[0] == 0 {
        return Ok(());
    }
    let mut line = Vec::new();
    while ch.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
        line.push(b[0]);
    }
    Err(shell_error(what, &line))
}

impl SshSession {
    fn exec_channel(&mut self, cmd: &[u8]) -> Result<Channel> {
        let mut ch = self.session.channel_session().map_err(ssh_err)?;
        let cmd = std::str::from_utf8(cmd).map_err(|_| {
            Error::Unsupported("a name here has characters SSH exec cannot send".into())
        })?;
        ch.exec(cmd).map_err(ssh_err)?;
        Ok(ch)
    }

    /// Run a shell command that must succeed; its output.
    fn sh(&mut self, what: &str, cmd: &[u8]) -> Result<Vec<u8>> {
        let mut full = b"LC_ALL=C; export LC_ALL; ".to_vec();
        full.extend_from_slice(cmd);
        let r = self.run(&full)?;
        match r.exit_status {
            Some(0) | None => Ok(r.output),
            Some(_) => Err(shell_error(what, &r.errors)),
        }
    }

    fn qpath(&self, p: &RemotePath) -> Vec<u8> {
        sq(&self.path(p))
    }

    pub(crate) fn sh_home(&mut self) -> Result<RemotePath> {
        let out = self.sh("pwd", b"pwd")?;
        let dir = self.opts.charset.decode(out.trim_ascii());
        Ok(RemotePath::parse(lc_core::PathStyle::Unix, &dir))
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    pub(crate) fn sh_list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        // -L shows a linked folder as a folder, so it opens like any other.
        let mut cmd = b"ls -laL ".to_vec();
        cmd.extend_from_slice(&self.qpath(dir));
        let r = self.run(&[b"LC_ALL=C; export LC_ALL; ".as_slice(), &cmd].concat())?;
        if r.output.is_empty() && r.exit_status.unwrap_or(0) != 0 {
            return Err(shell_error(&dir.to_string(), &r.errors));
        }
        Ok(lc_core::listing::parse(
            &r.output,
            self.opts.charset,
            Self::now(),
        ))
    }

    pub(crate) fn sh_stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let mut cmd = b"ls -ldL ".to_vec();
        cmd.extend_from_slice(&self.qpath(path));
        let out = self.sh(&path.to_string(), &cmd)?;
        let line = out.split(|&b| b == b'\n').next().unwrap_or(&[]);
        let mut e = lc_core::listing::parse_unix(line, self.opts.charset, Self::now())
            .ok_or_else(|| Error::Protocol(format!("cannot read ls output for {path}")))?;
        e.name = path.name().to_string();
        e.raw_name = self.enc(path.name());
        Ok(e)
    }

    pub(crate) fn sh_simple(
        &mut self,
        what: &str,
        verb: &[u8],
        paths: &[&RemotePath],
    ) -> Result<()> {
        let mut cmd = verb.to_vec();
        for p in paths {
            cmd.push(b' ');
            cmd.extend_from_slice(&self.qpath(p));
        }
        self.sh(what, &cmd).map(drop)
    }

    pub(crate) fn sh_remove(&mut self, path: &RemotePath) -> Result<()> {
        let q = self.qpath(path);
        let mut cmd = b"if [ -d ".to_vec();
        for part in [
            &q[..],
            b" ]; then rmdir ",
            &q,
            b"; else rm -f ",
            &q,
            b"; fi",
        ] {
            cmd.extend_from_slice(part);
        }
        self.sh(&path.to_string(), &cmd).map(drop)
    }

    /// Read a file with `scp -f`; `at` bytes are skipped, since SCP cannot seek.
    pub(crate) fn scp_read(&mut self, path: &RemotePath, at: u64) -> Result<ScpReader> {
        let mut cmd = b"scp -f ".to_vec();
        cmd.extend_from_slice(&self.qpath(path));
        let mut ch = self.exec_channel(&cmd)?;
        let what = path.to_string();
        ch.write_all(&[0])?;
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            ch.read_exact(&mut b)
                .map_err(|_| Error::Protocol(format!("{what}: scp sent nothing")))?;
            if line.is_empty() && (b[0] == 1 || b[0] == 2) {
                let mut msg = Vec::new();
                while ch.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
                    msg.push(b[0]);
                }
                return Err(shell_error(&what, &msg));
            }
            if b[0] == b'\n' {
                // `T` lines (times) come only with -p; skip one if a server sends it anyway.
                if line.first() == Some(&b'T') {
                    ch.write_all(&[0])?;
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
                .map_err(|_| Error::Protocol(format!("{what}: scp said {text:?}")))?,
            _ => {
                return Err(Error::Unsupported(format!(
                    "{what} is not a plain file ({text})"
                )))
            }
        };
        ch.write_all(&[0])?;
        let mut r = ScpReader {
            ch,
            left: size,
            done: false,
        };
        std::io::copy(&mut (&mut r).take(at), &mut std::io::sink())?;
        Ok(r)
    }

    /// Write a file of `size` bytes with `scp -t`.
    pub(crate) fn scp_write(&mut self, path: &RemotePath, size: u64) -> Result<ScpWriter> {
        let parent = path
            .parent()
            .unwrap_or_else(|| RemotePath::root(path.style));
        let mut cmd = b"scp -t ".to_vec();
        cmd.extend_from_slice(&self.qpath(&parent));
        let mut ch = self.exec_channel(&cmd)?;
        let what = path.to_string();
        ack(&mut ch, &what)?;
        let mut header = format!("C0644 {size} ").into_bytes();
        header.extend_from_slice(&self.enc(path.name()));
        header.push(b'\n');
        ch.write_all(&header)?;
        ack(&mut ch, &what)?;
        let mut w = ScpWriter {
            ch,
            left: size,
            what,
            finished: false,
        };
        if size == 0 {
            w.finish()?;
        }
        Ok(w)
    }
}

pub struct ScpReader {
    ch: Channel,
    left: u64,
    done: bool,
}

impl Read for ScpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            if !self.done {
                self.done = true;
                // The sender's closing 0, then ours.
                let mut b = [0u8; 1];
                self.ch.read_exact(&mut b)?;
                self.ch.write_all(&[0])?;
            }
            return Ok(0);
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.ch.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

impl Drop for ScpReader {
    fn drop(&mut self) {
        let _ = self.ch.send_eof();
        let _ = self.ch.close();
    }
}

pub struct ScpWriter {
    ch: Channel,
    left: u64,
    what: String,
    finished: bool,
}

impl ScpWriter {
    /// Our closing 0, and the server's word that the file is complete.
    fn finish(&mut self) -> Result<()> {
        self.finished = true;
        self.ch.write_all(&[0])?;
        ack(&mut self.ch, &self.what)?;
        let _ = self.ch.send_eof();
        let _ = self.ch.wait_close();
        Ok(())
    }
}

impl Write for ScpWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other(format!(
                "{}: more bytes than announced to scp",
                self.what
            )));
        }
        let n = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.ch.write(&buf[..n])?;
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

impl Drop for ScpWriter {
    fn drop(&mut self) {
        if !self.finished {
            // Cut short: closing the channel makes scp give up on the file.
            let _ = self.ch.close();
        }
    }
}
