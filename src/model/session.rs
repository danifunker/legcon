//! The session manager: one file connection per machine, shared by every view of it.
//!
//! Each call returns a [`Job`] so nothing blocks the caller; the connection itself sits behind
//! a mutex, so a listing and a disconnect on one machine queue rather than collide. Connecting
//! is always something the user did (PLAN.md §1.4, D48): the controller reconnects when the
//! user works with a machine whose link was lost or logged off while idle, never on a timer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use lc_core::memory::MemoryFs;
use lc_core::{ConnState, Entry, FileSession, PathStyle, RemotePath};

use super::job::{Job, Notify};
use super::machines::{Machine, ServiceKind};
use super::worker::lock_status;

/// How long a clean log-off may take before the socket is closed anyway.
pub const LOGOFF_TIMEOUT: Duration = Duration::from_secs(5);

type Shared = Arc<Mutex<Box<dyn FileSession>>>;

/// A connection quiet this long is asked whether it is still there before it is used.
pub const CHECK_AFTER: Duration = Duration::from_secs(30);
/// How long a ping may take before the link counts as gone.
pub const PING_TIMEOUT: Duration = Duration::from_secs(8);
/// How often the watcher looks at a connection.
const WATCH_STEP: Duration = Duration::from_millis(500);

/// What keeps a connection up or lets it go while nobody uses it (Options, per machine).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Upkeep {
    /// Ping the server this often while idle, so it does not drop the link.
    pub keepalive: Option<Duration>,
    /// Log off cleanly after this long unused; using the machine connects again.
    pub idle_logoff: Option<Duration>,
}

impl Upkeep {
    fn watched(&self) -> bool {
        self.keepalive.is_some() || self.idle_logoff.is_some()
    }
}

#[derive(Default)]
struct Slot {
    state: ConnState,
    service: Option<ServiceKind>,
    session: Option<Shared>,
    /// Closes the socket from outside the session's lock (a call stuck on a dead server).
    breaker: Option<lc_core::net::Breaker>,
    /// Why the connection is Lost or Idle, for the header and the log.
    why: Option<String>,
    /// When work last started or ended on it (idle log-off counts from here).
    used: Option<Instant>,
    /// When the server last answered (a check before use counts from here).
    heard: Option<Instant>,
    /// Work is waiting for the connection (a running queue): no idle log-off.
    hold: bool,
    upkeep: Upkeep,
    /// The keepalive and idle watcher, while connected.
    watch: Option<Job<()>>,
    /// Wakes the view when the watcher changes the state; kept to start a watcher later.
    wake: Option<Arc<dyn Notify>>,
    /// The protocol trace, for protocols that keep one (FTP).
    trace: Option<Trace>,
    /// What the last SSH connect found out about the host key.
    key_note: super::ssh::KeySlot,
    /// Why the last rsh connect was refused.
    rsh_note: RshSlot,
}

impl Slot {
    /// Forget the connection, closing its socket; `state` and `why` say how it ended.
    fn end(&mut self, state: ConnState, why: Option<String>) {
        self.state = state;
        self.why = why;
        self.service = if state == ConnState::Disconnected {
            None
        } else {
            self.service
        };
        self.session = None;
        if let Some(w) = self.watch.take() {
            w.cancel();
        }
        if let Some(b) = self.breaker.take() {
            if state == ConnState::Lost {
                b.trip();
            }
            b.release();
        }
    }
}

/// Where a connect leaves why rsh refused it, for the view to offer a way on.
pub type RshSlot = Arc<Mutex<Option<lc_rcmd::rsh::Untrusted>>>;

/// A protocol's trace lines, as `> sent`, `< received` or `  note`.
type Trace = Arc<Mutex<Vec<lc_ftp::Trace>>>;

