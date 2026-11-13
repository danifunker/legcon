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
}

/// Every protocol, file or shell.
pub trait Connection: Send {
    fn state(&self) -> ConnState;

    /// Log off cleanly on the server (SMB `LOGOFF`, FTP `QUIT`, NFS `UMNT`, ...), then close.
    fn disconnect(&mut self, timeout: Duration) -> Result<()>;
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
    fn mkdir(&mut self, path: &RemotePath) -> Result<()>;
    fn remove(&mut self, path: &RemotePath) -> Result<()>;
    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()>;

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
