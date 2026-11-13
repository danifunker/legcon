//! Opening SSH connections for a machine, with its host key checked (docs/PLAN.md §5.1).
//!
//! The key a machine showed last time is kept in its `host_key` (`ssh-dss AAAA...`). A first
//! connection learns it; a different one stops the connection before the password is sent and
//! leaves a [`KeyNote::Changed`] so the controller can ask: accept the new key, accept it once,
//! or cancel. `check_host_key: false` on the machine skips all of it, for lab machines.

use std::sync::{Arc, Mutex};

use base64::Engine;
use lc_core::Charset;

use super::machines::Machine;
use super::worker::lock_status;

/// What the host key check found, for the controller to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyNote {
    /// First connection: this key is now the machine's.
    Learned(String),
    /// The machine showed a different key than last time; nothing was sent.
    Changed {
        line: String,
        md5: String,
        sha256: String,
    },
}

/// Where a connect leaves its [`KeyNote`].
pub type KeySlot = Arc<Mutex<Option<KeyNote>>>;

/// Take what the last connect left in `slot`.
pub fn take_note(slot: &KeySlot) -> Option<KeyNote> {
    lock_status(slot).take()
}

fn line(k: &lc_ssh::HostKey) -> String {
    format!(
        "{} {}",
        k.algorithm,
        base64::engine::general_purpose::STANDARD.encode(&k.blob)
    )
}

/// Connect and log in to `machine`'s SSH service on `port`.
pub fn connect(
    machine: &Machine,
    port: u16,
    password: Option<String>,
    note: &KeySlot,
) -> anyhow::Result<lc_ssh::SshSession> {
    let opts = lc_ssh::Options {
        charset: Charset::from_label(&machine.charset).unwrap_or_default(),
        ..lc_ssh::Options::default()
    };
    let known = machine.host_key.clone();
    let check = machine.check_host_key;
    *lock_status(note) = None;
    let mut verify = |k: &lc_ssh::HostKey| {
        let seen = line(k);
        match &known {
            _ if !check => Ok(()),
            None => {
                *lock_status(note) = Some(KeyNote::Learned(seen));
                Ok(())
            }
            Some(old) if *old == seen => Ok(()),
            Some(_) => {
                *lock_status(note) = Some(KeyNote::Changed {
                    line: seen,
                    md5: k.md5.clone(),
                    sha256: k.sha256.clone(),
                });
                Err(lc_core::Error::Denied(
                    "the machine's host key has changed since last time".into(),
                ))
            }
        }
    };
    let s = lc_ssh::connect(
        &machine.address,
        port,
        &machine.user,
        &password.unwrap_or_default(),
        &opts,
        &mut verify,
    )?;
    let n = s.negotiated();
    log::info!(
        "{}: {} ({}, {}, {}, {}){}",
        machine.name,
        n.banner,
        n.kex,
        n.host_key,
        n.cipher,
        n.mac,
        if n.weak() { "; weak algorithms" } else { "" }
    );
    Ok(s)
}
