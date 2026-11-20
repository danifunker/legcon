//! The traits every protocol implements (docs/PLAN.md §3).

use std::io::{Read, Write};
use std::time::Duration;

use crate::{Entry, RemotePath, Result};

/// Where one connection stands; drives the state light everywhere (PLAN.md §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    /// Connected and running a command or transfer.
    Busy,
    Disconnecting,
    /// The link dropped without a log-off.
    Lost,
    /// Logged off by legcon itself (idle time, sleep); using the machine connects again.
    Idle,
}

/// Every protocol, file or shell.
pub trait Connection: Send {
    fn state(&self) -> ConnState;

    /// Log off cleanly on the server (SMB `LOGOFF`, FTP `QUIT`, NFS `UMNT`, ...), then close.
    fn disconnect(&mut self, timeout: Duration) -> Result<()>;

    /// Ask the server something cheap (FTP `NOOP`, SMB `ECHO`, NFS `NULL`), to keep the link up
    /// or learn it is gone; protocols with a connection per request have nothing to ask.
    fn ping(&mut self, timeout: Duration) -> Result<()> {
        let _ = timeout;
        Ok(())
    }

    /// What closes the link from another thread, so a call stuck on a dead server returns.
    fn breaker(&self) -> Option<crate::net::Breaker> {
        None
    }
}

/// What a file protocol (and this server) can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileCaps {
    /// Read from any offset (SMB, NFS, SFTP; FTP only through `REST`).
    pub random_read: bool,
    /// Write at any offset; without it a file is sent whole.
    pub random_write: bool,
    pub resume_upload: bool,
    pub rename: bool,
    pub delete: bool,
    pub mkdir: bool,
    pub set_times: bool,
    pub set_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Create,
    Overwrite,
    /// Continue an upload at this offset.
    Resume(u64),
}

/// A classic Mac file's other half: its Finder info (type, creator, flags) and resource fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacExtras {
    pub finder_info: [u8; 32],
    pub resource_fork: Vec<u8>,
}

impl MacExtras {
    pub fn type_code(&self) -> [u8; 4] {
        self.finder_info[..4].try_into().unwrap()
    }

    pub fn creator(&self) -> [u8; 4] {
        self.finder_info[4..8].try_into().unwrap()
    }

    /// Nothing worth keeping: no fork and no type or creator.
    pub fn is_empty(&self) -> bool {
        self.resource_fork.is_empty() && self.finder_info[..8].iter().all(|&b| b == 0)
    }
}

/// Browsing and transferring files.
pub trait FileSession: Connection {
    fn caps(&self) -> FileCaps;
    /// The folder the server puts the user in after login.
    fn home(&mut self) -> Result<RemotePath>;
    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>>;
    fn stat(&mut self, path: &RemotePath) -> Result<Entry>;
    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>>;
    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>>;
    /// Open to write a file whose size is known up front; SCP must announce it before the data.
    fn open_write_sized(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
        size: u64,
    ) -> Result<Box<dyn Write + Send + '_>> {
        let _ = size;
        self.open_write(path, mode)
    }
    fn mkdir(&mut self, path: &RemotePath) -> Result<()>;
    fn remove(&mut self, path: &RemotePath) -> Result<()>;
    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()>;

    /// The character set names are in on the machine (UTF-8 unless the protocol says otherwise).
    fn charset(&self) -> crate::Charset {
        crate::Charset::Utf8
    }

    /// The longest name the machine takes, in bytes of its character set (31 for classic Mac OS).
    fn max_name_len(&self) -> Option<usize> {
        None
    }

    /// Whether the protocol keeps Finder info and resource forks (AFP), so they can travel.
    fn keeps_mac_extras(&self) -> bool {
        false
    }

    /// A file's Finder info and resource fork, where the protocol has them.
    fn mac_extras(&mut self, path: &RemotePath) -> Result<Option<MacExtras>> {
        let _ = path;
        Ok(None)
    }

    /// Set a file's Finder info and resource fork (the data fork is written separately).
    fn set_mac_extras(&mut self, path: &RemotePath, extras: &MacExtras) -> Result<()> {
        let _ = (path, extras);
        Err(crate::Error::Unsupported(
            "this protocol has no resource forks".into(),
        ))
    }

    /// A checksum the machine computes itself (`cksum` over a shell, FTP `XCRC`); `None` if it
    /// cannot, so a copy is checked by size alone.
    fn checksum(&mut self, path: &RemotePath) -> Result<Option<crate::Checksum>> {
        let _ = path;
        Ok(None)
    }

    /// Send one of the protocol's own commands as typed (the Console tab), in folder `dir` so
    /// relative names mean what the user sees; the reply text.
    fn quote(&mut self, dir: &RemotePath, line: &str) -> Result<String> {
        let _ = dir;
        Err(crate::Error::Unsupported(format!(
            "this protocol has no command console ({line})"
        )))
    }
}

/// What a command protocol can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShellCaps {
    /// An interactive terminal (Telnet, rlogin, SSH).
    pub pty: bool,
    /// One command with its output (rexec, rsh, SSH exec).
    pub exec: bool,
    /// The command's exit status is reported (SSH, not rexec).
    pub exit_status: bool,
    pub window_size: bool,
}

/// The result of a one-shot command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecResult {
    pub output: Vec<u8>,
    /// Separate only where the protocol keeps it apart.
    pub errors: Vec<u8>,
    pub exit_status: Option<i32>,
}

/// The sending half of a live terminal; the receiving half is a plain `Read`.
///
/// Reads may fail with `TimedOut` / `WouldBlock` when nothing arrived for a while, so a reader
/// thread can check for cancel; 0 bytes read means the machine closed the session.
pub trait PtyWriter: Send {
    /// Keystrokes or pasted text, already in the machine's character set.
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    /// Send something the machine ignores (Telnet `NOP`, SSH `IGNORE`), so an idle link stays up.
    fn keepalive(&mut self) -> Result<()> {
        Ok(())
    }
    /// Close the session (the caller sends any logout command first).
    fn close(&mut self) -> Result<()>;
}

/// A live terminal session, split so one thread reads while another sends.
pub struct Pty {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn PtyWriter>,
}

/// Running commands.
pub trait ShellSession: Connection {
    fn caps(&self) -> ShellCaps;
    fn exec(&mut self, command: &str) -> Result<ExecResult>;
    fn open_pty(&mut self, term: &str, cols: u16, rows: u16) -> Result<Pty>;
}
