//! Checking and setting up rsh trust (docs/PLAN.md §4.2, D40): one login over rexec, FTP or
//! Telnet, the server's view of our address, the `~/.rhosts` lines added, and rsh tried again.

use std::io::{ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use lc_core::shellfile::sq;
use lc_core::{Charset, FileSession, Pty, WriteMode};
use lc_rcmd::rsh::Rsh;
pub use lc_rcmd::rsh::{Untrusted, NO_LOW_PORT};
use lc_rcmd::Seen;

use super::job::{Job, Notify};
use super::machines::{Machine, ServiceKind};

/// The services that can set it up, best first: rexec runs one command; FTP and Telnet work too.
pub const VIA: [ServiceKind; 3] = [ServiceKind::Rexec, ServiceKind::Ftp, ServiceKind::Telnet];

/// Which of [`VIA`] a machine has.
pub fn via(machine: &Machine) -> Option<ServiceKind> {
    VIA.into_iter()
        .find(|k| machine.services.iter().any(|s| s.kind == *k))
}

/// Whether that service can also ask the server what it sees (FTP cannot run commands).
pub fn can_look(kind: ServiceKind) -> bool {
    matches!(kind, ServiceKind::Rexec | ServiceKind::Telnet)
}

/// What a check or a setup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustReport {
    pub via: Option<ServiceKind>,
    /// Our end of a connection, and what the server saw (when it could be asked).
    pub seen: Option<Seen>,
    /// Lines added to `~/.rhosts` now.
    pub added: Vec<String>,
    /// Lines that were already there.
    pub already: Vec<String>,
    /// rsh afterwards: `Ok` when it lets us in.
    pub rsh: Result<(), String>,
}

/// The name this computer goes by, for `.rhosts` (servers compare it ignoring case).
pub fn local_hostname() -> Option<String> {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())?;
    let name = name.trim().to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// The `.rhosts` lines that let `who` in from `seen`: the address, and this computer's name.
pub fn rhosts_lines(seen: &Seen, hostname: Option<&str>, who: &str) -> Vec<String> {
    let ip = seen.server.unwrap_or(seen.local).ip();
    let mut lines = vec![format!("{ip} {who}")];
    if let Some(h) = hostname.filter(|h| *h != ip.to_string()) {
        lines.push(format!("{h} {who}"));
    }
    lines
}

/// A basic regular expression matching `text` exactly.
fn bre(text: &str) -> String {
    let mut v = String::new();
    for c in text.chars() {
        if matches!(c, '.' | '[' | ']' | '*' | '^' | '$' | '\\') {
            v.push('\\');
        }
        v.push(c);
    }
    v
}

/// The shell command that adds each missing line to `~/.rhosts` and makes it private.
/// The marker is split by quotes so a terminal's echo of the command never matches it.
pub fn install_command(lines: &[String]) -> Vec<u8> {
    let mut inner = String::from("cd && touch .rhosts");
    for l in lines {
        inner += &format!(
            " && {{ grep '^{} *$' .rhosts >/dev/null 2>&1 && echo @legcon-\"\"had:{l} || echo '{l}' >> .rhosts; }}",
            bre(l)
        );
    }
    inner += " && chmod 600 .rhosts && echo @legcon-\"\"done || echo @legcon-\"\"failed";
    inner.into_bytes()
}

fn rsh_port(m: &Machine) -> u16 {
    port(m, ServiceKind::Rsh)
}

fn port(m: &Machine, kind: ServiceKind) -> u16 {
    m.services
        .iter()
        .find(|s| s.kind == kind)
        .map_or(kind.default_port(), |s| s.port)
}

fn who(m: &Machine) -> String {
    m.local_user.clone().unwrap_or_else(|| m.user.clone())
}

/// Try rsh the way a connect does: `Ok(None)` when it lets us in.
fn try_rsh(m: &Machine) -> anyhow::Result<Option<Untrusted>> {
    let charset = Charset::from_label(&m.charset).unwrap_or_default();
    match Rsh::check(
        &m.address,
        rsh_port(m),
        &who(m),
        &m.user,
        charset,
        &lc_rcmd::Options::default(),
    ) {
        Ok(Ok(_)) => Ok(None),
        Ok(Err(u)) => Ok(Some(u)),
        Err(e) => Err(anyhow!("rsh on port {}: {e}", rsh_port(m))),
    }
}

