//! A terminal for services that only run one command at a time (rexec; later rsh and SSH exec).
//!
//! It looks like a shell in the Shell tab: a prompt, line editing with local echo, and each line
//! run as its own command with the output shown below. `cd` is kept here, since every command
//! starts in the login folder: later commands run as `cd 'dir' && command`. `exit` or `logout`
//! ends the session without running anything.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::{Pty, PtyWriter};

use super::job::{Job, Notify};
use super::worker::lock_status;

/// Runs one command line (in the machine's character set) and returns its output.
pub type Runner = Arc<dyn Fn(&[u8]) -> anyhow::Result<Vec<u8>> + Send + Sync>;

const POLL: Duration = Duration::from_millis(250);

struct Shared {
    out: Mutex<Option<Sender<Vec<u8>>>>,
    closed: AtomicBool,
    busy: AtomicBool,
    cwd: Mutex<Option<Vec<u8>>>,
}

impl Shared {
    fn emit(&self, bytes: Vec<u8>) {
        if let Some(tx) = lock_status(&self.out).as_ref() {
            let _ = tx.send(bytes);
        }
    }

    fn prompt(&self, name: &str) {
        let mut p = name.as_bytes().to_vec();
        if let Some(cwd) = lock_status(&self.cwd).as_ref() {
            p.push(b':');
            p.extend_from_slice(cwd);
        }
        p.extend_from_slice(b"$ ");
        self.emit(p);
    }
}

