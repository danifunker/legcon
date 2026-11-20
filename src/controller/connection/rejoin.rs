//! Connecting again (PLAN.md §1.4, D48): a server that dropped an idle link, or legcon's own
//! idle or sleep log-off, leaves the machine `Lost` or `Idle`. Working with it again (a folder
//! opened, a refresh, a copy) is taken as wanting it back: legcon connects again, returns to
//! the folder that was open and carries on with what was asked, or asks first (Options, per
//! machine). Unfinished transfers go on, or are asked about, once the connection is back.

use super::{Action, Effect};
use super::{ConnState, ConnectionController, Pending, QueueOp, Side, TermTab};
use crate::controller::ChoiceSpec;
use crate::model::queue::{Id, State};
use crate::model::session::Upkeep;
use crate::model::settings::Auto;
use lc_core::RemotePath;
use std::time::Duration;

/// The app-wide connection choices (Options); each machine may override them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkPrefs {
    pub reconnect: Auto,
    pub resume_queue: Auto,
    /// 0: no keepalive.
    pub keepalive_secs: u64,
    /// 0: never logged off for idling.
    pub idle_logoff_mins: u64,
}

impl Default for LinkPrefs {
    fn default() -> Self {
        let s = crate::model::settings::Settings::default();
        LinkPrefs {
            reconnect: s.reconnect,
            resume_queue: s.resume_queue,
            keepalive_secs: s.keepalive_secs,
            idle_logoff_mins: s.idle_logoff_mins,
        }
    }
}

pub const REJOIN_PURPOSE: &str = "conn.rejoin";

/// The time of day for the Console and Shell lines that say when a link went and came back.
pub(super) fn clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
pub const RESUME_PURPOSE: &str = "conn.queue.resume";

/// How many reconnects may follow one another with nothing working in between.
const REJOINS_MAX: u8 = 2;

/// A connect that picks up where the lost connection was.
pub(super) struct Rejoin {
    /// The remote folder that was open.
    dir: Option<RemotePath>,
    /// What the user did that needed the connection, done once it is back.
    then: Option<Replay>,
}

/// An action to do again, with the question it answered (Rename..., New folder..., Delete).
pub(super) struct Replay {
    action: Action,
    pending: Option<Pending>,
}

/// What a "connect again?" question is about.
pub(super) enum RejoinFor {
    Files(Option<Replay>),
    Shell,
}

/// The lost-link state of a tab, kept apart from the rest of the controller.
#[derive(Default)]
pub(super) struct Link {
    pub prefs: LinkPrefs,
    pub rejoin: Option<Rejoin>,
    pub asking: Option<RejoinFor>,
    /// The last action that worked with the remote side, to do again after a reconnect.
    pub last: Option<Replay>,
    /// Reconnects in a row with nothing working in between; stops a loop on a broken server.
    pub in_a_row: u8,
    /// The action now running was done again after a reconnect: if it fails too, stop there.
    pub replayed: bool,
    /// The file connection's state at the last tick, to see it drop.
    pub was: ConnState,
    /// Why the file connection is Lost or Idle, shown in the remote pane.
    pub note: Option<String>,
    /// The user closed the shell themselves, so typing does not open another.
    pub shell_closed: bool,
    /// Queue entries stopped by the lost link, and whether the queue was running then.
    pub cut: Vec<Id>,
    pub queue_was_running: bool,
    /// The remote folder last listed: where a reconnect goes back to.
    pub listed: Option<RemotePath>,
    /// The computer woke; acted on once the sleep's log-off has settled.
    pub woke: bool,
    /// A job of ours failed this tick (a listing, an operation, a transfer).
    pub failed: bool,
}

impl Pending {
    /// The simple questions an answer may be replayed with; the queue's own are not.
    fn replayable(&self) -> Option<Pending> {
        match self {
            Pending::NewFolder(s) => Some(Pending::NewFolder(*s)),
            Pending::Rename(s, n) => Some(Pending::Rename(*s, n.clone())),
            Pending::Delete(s, p) => Some(Pending::Delete(*s, p.clone())),
            _ => None,
        }
    }
}

impl ConnectionController {
    pub(crate) fn set_link_prefs(&mut self, prefs: LinkPrefs) {
        self.link.prefs = prefs;
        self.push_upkeep();
    }

    /// The machine's own choice, else Options.
    fn reconnect_policy(&self) -> Auto {
        self.machine.reconnect.unwrap_or(self.link.prefs.reconnect)
    }

    fn resume_policy(&self) -> Auto {
        self.machine
            .resume_queue
            .unwrap_or(self.link.prefs.resume_queue)
    }

