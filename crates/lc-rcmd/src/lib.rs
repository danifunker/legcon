//! The BSD r-commands for legcon (docs/PLAN.md §4.1, §4.2): rexec, and rsh / rlogin / rcp ([`rsh`]).
//!
//! rexec (port 512) runs one command with a user name and password, from any source port, so it
//! needs no privileges on the client. legcon sends `0` as the stderr port: the server then
//! merges stderr into the output instead of connecting back to us (D26). rexec reports no exit
//! status.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub mod rsh;

use lc_core::{ConnState, Connection, Error, ExecResult, Pty, Result, ShellCaps, ShellSession};

/// rexec's port (`exec` in /etc/services).
pub const REXEC_PORT: u16 = 512;

#[derive(Debug, Clone)]
pub struct Options {
    pub connect_timeout: Duration,
    /// How long the server may stay silent before a read gives up.
    pub read_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(120),
        }
    }
}

fn dial(host: &str, port: u16, opts: &Options) -> Result<TcpStream> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let s = TcpStream::connect_timeout(&addr, opts.connect_timeout).map_err(|e| {
        if e.kind() == ErrorKind::TimedOut {
            Error::Timeout(opts.connect_timeout)
        } else {
            Error::Io(e)
        }
    })?;
    s.set_read_timeout(Some(opts.read_timeout))?;
    s.set_nodelay(true)?;
    Ok(s)
}

/// The request as rexecd reads it: stderr port, user, password, command, each NUL-terminated.
pub fn request(user: &[u8], password: &[u8], command: &[u8]) -> Vec<u8> {
    let mut v = b"0\0".to_vec();
    for part in [user, password, command] {
        v.extend_from_slice(part);
        v.push(0);
    }
    v
}

/// Start `command`; the stream yields its output (stderr merged) until the command ends.
pub fn start(
    host: &str,
    port: u16,
    user: &[u8],
    password: &[u8],
    command: &[u8],
    opts: &Options,
) -> Result<TcpStream> {
    let mut s = dial(host, port, opts)?;
    s.write_all(&request(user, password, command))?;
    let mut status = [0u8; 1];
    if s.read(&mut status)? == 0 {
        return Err(Error::Protocol(
            "the server closed the connection without a reply".into(),
        ));
    }
    if status[0] != 0 {
        // A refusal is `\x01` and one line of text: "Login incorrect.", "Password incorrect."
        let mut why = Vec::new();
        let _ = s.read_to_end(&mut why);
        let text = String::from_utf8_lossy(&why).trim().to_string();
        let lower = text.to_lowercase();
        return Err(if lower.contains("login") || lower.contains("password") {
            Error::Auth(text)
        } else {
            Error::Denied(text)
        });
    }
    let _ = s.shutdown(Shutdown::Write);
    Ok(s)
}

/// A machine's rexec service: every command is its own connection, so nothing stays open.
pub struct Rexec {
    host: String,
    port: u16,
    user: Vec<u8>,
    password: Vec<u8>,
    opts: Options,
    closed: bool,
}

impl Rexec {
    /// Check the login with a harmless command, so a wrong password shows at connect.
    pub fn connect(
        host: &str,
        port: u16,
        user: &[u8],
        password: &[u8],
        opts: &Options,
    ) -> Result<Rexec> {
        let r = Rexec {
            host: host.to_string(),
            port,
            user: user.to_vec(),
            password: password.to_vec(),
            opts: opts.clone(),
            closed: false,
        };
        r.run(b"true")?;
        Ok(r)
    }

    /// Run `command` (bytes in the machine's character set) and collect its output.
    pub fn run(&self, command: &[u8]) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let mut s = start(
            &self.host,
            self.port,
            &self.user,
            &self.password,
            command,
            &self.opts,
        )?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).map_err(|e| {
            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                Error::Timeout(self.opts.read_timeout)
            } else {
                Error::Io(e)
            }
        })?;
        Ok(out)
    }
}

impl Connection for Rexec {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    /// Nothing is held open between commands, so there is nothing to log off.
    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}

impl ShellSession for Rexec {
    fn caps(&self) -> ShellCaps {
        ShellCaps {
            pty: false,
            exec: true,
            exit_status: false,
            window_size: false,
        }
    }

    fn exec(&mut self, command: &str) -> Result<ExecResult> {
        Ok(ExecResult {
            output: self.run(command.as_bytes())?,
            errors: Vec::new(),
            exit_status: None,
        })
    }

    fn open_pty(&mut self, _term: &str, _cols: u16, _rows: u16) -> Result<Pty> {
        Err(Error::Unsupported(
            "rexec runs single commands; it has no terminal".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn request_layout() {
        assert_eq!(
            super::request(b"dani", b"pw", b"ls -l"),
            b"0\0dani\0pw\0ls -l\0"
        );
    }
}