/// Check rsh and, with `install`, add this computer to `~/.rhosts` over rexec, FTP or Telnet.
pub fn rsh_trust(
    machine: &Machine,
    password: Option<String>,
    install: bool,
    notify: Arc<dyn Notify>,
) -> Job<TrustReport> {
    let m = machine.clone();
    let label = if install {
        format!("Set up rsh trust on {}", m.name)
    } else {
        format!("Check rsh on {}", m.name)
    };
    Job::spawn(label, notify, move |ctx| {
        ctx.set_phase("Trying rsh");
        let local = match try_rsh(&m)? {
            None => {
                return Ok(TrustReport {
                    via: None,
                    seen: None,
                    added: Vec::new(),
                    already: Vec::new(),
                    rsh: Ok(()),
                })
            }
            Some(Untrusted::NoLowPort) => bail!("{}", lc_rcmd::rsh::NO_LOW_PORT),
            Some(Untrusted::Refused { local, .. }) => local,
        };
        let kind = via(&m).ok_or_else(|| {
            anyhow!(
                "{} has no rexec, FTP or Telnet to log in with a password",
                m.name
            )
        })?;
        let password = password.unwrap_or_default();
        let mut report = TrustReport {
            via: Some(kind),
            seen: Some(Seen {
                local,
                server: None,
            }),
            added: Vec::new(),
            already: Vec::new(),
            rsh: Err(String::new()),
        };
        ctx.set_phase(format!("Logging in over {}", kind.label()));
        let host = local_hostname();
        let lines = |seen: &Seen| rhosts_lines(seen, host.as_deref(), &who(&m));
        match kind {
            ServiceKind::Rexec => {
                let p = port(&m, kind);
                let opts = lc_rcmd::Options::default();
                let seen = lc_rcmd::seen_via_rexec(
                    &m.address,
                    p,
                    m.user.as_bytes(),
                    password.as_bytes(),
                    &opts,
                )?;
                report.seen = Some(seen);
                if install {
                    ctx.set_phase("Adding this computer to ~/.rhosts");
                    let r = lc_rcmd::Rexec::connect(
                        &m.address,
                        p,
                        m.user.as_bytes(),
                        password.as_bytes(),
                        &opts,
                    )?;
                    let cmd = lc_core::shellfile::under_sh(&install_command(&lines(&seen)));
                    read_install(&r.run(&cmd)?, &lines(&seen), &mut report)?;
                }
            }
            ServiceKind::Telnet => {
                let mut t = Telnet::log_in(&m, port(&m, kind), &password)?;
                let seen = t.seen(port(&m, kind))?;
                report.seen = Some(seen);
                if install {
                    ctx.set_phase("Adding this computer to ~/.rhosts");
                    let out = t.run(
                        &install_command(&lines(&seen)),
                        &["@legcon-done", "@legcon-failed"],
                    )?;
                    read_install(&out, &lines(&seen), &mut report)?;
                }
                t.close();
            }
            ServiceKind::Ftp if install => {
                let seen = report.seen.unwrap_or(Seen {
                    local,
                    server: None,
                });
                ctx.set_phase("Adding this computer to ~/.rhosts over FTP");
                ftp_install(&m, &password, &lines(&seen), &mut report)?;
            }
            _ => {}
        }
        if install {
            ctx.set_phase("Trying rsh again");
        }
        report.rsh = match try_rsh(&m)? {
            None => Ok(()),
            Some(u) => Err(u.to_string()),
        };
        Ok(report)
    })
}

/// What the install command printed: which lines were there, and whether it all worked.
fn read_install(out: &[u8], lines: &[String], report: &mut TrustReport) -> anyhow::Result<()> {
    let text = String::from_utf8_lossy(out);
    // A terminal echoes the command (its markers split by quotes, so they never match) and may
    // put control characters before a line, so a marker is looked for anywhere in a line.
    let had: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.split_once("@legcon-had:")
                .map(|(_, rest)| rest.trim().to_string())
        })
        .collect();
    let ok = text.lines().any(|l| l.contains("@legcon-done"));
    if !ok {
        let said: Vec<&str> = text
            .lines()
            .filter(|l| !l.contains("@legcon") && !l.trim().is_empty())
            .collect();
        bail!("could not change ~/.rhosts: {}", said.join(" ").trim());
    }
    for l in lines {
        if had.contains(l) {
            report.already.push(l.clone());
        } else {
            report.added.push(l.clone());
        }
    }
    Ok(())
}