    /// How long a quiet shell or connection waits for a keepalive; `None` for none.
    pub(super) fn keepalive(&self) -> Option<Duration> {
        let secs = self
            .machine
            .keepalive_secs
            .unwrap_or(self.link.prefs.keepalive_secs);
        // Not more often than every 5 s, whatever was typed.
        (secs > 0).then(|| Duration::from_secs(secs.max(5)))
    }

    /// Tell the session manager how to keep this machine's connection.
    pub(super) fn push_upkeep(&self) {
        let mins = self
            .machine
            .idle_logoff_mins
            .unwrap_or(self.link.prefs.idle_logoff_mins);
        let upkeep = Upkeep {
            keepalive: self.keepalive(),
            idle_logoff: (mins > 0).then(|| Duration::from_secs(mins.saturating_mul(60))),
        };
        self.sessions.set_upkeep(&self.machine.id, upkeep);
    }

    fn lost_or_idle(&self) -> bool {
        matches!(self.state(), ConnState::Lost | ConnState::Idle)
    }

    /// Whether `a` works with the remote side, so it needs the file connection.
    fn needs_files(&self, a: &Action) -> bool {
        let remote_pending = |side: Side| match &self.pending {
            Some(Pending::NewFolder(s) | Pending::Rename(s, _) | Pending::Delete(s, _)) => {
                *s == side
            }
            _ => false,
        };
        match a {
            Action::RemoteHome
            | Action::RemoteUp
            | Action::RemoteOpen(_)
            | Action::Refresh(Side::Remote)
            | Action::GoTo(Side::Remote)
            | Action::RemoteGoToAnswered(Some(_))
            | Action::PlaceChosen(Side::Remote, Some(_))
            | Action::Copy(..)
            | Action::AddToQueue(..)
            | Action::CopyOut(..)
            | Action::Drop(Side::Remote, _)
            | Action::ConsoleSend(_)
            | Action::NewFolder(Side::Remote)
            | Action::Rename(Side::Remote, _)
            | Action::Delete(Side::Remote, _)
            | Action::Queue(QueueOp::Start | QueueOp::Resume | QueueOp::Retry, _) => true,
            Action::NewFolderAnswered(Some(_)) | Action::RenameAnswered(Some(_)) => {
                remote_pending(Side::Remote)
            }
            Action::DeleteConfirmed(true) => remote_pending(Side::Remote),
            _ => false,
        }
    }

    /// Before an action runs: one that needs a lost connection connects again first.
    pub(super) fn gate(&mut self, a: &Action) -> Option<Vec<Effect>> {
        if !self.needs_files(a) {
            return None;
        }
        let replay = Replay {
            action: a.clone(),
            pending: self.pending.as_ref().and_then(Pending::replayable),
        };
        if !self.lost_or_idle() {
            // A console command is never sent twice: it may not be safe to repeat.
            if !matches!(a, Action::ConsoleSend(_)) {
                self.link.last = Some(replay);
            }
            return None;
        }
        Some(self.offer_rejoin(Some(replay)))
    }

    /// Connect again now, ask first, or only say why, as the machine's choice says.
    fn offer_rejoin(&mut self, then: Option<Replay>) -> Vec<Effect> {
        let why = self.link_why();
        match self.reconnect_policy() {
            Auto::Always => self.rejoin(then),
            Auto::Ask => {
                self.link.asking = Some(RejoinFor::Files(then));
                vec![self.rejoin_question(&why)]
            }
            Auto::Never => vec![Effect::Message(format!(
                "{}: {why} Use Reconnect to connect again.",
                self.machine.name
            ))],
        }
    }

    fn link_why(&self) -> String {
        self.sessions
            .why(&self.machine.id)
            .unwrap_or_else(|| "The connection was lost.".into())
    }

    fn rejoin_question(&self, why: &str) -> Effect {
        Effect::Choose(ChoiceSpec {
            purpose: REJOIN_PURPOSE,
            title: "Connect again?".into(),
            message: format!("{why}\n\nConnect to {} again?", self.machine.name),
            choices: vec!["Connect".into(), "Not now".into()],
            dismiss: 1,
        })
    }

    pub(super) fn rejoin_answered(&mut self, choice: usize) -> Vec<Effect> {
        match (self.link.asking.take(), choice) {
            (Some(RejoinFor::Files(then)), 0) => self.rejoin(then),
            (Some(RejoinFor::Shell), 0) => {
                self.term = TermTab::Shell;
                self.shell_connect()
            }
            _ => Vec::new(),
        }
    }

