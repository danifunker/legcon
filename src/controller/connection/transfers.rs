//! A machine tab's transfer queue: picks are walked into one entry per file (asking first when
//! the batch is large or targets exist), the queue runs one file at a time, and the Transfers
//! list with its buttons is the same for every view (PLAN.md §5, D42).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use super::{size_text, ConnState, ConnectionController, Group, Pending, Side, TermTab};
use crate::controller::{ChoiceSpec, Effect, TextSpec};
use crate::model::job::{Cancelled, Job, Status};
use crate::model::local;
use crate::model::queue::{Id, OnExisting, Queue, State};
use crate::model::session::suspect;
use crate::model::transfer::{self, Copied, Direction, Mismatch, Plan, Summary, Verified};

/// The app settings the queue follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuePrefs {
    /// Ask before adding more files than this; 0 never asks.
    pub ask_over: usize,
    pub checksum: bool,
    /// What to do with targets already there without asking; `Ask` asks.
    pub on_existing: OnExisting,
}

impl Default for QueuePrefs {
    fn default() -> Self {
        QueuePrefs {
            ask_over: crate::model::settings::QUEUE_ASK_OVER,
            checksum: true,
            on_existing: OnExisting::Ask,
        }
    }
}

/// The button for each "already there" choice, as both questions word it.
pub(super) fn label(on: OnExisting, many: bool) -> &'static str {
    match on {
        OnExisting::Overwrite => "Overwrite",
        OnExisting::IfDifferent => "Replace if different",
        OnExisting::Resume => "Resume",
        OnExisting::Rename if many => "Rename each...",
        OnExisting::Rename => "Rename...",
        OnExisting::Skip if many => "Skip them",
        OnExisting::Skip => "Skip",
        OnExisting::Ask => "Ask for each",
    }
}

/// What Replace if different does, for the questions' text.
const IF_DIFFERENT: &str = "Replace if different leaves a file that is the same (same size, then \
the machine's own checksum where it makes one, else the same date).";

/// What a Transfers button does; it acts on the selected rows where it needs any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOp {
    Start,
    PauseAll,
    Pause,
    Resume,
    Retry,
    Remove,
    Up,
    Down,
    ClearFinished,
    CancelAll,
}

/// One button above the Transfers list, as every view draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueButton {
    pub op: QueueOp,
    /// The widget id; Start and Pause queue share one button.
    pub id: &'static str,
    pub label: &'static str,
    pub enabled: bool,
}

/// One row of the Transfers list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    pub id: Id,
    pub name: String,
    pub size: String,
    pub direction: String,
    pub status: String,
    /// Amounts, speed and time left while it runs; why, when it failed.
    pub progress: String,
    pub from: String,
    pub to: String,
}

/// The Transfers list's columns, as (title, width in DIPs).
pub const QUEUE_COLUMNS: [(&str, i32); 7] = [
    ("Name", 180),
    ("Size", 70),
    ("Direction", 130),
    ("Status", 170),
    ("Progress", 200),
    ("From", 220),
    ("To", 220),
];

/// The text prompt for a copy's new name (queue and local copies alike).
pub const RENAME_PURPOSE: &str = "conn.copy.rename";

/// A batch being walked into queue entries.
pub(super) struct Planning {
    id: Id,
    label: String,
    job: Job<Plan>,
    start: bool,
}

/// Files dropped from the OS onto the local pane: copied on this computer, listed with the queue.
pub(super) struct LocalCopy {
    id: Id,
    label: String,
    job: Job<Summary>,
    result: Option<String>,
}

/// A copy job's end short of a cancel: the copy, or why not and whether a retry starts over.
type ItemResult = Result<Copied, (String, bool)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    No,
    Pause,
    Remove,
}

pub(super) struct Running {
    id: Id,
    job: Job<ItemResult>,
    stop: Stop,
}

/// An answer to "already there" for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemAnswer {
    This(OnExisting),
    Rest(OnExisting),
}

