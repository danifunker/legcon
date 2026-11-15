//! The session manager: one file connection per machine, shared by every view of it.
//!
//! Each call returns a [`Job`] so nothing blocks the caller; the connection itself sits behind
//! a mutex, so a listing and a disconnect on one machine queue rather than collide. Connecting
//! is always something the user asked for (PLAN.md §1.4): nothing here reconnects on its own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use lc_core::memory::MemoryFs;
use lc_core::{ConnState, Entry, FileSession, PathStyle, RemotePath};

use super::job::{Job, Notify};
use super::machines::{Machine, ServiceKind};
use super::worker::lock_status;

/// How long a clean log-off may take before the socket is closed anyway.
pub const LOGOFF_TIMEOUT: Duration = Duration::from_secs(5);

type Shared = Arc<Mutex<Box<dyn FileSession>>>;

#[derive(Default)]
struct Slot {
    state: ConnState,
    service: Option<ServiceKind>,
    session: Option<Shared>,
    /// The protocol trace, for protocols that keep one (FTP).
    trace: Option<Trace>,
    /// What the last SSH connect found out about the host key.
    key_note: super::ssh::KeySlot,
    /// Why the last rsh connect was refused.
    rsh_note: RshSlot,
}

/// Where a connect leaves why rsh refused it, for the view to offer a way on.
pub type RshSlot = Arc<Mutex<Option<lc_rcmd::rsh::Untrusted>>>;

/// A protocol's trace lines, as `> sent`, `< received` or `  note`.
type Trace = Arc<Mutex<Vec<lc_ftp::Trace>>>;