    /// Connect again, keeping the open folder and what to do once connected.
    pub(super) fn rejoin(&mut self, then: Option<Replay>) -> Vec<Effect> {
        if self.connect_job.is_some() {
            if let (Some(r), Some(t)) = (self.link.rejoin.as_mut(), then) {
                r.then = Some(t);
            }
            return Vec::new();
        }
        self.link.in_a_row += 1;
        self.link.rejoin = Some(Rejoin {
            dir: self.link.listed.clone().or_else(|| self.remote_dir.clone()),
            then,
        });
        log::info!("{}: connecting again", self.machine.name);
        self.start_connect()
    }

    /// Where a connect should go: back to the folder that was open, and the action after it.
    pub(super) fn rejoined(&mut self, home: &RemotePath) -> Option<Vec<Effect>> {
        let r = self.link.rejoin.take()?;
        self.console.push(format!(
            "-- {} Connected again to {}.",
            clock(),
            self.machine.name
        ));
        let mut out = Vec::new();
        let dir = r.dir.unwrap_or_else(|| home.clone());
        match &r.then {
            // Navigation lists a folder itself; it starts from the folder that was open.
            Some(Replay {
                action:
                    Action::RemoteHome
                    | Action::RemoteUp
                    | Action::RemoteOpen(_)
                    | Action::Refresh(Side::Remote)
                    | Action::RemoteGoToAnswered(_)
                    | Action::PlaceChosen(Side::Remote, _),
                ..
            }) => self.remote_dir = Some(dir),
            _ => self.go_remote(dir),
        }
        // The resume question decides about the queue; a replayed Start would not wait for it.
        let asks = self.resume_policy() == Auto::Ask && self.cut_pending();
        let then = r
            .then
            .filter(|t| !(asks && matches!(t.action, Action::Queue(..))));
        if let Some(t) = then {
            if t.pending.is_some() {
                self.pending = t.pending;
            }
            out.extend(super::Controller::apply(self, t.action));
            self.link.replayed = true;
        }
        out.extend(self.offer_resume());
        Some(out)
    }

    /// A connect that failed keeps the folder to go back to, but not the action.
    pub(super) fn rejoin_failed(&mut self) {
        if let Some(r) = self.link.rejoin.as_mut() {
            r.then = None;
        }
    }

    /// After a reconnect: transfers the dropped link stopped go on, or are asked about.
    fn offer_resume(&mut self) -> Vec<Effect> {
        let n = self.left_to_copy();
        if !self.cut_pending() {
            self.link.cut.clear();
            self.link.queue_was_running = false;
            return Vec::new();
        }
        match self.resume_policy() {
            Auto::Always => self.resume_cut(),
            Auto::Ask => {
                vec![Effect::Choose(ChoiceSpec {
                    purpose: RESUME_PURPOSE,
                    title: "Transfers are not finished".into(),
                    message: format!(
                        "{n} file(s) were still to copy when the connection to {} stopped. \
                         Go on with them?",
                        self.machine.name
                    ),
                    choices: vec!["Go on".into(), "Not now".into()],
                    dismiss: 1,
                })]
            }
            Auto::Never => {
                self.link.cut.clear();
                self.link.queue_was_running = false;
                Vec::new()
            }
        }
    }

    /// Files still to copy: the queue's unfinished ones and those the link failed.
    fn left_to_copy(&self) -> usize {
        let failed = self
            .link
            .cut
            .iter()
            .filter_map(|id| self.xfer.queue.get(*id))
            .filter(|e| matches!(e.state, State::Failed(_)) && !e.item.folder)
            .count();
        self.xfer.queue.unfinished() + failed
    }

    /// The link stopped transfers that are still to copy.
    fn cut_pending(&self) -> bool {
        (!self.link.cut.is_empty() || self.link.queue_was_running) && self.left_to_copy() > 0
    }

    pub(super) fn resume_answered(&mut self, choice: usize) -> Vec<Effect> {
        if choice == 0 {
            return self.resume_cut();
        }
        self.link.cut.clear();
        self.link.queue_was_running = false;
        Vec::new()
    }

    /// The entries the lost link failed wait again, resuming their partial copies; the queue runs.
    fn resume_cut(&mut self) -> Vec<Effect> {
        let ids = std::mem::take(&mut self.link.cut);
        self.link.queue_was_running = false;
        self.xfer.queue.retry(&ids);
        self.xfer.queue.running = true;
        self.nudge_queue(Vec::new())
    }