struct Reader {
    rx: Receiver<Vec<u8>>,
    shared: Arc<Shared>,
    pending: Vec<u8>,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv_timeout(POLL) {
                Ok(v) => self.pending = v,
                Err(RecvTimeoutError::Timeout) if !self.shared.closed.load(Ordering::Relaxed) => {
                    return Err(io::ErrorKind::TimedOut.into())
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

#[derive(Default, PartialEq)]
enum Esc {
    #[default]
    None,
    Started,
    /// Inside `ESC [ ...` or `ESC O x`, until its final byte.
    Sequence,
}

struct Writer {
    shared: Arc<Shared>,
    runner: Runner,
    name: String,
    notify: Arc<dyn Notify>,
    line: Vec<u8>,
    esc: Esc,
}

fn quoted(dir: &[u8]) -> Vec<u8> {
    let mut v = b"'".to_vec();
    for &b in dir {
        if b == b'\'' {
            v.extend_from_slice(b"'\\''");
        } else {
            v.push(b);
        }
    }
    v.push(b'\'');
    v
}

/// Unix output ends lines with LF alone; the screen needs CR LF.
fn crlf(out: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(out.len() + out.len() / 16);
    let mut prev = 0u8;
    for &b in out {
        if b == b'\n' && prev != b'\r' {
            v.push(b'\r');
        }
        v.push(b);
        prev = b;
    }
    if !v.is_empty() && !v.ends_with(b"\n") {
        v.extend_from_slice(b"\r\n");
    }
    v
}

impl Writer {
    fn enter(&mut self) {
        let line = std::mem::take(&mut self.line);
        self.shared.emit(b"\r\n".to_vec());
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() {
            self.shared.prompt(&self.name);
            return;
        }
        if trimmed == b"exit" || trimmed == b"logout" {
            self.shared.closed.store(true, Ordering::Relaxed);
            lock_status(&self.shared.out).take();
            return;
        }
        let cwd = lock_status(&self.shared.cwd).clone();
        let mut command = Vec::new();
        if let Some(d) = &cwd {
            command.extend_from_slice(b"cd ");
            command.extend_from_slice(&quoted(d));
            command.extend_from_slice(b" && ");
        }
        let is_cd = trimmed == b"cd" || trimmed.starts_with(b"cd ");
        command.extend_from_slice(trimmed);
        if is_cd {
            command.extend_from_slice(b" && pwd");
        }
        self.shared.busy.store(true, Ordering::Relaxed);
        let shared = Arc::clone(&self.shared);
        let runner = Arc::clone(&self.runner);
        let name = self.name.clone();
        let label = format!("Run {}", String::from_utf8_lossy(trimmed));
        // The handle is not kept: the job reports through the terminal itself.
        drop(Job::spawn(label, Arc::clone(&self.notify), move |_ctx| {
            match runner(&command) {
                Ok(out) => {
                    let dir = out.trim_ascii();
                    if is_cd && dir.starts_with(b"/") && !dir.contains(&b'\n') {
                        *lock_status(&shared.cwd) = Some(dir.to_vec());
                    } else {
                        shared.emit(crlf(&out));
                    }
                }
                Err(e) => shared.emit(format!("legcon: {e:#}\r\n").into_bytes()),
            }
            shared.busy.store(false, Ordering::Relaxed);
            shared.prompt(&name);
            Ok(())
        }));
    }
}

impl PtyWriter for Writer {
    fn send(&mut self, bytes: &[u8]) -> lc_core::Result<()> {
        if self.shared.closed.load(Ordering::Relaxed) {
            return Err(lc_core::Error::NotConnected);
        }
        for &b in bytes {
            match (&self.esc, b) {
                (Esc::Started, b'[' | b'O') => {
                    self.esc = Esc::Sequence;
                    continue;
                }
                (Esc::Started, _) => self.esc = Esc::None,
                (Esc::Sequence, 0x40..=0x7e) => {
                    self.esc = Esc::None;
                    continue;
                }
                (Esc::Sequence, _) => continue,
                (Esc::None, _) => {}
            }
            if self.shared.busy.load(Ordering::Relaxed) {
                continue;
            }
            match b {
                0x1b => self.esc = Esc::Started,
                b'\r' | b'\n' => self.enter(),
                0x08 | 0x7f => {
                    if self.line.pop().is_some() {
                        self.shared.emit(b"\x08 \x08".to_vec());
                    }
                }
                0x15 => {
                    let n = self.line.len();
                    self.line.clear();
                    self.shared.emit(b"\x08 \x08".repeat(n));
                }
                0x20.. => {
                    self.line.push(b);
                    self.shared.emit(vec![b]);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn resize(&mut self, _cols: u16, _rows: u16) -> lc_core::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> lc_core::Result<()> {
        self.shared.closed.store(true, Ordering::Relaxed);
        lock_status(&self.shared.out).take();
        Ok(())
    }
}

/// A terminal that runs each line through `runner`; `banner` is shown first.
pub fn open(name: &str, banner: &str, runner: Runner, notify: Arc<dyn Notify>) -> Pty {
    let (tx, rx) = channel();
    let shared = Arc::new(Shared {
        out: Mutex::new(Some(tx)),
        closed: AtomicBool::new(false),
        busy: AtomicBool::new(false),
        cwd: Mutex::new(None),
    });
    shared.emit(crlf(banner.as_bytes()));
    shared.prompt(name);
    Pty {
        reader: Box::new(Reader {
            rx,
            shared: Arc::clone(&shared),
            pending: Vec::new(),
        }),
        writer: Box::new(Writer {
            shared,
            runner,
            name: name.to_string(),
            notify,
            line: Vec::new(),
            esc: Esc::None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::job::NoNotify;

    fn drain(r: &mut dyn Read, until: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        for _ in 0..40 {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
            if got.ends_with(until) {
                break;
            }
        }
        got
    }

    #[test]
    fn runs_lines_keeps_cd_and_ends_on_exit() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&seen);
        let runner: Runner = Arc::new(move |cmd: &[u8]| {
            let c = String::from_utf8_lossy(cmd).to_string();
            lock_status(&log).push(c.clone());
            Ok(if c.ends_with("&& pwd") {
                b"/usr/local\n".to_vec()
            } else {
                b"one\ntwo\n".to_vec()
            })
        });
        let mut pty = open("sun", "rexec to sun", runner, Arc::new(NoNotify));
        assert_eq!(drain(pty.reader.as_mut(), b"$ "), b"rexec to sun\r\nsun$ ");
        pty.writer.send(b"lx\x7fs\r").unwrap();
        let out = drain(pty.reader.as_mut(), b"$ ");
        assert_eq!(out, b"lx\x08 \x08s\r\none\r\ntwo\r\nsun$ ");
        pty.writer.send(b"cd /usr/local\r").unwrap();
        assert!(drain(pty.reader.as_mut(), b"$ ").ends_with(b"sun:/usr/local$ "));
        pty.writer.send(b"\x1b[Als\r").unwrap();
        drain(pty.reader.as_mut(), b"$ ");
        assert_eq!(
            *lock_status(&seen),
            ["ls", "cd /usr/local && pwd", "cd '/usr/local' && ls"]
        );
        pty.writer.send(b"exit\r").unwrap();
        drain(pty.reader.as_mut(), b"\0");
        let mut buf = [0u8; 8];
        assert_eq!(pty.reader.read(&mut buf).unwrap(), 0);
    }
}