/// Whether an error may mean the link is gone (worth a ping to find out).
pub fn suspect(e: &anyhow::Error) -> bool {
    lc_core::is_link_lost(e.as_ref()) || lc_core::is_timeout(e.as_ref())
}

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

    /// Why a machine's connection is Lost or Idle.
    pub fn why(&self, machine: &str) -> Option<String> {
        lock_status(&self.slots)
            .get(machine)
            .and_then(|s| s.why.clone())
    }

    /// The service a machine's file connection uses, while connected.
    pub fn service(&self, machine: &str) -> Option<ServiceKind> {
        lock_status(&self.slots)
            .get(machine)
            .and_then(|s| s.service)
    }

    /// How the machine's connection is kept up or let go; read by the watcher as it runs.
    pub fn set_upkeep(&self, machine: &str, upkeep: Upkeep) {
        let mut slots = lock_status(&self.slots);
        let slot = slots.entry(machine.to_string()).or_default();
        slot.upkeep = upkeep;
        // Turned on while connected: start watching now, not at the next connect.
        if let (true, None, Some(session), Some(wake)) = (
            upkeep.watched(),
            &slot.watch,
            slot.session.clone(),
            slot.wake.clone(),
        ) {
            slot.watch = Some(self.watch(machine, &session, wake));
        }
    }

    /// Work is waiting for the machine (a running queue), so it is not logged off for idling.
    pub fn hold(&self, machine: &str, hold: bool) {
        if let Some(slot) = lock_status(&self.slots).get_mut(machine) {
            slot.hold = hold;
        }
    }

    /// After the computer slept: ask every connection whether it is still there before using it.
    pub fn woke(&self) {
        for slot in lock_status(&self.slots).values_mut() {
            slot.heard = None;
        }
    }

    /// A machine's trace, kept across connections (and failed ones) for the Log tab.
    pub fn trace_log(&self, machine: &str) -> lc_core::TraceLog {
        let mut slots = lock_status(&self.slots);
        let slot = slots.entry(machine.to_string()).or_default();
        slot.trace.get_or_insert_with(Default::default).clone()
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
        let slots = lock_status(&self.slots);
        let slot = slots.get(machine);
        if let Some(s) = slot.and_then(|s| s.session.clone()) {
            return Ok(s);
        }
        match slot.map(|s| (s.state, s.why.clone())) {
            Some((ConnState::Lost | ConnState::Idle, Some(why))) => bail!("{machine}: {why}"),
            _ => Err(anyhow!("{machine} is not connected")),
        }
    }

    /// Run `f` with this slot, if it still holds `session` (not a newer connection).
    fn with_slot(&self, machine: &str, session: &Shared, f: impl FnOnce(&mut Slot)) {
        let mut slots = lock_status(&self.slots);
        if let Some(slot) = slots.get_mut(machine) {
            if slot
                .session
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, session))
            {
                f(slot);
            }
        }
    }

    fn touch(&self, machine: &str, session: &Shared, answered: bool) {
        self.with_slot(machine, session, |slot| {
            let now = Instant::now();
            slot.used = Some(now);
            if answered {
                slot.heard = Some(now);
            }
        });
    }

    /// The link is gone: forget it without a log-off, and say why.
    fn lost(&self, machine: &str, session: &Shared, e: &dyn std::fmt::Display) {
        let why = format!("The connection was lost: {e}");
        log::warn!("{machine}: {why}");
        // A log-off under way (Disconnect, sleep) settles the state itself.
        self.with_slot(machine, session, |slot| {
            if slot.state != ConnState::Disconnecting {
                slot.end(ConnState::Lost, Some(why))
            }
        });
    }

    /// Whether the slot still holds `session`: an idle log-off or a lost link may have ended it.
    fn current(&self, machine: &str, session: &Shared) -> anyhow::Result<()> {
        let slots = lock_status(&self.slots);
        let slot = slots.get(machine);
        if slot
            .and_then(|s| s.session.as_ref())
            .is_some_and(|s| Arc::ptr_eq(s, session))
        {
            return Ok(());
        }
        let why = slot.and_then(|s| s.why.clone());
        bail!(
            "{machine}: {}",
            why.unwrap_or_else(|| "not connected".into())
        )
    }

    /// Run `f` on a machine's file connection in a job; other work on it waits its turn.
    /// A quiet connection is pinged first, a doubtful error is checked with a ping (D48).
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
            me.current(&machine, &session)?;
            me.touch(&machine, &session, false);
            let stale = lock_status(&me.slots)
                .get(&machine)
                .and_then(|s| s.heard)
                .is_none_or(|t| t.elapsed() >= CHECK_AFTER);
            if stale {
                ctx.set_phase("Checking the connection");
                if let Some(e) = dead(guard.as_mut()) {
                    me.lost(&machine, &session, &e);
                    bail!("{machine}: the connection was lost ({e})");
                }
                me.touch(&machine, &session, true);
            }
            set_busy(&me, &machine, true);
            let r = f(guard.as_mut(), ctx);
            set_busy(&me, &machine, false);
            match &r {
                Ok(_) => me.touch(&machine, &session, true),
                Err(e) if suspect(e) => {
                    // A reply that never came, or a socket error: is the server still there?
                    let gone = guard.state() == ConnState::Lost || dead(guard.as_mut()).is_some();
                    if gone {
                        me.lost(&machine, &session, &format!("{e:#}"));
                    } else {
                        me.touch(&machine, &session, true);
                    }
                }
                Err(_) => me.touch(&machine, &session, false),
            }
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
        // A reconnect that fails leaves the machine as it was, so using it tries again.
        let before = me.state(&machine.id);
        me.set_state(&machine.id, ConnState::Connecting);
        let (note, rsh_note) = {
            let mut slots = lock_status(&me.slots);
            let slot = slots.entry(machine.id.clone()).or_default();
            (slot.key_note.clone(), slot.rsh_note.clone())
        };
        let log = me.trace_log(&machine.id);
        let label = format!("Connect to {}", machine.name);
        let wake = Arc::clone(&notify);
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
                    open_file_session(&machine, service.kind, password, &note, &rsh_note, &log)?;
                let home = session.home()?;
                Ok::<_, anyhow::Error>((service.kind, session, home, trace))
            })();
            match result {
                Ok((kind, session, home, trace)) => {
                    me.install(&machine.id, kind, session, trace, wake);
                    Ok(home)
                }
                Err(e) => {
                    if let Some(slot) = lock_status(&me.slots).get_mut(&machine.id) {
                        slot.state = match before {
                            ConnState::Lost | ConnState::Idle => before,
                            _ => ConnState::Disconnected,
                        };
                        if slot.state != ConnState::Disconnected {
                            slot.why = Some(format!("Could not connect again: {e:#}"));
                        }
                    }
                    Err(e)
                }
            }
        })
    }

    /// Take a logged-in session as the machine's connection.
    fn install(
        &self,
        machine: &str,
        kind: ServiceKind,
        session: Box<dyn FileSession>,
        trace: Option<Trace>,
        wake: Arc<dyn Notify>,
    ) {
        let mut slots = lock_status(&self.slots);
        let slot = slots.entry(machine.to_string()).or_default();
        let now = Instant::now();
        slot.state = ConnState::Connected;
        slot.why = None;
        slot.service = Some(kind);
        slot.breaker = session.breaker();
        let shared = Arc::new(Mutex::new(session));
        slot.session = Some(Arc::clone(&shared));
        (slot.used, slot.heard) = (Some(now), Some(now));
        // FTP keeps its own trace; the others write to the machine's.
        if trace.is_some() {
            slot.trace = trace;
        }
        if let Some(w) = slot.watch.take() {
            w.cancel();
        }
        slot.wake = Some(Arc::clone(&wake));
        if slot.upkeep.watched() {
            slot.watch = Some(self.watch(machine, &shared, wake));
        }
    }

    /// For tests: `session` as the machine's connection, as if a connect had opened it.
    #[cfg(test)]
    pub(crate) fn adopt(
        &self,
        machine: &str,
        session: Box<dyn FileSession>,
        wake: Arc<dyn Notify>,
    ) {
        self.install(machine, ServiceKind::Demo, session, None, wake);
    }

    /// For tests: make the last answer from the server look `ago` old.
    #[cfg(test)]
    pub(crate) fn age(&self, machine: &str, ago: Duration) {
        if let Some(slot) = lock_status(&self.slots).get_mut(machine) {
            let then = Instant::now().checked_sub(ago);
            slot.heard = then;
            slot.used = then;
        }
    }

    /// Keep the connection up while idle (keepalive), or log it off after its idle time.
    fn watch(&self, machine: &str, session: &Shared, notify: Arc<dyn Notify>) -> Job<()> {
        let me = self.clone();
        let machine = machine.to_string();
        let weak = Arc::downgrade(session);
        let wake = Arc::clone(&notify);
        Job::spawn(format!("Watch {machine}"), notify, move |ctx| loop {
            std::thread::sleep(WATCH_STEP);
            let Some(session) = weak.upgrade() else {
                return Ok(());
            };
            if ctx.is_cancelled() {
                return Ok(());
            }
            let look = {
                let slots = lock_status(&me.slots);
                slots
                    .get(&machine)
                    .filter(|s| s.session.as_ref().is_some_and(|x| Arc::ptr_eq(x, &session)))
                    .map(|s| (s.upkeep, s.used, s.heard, s.hold))
            };
            let Some((upkeep, used, heard, hold)) = look else {
                return Ok(());
            };
            let mut guard = match session.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                // Work is running on it: that is use, not idling.
                Err(std::sync::TryLockError::WouldBlock) => {
                    me.touch(&machine, &session, false);
                    continue;
                }
            };
            let idle = used.map_or(Duration::ZERO, |t| t.elapsed());
            if let Some(limit) = upkeep.idle_logoff.filter(|l| !hold && idle >= *l) {
                let r = logoff(guard.as_mut(), me.breaker(&machine));
                drop(guard);
                let why = format!("Logged off after {} unused.", minutes(limit));
                log::info!("{machine}: {why}");
                if let Err(e) = r {
                    log::warn!("{machine}: the log-off was not clean: {e}");
                }
                me.with_slot(&machine, &session, |slot| {
                    slot.watch = None;
                    slot.end(ConnState::Idle, Some(why))
                });
                wake.wake();
                return Ok(());
            }
            let quiet = heard.map_or(Duration::MAX, |t| t.elapsed());
            if let Some(every) = upkeep.keepalive.filter(|e| quiet >= *e) {
                match dead(guard.as_mut()).map_or(Ok(()), Err) {
                    Ok(()) => {
                        drop(guard);
                        me.with_slot(&machine, &session, |s| s.heard = Some(Instant::now()));
                        log::debug!("{machine}: keepalive after {every:?}");
                    }
                    Err(e) => {
                        drop(guard);
                        me.with_slot(&machine, &session, |slot| slot.watch = None);
                        me.lost(&machine, &session, &e);
                        wake.wake();
                        return Ok(());
                    }
                }
            }
        })
    }

    fn breaker(&self, machine: &str) -> Option<lc_core::net::Breaker> {
        lock_status(&self.slots)
            .get(machine)
            .and_then(|s| s.breaker.clone())
    }

    /// List a folder on a connected machine.
    pub fn list(&self, machine: &str, dir: RemotePath, notify: Arc<dyn Notify>) -> Job<Vec<Entry>> {
        self.run(machine, format!("List {dir}"), notify, move |fs, ctx| {
            ctx.set_phase(format!("Listing {dir}"));
            let mut entries = fs.list(&dir)?;
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
        self.log_off(machine, notify, ConnState::Disconnected, None)
    }

    /// Log off for the computer's sleep; the machine is `Idle`, so using it connects again.
    pub fn suspend(&self, machine: &str, notify: Arc<dyn Notify>) -> Job<()> {
        let why = "Logged off while this computer slept.".to_string();
        self.log_off(machine, notify, ConnState::Idle, Some(why))
    }

    fn log_off(
        &self,
        machine: &str,
        notify: Arc<dyn Notify>,
        end: ConnState,
        why: Option<String>,
    ) -> Job<()> {
        let me = self.clone();
        let machine = machine.to_string();
        Job::spawn(format!("Disconnect {machine}"), notify, move |ctx| {
            let Ok(session) = me.session(&machine) else {
                // Lost or Idle already: nothing to log off, only the state to settle.
                if let Some(slot) = lock_status(&me.slots).get_mut(&machine) {
                    if slot.state != ConnState::Idle || end == ConnState::Disconnected {
                        slot.end(end, why);
                    }
                }
                return Ok(());
            };
            let breaker = {
                let mut slots = lock_status(&me.slots);
                let slot = slots.entry(machine.clone()).or_default();
                if let Some(w) = slot.watch.take() {
                    w.cancel();
                }
                slot.state = ConnState::Disconnecting;
                slot.breaker.clone()
            };
            ctx.set_phase("Logging off");
            // A call stuck on a dead server holds the lock for its whole timeout: break it, and
            // where that cannot wake it (Windows), leave it to end on its own, without a log-off.
            let got = lock_within(&session, LOGOFF_TIMEOUT).or_else(|| {
                if let Some(b) = &breaker {
                    b.trip();
                }
                lock_within(&session, Duration::from_secs(1))
            });
            let Some(mut guard) = got else {
                if let Some(slot) = lock_status(&me.slots).get_mut(&machine) {
                    slot.end(end, why);
                }
                bail!("{machine} did not answer; closed without logging off");
            };
            let result = logoff(guard.as_mut(), breaker);
            drop(guard);
            if let Some(slot) = lock_status(&me.slots).get_mut(&machine) {
                slot.end(end, why);
            }
            result.map_err(|e| anyhow!("log-off was not clean: {e}"))
        })
    }
}

/// Ping, and say why if the link is gone; a server that answers with a refusal is still there.
fn dead(s: &mut dyn FileSession) -> Option<lc_core::Error> {
    match s.ping(PING_TIMEOUT) {
        Ok(()) => None,
        Err(e) if lc_core::is_link_lost(&e) || lc_core::is_timeout(&e) => Some(e),
        Err(e) => {
            log::debug!("ping answered with an error, so the server is there: {e}");
            None
        }
    }
}

/// The session's clean log-off, cut short by its breaker if it takes too long.
fn logoff(s: &mut dyn FileSession, breaker: Option<lc_core::net::Breaker>) -> lc_core::Result<()> {
    if s.state() == ConnState::Lost {
        return Ok(());
    }
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(b) = breaker {
        let done = Arc::clone(&done);
        super::worker::spawn_detached("log-off watchdog", move || {
            let end = Instant::now() + LOGOFF_TIMEOUT;
            while Instant::now() < end {
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !done.load(std::sync::atomic::Ordering::Relaxed) {
                b.trip();
            }
            Ok(())
        });
    }
    let r = s.disconnect(LOGOFF_TIMEOUT);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    r
}

/// The lock, if it comes free within `wait`.
fn lock_within(
    m: &Shared,
    wait: Duration,
) -> Option<std::sync::MutexGuard<'_, Box<dyn FileSession>>> {
    let end = Instant::now() + wait;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= end => return None,
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(50))
            }
        }
    }
}