/// Every machine's file connection; cheap to clone and share.
#[derive(Clone, Default)]
pub struct Sessions {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self, machine: &str) -> ConnState {
        lock_status(&self.slots)
            .get(machine)
            .map(|s| s.state)
            .unwrap_or_default()
    }

    /// The service a machine's file connection uses, while connected.
    pub fn service(&self, machine: &str) -> Option<ServiceKind> {
        lock_status(&self.slots)
            .get(machine)
            .and_then(|s| s.service)
    }

    /// The file connection's protocol trace, newest last, for the Log tab.
    pub fn trace(&self, machine: &str) -> Vec<String> {
        let Some(t) = lock_status(&self.slots)
            .get(machine)
            .and_then(|s| s.trace.clone())
        else {
            return Vec::new();
        };
        let lines = lock_status(&t);
        lines
            .iter()
            .map(|l| match l {
                lc_ftp::Trace::Sent(s) => format!("> {s}"),
                lc_ftp::Trace::Received(s) => format!("< {s}"),
                lc_ftp::Trace::Note(s) => format!("  {s}"),
            })
            .collect()
    }

    /// Machines with a live connection, in no particular order.
    pub fn connected(&self) -> Vec<String> {
        lock_status(&self.slots)
            .iter()
            .filter(|(_, s)| s.session.is_some())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// What the last connect found out about the machine's SSH host key, once.
    pub fn take_key_note(&self, machine: &str) -> Option<super::ssh::KeyNote> {
        let slot = lock_status(&self.slots).get(machine)?.key_note.clone();
        let note = lock_status(&slot).take();
        note
    }

    /// Why the last rsh connect to the machine was refused, once.
    pub fn take_rsh_note(&self, machine: &str) -> Option<lc_rcmd::rsh::Untrusted> {
        let slot = lock_status(&self.slots).get(machine)?.rsh_note.clone();
        let note = lock_status(&slot).take();
        note
    }

    fn set_state(&self, machine: &str, state: ConnState) {
        lock_status(&self.slots)
            .entry(machine.to_string())
            .or_default()
            .state = state;
    }

    fn session(&self, machine: &str) -> anyhow::Result<Shared> {
        lock_status(&self.slots)
            .get(machine)
            .and_then(|s| s.session.clone())
            .ok_or_else(|| anyhow!("{machine} is not connected"))
    }

    /// Run `f` on a machine's file connection in a job; other work on it waits its turn.
    pub fn run<R: Send + 'static>(
        &self,
        machine: &str,
        label: String,
        notify: Arc<dyn Notify>,
        f: impl FnOnce(&mut dyn FileSession, &crate::model::job::JobCtx) -> anyhow::Result<R>
            + Send
            + 'static,
    ) -> Job<R> {
        let me = self.clone();
        let machine = machine.to_string();
        Job::spawn(label, notify, move |ctx| {
            let session = me.session(&machine)?;
            let mut guard = lock_status(&session);
            set_busy(&me, &machine, true);
            let r = f(guard.as_mut(), ctx);
            set_busy(&me, &machine, false);
            r
        })
    }

    /// Open the machine's file service; the job's result is the login folder.
    pub fn connect(
        &self,
        machine: &Machine,
        password: Option<String>,
        notify: Arc<dyn Notify>,
    ) -> Job<RemotePath> {
        let me = self.clone();
        let machine = machine.clone();
        me.set_state(&machine.id, ConnState::Connecting);
        let (note, rsh_note) = {
            let mut slots = lock_status(&me.slots);
            let slot = slots.entry(machine.id.clone()).or_default();
            (slot.key_note.clone(), slot.rsh_note.clone())
        };
        let label = format!("Connect to {}", machine.name);
        Job::spawn(label, notify, move |ctx| {
            let result = (|| {
                let service = machine
                    .file_service()
                    .ok_or_else(|| anyhow!("{} has no file service set up", machine.name))?;
                ctx.set_phase(format!(
                    "Connecting to {} ({})",
                    machine.address,
                    service.kind.label()
                ));
                let (mut session, trace) =
                    open_file_session(&machine, service.kind, password, &note, &rsh_note)?;
                let home = session.home()?;
                Ok::<_, anyhow::Error>((service.kind, session, home, trace))
            })();
            let mut slots = lock_status(&me.slots);
            let slot = slots.entry(machine.id.clone()).or_default();
            match result {
                Ok((kind, session, home, trace)) => {
                    slot.state = ConnState::Connected;
                    slot.service = Some(kind);
                    slot.session = Some(Arc::new(Mutex::new(session)));
                    slot.trace = trace;
                    Ok(home)
                }
                Err(e) => {
                    slot.state = ConnState::Disconnected;
                    Err(e)
                }
            }
        })
    }

    /// List a folder on a connected machine.
    pub fn list(&self, machine: &str, dir: RemotePath, notify: Arc<dyn Notify>) -> Job<Vec<Entry>> {
        let me = self.clone();
        let machine = machine.to_string();
        Job::spawn(format!("List {dir}"), notify, move |ctx| {
            let session = me.session(&machine)?;
            ctx.set_phase(format!("Listing {dir}"));
            let mut entries = lock_status(&session).list(&dir)?;
            entries.sort_by(|a, b| {
                b.is_folder()
                    .cmp(&a.is_folder())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            Ok(entries)
        })
    }

    /// Log off cleanly and forget the connection; a failed log-off still drops it.
    pub fn disconnect(&self, machine: &str, notify: Arc<dyn Notify>) -> Job<()> {
        let me = self.clone();
        let machine = machine.to_string();
        Job::spawn(format!("Disconnect {machine}"), notify, move |ctx| {
            let Ok(session) = me.session(&machine) else {
                me.set_state(&machine, ConnState::Disconnected);
                return Ok(());
            };
            me.set_state(&machine, ConnState::Disconnecting);
            ctx.set_phase("Logging off");
            let result = lock_status(&session).disconnect(LOGOFF_TIMEOUT);
            if let Some(slot) = lock_status(&me.slots).get_mut(&machine) {
                slot.state = ConnState::Disconnected;
                slot.service = None;
                slot.session = None;
            }
            result.map_err(|e| anyhow!("log-off was not clean: {e}"))
        })
    }
}

fn set_busy(s: &Sessions, machine: &str, busy: bool) {
    if let Some(slot) = lock_status(&s.slots).get_mut(machine) {
        if slot.session.is_some() {
            slot.state = if busy {
                ConnState::Busy
            } else {
                ConnState::Connected
            };
        }
    }
}

/// Open `kind` on `machine`; each protocol crate plugs in here as its phase lands.
fn open_file_session(
    machine: &Machine,
    kind: ServiceKind,
    password: Option<String>,
    note: &super::ssh::KeySlot,
    rsh_note: &RshSlot,
) -> anyhow::Result<(Box<dyn FileSession>, Option<Trace>)> {
    match kind {
        ServiceKind::Demo => Ok((Box::new(demo_machine()), None)),
        ServiceKind::Ftp => {
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == ServiceKind::Ftp)
                .map_or(lc_ftp::PORT, |s| s.port);
            let opts = lc_ftp::Options {
                charset: lc_core::Charset::from_label(&machine.charset).unwrap_or_default(),
                ..lc_ftp::Options::default()
            };
            let pw = password.unwrap_or_default();
            let session = lc_ftp::connect(&machine.address, port, &machine.user, &pw, &opts)?;
            let trace = session.trace();
            Ok((Box::new(session), Some(trace)))
        }
        ServiceKind::Smb => {
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == ServiceKind::Smb)
                .map_or(lc_smb::PORT, |s| s.port);
            let opts = lc_smb::Options {
                charset: lc_core::Charset::from_label(&machine.charset)
                    .unwrap_or(lc_core::Charset::Windows1252),
                ..lc_smb::Options::default()
            };
            let pw = password.unwrap_or_default();
            let session = lc_smb::connect(&machine.address, port, &machine.user, &pw, &opts)?;
            Ok((Box::new(session), None))
        }
        ServiceKind::Ssh => {
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == ServiceKind::Ssh)
                .map_or(lc_ssh::PORT, |s| s.port);
            Ok((super::ssh::open_files(machine, port, password, note)?, None))
        }
        ServiceKind::Nfs => {
            // NFS is found through the portmapper (111); another port names a portmapper there.
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == ServiceKind::Nfs)
                .map_or(lc_nfs::PORT, |s| s.port);
            let opts = lc_nfs::Options {
                charset: lc_core::Charset::from_label(&machine.charset).unwrap_or_default(),
                uid: machine.uid.unwrap_or(65534),
                gid: machine.gid.unwrap_or(65534),
                portmap_port: if port == lc_nfs::PORT { 111 } else { port },
                version: machine.nfs_version,
                ..lc_nfs::Options::default()
            };
            let session = lc_nfs::connect(&machine.address, &opts)?;
            log::info!(
                "{}: {}, exports {:?}",
                machine.name,
                session.describe(),
                session.exports()
            );
            Ok((Box::new(session), None))
        }
        ServiceKind::Rsh => {
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == ServiceKind::Rsh)
                .map_or(lc_rcmd::rsh::RSH_PORT, |s| s.port);
            let checked = lc_rcmd::rsh::Rsh::check(
                &machine.address,
                port,
                machine.local_user.as_deref().unwrap_or(&machine.user),
                &machine.user,
                lc_core::Charset::from_label(&machine.charset).unwrap_or_default(),
                &lc_rcmd::Options::default(),
            )?;
            match checked {
                Ok(session) => Ok((Box::new(session), None)),
                Err(u) => {
                    let text = format!("rsh: {u}");
                    *lock_status(rsh_note) = Some(u);
                    bail!(text)
                }
            }
        }
        ServiceKind::Afp => {
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == ServiceKind::Afp)
                .map_or(lc_afp::PORT, |s| s.port);
            let opts = lc_afp::Options {
                charset: lc_core::Charset::from_label(&machine.charset)
                    .unwrap_or(lc_core::Charset::MacRoman),
                ..lc_afp::Options::default()
            };
            let pw = password.unwrap_or_default();
            let s = lc_afp::connect(&machine.address, port, &machine.user, &pw, &opts)?;
            log::info!(
                "{}: {} ({}), {}, {}",
                machine.name,
                s.info.name,
                s.info.machine,
                s.version,
                s.uam
            );
            Ok((Box::new(s), None))
        }
        ServiceKind::Http | ServiceKind::Gopher => {
            let port = machine
                .services
                .iter()
                .find(|s| s.kind == kind)
                .map_or(kind.default_port(), |s| s.port);
            let opts = lc_http::Options {
                charset: lc_core::Charset::from_label(&machine.charset).unwrap_or_default(),
                ..lc_http::Options::default()
            };
            if kind == ServiceKind::Gopher {
                return Ok((
                    Box::new(lc_http::gopher::connect(&machine.address, port, &opts)?),
                    None,
                ));
            }
            let pw = password.unwrap_or_default();
            let s = lc_http::http::connect(&machine.address, port, &machine.user, &pw, &opts)?;
            log::info!("{}: {}", machine.name, s.server);
            Ok((Box::new(s), None))
        }
        ServiceKind::Telnet | ServiceKind::Rexec | ServiceKind::Rlogin => {
            bail!("{} is a command service, not a file service", kind.label())
        }
    }
}

