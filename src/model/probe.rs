//! Finding out what a machine offers before it is added (docs/PLAN.md §5, phase 2).
//!
//! Each usual port is tried with a short timeout; where a service answers without a login, its
//! greeting says more: the FTP and SSH banners, the web server's name, the SMB dialect and
//! security mode, the AFP server's name and versions, the NFS exports. From that come the
//! services to use (best file service first), a character set, and a guess at the system.
//! Nothing logs in, and nothing is written.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use super::job::{Job, JobCtx, Notify};
use super::machines::ServiceKind;

const CONNECT: Duration = Duration::from_millis(800);
const READ: Duration = Duration::from_secs(3);

/// How a finding's detail starts when the port answered but the service cannot be used.
const UNUSABLE: &str = "answered, but ";

/// One service that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: ServiceKind,
    pub port: u16,
    /// What it said about itself, such as `Microsoft FTP Service` or `NT LM 0.12, user-level`.
    pub detail: String,
}

/// What a probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub address: String,
    pub findings: Vec<Finding>,
    /// A guess such as `Windows NT (IIS)` or `Mac OS (AppleShare)`.
    pub system: Option<String>,
    /// The character set names are probably in, as a `Charset` label.
    pub charset: String,
}

impl Report {
    /// The services to set up, best file service first.
    pub fn services(&self) -> Vec<(ServiceKind, u16)> {
        const ORDER: [ServiceKind; 11] = [
            ServiceKind::Afp,
            ServiceKind::Smb,
            ServiceKind::Ssh,
            ServiceKind::Ftp,
            ServiceKind::Nfs,
            ServiceKind::Telnet,
            ServiceKind::Rlogin,
            ServiceKind::Rexec,
            ServiceKind::Rsh,
            ServiceKind::Http,
            ServiceKind::Gopher,
        ];
        let mut out = Vec::new();
        for k in ORDER {
            if let Some(f) = self
                .findings
                .iter()
                .find(|f| f.kind == k && !f.detail.starts_with(UNUSABLE))
            {
                out.push((k, f.port));
            }
        }
        out
    }

    /// One line per service, for a message or the CLI.
    pub fn lines(&self) -> Vec<String> {
        self.findings
            .iter()
            .map(|f| {
                if f.detail.is_empty() {
                    format!("{} (port {})", f.kind.label(), f.port)
                } else {
                    format!("{} (port {}): {}", f.kind.label(), f.port, f.detail)
                }
            })
            .collect()
    }
}

/// Which of `ports` accept a connection, all tried at once so closed ones cost no extra time.
fn listening(addr: SocketAddr, ports: &[u16]) -> std::collections::HashSet<u16> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut wanted: Vec<u16> = ports.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    for &port in &wanted {
        let tx = tx.clone();
        super::worker::spawn_detached("probing a port", move || {
            let _ = tx.send((port, open(addr, port).is_some()));
            Ok(())
        });
    }
    drop(tx);
    let mut up = std::collections::HashSet::new();
    let end = std::time::Instant::now() + CONNECT + Duration::from_millis(500);
    while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((port, true)) => {
                up.insert(port);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    up
}

fn open(addr: SocketAddr, port: u16) -> Option<TcpStream> {
    let mut a = addr;
    a.set_port(port);
    let s = TcpStream::connect_timeout(&a, CONNECT).ok()?;
    s.set_read_timeout(Some(READ)).ok()?;
    Some(s)
}

/// The first line a server sends on connecting (FTP, SSH), printable characters only.
fn greeting(s: TcpStream) -> String {
    let mut line = Vec::new();
    let _ = BufReader::new(s).read_until(b'\n', &mut line);
    String::from_utf8_lossy(&line)
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Where each service is looked for; the well-known ports, except in tests.
#[derive(Debug, Clone)]
pub struct Ports {
    pub ftp: u16,
    pub ssh: u16,
    pub telnet: u16,
    pub gopher: u16,
    pub http: u16,
    pub smb: [u16; 2],
    pub portmap: u16,
    pub rexec: u16,
    pub rlogin: u16,
    pub rsh: u16,
    pub afp: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Ports {
            ftp: 21,
            ssh: 22,
            telnet: 23,
            gopher: 70,
            http: 80,
            smb: [lc_smb::PORT, lc_smb::DIRECT_PORT],
            portmap: 111,
            rexec: 512,
            rlogin: 513,
            rsh: 514,
            afp: lc_afp::PORT,
        }
    }
}

/// Try `address`, reporting progress through `ctx` when given.
pub fn probe(address: &str, ctx: Option<&JobCtx>) -> anyhow::Result<Report> {
    probe_ports(address, &Ports::default(), ctx)
}

