//! Files over SSH without SFTP: SCP for contents, shell commands (`ls -laL`, `mkdir`, `mv`, `rm`)
//! for the rest. Servers before OpenSSH 2.3, Dropbear without `sftp-server`, and some SSH.com
//! installs have no SFTP subsystem but always have `scp` and a Bourne shell.
//!
//! SCP (the rcp protocol over exec): `scp -f` sends `C<mode> <size> <name>\n` then the bytes;
//! `scp -t` takes the same. Each step is acknowledged with a 0 byte; 1 or 2 and a line is an error.

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::transport::Channel;
use crate::SshSession;
use lc_core::rcp::{self, shell_error};
use lc_core::shellfile::{self, Append, Tail};
use lc_core::{Entry, Error, RemotePath, Result};

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

/// An exec channel as the link an rcp copy runs over.
pub struct ChannelLink(Channel);

impl Read for ChannelLink {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(b)
    }
}

impl Write for ChannelLink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl rcp::Link for ChannelLink {
    fn close_write(&mut self) -> std::io::Result<()> {
        self.0
            .send_eof()
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn end(&mut self) {
        let _ = self.0.send_eof();
        let _ = self.0.wait_close();
    }
    fn abort(&mut self) {
        // Closing the channel makes scp give up on the file.
        let _ = self.0.close();
    }
}

pub type ScpReader = rcp::Receiver<ChannelLink>;
pub type ScpWriter = rcp::Sender<ChannelLink>;

impl SshSession {
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

    /// Read a file with `scp -f`.
    pub(crate) fn scp_read(&mut self, path: &RemotePath) -> Result<ScpReader> {
        let mut cmd = b"scp -f ".to_vec();
        cmd.extend_from_slice(&self.qpath(path));
        let ch = self.exec_channel(&cmd)?;
        rcp::receive(ChannelLink(ch), &path.to_string(), 0)
    }

    /// Read a file from byte `at` on with `dd`, since SCP would send the skipped bytes too.
    pub(crate) fn sh_tail(&mut self, path: &RemotePath, at: u64) -> Result<Tail<ChannelLink>> {
        let ch = self.exec_channel(&shellfile::tail_command(&self.path(path), at))?;
        shellfile::tail(ChannelLink(ch), &path.to_string(), at)
    }

    /// Append `size` bytes to a file with `cat >>`, since SCP cannot.
    pub(crate) fn sh_append(
        &mut self,
        path: &RemotePath,
        size: u64,
    ) -> Result<Append<ChannelLink>> {
        let ch = self.exec_channel(&shellfile::append_command(&self.path(path)))?;
        shellfile::append(ChannelLink(ch), size, &path.to_string())
    }

    /// Write a file of `size` bytes with `scp -t`.
    pub(crate) fn scp_write(&mut self, path: &RemotePath, size: u64) -> Result<ScpWriter> {
        let parent = path
            .parent()
            .unwrap_or_else(|| RemotePath::root(path.style));
        let mut cmd = b"scp -t ".to_vec();
        cmd.extend_from_slice(&self.qpath(&parent));
        let ch = self.exec_channel(&cmd)?;
        let name = self.enc(path.name());
        rcp::send(ChannelLink(ch), &name, size, &path.to_string())
    }
}