/// The built-in demo machine: a small SunOS-like tree in memory.
pub fn demo_machine() -> MemoryFs {
    let fs = MemoryFs::new(PathStyle::Unix, "/export/home/dani");
    fs.add_file(
        "/export/home/dani/.profile",
        b"PATH=/usr/local/bin:$PATH\nexport PATH\n",
    );
    fs.add_file(
        "/export/home/dani/src/Makefile.local",
        b"CC = gcc\nCFLAGS = -O2 -g\nPREFIX = /usr/local\n",
    );
    fs.add_file(
        "/export/home/dani/src/bash-2.05b.tar.gz",
        &[0x1f, 0x8b, 8, 0],
    );
    fs.add_folder("/export/home/dani/src/gcc-2.95.3");
    fs.add_folder("/export/home/dani/bin");
    fs.add_file("/etc/motd", b"Sun Microsystems Inc.   SunOS 5.8\n");
    fs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::job::NoNotify;
    use crate::model::machines::Service;

    fn demo() -> Machine {
        let mut m = Machine::new("sparc", "demo");
        m.services.push(Service {
            kind: ServiceKind::Demo,
            port: 0,
        });
        m
    }

    #[test]
    fn connect_list_disconnect() {
        let s = Sessions::new();
        let quiet: Arc<dyn Notify> = Arc::new(NoNotify);
        let job = s.connect(&demo(), None, quiet.clone());
        job.wait();
        let home = job.take_result().unwrap();
        assert_eq!(home.to_string(), "/export/home/dani");
        assert_eq!(s.state("sparc"), ConnState::Connected);
        assert_eq!(s.connected(), ["sparc"]);
        let job = s.list("sparc", home, quiet.clone());
        job.wait();
        let names: Vec<_> = job
            .take_result()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["bin", "src", ".profile"]);
        s.disconnect("sparc", quiet).wait();
        assert_eq!(s.state("sparc"), ConnState::Disconnected);
        assert!(s.connected().is_empty());
    }

    #[test]
    fn a_machine_without_a_file_service_fails_and_stays_disconnected() {
        let s = Sessions::new();
        let mut m = Machine::new("indy", "192.168.1.42");
        m.services.push(Service {
            kind: ServiceKind::Telnet,
            port: 23,
        });
        let job = s.connect(&m, None, Arc::new(NoNotify));
        let snap = job.wait();
        assert!(
            matches!(snap.status, crate::model::job::Status::Failed(ref e) if e.contains("no file service"))
        );
        assert_eq!(s.state("indy"), ConnState::Disconnected);
    }
}