/// `5 minutes`, `1 minute`, `30 seconds`.
pub fn minutes(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 60, s % 60) {
        (1, 0) => "1 minute".into(),
        (m, 0) => format!("{m} minutes"),
        _ => format!("{s} seconds"),
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
    log: &lc_core::TraceLog,
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
            Ok((
                super::ssh::open_files(machine, port, password, note, Some(log))?,
                None,
            ))
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

    fn lab(s: &Sessions) -> MemoryFs {
        let fs = demo_machine();
        s.adopt("sparc", Box::new(fs.clone()), Arc::new(NoNotify));
        fs
    }

    fn list_home(s: &Sessions) -> crate::model::job::Status {
        let dir = RemotePath::parse(PathStyle::Unix, "/export/home/dani");
        s.list("sparc", dir, Arc::new(NoNotify)).wait().status
    }

    #[test]
    fn a_dropped_link_leaves_the_machine_lost_with_why() {
        let s = Sessions::new();
        let fs = lab(&s);
        assert_eq!(list_home(&s), crate::model::job::Status::Done);
        fs.dropped.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            list_home(&s),
            crate::model::job::Status::Failed(_)
        ));
        assert_eq!(s.state("sparc"), ConnState::Lost);
        assert!(s.why("sparc").unwrap().contains("lost"));
        assert!(s.connected().is_empty());
        // Disconnect from Lost settles it, with nothing to log off.
        s.disconnect("sparc", Arc::new(NoNotify)).wait();
        assert_eq!(s.state("sparc"), ConnState::Disconnected);
        assert!(!*fs.logged_off.lock().unwrap());
    }

    #[test]
    fn a_quiet_connection_is_checked_before_use() {
        let s = Sessions::new();
        let fs = lab(&s);
        let pings = || fs.pings.load(std::sync::atomic::Ordering::Relaxed);
        list_home(&s);
        assert_eq!(pings(), 0, "just connected: no check");
        s.age("sparc", CHECK_AFTER);
        list_home(&s);
        assert_eq!(pings(), 1);
        s.woke();
        list_home(&s);
        assert_eq!(pings(), 2, "after sleep every connection is checked");
    }

    #[test]
    fn idle_connections_log_off_and_keepalive_pings() {
        let s = Sessions::new();
        s.set_upkeep(
            "sparc",
            Upkeep {
                keepalive: Some(Duration::from_millis(1)),
                idle_logoff: None,
            },
        );
        let fs = lab(&s);
        let t = Instant::now();
        while fs.pings.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(t.elapsed() < Duration::from_secs(5), "no keepalive");
            std::thread::sleep(Duration::from_millis(20));
        }
        s.set_upkeep(
            "sparc",
            Upkeep {
                keepalive: None,
                idle_logoff: Some(Duration::from_millis(1)),
            },
        );
        s.hold("sparc", true);
        std::thread::sleep(WATCH_STEP * 3);
        assert_eq!(s.state("sparc"), ConnState::Connected, "held by the queue");
        s.hold("sparc", false);
        let t = Instant::now();
        while s.state("sparc") != ConnState::Idle {
            assert!(t.elapsed() < Duration::from_secs(5), "no idle log-off");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(*fs.logged_off.lock().unwrap(), "logged off cleanly");
        assert!(s.why("sparc").unwrap().starts_with("Logged off after"));
    }

    #[test]
    fn a_keepalive_finds_a_dead_link() {
        let s = Sessions::new();
        s.set_upkeep(
            "sparc",
            Upkeep {
                keepalive: Some(Duration::from_millis(1)),
                idle_logoff: None,
            },
        );
        let fs = lab(&s);
        fs.dropped.store(true, std::sync::atomic::Ordering::Relaxed);
        let t = Instant::now();
        while s.state("sparc") != ConnState::Lost {
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_failed_reconnect_stays_lost_and_keepalive_starts_while_connected() {
        let s = Sessions::new();
        let fs = lab(&s);
        // Keepalive turned on in Options while connected takes effect at once.
        s.set_upkeep(
            "sparc",
            Upkeep {
                keepalive: Some(Duration::from_millis(1)),
                idle_logoff: None,
            },
        );
        let t = Instant::now();
        while fs.pings.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(t.elapsed() < Duration::from_secs(5), "no keepalive");
            std::thread::sleep(Duration::from_millis(20));
        }
        s.set_upkeep("sparc", Upkeep::default());
        fs.dropped.store(true, std::sync::atomic::Ordering::Relaxed);
        list_home(&s);
        assert_eq!(s.state("sparc"), ConnState::Lost);
        // The server is still away: the machine stays Lost, so using it tries again.
        let mut m = Machine::new("sparc", "demo");
        m.services.clear();
        s.connect(&m, None, Arc::new(NoNotify)).wait();
        assert_eq!(s.state("sparc"), ConnState::Lost);
        assert!(s
            .why("sparc")
            .unwrap()
            .starts_with("Could not connect again"));
    }

    #[test]
    fn a_call_stuck_on_a_dead_server_does_not_hold_up_disconnect() {
        let s = Sessions::new();
        let fs = lab(&s);
        fs.stall.store(true, std::sync::atomic::Ordering::Relaxed);
        let _stuck = s.list(
            "sparc",
            RemotePath::parse(PathStyle::Unix, "/"),
            Arc::new(NoNotify),
        );
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        let snap = s.disconnect("sparc", Arc::new(NoNotify)).wait();
        assert!(t.elapsed() < LOGOFF_TIMEOUT + Duration::from_secs(3));
        assert!(
            matches!(snap.status, crate::model::job::Status::Failed(ref e) if e.contains("did not answer"))
        );
        assert_eq!(s.state("sparc"), ConnState::Disconnected);
        fs.stall.store(false, std::sync::atomic::Ordering::Relaxed);
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
