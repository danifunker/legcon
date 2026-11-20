//! A live shell session and its screen: a protocol's pty feeding a `vt100` screen model.
//!
//! A reader job pulls bytes off the pty, decodes them with the machine's character set and feeds
//! the screen, waking the view after each chunk. The view draws from [`TermSession::screen`],
//! a plain snapshot of runs of same-coloured text. Telnet and SSH open a real pty; rexec gets a line-mode one ([`line_term`]).

use std::io::{ErrorKind, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use lc_core::{Charset, ConnState, PtyWriter};

use super::job::{Job, Notify};
use super::line_term;
use super::machines::{Machine, ServiceKind};
use super::worker::lock_status;

/// Lines kept above the screen for scrolling back.
pub const SCROLLBACK: usize = 2000;
/// The terminal's own colours when the machine sets none (Windows Terminal's Campbell scheme).
pub const DEFAULT_FG: u32 = 0xcccccc;
pub const DEFAULT_BG: u32 = 0x0c0c0c;

const ANSI16: [u32; 16] = [
    0x0c0c0c, 0xc50f1f, 0x13a10e, 0xc19c00, 0x0037da, 0x881798, 0x3a96dd, 0xcccccc, 0x767676,
    0xe74856, 0x16c60c, 0xf9f1a5, 0x3b78ff, 0xb4009e, 0x61d6d6, 0xf2f2f2,
];

/// An indexed colour as 0xRRGGBB (the 16 ANSI colours, the 6x6x6 cube, the grey ramp).
pub fn indexed(i: u8) -> u32 {
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let n = u32::from(i - 16);
            let level = |v: u32| if v == 0 { 0 } else { 55 + v * 40 };
            (level(n / 36) << 16) | (level(n / 6 % 6) << 8) | level(n % 6)
        }
        232..=255 => {
            let g = 8 + 10 * u32::from(i - 232);
            (g << 16) | (g << 8) | g
        }
    }
}

fn rgb(c: vt100::Color, default: u32) -> u32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => indexed(i),
        vt100::Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
    }
}

/// A stretch of one line drawn the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermRun {
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub underline: bool,
}

/// What a view draws: every line as runs, and where the cursor is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TermScreen {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Vec<TermRun>>,
    /// Row and column, or `None` while the machine hides it.
    pub cursor: Option<(u16, u16)>,
    /// Cursor keys send `ESC O x` instead of `ESC [ x` (full-screen programs ask for it).
    pub app_cursor: bool,
}

/// What the machine asks the terminal and a real one answers: where the cursor is (login scripts
/// move it to the far corner and ask, to learn the screen's size), whether it is well, what it
/// is, how big it is. Without answers a prompt starts wherever the question left the cursor.
#[derive(Default)]
pub(crate) struct Answers {
    out: Vec<u8>,
}

impl vt100::Callbacks for Answers {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let (row, col) = screen.cursor_position();
        let (rows, cols) = screen.size();
        let reply = match (i1, c, first) {
            // Cursor position report (DSR 6), also the DEC form.
            (None, 'n', 6) => format!("\x1b[{};{}R", row + 1, col + 1),
            (Some(b'?'), 'n', 6) => format!("\x1b[?{};{}R", row + 1, col + 1),
            // Status report: all well.
            (None, 'n', 5) => "\x1b[0n".into(),
            // Primary device attributes: a VT100 with advanced video.
            (None, 'c', 0) => "\x1b[?1;2c".into(),
            // Secondary device attributes: a plain VT220-class terminal.
            (Some(b'>'), 'c', 0) => "\x1b[>1;10;0c".into(),
            // The text area's size in characters.
            (None, 't', 18) => format!("\x1b[8;{rows};{cols}t"),
            _ => return,
        };
        self.out.extend_from_slice(reply.as_bytes());
    }
}

struct Inner {
    parser: Mutex<vt100::Parser<Answers>>,
    writer: Mutex<Box<dyn PtyWriter>>,
    charset: Charset,
    /// Why the session ended, once it has.
    ended: Mutex<Option<String>>,
    reader: Mutex<Option<Job<()>>>,
}

