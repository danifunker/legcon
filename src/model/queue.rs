//! A machine tab's transfer queue: one entry per file (or folder to make), in the order they run.
//!
//! The queue only holds state; the controller starts one [`super::transfer::copy_item`] job at a
//! time for [`Queue::next_waiting`] and records how it ended. Each item is its own job, so the
//! machine's connection is free between files for a listing or a rename. Entries can be paused,
//! resumed (from the partial file where the protocol allows), retried, removed and reordered.

use super::transfer::{Item, Mode, Plan};

pub type Id = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Waiting,
    /// Waiting for the user to say what to do with a target that is already there.
    Asking,
    Running,
    Paused,
    /// Copied; how it was checked.
    Done(String),
    Skipped,
    Failed(String),
}

impl State {
    pub fn finished(&self) -> bool {
        matches!(self, State::Done(_) | State::Skipped)
    }
}

/// What to do when the target is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExisting {
    Ask,
    Overwrite,
    /// Continue a shorter target (where the protocol can), overwrite otherwise.
    Resume,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: Id,
    pub item: Item,
    pub state: State,
    pub on_existing: OnExisting,
    /// The protocol can continue this item from a partial target.
    pub can_resume: bool,
    /// A copy that did not match its source starts over on retry rather than resuming.
    pub restart: bool,
}

impl Entry {
    /// What the copy job is told: resume a partial target, or write it whole.
    pub fn mode(&self) -> Mode {
        if self.on_existing == OnExisting::Resume && self.can_resume && !self.restart {
            Mode::Resume
        } else {
            Mode::Overwrite
        }
    }

    /// The target is there and nobody has said what to do with it yet.
    pub fn needs_answer(&self) -> bool {
        self.item.exists.is_some() && self.on_existing == OnExisting::Ask && !self.item.folder
    }

    /// The target is there and is to be left alone.
    pub fn skips(&self) -> bool {
        self.item.exists.is_some() && self.on_existing == OnExisting::Skip
    }

    pub fn unfinished(&self) -> bool {
        matches!(
            self.state,
            State::Waiting | State::Asking | State::Running | State::Paused
        )
    }
}

#[derive(Debug, Default)]
pub struct Queue {
    entries: Vec<Entry>,
    next: Id,
    /// Whether waiting entries start on their own; off means "Add to queue" only builds it up.
    pub running: bool,
}

impl Queue {
    /// A fresh id, for rows shown beside the queue's own (local copies).
    pub fn new_id(&mut self) -> Id {
        self.next += 1;
        self.next
    }

    /// Append a planned batch; returns how many entries it added.
    pub fn add(&mut self, plan: Plan, on_existing: OnExisting) -> usize {
        let n = plan.items.len();
        for item in plan.items {
            let id = self.new_id();
            self.entries.push(Entry {
                id,
                item,
                state: State::Waiting,
                on_existing,
                can_resume: plan.can_resume,
                restart: false,
            });
        }
        n
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: Id) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn get_mut(&mut self, id: Id) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn running_id(&self) -> Option<Id> {
        self.entries
            .iter()
            .find(|e| e.state == State::Running)
            .map(|e| e.id)
    }

    pub fn asking(&self) -> bool {
        self.entries.iter().any(|e| e.state == State::Asking)
    }

    /// The first waiting entry, the one to start next.
    pub fn next_waiting(&self) -> Option<Id> {
        self.entries
            .iter()
            .find(|e| e.state == State::Waiting)
            .map(|e| e.id)
    }