#[derive(Default)]
pub(super) struct Transfers {
    pub queue: Queue,
    planning: Vec<Planning>,
    /// Planned batches waiting for their questions, in arrival order.
    arrived: VecDeque<(Plan, bool)>,
    running: Option<Running>,
    local: Vec<LocalCopy>,
    pub prefs: QueuePrefs,
    /// Log off once the queue has stopped (the disconnect question's answers).
    disconnect_after: bool,
    /// Panes to list again once the queue goes quiet.
    touched: (bool, bool),
}

fn done_text(c: &Copied) -> String {
    if let Some(how) = &c.same {
        return format!("Same as there, not copied ({how})");
    }
    let mut s = match &c.verified {
        Verified::Sum(sum) => format!("Done, {} matches", sum.name()),
        Verified::Size(_) => "Done, size matches".to_string(),
        Verified::Unchecked => "Done".to_string(),
    };
    if c.resumed_at > 0 {
        s += &format!(" (resumed at {})", size_text(c.resumed_at));
    }
    s
}

fn state_text(s: &State) -> &str {
    match s {
        State::Waiting => "Waiting",
        State::Asking => "Already there?",
        State::Running => "Copying",
        State::Paused => "Paused",
        State::Done(t) => t,
        State::Skipped => "Skipped, already there",
        State::Failed(_) => "Failed",
    }
}

impl ConnectionController {
    fn direction_text(&self, dir: Direction) -> String {
        match dir {
            Direction::Download => "To this computer".into(),
            Direction::Upload => format!("To {}", self.machine.name),
        }
    }

    /// The Transfers list: the queue in running order, then batches still being looked at,
    /// then local copies.
    pub fn queue_rows(&self) -> Vec<QueueRow> {
        let running = self.xfer.running.as_ref();
        let mut rows: Vec<QueueRow> = self
            .xfer
            .queue
            .entries()
            .iter()
            .filter(|e| !e.item.folder || matches!(e.state, State::Failed(_)))
            .map(|e| {
                let it = &e.item;
                let (from, to) = match it.dir {
                    Direction::Download => (it.remote.to_string(), it.local.display().to_string()),
                    Direction::Upload => (it.local.display().to_string(), it.remote.to_string()),
                };
                let mut status = state_text(&e.state).to_string();
                let mut progress = String::new();
                match &e.state {
                    State::Running => {
                        if let Some(r) = running.filter(|r| r.id == e.id) {
                            let s = r.job.snapshot();
                            if let Some(f) = s.progress.fraction() {
                                status = format!("Copying {:.0}%", f * 100.0);
                            }
                            if s.phase.starts_with("Checking") {
                                status = "Checking".into();
                            }
                            progress = s.detail();
                        }
                    }
                    State::Failed(why) => progress = why.clone(),
                    State::Waiting if e.needs_answer() => status = "Waiting, already there".into(),
                    _ => {}
                }
                QueueRow {
                    id: e.id,
                    name: if it.folder {
                        format!("{}/", it.name())
                    } else {
                        it.name()
                    },
                    size: it.size.map(size_text).unwrap_or_default(),
                    direction: self.direction_text(it.dir),
                    status,
                    progress,
                    from,
                    to,
                }
            })
            .collect();
        for p in &self.xfer.planning {
            let s = p.job.snapshot();
            rows.push(QueueRow {
                id: p.id,
                name: p.label.clone(),
                size: String::new(),
                direction: String::new(),
                status: "Looking".into(),
                progress: s.phase,
                from: String::new(),
                to: String::new(),
            });
        }
        for b in &self.xfer.local {
            let (status, progress) = match &b.result {
                Some(r) => ("Done".to_string(), r.clone()),
                None => {
                    let s = b.job.snapshot();
                    (
                        "Copying".to_string(),
                        format!("{}   {}", s.phase, s.detail()),
                    )
                }
            };
            rows.push(QueueRow {
                id: b.id,
                name: b.label.clone(),
                size: String::new(),
                direction: "On this computer".into(),
                status,
                progress,
                from: String::new(),
                to: String::new(),
            });
        }
        rows
    }

