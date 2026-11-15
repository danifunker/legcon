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
pub mod machine_form;
pub mod machines;
pub mod options;
pub mod picks;
pub mod ssh_keys;
pub mod task;

use std::path::PathBuf;

/// What views may name from below the controller layer: plain data and the wake trait.
pub use crate::model::job::{NoNotify, Notify, Snapshot, Status};
pub use crate::model::machines::{Layout, ServiceKind, TabColor};
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
    /// An existing file to open (Import key); answer with the controller's `...Picked`.
    PickFile(PickSpec),
    /// A local folder path to type, paste or browse for, with recent places (Leave home...).
    AskPath(PathSpec),
    Confirm(ConfirmSpec),
    AskPassword(PasswordSpec),
    /// A one-line text prompt; answer with the controller's `...Answered` action.
    AskText(TextSpec),
    /// One of several buttons; answer with the controller's `Chose`-style action.
    Choose(ChoiceSpec),
    /// A dialog of labelled fields (settings); answer with the controller's `...FormAnswered`.
    Form(FormSpec),
    /// A popup menu under a control (Places); answer with the controller's `...Chosen`.
    Menu(MenuSpec),
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

/// A popup menu shown under the control `anchor` (a widget id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuSpec {
    pub purpose: &'static str,
    pub anchor: &'static str,
    pub items: Vec<MenuItem>,
}

/// One line of a popup menu; the answer is an index into the items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    Item {
        label: String,
        enabled: bool,
    },
    /// A greyed label over a group, such as Pinned.
    Heading(String),
    Separator,
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

/// A dialog of fields every shell draws the same way: label on the left, control on the right.
#[derive(Debug, Clone)]
pub struct FormSpec {
    pub purpose: &'static str,
    pub title: String,
    pub fields: Vec<Field>,
    pub ok_label: String,
    /// The whole form's rule, run as the user types and again on the answer.
    pub validator: fn(&FormValues) -> Result<(), String>,
}

/// One row of a form; `id` is also the control's widget id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub id: &'static str,
    pub label: String,
    pub kind: FieldKind,
    /// A grey line under the control; empty for none.
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// A section title across the form, with no control.
    Heading,
    Text(String),
    /// Text shown as dots.
    Secret(String),
    /// A local folder: a text box and Browse...
    Folder(String),
    Check(bool),
    Choice {
        options: Vec<String>,
        selected: usize,
    },
    /// Read-only text the user can select and copy.
    Info(String),
}

/// A field's value as the user left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Text(String),
    Check(bool),
    Choice(usize),
    None,
}

/// A form's answer, by field id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FormValues(pub Vec<(&'static str, FieldValue)>);

impl FieldKind {
    /// The value this control starts with.
    pub fn value(&self) -> FieldValue {
        match self {
            FieldKind::Text(t) | FieldKind::Secret(t) | FieldKind::Folder(t) => {
                FieldValue::Text(t.clone())
            }
            FieldKind::Check(b) => FieldValue::Check(*b),
            FieldKind::Choice { selected, .. } => FieldValue::Choice(*selected),
            FieldKind::Heading | FieldKind::Info(_) => FieldValue::None,
        }
    }
}

impl FormSpec {
    /// The values the form opens with.
    pub fn initial(&self) -> FormValues {
        FormValues(self.fields.iter().map(|f| (f.id, f.kind.value())).collect())
    }

    pub fn validate(&self, values: &FormValues) -> Result<(), String> {
        (self.validator)(values)
    }
}

impl FormValues {
    fn get(&self, id: &str) -> Option<&FieldValue> {
        self.0.iter().find(|(i, _)| *i == id).map(|(_, v)| v)
    }

    /// A text field's value; empty when the form has no such field.
    pub fn text(&self, id: &str) -> &str {
        match self.get(id) {
            Some(FieldValue::Text(t)) => t,
            _ => "",
        }
    }

    pub fn check(&self, id: &str) -> bool {
        matches!(self.get(id), Some(FieldValue::Check(true)))
    }

    pub fn choice(&self, id: &str) -> usize {
        match self.get(id) {
            Some(FieldValue::Choice(i)) => *i,
            _ => 0,
        }
    }

    pub fn has(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Change one value (a view as the user edits, a test, the CLI).
    pub fn set(&mut self, id: &str, value: FieldValue) {
        if let Some(slot) = self.0.iter_mut().find(|(i, _)| *i == id) {
            slot.1 = value;
        }
    }
}

impl PartialEq for FormSpec {
    fn eq(&self, o: &Self) -> bool {
        (self.purpose, &self.title, &self.fields, &self.ok_label)
            == (o.purpose, &o.title, &o.fields, &o.ok_label)
            && self.validator as usize == o.validator as usize
    }
}

impl Eq for FormSpec {}

/// Accept any non-empty text.
pub fn not_empty(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err(String::new())
    } else {
        Ok(())
    }
}
