//! Panic-safe helpers next to [`super::job`]: a poison-tolerant lock and a detached guarded thread.

use std::sync::{Mutex, MutexGuard};

/// Lock a status even when a panicking worker poisoned it.
pub fn lock_status<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Spawn a thread no status watches; a panic or error is logged instead of lost.
pub fn spawn_detached(
    what: &'static str,
    body: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(super::job::Failure::Error(message)) = super::job::catch(what, body) {
            log::error!("{message}");
        }
    })
}