    /// The buttons above the list, enabled for these selected rows.
    pub fn queue_buttons(&self, selected: &[Id]) -> Vec<QueueButton> {
        use crate::controller::ids;
        let q = &self.xfer.queue;
        let picked: Vec<&State> = q
            .entries()
            .iter()
            .filter(|e| selected.contains(&e.id))
            .map(|e| &e.state)
            .collect();
        let any = |f: fn(&State) -> bool| picked.iter().any(|s| f(s));
        let waiting = q.next_waiting().is_some();
        let finished = q.entries().iter().any(|e| e.state.finished())
            || self.xfer.local.iter().any(|b| b.result.is_some());
        let local_picked = self
            .xfer
            .local
            .iter()
            .any(|b| b.result.is_some() && selected.contains(&b.id));
        let b = |op, id, label, enabled| QueueButton {
            op,
            id,
            label,
            enabled,
        };
        vec![
            if q.running {
                b(QueueOp::PauseAll, ids::QUEUE_RUN, "Pause queue", true)
            } else {
                b(QueueOp::Start, ids::QUEUE_RUN, "Start queue", waiting)
            },
            b(
                QueueOp::Pause,
                ids::QUEUE_PAUSE,
                "Pause",
                any(|s| matches!(s, State::Waiting | State::Asking | State::Running)),
            ),
            b(
                QueueOp::Resume,
                ids::QUEUE_RESUME,
                "Resume",
                any(|s| *s == State::Paused),
            ),
            b(
                QueueOp::Retry,
                ids::QUEUE_RETRY,
                "Retry",
                any(|s| matches!(s, State::Failed(_) | State::Skipped)),
            ),
            b(
                QueueOp::Remove,
                ids::QUEUE_REMOVE,
                "Remove",
                !picked.is_empty() || local_picked,
            ),
            b(
                QueueOp::Up,
                ids::QUEUE_UP,
                "Up",
                !picked.is_empty() && q.entries().len() > 1,
            ),
            b(
                QueueOp::Down,
                ids::QUEUE_DOWN,
                "Down",
                !picked.is_empty() && q.entries().len() > 1,
            ),
            b(
                QueueOp::ClearFinished,
                ids::QUEUE_CLEAR,
                "Clear finished",
                finished,
            ),
            b(
                QueueOp::CancelAll,
                ids::QUEUE_CANCEL,
                "Cancel all",
                self.copying(),
            ),
        ]
    }

    /// One line on the queue, for a status bar: what is left and whether it runs.
    pub fn queue_summary(&self) -> String {
        let q = &self.xfer.queue;
        let left = q.unfinished();
        if left == 0 && self.xfer.planning.is_empty() {
            return String::new();
        }
        let failed = q
            .entries()
            .iter()
            .filter(|e| matches!(e.state, State::Failed(_)))
            .count();
        let mut s = format!("{left} file(s) to copy ({})", size_text(q.bytes_left()));
        if failed > 0 {
            s += &format!(", {failed} failed");
        }
        if !q.running && left > 0 {
            s += "; the queue is paused";
        }
        s
    }