/// Over FTP: read `.rhosts` from the login folder, add what is missing, write it back.
fn ftp_install(
    m: &Machine,
    password: &str,
    lines: &[String],
    report: &mut TrustReport,
) -> anyhow::Result<()> {
    let opts = lc_ftp::Options {
        charset: Charset::from_label(&m.charset).unwrap_or_default(),
        ..lc_ftp::Options::default()
    };
    let mut s = lc_ftp::connect(
        &m.address,
        port(m, ServiceKind::Ftp),
        &m.user,
        password,
        &opts,
    )?;
    let home = s.home()?;
    let file = home.join(".rhosts");
    let mut old = Vec::new();
    // Listed first: a RETR refused for a missing file can upset proxies in front of the server.
    if s.list(&home)?.iter().any(|e| e.name == ".rhosts") {
        s.open_read(&file, 0)
            .context("reading ~/.rhosts")?
            .read_to_end(&mut old)?;
    }
    let text = String::from_utf8_lossy(&old).into_owned();
    let have: Vec<String> = text
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let mut new = old.clone();
    if !new.is_empty() && !new.ends_with(b"\n") {
        new.push(b'\n');
    }
    for l in lines {
        if have.contains(l) {
            report.already.push(l.clone());
        } else {
            new.extend_from_slice(l.as_bytes());
            new.push(b'\n');
            report.added.push(l.clone());
        }
    }
    if !report.added.is_empty() {
        let mut w = s
            .open_write_sized(&file, WriteMode::Overwrite, new.len() as u64)
            .context("writing ~/.rhosts")?;
        w.write_all(&new)?;
        w.flush()?;
    }
    // rshd ignores a .rhosts others can write; not every FTP server takes SITE CHMOD.
    if let Err(e) = FileSession::quote(&mut s, &home, "SITE CHMOD 600 .rhosts") {
        log::warn!("{}: SITE CHMOD 600 .rhosts: {e}", m.name);
    }
    let _ = lc_core::Connection::disconnect(&mut s, super::session::LOGOFF_TIMEOUT);
    Ok(())
}

/// A Telnet login driven like a person would: user name, password, then one command at a time.
struct Telnet {
    pty: Pty,
    local: SocketAddr,
    text: Vec<u8>,
}

impl Telnet {
    fn log_in(m: &Machine, port: u16, password: &str) -> anyhow::Result<Telnet> {
        let (pty, local) = lc_telnet::connect_with_local(&m.address, port, &Default::default())?;
        let mut t = Telnet {
            pty,
            local,
            text: Vec::new(),
        };
        t.until(0, &["login:", "username:"], Duration::from_secs(20))
            .context("no login prompt")?;
        t.send(m.user.as_bytes())?;
        t.until(t.text.len(), &["password:"], Duration::from_secs(15))
            .context("no password prompt")?;
        t.send(password.as_bytes())?;
        let mark = t.text.len();
        t.quiet(Duration::from_millis(1500), Duration::from_secs(15))?;
        let said = String::from_utf8_lossy(&t.text[mark..]).to_lowercase();
        // "Last login: ..." is a greeting; a fresh prompt at the end means the password failed.
        let again = said.trim_end().ends_with("login:");
        if said.contains("incorrect") || said.contains("denied") || again {
            bail!("Telnet login refused for {}", m.user);
        }
        Ok(t)
    }

    fn send(&mut self, line: &[u8]) -> anyhow::Result<()> {
        let mut v = line.to_vec();
        v.push(b'\r');
        self.pty.writer.send(&v)?;
        Ok(())
    }

    /// Read until one of `needles` (ignoring case) shows up after `from`.
    fn until(&mut self, from: usize, needles: &[&str], limit: Duration) -> anyhow::Result<()> {
        let end = Instant::now() + limit;
        loop {
            let hay =
                String::from_utf8_lossy(&self.text[from.min(self.text.len())..]).to_lowercase();
            if needles.iter().any(|n| hay.contains(n)) {
                return Ok(());
            }
            if Instant::now() > end {
                bail!("waited {limit:?} for {needles:?}");
            }
            self.read_some()?;
        }
    }