pub fn probe_ports(address: &str, ports: &Ports, ctx: Option<&JobCtx>) -> anyhow::Result<Report> {
    let addr = (address, 0)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no address for {address}"))?;
    let host = addr.ip().to_string();
    let phase = |what: &str| {
        if let Some(c) = ctx {
            c.set_phase(format!("Looking for {what} on {address}"));
        }
    };
    phase("services");
    let up = listening(
        addr,
        &[
            ports.ftp,
            ports.ssh,
            ports.telnet,
            ports.gopher,
            ports.http,
            ports.smb[0],
            ports.smb[1],
            ports.portmap,
            ports.rexec,
            ports.rlogin,
            ports.rsh,
            ports.afp,
        ],
    );
    let is_up = |p: u16| up.contains(&p);
    let mut found = Vec::new();
    let mut add = |kind, port, detail: String| {
        found.push(Finding { kind, port, detail });
    };

    phase("FTP");
    if let Some(s) = is_up(ports.ftp).then(|| open(addr, ports.ftp)).flatten() {
        let g = greeting(s);
        add(
            ServiceKind::Ftp,
            ports.ftp,
            g.trim_start_matches("220")
                .trim_start_matches('-')
                .trim()
                .to_string(),
        );
    }
    phase("SSH");
    if let Some(s) = is_up(ports.ssh).then(|| open(addr, ports.ssh)).flatten() {
        let g = greeting(s);
        let note = if lc_ssh::ssh1::is_ssh1(&g) {
            format!("{g} (SSH-1 only: 3DES, weak by today's measure)")
        } else {
            g
        };
        add(ServiceKind::Ssh, ports.ssh, note);
    }
    phase("Telnet");
    if is_up(ports.telnet) {
        add(ServiceKind::Telnet, ports.telnet, String::new());
    }
    phase("Gopher");
    if is_up(ports.gopher) {
        add(ServiceKind::Gopher, ports.gopher, String::new());
    }
    phase("a web server");
    if is_up(ports.http) {
        let opts = lc_http::Options {
            read_timeout: READ,
            ..lc_http::Options::default()
        };
        let detail = lc_http::http::connect(&host, ports.http, "", "", &opts)
            .map(|s| s.server.clone())
            .unwrap_or_default();
        add(ServiceKind::Http, ports.http, detail);
    }
    phase("SMB");
    let smb_opts = lc_smb::Options {
        timeout: READ,
        ..lc_smb::Options::default()
    };
    for port in ports.smb {
        if !is_up(port) {
            continue;
        }
        let detail = match lc_smb::probe(&host, port, &smb_opts) {
            Ok((server, n)) => format!(
                "{server}, {}, {}{}",
                n.dialect,
                if n.user_level {
                    "user-level"
                } else {
                    "share-level"
                },
                if n.domain.is_empty() {
                    String::new()
                } else {
                    format!(", {}", n.domain)
                }
            ),
            // Windows Vista and later turned SMB1 off: the server drops the SMB1 negotiate.
            Err(_) if port == lc_smb::DIRECT_PORT => {
                format!("{UNUSABLE}SMB2 or later only (legcon speaks SMB1; Windows Vista or later)")
            }
            Err(e) => format!("{UNUSABLE}negotiating failed: {e}"),
        };
        add(ServiceKind::Smb, port, detail);
        break;
    }
    phase("NFS");
    if is_up(ports.portmap) {
        let opts = lc_nfs::Options {
            timeout: READ,
            portmap_port: ports.portmap,
            ..lc_nfs::Options::default()
        };
        if let Ok(s) = lc_nfs::connect(&host, &opts) {
            let ex = s.exports();
            add(
                ServiceKind::Nfs,
                lc_nfs::PORT,
                format!("{}, exports {}", s.describe(), ex.join(", ")),
            );
        }
    }
    phase("rexec, rlogin and rsh");
    for (kind, port) in [
        (ServiceKind::Rexec, ports.rexec),
        (ServiceKind::Rlogin, ports.rlogin),
        (ServiceKind::Rsh, ports.rsh),
    ] {
        if is_up(port) {
            add(kind, port, String::new());
        }
    }
    phase("AFP");
    if is_up(ports.afp) {
        let opts = lc_afp::Options {
            timeout: READ,
            ..lc_afp::Options::default()
        };
        let detail = match lc_afp::server_info(&host, ports.afp, &opts) {
            Ok(i) => format!(
                "{} ({}), {}",
                i.name,
                i.machine,
                i.versions.last().cloned().unwrap_or_default()
            ),
            Err(e) => format!("{UNUSABLE}not as AFP: {e}"),
        };
        add(ServiceKind::Afp, ports.afp, detail);
    }

    let system = guess(&found);
    let smb1 = found
        .iter()
        .any(|f| f.kind == ServiceKind::Smb && !f.detail.starts_with(UNUSABLE));
    let charset = if found.iter().any(|f| f.kind == ServiceKind::Afp) {
        lc_core::Charset::MacRoman
    } else if smb1 && !system.as_deref().is_some_and(|s| s.contains("Samba")) {
        // SMB without Unicode names files in the OEM code page: 437 on US systems.
        lc_core::Charset::Cp437
    } else if system.as_deref().is_some_and(|s| s.starts_with("Windows")) {
        lc_core::Charset::Windows1252
    } else {
        lc_core::Charset::Latin1
    };
    Ok(Report {
        address: address.to_string(),
        findings: found,
        system,
        charset: charset.label().to_string(),
    })
}

