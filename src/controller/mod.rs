//! Controllers: one per task, screen or dialog, shared by every view (CONTRIBUTING.md § Architecture policy).
//!
//! A view (the Win32 shell, the `lc` CLI) owns a controller, sends it user intents as
//! `Action`s, draws from its read accessors, and carries out the [`Effect`]s it returns,
//! replying with another `Action` where the effect has an answer. Slow work goes through
//! [`crate::model::job`], so a view only ever sees a wait state through a job snapshot.
//!
//! Rules (from rusty-backup):
//! - Nothing toolkit-specific here; `scripts/check-boundaries.sh` enforces it.
//! - `Action` and [`Effect`] enums are exhaustive and views match them with no `_` arm.
//! - Every actionable element has a stable widget id ([`ids`]), set as its accessibility id.
//! - Shortcuts are data in [`keys`], never hard-coded in a view.

pub mod app;
pub mod cli_ops;
pub mod connection;
pub mod ids;
pub mod keys;
pub mod log;
pub mod machines;
pub mod picks;
pub mod task;

use std::path::PathBuf;

/// What views may name from below the controller layer: plain data and the wake trait.
pub use crate::model::job::{NoNotify, Notify, Snapshot, Status};
pub use crate::model::machines::{Layout, ServiceKind};
pub use lc_core::ConnState;

/// A job's outcome once it ended: its result, or why it failed or was cancelled.
pub(crate) fn job_outcome<R: Send + 'static>(
    job: &crate::model::job::Job<R>,
) -> Option<Result<R, String>> {
    match job.snapshot().status {
        Status::Running => None,
        Status::Done => Some(job.take_result().ok_or_else(|| "no result".to_string())),
        Status::Failed(e) => Some(Err(e)),
        Status::Cancelled => Some(Err("cancelled".to_string())),
    }
}

/// The shape every controller shares.
pub trait Controller {
    type Action;

    fn apply(&mut self, action: Self::Action) -> Vec<Effect>;

    /// Called by the shell when woken or about to draw: drains finished jobs.
    fn tick(&mut self) -> Vec<Effect>;
}

/// Something only the shell can do; it reports any answer back as an `Action`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    PickFolder(PickSpec),
    /// A local folder path to type, paste or browse for, with recent places (Leave home...).
    AskPath(PathSpec),
    Confirm(ConfirmSpec),
    AskPassword(PasswordSpec),
    /// A one-line text prompt; answer with the controller's `...Answered` action.
    AskText(TextSpec),
    /// One of several buttons; answer with the controller's `Chose`-style action.
    Choose(ChoiceSpec),
    /// A transient message for the user (status line or alert).
    Message(String),
    CopyToClipboard(String),
    /// Quit the application; every machine has been logged off.
    Quit,
}

/// A file or folder picker; `purpose` says which `Action` the answer belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickSpec {
    pub purpose: &'static str,
    pub title: String,
    /// Where the dialog opens; empty for the system's choice.
    pub start: Option<PathBuf>,
}

/// A path prompt: a text box, a Browse... button (the system folder picker) and recent places.
#[derive(Debug, Clone)]
pub struct PathSpec {
    pub purpose: &'static str,
    pub title: String,
    pub prompt: String,
    pub initial: String,
    pub recent: Vec<PathBuf>,
    /// A line under the box, such as where Home is.
    pub hint: String,
    pub validator: fn(&str) -> Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmSpec {
    pub purpose: &'static str,
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub destructive: bool,
}

/// A question with several answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceSpec {
    pub purpose: &'static str,
    pub title: String,
    pub message: String,
    /// The button labels in order; the answer is an index into this list.
    pub choices: Vec<String>,
    /// The answer Esc (or closing the window) gives.
    pub dismiss: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordSpec {
    pub purpose: &'static str,
    pub prompt: String,
}

/// A one-line text prompt whose rule the controller defines and the view runs as the user types.
#[derive(Debug, Clone)]
pub struct TextSpec {
    pub purpose: &'static str,
    pub title: String,
    pub prompt: String,
    pub initial: String,
    pub ok_label: String,
    /// `Err("")` disables OK without a message (an empty field).
    pub validator: fn(&str) -> Result<(), String>,
}

// Spelled out: comparing fn pointers directly trips rustc's unpredictable-comparison lint.
impl PartialEq for TextSpec {
    fn eq(&self, o: &Self) -> bool {
        (
            self.purpose,
            &self.title,
            &self.prompt,
            &self.initial,
            &self.ok_label,
        ) == (o.purpose, &o.title, &o.prompt, &o.initial, &o.ok_label)
            && self.validator as usize == o.validator as usize
    }
}

impl Eq for TextSpec {}

impl PartialEq for PathSpec {
    fn eq(&self, o: &Self) -> bool {
        (
            self.purpose,
            &self.title,
            &self.prompt,
            &self.initial,
            &self.recent,
            &self.hint,
        ) == (
            o.purpose, &o.title, &o.prompt, &o.initial, &o.recent, &o.hint,
        ) && self.validator as usize == o.validator as usize
    }
}

impl Eq for PathSpec {}

impl TextSpec {
    pub fn validate(&self, text: &str) -> Result<(), String> {
        (self.validator)(text)
    }
}

impl PathSpec {
    pub fn validate(&self, text: &str) -> Result<(), String> {
        (self.validator)(text)
    }
}

/// Accept any non-empty text.
pub fn not_empty(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err(String::new())
    } else {
        Ok(())
    }
}