/// One shell session; cheap to clone.
#[derive(Clone)]
pub struct TermSession {
    inner: Arc<Inner>,
}

fn snapshot(screen: &vt100::Screen) -> TermScreen {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut runs: Vec<TermRun> = Vec::new();
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let (mut fg, mut bg) = (
                rgb(cell.fgcolor(), DEFAULT_FG),
                rgb(cell.bgcolor(), DEFAULT_BG),
            );
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            let (bold, underline) = (cell.bold(), cell.underline());
            match runs.last_mut() {
                Some(run)
                    if run.fg == fg
                        && run.bg == bg
                        && run.bold == bold
                        && run.underline == underline =>
                {
                    run.text.push_str(text)
                }
                _ => runs.push(TermRun {
                    text: text.to_string(),
                    fg,
                    bg,
                    bold,
                    underline,
                }),
            }
        }
        lines.push(runs);
    }
    TermScreen {
        cols,
        rows,
        lines,
        cursor: (!screen.hide_cursor()).then(|| screen.cursor_position()),
        app_cursor: screen.application_cursor(),
    }
}

impl TermSession {
    /// Open the machine's command service in a job; the result is the running session.
    pub fn open(
        machine: &Machine,
        password: Option<String>,
        (cols, rows): (u16, u16),
        key_note: super::ssh::KeySlot,
        notify: Arc<dyn Notify>,
        trace: Option<lc_core::TraceLog>,
        keepalive: Option<std::time::Duration>,
    ) -> Job<TermSession> {
        let machine = machine.clone();
        let label = format!("Shell on {}", machine.name);
        Job::spawn(label, Arc::clone(&notify), move |ctx| {
            let service = machine
                .command_service()
                .ok_or_else(|| anyhow!("{} has no command service set up", machine.name))?;
            ctx.set_phase(format!(
                "Connecting to {} ({})",
                machine.address,
                service.kind.label()
            ));
            let pty = match service.kind {
                ServiceKind::Telnet => {
                    let opts = lc_telnet::Options {
                        cols,
                        rows,
                        term: machine.terminal_type(None),
                        ..lc_telnet::Options::default()
                    };
                    lc_telnet::connect(&machine.address, service.port, &opts)?
                }
                ServiceKind::Ssh => super::ssh::open_pty(
                    &machine,
                    service.port,
                    password,
                    &key_note,
                    cols,
                    rows,
                    trace.as_ref(),
                )?,
                ServiceKind::Rexec => {
                    let cs = Charset::from_label(&machine.charset).unwrap_or_default();
                    let enc = |t: &str| cs.encode(t).unwrap_or_else(|| t.as_bytes().to_vec());
                    let (user, pw) = (enc(&machine.user), enc(&password.unwrap_or_default()));
                    let r = lc_rcmd::Rexec::connect(
                        &machine.address,
                        service.port,
                        &user,
                        &pw,
                        &lc_rcmd::Options::default(),
                    )?;
                    let runner: line_term::Runner =
                        Arc::new(move |cmd: &[u8]| r.run(cmd).map_err(Into::into));
                    let banner = format!(
                        "rexec to {}: each line runs as its own command. cd is remembered; exit ends.",
                        machine.address
                    );
                    line_term::open(&machine.name, &banner, runner, Arc::clone(&notify))
                }
                ServiceKind::Rlogin => {
                    let cs = Charset::from_label(&machine.charset).unwrap_or_default();
                    let enc = |t: &str| cs.encode(t).unwrap_or_else(|| t.as_bytes().to_vec());
                    let local = machine.local_user.as_deref().unwrap_or(&machine.user);
                    lc_rcmd::rsh::rlogin(
                        &machine.address,
                        service.port,
                        &enc(local),
                        &enc(&machine.user),
                        &machine.terminal_type(None),
                        cols,
                        rows,
                        Duration::from_millis(250),
                        &lc_rcmd::Options::default(),
                    )?
                }
                ServiceKind::Rsh => {
                    let r = lc_rcmd::rsh::Rsh::connect(
                        &machine.address,
                        service.port,
                        machine.local_user.as_deref().unwrap_or(&machine.user),
                        &machine.user,
                        Charset::from_label(&machine.charset).unwrap_or_default(),
                        &lc_rcmd::Options::default(),
                    )?;
                    let runner: line_term::Runner =
                        Arc::new(move |cmd: &[u8]| r.run(cmd).map_err(Into::into));
                    let banner = format!(
                        "rsh to {}: each line runs as its own command. cd is remembered; exit ends.",
                        machine.address
                    );
                    line_term::open(&machine.name, &banner, runner, Arc::clone(&notify))
                }
                other => bail!("{} is not a command service", other.label()),
            };
            let charset = Charset::from_label(&machine.charset).unwrap_or_default();
            let session = TermSession {
                inner: Arc::new(Inner {
                    parser: Mutex::new(vt100::Parser::new_with_callbacks(
                        rows,
                        cols,
                        SCROLLBACK,
                        Answers::default(),
                    )),
                    writer: Mutex::new(pty.writer),
                    charset,
                    ended: Mutex::new(None),
                    reader: Mutex::new(None),
                }),
            };
            let reader = session.read_forever(pty.reader, notify, keepalive);
            *lock_status(&session.inner.reader) = Some(reader);
            Ok(session)
        })
    }