/// Words in banners, and the system they name.
const UNIXES: [(&str, &str); 13] = [
    ("sunos", "SunOS"),
    ("solaris", "Solaris"),
    ("irix", "IRIX"),
    ("hp-ux", "HP-UX"),
    ("aix", "AIX"),
    ("nextstep", "NeXTSTEP"),
    ("ultrix", "Ultrix"),
    ("osf1", "Digital UNIX"),
    ("digital unix", "Digital UNIX"),
    ("freebsd", "FreeBSD"),
    ("netbsd", "NetBSD"),
    ("openbsd", "OpenBSD"),
    ("linux", "Linux"),
];

/// A guess at the system from what the services said.
fn guess(found: &[Finding]) -> Option<String> {
    let said: String = found
        .iter()
        .map(|f| f.detail.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let has = |k| found.iter().any(|f| f.kind == k);
    let smb = found.iter().find(|f| f.kind == ServiceKind::Smb);
    if smb.is_some_and(|f| f.detail.contains("SMB2 or later only")) && !said.contains("samba") {
        return Some("Windows Vista or later".into());
    }
    let smb = smb.filter(|f| !f.detail.starts_with(UNUSABLE));
    Some(
        if let Some(a) = found.iter().find(|f| f.kind == ServiceKind::Afp) {
            if a.detail.contains("Netatalk") || said.contains("netatalk") {
                "Unix with netatalk".into()
            } else {
                "Mac OS".into()
            }
        } else if said.contains("microsoft ftp") || said.contains("microsoft-iis") {
            "Windows NT (IIS)".into()
        } else if said.contains("samba") {
            "Unix with Samba".into()
        } else if let Some(s) = smb {
            if s.detail.contains("NT LM 0.12") {
                "Windows NT or later".into()
            } else if s.detail.contains("share-level") {
                "Windows for Workgroups / 95 / 98".into()
            } else {
                "OS/2 or LAN Manager".into()
            }
        } else if let Some((_, os)) = UNIXES.iter().find(|(key, _)| said.contains(key)) {
            os.to_string()
        } else if has(ServiceKind::Rsh) || has(ServiceKind::Rexec) || has(ServiceKind::Nfs) {
            "Unix".into()
        } else {
            return None;
        },
    )
}

/// Probe in a job, so the window keeps answering.
pub fn probe_job(address: &str, notify: Arc<dyn Notify>) -> Job<Report> {
    let address = address.to_string();
    Job::spawn(format!("Probe {address}"), notify, move |ctx| {
        probe(&address, Some(ctx))
    })
}

/// Write a report the way `lc probe` prints it.
pub fn print(r: &Report, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{}:", r.address)?;
    if r.findings.is_empty() {
        writeln!(out, "  nothing answered on the usual ports")?;
    }
    for l in r.lines() {
        writeln!(out, "  {l}")?;
    }
    if let Some(s) = &r.system {
        writeln!(out, "  looks like {s}; names in {}", r.charset)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(kind: ServiceKind, port: u16, detail: &str) -> Finding {
        Finding {
            kind,
            port,
            detail: detail.into(),
        }
    }

    #[test]
    fn guesses_and_service_order() {
        let nt = [
            f(ServiceKind::Ftp, 21, "Microsoft FTP Service"),
            f(
                ServiceKind::Smb,
                139,
                "NTBOX, NT LM 0.12, user-level, WORKGROUP",
            ),
        ];
        assert_eq!(guess(&nt).as_deref(), Some("Windows NT (IIS)"));
        let r = Report {
            findings: nt.to_vec(),
            ..Report::default()
        };
        assert_eq!(
            r.services(),
            [(ServiceKind::Smb, 139), (ServiceKind::Ftp, 21)]
        );
        assert_eq!(
            guess(&[f(
                ServiceKind::Afp,
                548,
                "Admin's Computer (Macintosh), AFP2.2"
            )])
            .as_deref(),
            Some("Mac OS")
        );
        assert_eq!(
            guess(&[f(ServiceKind::Ftp, 21, "sun FTP server (SunOS 5.8) ready.")]).as_deref(),
            Some("SunOS")
        );
        assert_eq!(guess(&[f(ServiceKind::Telnet, 23, "")]), None);
        // A modern Windows answers on 445 but drops SMB1: listed, not suggested.
        let win11 = [
            f(ServiceKind::Ssh, 22, "SSH-2.0-OpenSSH_for_Windows_9.5"),
            f(
                ServiceKind::Smb,
                445,
                "answered, but SMB2 or later only (legcon speaks SMB1; Windows Vista or later)",
            ),
        ];
        assert_eq!(guess(&win11).as_deref(), Some("Windows Vista or later"));
        let r = Report {
            findings: win11.to_vec(),
            ..Report::default()
        };
        assert_eq!(r.services(), [(ServiceKind::Ssh, 22)]);
    }
}
