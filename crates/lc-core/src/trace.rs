//! A protocol trace for the Log tab: what was sent (passwords masked), received, or a note.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    Sent(String),
    Received(String),
    Note(String),
}

/// A trace shared between a connection and whoever shows it.
pub type TraceLog = Arc<Mutex<Vec<Trace>>>;

/// Add a line, keeping the log bounded; the Log tab shows the recent end.
pub fn push(log: &TraceLog, t: Trace) {
    if let Ok(mut v) = log.lock() {
        if v.len() >= 2000 {
            v.drain(..500);
        }
        v.push(t);
    }
}

/// Add a note to `log`, if there is one.
pub fn note(log: Option<&TraceLog>, text: impl Into<String>) {
    if let Some(l) = log {
        push(l, Trace::Note(text.into()));
    }
}