    fn read_forever(
        &self,
        mut pty: Box<dyn Read + Send>,
        notify: Arc<dyn Notify>,
        keepalive: Option<std::time::Duration>,
    ) -> Job<()> {
        let inner = Arc::clone(&self.inner);
        let wake = Arc::clone(&notify);
        Job::spawn("Shell reader", notify, move |ctx| {
            let mut buf = [0u8; 8192];
            let mut quiet = std::time::Instant::now();
            let why = loop {
                if ctx.is_cancelled() {
                    break "Session closed.".to_string();
                }
                match pty.read(&mut buf) {
                    Ok(0) => break "The machine closed the session.".to_string(),
                    Ok(n) => {
                        quiet = std::time::Instant::now();
                        let text = inner.charset.decode(&buf[..n]);
                        let answers = {
                            let mut p = lock_status(&inner.parser);
                            p.process(text.as_bytes());
                            std::mem::take(&mut p.callbacks_mut().out)
                        };
                        if !answers.is_empty() {
                            let _ = lock_status(&inner.writer).send(&answers);
                        }
                        wake.wake();
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                        ) =>
                    {
                        // Something the machine ignores, so a quiet shell is not dropped.
                        if keepalive.is_some_and(|k| quiet.elapsed() >= k) {
                            quiet = std::time::Instant::now();
                            if let Err(e) = lock_status(&inner.writer).keepalive() {
                                break format!("The connection broke: {e}");
                            }
                        }
                    }
                    Err(e) => break format!("The connection broke: {e}"),
                }
            };
            let at = chrono::Local::now().format("%H:%M:%S");
            *lock_status(&inner.ended) = Some(format!("-- {at} {why}"));
            wake.wake();
            Ok(())
        })
    }