    /// Entries not yet done, skipped or failed.
    pub fn unfinished(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.unfinished() && !e.item.folder)
            .count()
    }

    /// Bytes still to copy in unfinished entries, where sizes are known.
    pub fn bytes_left(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.unfinished())
            .filter_map(|e| e.item.size)
            .sum()
    }

    pub fn set_state(&mut self, id: Id, state: State) {
        if let Some(e) = self.get_mut(id) {
            e.state = state;
        }
    }

    /// Hold waiting entries; a running one is stopped by its owner, which then marks it paused.
    pub fn pause(&mut self, ids: &[Id]) {
        for e in self.entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            if matches!(e.state, State::Waiting | State::Asking) {
                e.state = State::Paused;
            }
        }
    }

    /// Paused entries wait again, continuing the partial copy they left.
    pub fn resume(&mut self, ids: &[Id]) {
        for e in self.entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            if e.state == State::Paused {
                e.state = State::Waiting;
                if e.on_existing != OnExisting::Ask || e.item.exists.is_none() {
                    e.on_existing = OnExisting::Resume;
                }
            }
        }
    }

    /// Failed entries wait again (resuming unless the copy was wrong); skipped ones ask again.
    pub fn retry(&mut self, ids: &[Id]) {
        for e in self.entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            match e.state {
                State::Failed(_) => {
                    e.state = State::Waiting;
                    if e.on_existing != OnExisting::Skip {
                        e.on_existing = OnExisting::Resume;
                    }
                }
                State::Skipped => {
                    e.state = State::Waiting;
                    e.on_existing = OnExisting::Ask;
                }
                _ => {}
            }
        }
    }

    /// Drop entries that are not running.
    pub fn remove(&mut self, ids: &[Id]) {
        self.entries
            .retain(|e| !ids.contains(&e.id) || e.state == State::Running);
    }

    /// Move the given entries one place up (`-1`) or down (`1`), keeping their own order.
    pub fn shift(&mut self, ids: &[Id], by: i32) {
        let n = self.entries.len();
        let order: Vec<usize> = if by < 0 {
            (0..n).collect()
        } else {
            (0..n).rev().collect()
        };
        for i in order {
            if !ids.contains(&self.entries[i].id) {
                continue;
            }
            let j = i as i64 + by as i64;
            if j < 0 || j >= n as i64 || ids.contains(&self.entries[j as usize].id) {
                continue;
            }
            self.entries.swap(i, j as usize);
        }
    }

    /// Remove what is done or skipped.
    pub fn clear_finished(&mut self) {
        self.entries.retain(|e| !e.state.finished());
    }

    /// Remove everything not finished except the running entry, which its owner cancels.
    pub fn cancel_unfinished(&mut self) {
        self.entries
            .retain(|e| !e.unfinished() || e.state == State::Running);
    }

    /// Answer "already there" for every entry still waiting on that question.
    pub fn answer_rest(&mut self, on: OnExisting) {
        for e in &mut self.entries {
            if e.on_existing == OnExisting::Ask && e.unfinished() {
                e.on_existing = on;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transfer::Direction;
    use lc_core::{PathStyle, RemotePath};

    fn plan(names: &[&str], exists: bool) -> Plan {
        Plan {
            items: names
                .iter()
                .map(|n| Item {
                    dir: Direction::Download,
                    remote: RemotePath::parse(PathStyle::Unix, &format!("/h/{n}")),
                    local: format!("C:/x/{n}").into(),
                    size: Some(10),
                    modified: None,
                    folder: false,
                    exists: exists.then_some(Some(4)),
                    problem: None,
                })
                .collect(),
            can_resume: true,
        }
    }

    fn names(q: &Queue) -> Vec<String> {
        q.entries().iter().map(|e| e.item.name()).collect()
    }

    #[test]
    fn order_pause_resume_retry_and_clear() {
        let mut q = Queue::default();
        assert_eq!(
            q.add(plan(&["a", "b", "c", "d"], false), OnExisting::Ask),
            4
        );
        let ids: Vec<Id> = q.entries().iter().map(|e| e.id).collect();
        q.shift(&[ids[2], ids[3]], -1);
        assert_eq!(names(&q), ["a", "c", "d", "b"]);
        q.shift(&[ids[0]], -1);
        assert_eq!(names(&q), ["a", "c", "d", "b"]);
        q.shift(&[ids[0], ids[2]], 1);
        assert_eq!(names(&q), ["d", "a", "c", "b"]);
        assert_eq!(q.next_waiting(), Some(ids[3]));

        q.set_state(ids[3], State::Running);
        q.pause(&[ids[3], ids[0]]);
        assert_eq!(q.get(ids[3]).unwrap().state, State::Running);
        assert_eq!(q.get(ids[0]).unwrap().state, State::Paused);
        q.resume(&[ids[0]]);
        assert_eq!(q.get(ids[0]).unwrap().mode(), Mode::Resume);

        q.set_state(ids[3], State::Failed("lost".into()));
        q.get_mut(ids[3]).unwrap().restart = true;
        q.retry(&[ids[3]]);
        let d = q.get(ids[3]).unwrap();
        assert_eq!(
            (d.state.clone(), d.mode()),
            (State::Waiting, Mode::Overwrite)
        );

        q.set_state(ids[1], State::Done("checked".into()));
        q.set_state(ids[2], State::Running);
        q.clear_finished();
        assert_eq!(names(&q), ["d", "a", "c"]);
        q.cancel_unfinished();
        assert_eq!(names(&q), ["c"]);
        assert_eq!(q.unfinished(), 1);
    }

    #[test]
    fn already_there() {
        let mut q = Queue::default();
        q.add(plan(&["a", "b"], true), OnExisting::Ask);
        assert!(q.entries().iter().all(Entry::needs_answer));
        q.answer_rest(OnExisting::Skip);
        assert!(q.entries().iter().all(Entry::skips));
        q.add(plan(&["c"], true), OnExisting::Resume);
        assert_eq!(q.entries()[2].mode(), Mode::Resume);
    }
}