    /// Read until nothing arrives for `gap` (at most `limit` in all).
    fn quiet(&mut self, gap: Duration, limit: Duration) -> anyhow::Result<()> {
        let end = Instant::now() + limit;
        let mut last = Instant::now();
        while Instant::now() < end && last.elapsed() < gap {
            if self.read_some()? {
                last = Instant::now();
            }
        }
        Ok(())
    }

    fn read_some(&mut self) -> anyhow::Result<bool> {
        let mut buf = [0u8; 4096];
        match self.pty.reader.read(&mut buf) {
            Ok(0) => bail!("the Telnet session closed"),
            Ok(n) => {
                self.text.extend_from_slice(&buf[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Run a Bourne shell command line; what came back once a marker showed up.
    fn run(&mut self, inner: &[u8], markers: &[&str]) -> anyhow::Result<Vec<u8>> {
        let mark = self.text.len();
        let mut line = b"/bin/sh -c ".to_vec();
        line.extend_from_slice(&sq(inner));
        self.send(&line)?;
        self.until(mark, markers, Duration::from_secs(30))?;
        // The rest of the marker's line.
        let _ = self.quiet(Duration::from_millis(300), Duration::from_secs(2));
        Ok(self.text[mark..].to_vec())
    }

    fn seen(&mut self, port: u16) -> anyhow::Result<Seen> {
        let inner = b"netstat -an 2>/dev/null || ss -tn 2>/dev/null; echo @legcon-\"\"net";
        let out = self.run(inner, &["@legcon-net"])?;
        Ok(Seen {
            local: self.local,
            server: lc_rcmd::seen_peer(&out, &[port, lc_telnet::PORT], self.local),
        })
    }

    fn close(mut self) {
        let _ = self.send(b"exit");
        let _ = self.quiet(Duration::from_millis(300), Duration::from_secs(2));
        let _ = self.pty.writer.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_use_the_address_the_server_saw() {
        let seen = Seen {
            local: "192.168.1.10:1023".parse().unwrap(),
            server: Some("10.0.2.2:40112".parse().unwrap()),
        };
        assert_eq!(
            rhosts_lines(&seen, Some("retro-pc"), "dani"),
            ["10.0.2.2 dani", "retro-pc dani"]
        );
        let alone = Seen {
            server: None,
            ..seen
        };
        assert_eq!(rhosts_lines(&alone, None, "dani"), ["192.168.1.10 dani"]);
    }

    #[test]
    fn install_adds_only_missing_lines() {
        let c = String::from_utf8(install_command(&["10.0.0.1 dani".into()])).unwrap();
        assert!(c.contains("grep '^10\\.0\\.0\\.1 dani *$' .rhosts"), "{c}");
        assert!(c.contains("|| echo '10.0.0.1 dani' >> .rhosts"), "{c}");
        assert!(
            c.ends_with("echo @legcon-\"\"done || echo @legcon-\"\"failed"),
            "{c}"
        );
        let mut r = TrustReport {
            via: None,
            seen: None,
            added: Vec::new(),
            already: Vec::new(),
            rsh: Ok(()),
        };
        let lines = ["a dani".to_string(), "b dani".to_string()];
        read_install(
            b"@legcon-had:a dani\r\n^@@legcon-done\r\n% ",
            &lines,
            &mut r,
        )
        .unwrap();
        assert_eq!((&r.already[..], &r.added[..]), (&lines[..1], &lines[1..]));
        let e = read_install(
            b"touch: .rhosts: Permission denied\n@legcon-failed\n",
            &lines,
            &mut r,
        );
        assert!(e.unwrap_err().to_string().contains("Permission denied"));
    }

    #[test]
    fn via_prefers_rexec() {
        let mut m = Machine::new("irix", "10.0.0.5");
        for kind in [ServiceKind::Telnet, ServiceKind::Ftp, ServiceKind::Rsh] {
            m.services.push(super::super::machines::Service {
                kind,
                port: kind.default_port(),
            });
        }
        assert_eq!(via(&m), Some(ServiceKind::Ftp));
        assert!(!can_look(ServiceKind::Ftp));
    }
}