    pub fn state(&self) -> ConnState {
        if lock_status(&self.inner.ended).is_some() {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    /// A line of legcon's own on the screen (never sent to the machine), such as a reconnect.
    pub fn note(&self, text: &str) {
        let line = format!("-- {text}\r\n");
        lock_status(&self.inner.parser).process(line.as_bytes());
    }

    /// Why the session ended, once it has.
    pub fn ended(&self) -> Option<String> {
        lock_status(&self.inner.ended).clone()
    }

    pub fn screen(&self) -> TermScreen {
        snapshot(lock_status(&self.inner.parser).screen())
    }

    /// Text typed or pasted, encoded for the machine; characters it has no code for become `?`.
    pub fn send_text(&self, text: &str) -> anyhow::Result<()> {
        let cs = self.inner.charset;
        let bytes: Vec<u8> = text
            .chars()
            .flat_map(|c| {
                cs.encode(c.encode_utf8(&mut [0; 4]))
                    .unwrap_or_else(|| vec![b'?'])
            })
            .collect();
        self.send(&bytes)
    }

    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        lock_status(&self.inner.writer).send(bytes)?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let (r, c) = lock_status(&self.inner.parser).screen().size();
        if (r, c) == (rows, cols) {
            return Ok(());
        }
        resize_keeping_cursor(&mut lock_status(&self.inner.parser), rows, cols);
        lock_status(&self.inner.writer).resize(cols, rows)?;
        Ok(())
    }

    /// Log off: send `logout` (if any) and give it a moment, then close and stop the reader.
    pub fn close(&self, logout: &str, notify: Arc<dyn Notify>) -> Job<()> {
        let me = self.clone();
        let logout = logout.to_string();
        Job::spawn("Close shell", notify, move |ctx| {
            if !logout.is_empty() && me.state() == ConnState::Connected {
                ctx.set_phase(format!("Sending {logout}"));
                let _ = me.send_text(&format!("{logout}\r"));
                std::thread::sleep(Duration::from_millis(400));
            }
            let closed = lock_status(&me.inner.writer).close();
            if let Some(reader) = lock_status(&me.inner.reader).take() {
                reader.cancel();
            }
            closed.map_err(|e| anyhow!("{e}"))
        })
    }
}

/// Resize the screen as xterm does: when it gets shorter, the top lines go rather than the ones
/// at the cursor, so the prompt stays where it was in the text instead of losing the lines above.
fn resize_keeping_cursor<C: vt100::Callbacks>(parser: &mut vt100::Parser<C>, rows: u16, cols: u16) {
    let (old_rows, _) = parser.screen().size();
    let (row, _) = parser.screen().cursor_position();
    if rows < old_rows && row >= rows {
        let n = row + 1 - rows;
        // Save the cursor, scroll the whole screen up n lines from its bottom, come back n higher.
        let seq = format!(
            "\x1b7\x1b[r\x1b[{old_rows};1H{}\x1b8\x1b[{n}A",
            "\n".repeat(n as usize)
        );
        parser.process(seq.as_bytes());
    }
    parser.screen_mut().set_size(rows, cols);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_to_the_terminal_are_answered() {
        let mut p = vt100::Parser::new_with_callbacks(24, 80, 0, Answers::default());
        // The size-finding trick: far corner, then "where is the cursor?".
        p.process(b"\x1b7\x1b[999;999H\x1b[6n\x1b8");
        assert_eq!(p.callbacks_mut().out, b"\x1b[24;80R");
        p.callbacks_mut().out.clear();
        p.process(b"\x1b[5n\x1b[c\x1b[>c\x1b[18t");
        assert_eq!(
            String::from_utf8_lossy(&p.callbacks().out),
            "\x1b[0n\x1b[?1;2c\x1b[>1;10;0c\x1b[8;24;80t"
        );
        // And the cursor went back where it was, so the prompt starts at the top.
        assert_eq!(p.screen().cursor_position(), (0, 0));
    }

    #[test]
    fn a_shorter_screen_keeps_the_cursor_line() {
        let mut p = vt100::Parser::new(10, 20, 0);
        p.process(b"one\r\ntwo\r\nthree\r\nfour\r\n$ ls\r\n$ ");
        resize_keeping_cursor(&mut p, 3, 20);
        let rows: Vec<String> = p.screen().rows(0, 20).collect();
        assert_eq!(rows, ["four", "$ ls", "$ "]);
        assert_eq!(p.screen().cursor_position(), (2, 2));
        // Taller or a cursor still inside: nothing moves.
        resize_keeping_cursor(&mut p, 8, 30);
        assert_eq!(p.screen().rows(0, 30).next().unwrap(), "four");
    }

    #[test]
    fn palette_corners() {
        assert_eq!(indexed(1), 0xc50f1f);
        assert_eq!(indexed(16), 0x000000);
        assert_eq!(indexed(231), 0xffffff);
        assert_eq!(indexed(232), 0x080808);
    }

    #[test]
    fn runs_join_cells_of_one_style() {
        let mut p = vt100::Parser::new(2, 10, 0);
        p.process(b"ab\x1b[31mcd\x1b[0me");
        let s = snapshot(p.screen());
        let texts: Vec<_> = s.lines[0].iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["ab", "cd", "e     "]);
        assert_eq!(s.lines[0][1].fg, 0xc50f1f);
        assert_eq!(s.cursor, Some((0, 5)));
    }
}