    /// Once a tick: see the link drop under a job of ours, and carry on as the machine says.
    pub(super) fn watch_link(&mut self, out: &mut Vec<Effect>) {
        let failed = std::mem::take(&mut self.link.failed);
        let now = self.state();
        let dropped = matches!(now, ConnState::Lost | ConnState::Idle);
        self.link.note = dropped.then(|| self.link_why());
        // Work running or queued keeps the machine from an idle log-off.
        let busy = self.xfer.queue.running || self.xfer.queue.running_id().is_some();
        self.sessions.hold(&self.machine.id, busy);
        // The Console says when the file connection went, with why.
        if dropped && !matches!(self.link.was, ConnState::Lost | ConnState::Idle) {
            let why = self.link_why();
            self.console.push(format!("-- {} {why}", clock()));
        }
        self.link.was = now;
        if self.link.woke && !matches!(now, ConnState::Disconnecting | ConnState::Connecting) {
            let more = self.after_wake();
            out.extend(more);
        }
        let settling = self.connect_job.is_some() || self.disconnect_job.is_some();
        if !failed || now != ConnState::Lost || settling || self.link.asking.is_some() {
            return;
        }
        // Something the user did failed because the link went: do it again once connected.
        if self.link.in_a_row >= REJOINS_MAX || std::mem::take(&mut self.link.replayed) {
            log::warn!(
                "{}: lost again right after connecting; not trying again",
                self.machine.name
            );
            return;
        }
        self.remote_error = None;
        let last = self.link.last.take();
        out.extend(self.offer_rejoin(last));
    }

    /// Work on the machine went through: the link is good, so reconnects start counting again.
    pub(super) fn link_worked(&mut self) {
        self.link.in_a_row = 0;
        self.link.replayed = false;
        self.link.last = None;
    }

    /// A queue entry stopped while the link was gone: it goes on after the reconnect.
    pub(super) fn note_cut(&mut self, id: Id, queue_running: bool) {
        if matches!(
            self.state(),
            ConnState::Lost | ConnState::Idle | ConnState::Disconnecting
        ) && !self.link.cut.contains(&id)
        {
            self.link.cut.push(id);
            self.link.queue_was_running |= queue_running;
        }
    }

    /// Typing into a shell the machine closed (not the user) opens another, as the machine says.
    pub(super) fn shell_gate(&mut self) -> Option<Vec<Effect>> {
        let ended = self
            .shell
            .as_ref()
            .is_some_and(|s| s.state() != ConnState::Connected);
        if !ended
            || self.link.shell_closed
            || self.shell_open.is_some()
            || !self.can_shell_connect()
        {
            return None;
        }
        Some(match self.reconnect_policy() {
            Auto::Always => self.shell_connect(),
            Auto::Ask => {
                self.link.asking = Some(RejoinFor::Shell);
                let why = self
                    .shell
                    .as_ref()
                    .and_then(|s| s.ended())
                    .unwrap_or_default();
                vec![Effect::Choose(ChoiceSpec {
                    purpose: REJOIN_PURPOSE,
                    title: "Open the shell again?".into(),
                    message: format!("{why}\n\nOpen a new shell on {}?", self.machine.name),
                    choices: vec!["Connect".into(), "Not now".into()],
                    dismiss: 1,
                })]
            }
            Auto::Never => Vec::new(),
        })
    }

    /// Log off for the computer's sleep, keeping the folder and the queue for afterwards.
    pub(crate) fn begin_suspend(&mut self) -> Vec<crate::model::job::Job<()>> {
        let mut jobs = Vec::new();
        if let Some(s) = self
            .shell
            .as_ref()
            .filter(|s| s.state() == ConnState::Connected)
        {
            jobs.push(s.close(
                &self.machine.logout_command,
                std::sync::Arc::clone(&self.notify),
            ));
        }
        if self.can_disconnect() {
            self.link.queue_was_running |= self.xfer.queue.running;
            self.xfer.queue.running = false;
            jobs.push(
                self.sessions
                    .suspend(&self.machine.id, std::sync::Arc::clone(&self.notify)),
            );
        }
        jobs
    }

    /// After the computer woke: a queue the sleep stopped connects again and goes on.
    pub(crate) fn woke(&mut self) -> Vec<Effect> {
        self.link.woke = true;
        match self.state() {
            // The sleep's log-off may still be finishing: wait for it (watch_link).
            ConnState::Disconnecting | ConnState::Connecting => Vec::new(),
            _ => self.after_wake(),
        }
    }

    fn after_wake(&mut self) -> Vec<Effect> {
        self.link.woke = false;
        let go =
            self.cut_pending() && self.lost_or_idle() && self.reconnect_policy() != Auto::Never;
        if go && self.link.asking.is_none() {
            return self.offer_rejoin(None);
        }
        Vec::new()
    }
}