    /// The Transfers list as text (the CLI and tests); newest last, as the queue runs.
    pub fn transfers_text(&self) -> String {
        let rows = self.queue_rows();
        if rows.is_empty() {
            return "No transfers yet. Select files and press F6, or right-click and choose Copy."
                .into();
        }
        let mut lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let mut l = format!("{}  {}  {}", r.status, r.name, r.direction);
                if !r.progress.is_empty() {
                    l += &format!("  {}", r.progress);
                }
                l.trim_end().to_string()
            })
            .collect();
        let summary = self.queue_summary();
        if !summary.is_empty() {
            lines.push(summary);
        }
        lines.join("\n")
    }

    /// Whether anything is being looked at, copied or still waiting to copy.
    pub fn copying(&self) -> bool {
        self.xfer.running.is_some()
            || !self.xfer.planning.is_empty()
            || !self.xfer.arrived.is_empty()
            || self.xfer.local.iter().any(|b| b.result.is_none())
            || (self.xfer.queue.running
                && self.xfer.queue.next_waiting().is_some()
                && !self.xfer.queue.asking())
    }

    /// Walk a group of picks into queue entries; `start` runs the queue once they are in.
    pub(super) fn start_plan(&mut self, dir: Direction, g: Group, start: bool) {
        let Group {
            local_dir,
            remote_dir,
            picks,
            sizes,
        } = g;
        let Some(remote_dir) = remote_dir.or_else(|| self.remote_dir.clone()) else {
            return;
        };
        self.note_local(local_dir.clone());
        self.note_remote(&remote_dir);
        let what = if picks.len() == 1 {
            picks[0].name.clone()
        } else {
            format!("{} items", picks.len())
        };
        let label = match dir {
            Direction::Download => format!("{what} from {}", self.machine.name),
            Direction::Upload => format!("{what} to {}", self.machine.name),
        };
        let job = self.sessions.run(
            &self.machine.id,
            format!("Looking at {label}"),
            Arc::clone(&self.notify),
            move |fs, ctx| {
                transfer::plan(
                    fs,
                    &transfer::Batch {
                        dir,
                        remote_dir: &remote_dir,
                        local_dir: &local_dir,
                        picks: &picks,
                        sizes: &sizes,
                    },
                    ctx,
                )
            },
        );
        let id = self.xfer.queue.new_id();
        self.xfer.planning.push(Planning {
            id,
            label,
            job,
            start,
        });
        self.show_transfers();
    }

    fn show_transfers(&mut self) {
        if self.shell_state() != ConnState::Connected {
            self.term = TermTab::Transfers;
        }
    }

    pub(super) fn start_import(
        &mut self,
        paths: Vec<PathBuf>,
        dest: PathBuf,
        choices: local::Choices,
    ) {
        let what = if paths.len() == 1 {
            paths[0]
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            format!("{} items", paths.len())
        };
        let label = format!("Copy {what} into {}", dest.display());
        let job = Job::spawn(label.clone(), Arc::clone(&self.notify), move |ctx| {
            local::copy_into(&paths, &dest, &choices, ctx)
        });
        let id = self.xfer.queue.new_id();
        self.xfer.local.push(LocalCopy {
            id,
            label,
            job,
            result: None,
        });
        self.show_transfers();
    }

    /// A planned batch: "that many files?" first, then "already there", then into the queue.
    fn ask_many(&mut self, plan: Plan, start: bool) -> Vec<Effect> {
        let (files, over) = (plan.files(), self.xfer.prefs.ask_over);
        if over > 0 && files > over {
            let message = format!(
                "Adding {files} files ({}) to the transfer queue. Continue?",
                size_text(plan.bytes())
            );
            self.pending = Some(Pending::QueueMany(plan, start));
            return vec![Effect::Choose(ChoiceSpec {
                purpose: "conn.queue.many",
                title: "Transfer queue".into(),
                message,
                choices: vec!["Add them".into(), "Cancel".into()],
                dismiss: 1,
            })];
        }
        self.ask_exists(plan, start)
    }

    fn ask_exists(&mut self, plan: Plan, start: bool) -> Vec<Effect> {
        let there: Vec<&transfer::Item> = plan
            .items
            .iter()
            .filter(|i| i.exists.is_some() && !i.folder)
            .collect();
        let Some(first) = there.first() else {
            self.enqueue(plan, OnExisting::Overwrite, start);
            return Vec::new();
        };
        if self.xfer.prefs.on_existing != OnExisting::Ask {
            let on = self.xfer.prefs.on_existing;
            self.enqueue(plan, on, start);
            return Vec::new();
        }
        let place = match first.dir {
            Direction::Download => "on this computer".to_string(),
            Direction::Upload => format!("on {}", self.machine.name),
        };
        let shorter = there
            .iter()
            .any(|i| matches!((i.exists, i.size), (Some(Some(a)), Some(b)) if a < b));
        let mut message = if there.len() == 1 {
            format!("{} is already {place}.", first.name())
        } else {
            format!(
                "{} of these are already {place}, such as {}.",
                there.len(),
                first.name()
            )
        };
        let many = there.len() > 1;
        let mut on = vec![OnExisting::Overwrite, OnExisting::IfDifferent];
        if plan.can_resume && shorter {
            message += " Resume continues the ones that are shorter there.";
            on.push(OnExisting::Resume);
        }
        message += " ";
        message += IF_DIFFERENT;
        on.extend([OnExisting::Rename, OnExisting::Skip]);
        if many {
            on.push(OnExisting::Ask);
        }
        let mut choices: Vec<String> = on.iter().map(|o| label(*o, many).to_string()).collect();
        let mut answers: Vec<Option<OnExisting>> = on.into_iter().map(Some).collect();
        choices.push("Cancel".into());
        answers.push(None);
        let dismiss = choices.len() - 1;
        self.pending = Some(Pending::QueueExists(plan, start, answers));
        vec![Effect::Choose(ChoiceSpec {
            purpose: "conn.queue.exists",
            title: "Transfer queue".into(),
            message,
            choices,
            dismiss,
        })]
    }

    fn enqueue(&mut self, plan: Plan, on: OnExisting, start: bool) {
        self.xfer.queue.add(plan, on);
        if start {
            self.xfer.queue.running = true;
        }
        self.show_transfers();
    }

    pub(super) fn queue_many_answered(&mut self, choice: usize) -> Vec<Effect> {
        match self.pending.take() {
            Some(Pending::QueueMany(plan, start)) if choice == 0 => self.ask_exists(plan, start),
            Some(Pending::QueueMany(..)) => Vec::new(),
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    pub(super) fn queue_exists_answered(&mut self, choice: usize) {
        match self.pending.take() {
            Some(Pending::QueueExists(plan, start, answers)) => {
                if let Some(Some(on)) = answers.get(choice) {
                    self.enqueue(plan, *on, start);
                }
            }
            other => self.pending = other,
        }
    }

    pub(super) fn queue_item_answered(&mut self, choice: usize) {
        match self.pending.take() {
            Some(Pending::QueueItem(id, answers)) => {
                let answer = answers
                    .get(choice)
                    .copied()
                    .unwrap_or(ItemAnswer::This(OnExisting::Skip));
                let on = match answer {
                    ItemAnswer::This(on) => on,
                    ItemAnswer::Rest(on) => {
                        self.xfer.queue.answer_rest(on);
                        on
                    }
                };
                if let Some(e) = self.xfer.queue.get_mut(id) {
                    e.on_existing = on;
                    if e.state == State::Asking {
                        e.state = State::Waiting;
                    }
                }
            }
            other => self.pending = other,
        }
    }

    fn ask_item(&mut self, id: Id) -> Vec<Effect> {
        let Some(e) = self.xfer.queue.get(id) else {
            return Vec::new();
        };
        let it = &e.item;
        let have = it.exists.flatten();
        let place = match it.dir {
            Direction::Download => "this computer".to_string(),
            Direction::Upload => self.machine.name.clone(),
        };
        let mut message = format!("{} is already on {place}", it.name());
        if let (Some(a), Some(b)) = (have, it.size) {
            message += &format!(" ({} there, {} to copy)", size_text(a), size_text(b));
        }
        message.push('.');
        let mut answers = vec![
            ItemAnswer::This(OnExisting::Overwrite),
            ItemAnswer::This(OnExisting::IfDifferent),
        ];
        if e.can_resume && matches!((have, it.size), (Some(a), Some(b)) if a < b) {
            answers.push(ItemAnswer::This(OnExisting::Resume));
        }
        answers.push(ItemAnswer::This(OnExisting::Rename));
        let skip = answers.len();
        answers.extend([
            ItemAnswer::This(OnExisting::Skip),
            ItemAnswer::Rest(OnExisting::Overwrite),
            ItemAnswer::Rest(OnExisting::Skip),
        ]);
        let choices = answers
            .iter()
            .map(|a| match a {
                ItemAnswer::This(o) => label(*o, false).to_string(),
                ItemAnswer::Rest(OnExisting::Overwrite) => "Overwrite all".into(),
                ItemAnswer::Rest(_) => "Skip all".into(),
            })
            .collect();
        message.push(' ');
        message += IF_DIFFERENT;
        self.xfer.queue.set_state(id, State::Asking);
        self.pending = Some(Pending::QueueItem(id, answers));
        vec![Effect::Choose(ChoiceSpec {
            purpose: "conn.queue.item",
            title: "Transfer queue".into(),
            message,
            choices,
            dismiss: skip,
        })]
    }

    /// "Copy it under another name": the user's own name, offered as `name (2)` to edit.
    fn ask_rename(&mut self, id: Id, taken: Option<&str>) -> Vec<Effect> {
        let Some(e) = self.xfer.queue.get(id) else {
            return Vec::new();
        };
        let side = match e.item.dir {
            Direction::Download => Side::Local,
            Direction::Upload => Side::Remote,
        };
        let place = match e.item.dir {
            Direction::Download => "this computer".to_string(),
            Direction::Upload => self.machine.name.clone(),
        };
        let prompt = match taken {
            Some(n) => format!("{n} is also on {place}. Copy {} as:", e.item.name()),
            None => format!(
                "{} is already on {place}. Copy it as:",
                e.item.target_name()
            ),
        };
        let initial = self.xfer.queue.suggest_name(id);
        self.xfer.queue.set_state(id, State::Asking);
        self.pending = Some(Pending::QueueRename(id));
        vec![Effect::AskText(TextSpec {
            purpose: RENAME_PURPOSE,
            title: "Copy under another name".into(),
            prompt,
            initial,
            ok_label: "Copy".into(),
            validator: self.name_rule(side),
        })]
    }

    /// The name for a renamed copy; no name asks the "already there" question again.
    pub(super) fn queue_rename_answered(&mut self, answer: Option<String>) -> Vec<Effect> {
        let Some(Pending::QueueRename(id)) = self.pending.take() else {
            return Vec::new();
        };
        let name = answer
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty());
        match name {
            Some(n) if self.xfer.queue.name_taken(id, &n) => self.ask_rename(id, Some(&n)),
            Some(n) => {
                self.xfer.queue.rename(id, &n);
                Vec::new()
            }
            None => {
                if let Some(e) = self.xfer.queue.get_mut(id) {
                    e.on_existing = OnExisting::Ask;
                    e.state = State::Waiting;
                }
                Vec::new()
            }
        }
    }

    /// Before Disconnect: ask what happens to unfinished transfers; `None` when there are none.
    pub(super) fn ask_disconnect(&mut self) -> Option<Vec<Effect>> {
        let left = self.xfer.queue.unfinished() + self.xfer.planning.len();
        if left == 0 || !self.can_disconnect() {
            return None;
        }
        self.pending = Some(Pending::QueueDisconnect);
        Some(vec![Effect::Choose(ChoiceSpec {
            purpose: "conn.queue.disconnect",
            title: format!("Disconnect {}", self.machine.name),
            message: format!(
                "{left} transfer(s) to or from {} are not finished.",
                self.machine.name
            ),
            choices: vec![
                "Finish first".into(),
                "Pause them".into(),
                "Cancel them".into(),
                "Stay connected".into(),
            ],
            dismiss: 3,
        })])
    }

    pub(super) fn disconnect_answered(&mut self, choice: usize) {
        match self.pending.take() {
            Some(Pending::QueueDisconnect) => match choice {
                0 => {
                    self.xfer.queue.running = true;
                    self.xfer.disconnect_after = true;
                }
                1 => {
                    self.queue_op(QueueOp::PauseAll, &[]);
                    self.xfer.disconnect_after = true;
                }
                2 => {
                    self.queue_op(QueueOp::CancelAll, &[]);
                    self.xfer.disconnect_after = true;
                }
                _ => {}
            },
            other => self.pending = other,
        }
    }

    /// The queue stops on a disconnect; Start runs it again after the next connect.
    pub(super) fn queue_on_disconnect(&mut self) {
        self.xfer.queue.running = false;
    }

    fn stop_running(&mut self, stop: Stop) {
        if let Some(r) = self.xfer.running.as_mut() {
            if r.stop != Stop::Remove {
                r.stop = stop;
            }
            r.job.cancel();
        }
    }

    pub(super) fn queue_op(&mut self, op: QueueOp, ids: &[Id]) -> Vec<Effect> {
        let running = self.xfer.queue.running_id();
        let hits_running = running.is_some_and(|r| ids.contains(&r));
        let q = &mut self.xfer.queue;
        match op {
            QueueOp::Start => q.running = true,
            QueueOp::PauseAll => {
                q.running = false;
                self.stop_running(Stop::Pause);
            }
            QueueOp::Pause => {
                q.pause(ids);
                if hits_running {
                    self.stop_running(Stop::Pause);
                }
            }
            QueueOp::Resume => {
                q.resume(ids);
                q.running = true;
            }
            QueueOp::Retry => {
                q.retry(ids);
                q.running = true;
            }
            QueueOp::Remove => {
                q.remove(ids);
                if hits_running {
                    self.stop_running(Stop::Remove);
                }
                self.xfer
                    .local
                    .retain(|b| b.result.is_none() || !ids.contains(&b.id));
                for p in self.xfer.planning.iter().filter(|p| ids.contains(&p.id)) {
                    p.job.cancel();
                }
                if let Some(Pending::QueueItem(id, _) | Pending::QueueRename(id)) = &self.pending {
                    if ids.contains(id) {
                        self.pending = None;
                    }
                }
            }
            QueueOp::Up => q.shift(ids, -1),
            QueueOp::Down => q.shift(ids, 1),
            QueueOp::ClearFinished => {
                q.clear_finished();
                self.xfer.local.retain(|b| b.result.is_none());
            }
            QueueOp::CancelAll => {
                q.cancel_unfinished();
                q.running = false;
                self.stop_running(Stop::Remove);
                for p in &self.xfer.planning {
                    p.job.cancel();
                }
                self.xfer.arrived.clear();
                for b in self.xfer.local.iter().filter(|b| b.result.is_none()) {
                    b.job.cancel();
                }
                if matches!(
                    self.pending,
                    Some(
                        Pending::QueueItem(..)
                            | Pending::QueueMany(..)
                            | Pending::QueueExists(..)
                            | Pending::QueueRename(..)
                    )
                ) {
                    self.pending = None;
                }
            }
        }
        Vec::new()
    }

    /// Advance the queue now, after an answer or a button (no job may be about to wake us).
    pub(super) fn nudge_queue(&mut self, mut out: Vec<Effect>) -> Vec<Effect> {
        let mut refresh = (false, false);
        self.tick_transfers(&mut out, &mut refresh);
        if refresh.0 {
            self.list_local();
        }
        if refresh.1 && self.list_job.is_none() {
            self.list_remote();
        }
        out
    }

    /// Advance the queue: finished jobs, questions for new batches, the next file.
    pub(super) fn tick_transfers(&mut self, out: &mut Vec<Effect>, refresh: &mut (bool, bool)) {
        for b in self.xfer.local.iter_mut().filter(|b| b.result.is_none()) {
            if let Some(r) = crate::controller::job_outcome(&b.job) {
                b.result = Some(match r {
                    Ok(s) => {
                        let mut line = format!("{} file(s), {}", s.files, size_text(s.bytes));
                        if !s.skipped.is_empty() {
                            line += &format!("; skipped {} already there", s.skipped.len());
                        }
                        if let Some((name, why)) = s.failed.first() {
                            line += &format!("; {} failed (first: {name}: {why})", s.failed.len());
                        }
                        line
                    }
                    Err(e) => format!("Stopped: {e}"),
                });
                refresh.0 = true;
            }
        }
        let mut i = 0;
        while i < self.xfer.planning.len() {
            let p = &self.xfer.planning[i];
            match p.job.snapshot().status {
                Status::Running => i += 1,
                status => {
                    let p = self.xfer.planning.remove(i);
                    match status {
                        Status::Done => {
                            self.link_worked();
                            if let Some(plan) = p.job.take_result().filter(|p| !p.items.is_empty())
                            {
                                self.xfer.arrived.push_back((plan, p.start));
                            }
                        }
                        Status::Failed(e) if self.state() == ConnState::Lost => {
                            self.link.failed = true;
                            log::warn!("Could not copy {}: {e}", p.label);
                        }
                        Status::Failed(e) => {
                            out.push(Effect::Message(format!("Could not copy {}: {e}", p.label)))
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(r) = &self.xfer.running {
            let status = r.job.snapshot().status;
            if status != Status::Running {
                let r = self.xfer.running.take().expect("checked above");
                let q = &mut self.xfer.queue;
                let to = q.get(r.id).map(|e| e.item.dir);
                match status {
                    Status::Done => match r.job.take_result() {
                        Some(Ok(c)) => {
                            q.set_state(r.id, State::Done(done_text(&c)));
                            match to {
                                Some(Direction::Download) => self.xfer.touched.0 = true,
                                Some(Direction::Upload) => self.xfer.touched.1 = true,
                                None => {}
                            }
                        }
                        Some(Err((why, restart))) => {
                            q.set_state(r.id, State::Failed(why));
                            if let Some(e) = q.get_mut(r.id) {
                                e.restart = restart;
                            }
                        }
                        None => q.set_state(r.id, State::Failed("no result".into())),
                    },
                    Status::Cancelled => {
                        q.set_state(r.id, State::Paused);
                        if r.stop == Stop::Remove {
                            q.remove(&[r.id]);
                        }
                    }
                    Status::Failed(why) => q.set_state(r.id, State::Failed(why)),
                    Status::Running => {}
                }
                if self
                    .xfer
                    .queue
                    .get(r.id)
                    .is_some_and(|e| matches!(e.state, State::Done(_)))
                {
                    self.link_worked();
                }
                // A connection that went away fails every file after it; stop instead.
                if !self.can_disconnect() {
                    let failed = self
                        .xfer
                        .queue
                        .get(r.id)
                        .is_some_and(|e| matches!(e.state, State::Failed(_)));
                    if failed {
                        self.link.failed = true;
                        self.note_cut(r.id, self.xfer.queue.running);
                    }
                    self.xfer.queue.running = false;
                }
            }
        }
        if self.pending.is_none() {
            if let Some((plan, start)) = self.xfer.arrived.pop_front() {
                out.extend(self.ask_many(plan, start));
            }
        }
        while self.xfer.running.is_none()
            && self.xfer.queue.running
            && self.can_disconnect()
            && !self.xfer.queue.asking()
        {
            let Some(id) = self.xfer.queue.next_to_run() else {
                break;
            };
            let e = self.xfer.queue.get(id).expect("listed").clone();
            if e.skips() {
                self.xfer.queue.set_state(id, State::Skipped);
                continue;
            }
            if e.needs_answer() {
                if self.pending.is_none() {
                    out.extend(self.ask_item(id));
                }
                break;
            }
            if e.needs_name() {
                if self.pending.is_none() {
                    out.extend(self.ask_rename(id, None));
                }
                break;
            }
            let (mode, sum) = (e.mode(), self.xfer.prefs.checksum);
            let item = e.item;
            let label = format!("{} {}", self.direction_text(item.dir), item.name());
            let job = self.sessions.run(
                &self.machine.id,
                label,
                Arc::clone(&self.notify),
                move |fs, ctx| match transfer::copy_item(fs, &item, mode, sum, ctx) {
                    Ok(c) => Ok(Ok(c)),
                    // A dead link fails the job, so the session manager sees it and stops the queue.
                    Err(e) if e.downcast_ref::<Cancelled>().is_some() || suspect(&e) => Err(e),
                    Err(e) => Ok(Err((
                        format!("{e:#}"),
                        e.downcast_ref::<Mismatch>().is_some(),
                    ))),
                },
            );
            self.xfer.queue.set_state(id, State::Running);
            self.xfer.running = Some(Running {
                id,
                job,
                stop: Stop::No,
            });
        }
        let quiet = self.xfer.running.is_none()
            && (!self.xfer.queue.running
                || (self.xfer.queue.next_waiting().is_none() && !self.xfer.queue.asking()));
        if quiet {
            // A queue that ran dry stops, so Add to queue only fills it until Start.
            if self.xfer.queue.running && self.xfer.queue.next_waiting().is_none() {
                self.xfer.queue.running = false;
            }
            let t = std::mem::take(&mut self.xfer.touched);
            refresh.0 |= t.0;
            refresh.1 |= t.1;
            if self.xfer.disconnect_after && self.xfer.planning.is_empty() {
                self.xfer.disconnect_after = false;
                self.begin_disconnect();
            }
        }
    }
}

/// A side's copy direction: picks on the local side upload.
pub(super) fn direction_from(side: Side) -> Direction {
    match side {
        Side::Local => Direction::Upload,
        Side::Remote => Direction::Download,
    }
}
