//! Opening SSH connections for a machine, with its host key checked (docs/PLAN.md §5.1).
//!
//! The key a machine showed last time is kept in its `host_key` (`ssh-dss AAAA...`). A first
//! connection learns it; a different one stops the connection before the password is sent and
//! leaves a [`KeyNote::Changed`] so the controller can ask: accept the new key, accept it once,
//! or cancel. `check_host_key: false` on the machine skips all of it, for lab machines.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use lc_core::Charset;

use super::job::{Job, Notify};
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
    let key = machine.key_file.as_ref().map(|p| lc_ssh::KeyFiles {
        private: p.clone(),
        public: lc_ssh::keys::public_path(p),
    });
    let auth = lc_ssh::Auth {
        password: password.or_else(|| key.is_none().then(String::new)),
        key,
    };
    let s = lc_ssh::connect(
        &machine.address,
        port,
        &machine.user,
        &auth,
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

/// The machine's SSH port: its SSH service's, or 22.
pub fn port(machine: &Machine) -> u16 {
    machine
        .services
        .iter()
        .find(|s| s.kind == super::machines::ServiceKind::Ssh)
        .map_or(lc_ssh::PORT, |s| s.port)
}

/// Where a machine's own login key lives: `<machine id>_rsa` (or `_dsa`) in `keys_dir`.
pub fn key_path(machine: &Machine, kind: lc_ssh::keys::KeyKind, keys_dir: &Path) -> PathBuf {
    let suffix = match kind {
        lc_ssh::keys::KeyKind::Rsa2048 => "rsa",
        lc_ssh::keys::KeyKind::Dsa1024 => "dsa",
    };
    keys_dir.join(format!("{}_{suffix}", machine.id))
}

/// The public key blob in a `.pub` file's OpenSSH line.
fn read_blob(private: &Path) -> anyhow::Result<Vec<u8>> {
    let pub_path = lc_ssh::keys::public_path(private);
    let text = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("reading {}", pub_path.display()))?;
    let b64 = text
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("{} is not an OpenSSH public key", pub_path.display()))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
}

/// What "Set up key login" did, for the controller to report and remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySetup {
    pub key_file: PathBuf,
    /// `ssh-rsa` or `ssh-dss`.
    pub algorithm: String,
    /// Where it was installed, such as `~/.ssh/authorized_keys2`.
    pub installed_in: String,
    /// The key was made now rather than reused.
    pub new_key: bool,
}

/// Log in with the password, install this machine's key (made if needed) the way the server
/// wants it, then prove it by logging in again with the key alone (docs/PLAN.md §5.1).
pub fn set_up_key_login(
    machine: &Machine,
    password: Option<String>,
    keys_dir: PathBuf,
    note: KeySlot,
    notify: Arc<dyn Notify>,
) -> Job<KeySetup> {
    let mut m = machine.clone();
    Job::spawn(
        format!("Set up key login on {}", m.name),
        notify,
        move |ctx| {
            let port = port(&m);
            ctx.set_phase("Logging in with the password");
            m.key_file = None;
            let mut s = connect(&m, port, password, &note)?;
            let banner = s.negotiated().banner.clone();
            let family = lc_ssh::keys::Family::of(&banner);
            let kind = lc_ssh::keys::KeyKind::for_banner(&banner);
            let path = key_path(&m, kind, &keys_dir);
            let new_key = !path.exists();
            if new_key {
                ctx.set_phase(format!("Making a {} key", kind.algorithm()));
                let pair = lc_ssh::keys::generate(kind)?;
                lc_ssh::keys::save(&pair, &path, &format!("legcon-{}", m.id))?;
            }
            let blob = read_blob(&path)?;
            ctx.set_phase(format!("Installing it in {}", family.describe()));
            let cmd = lc_ssh::keys::install_command(family, &blob, &format!("legcon-{}", m.id));
            let r = s.run(cmd.as_bytes())?;
            if r.exit_status.unwrap_or(0) != 0 {
                bail!(
                    "installing the key failed: {}",
                    String::from_utf8_lossy(&r.errors).trim()
                );
            }
            let _ = lc_core::Connection::disconnect(&mut s, super::session::LOGOFF_TIMEOUT);
            ctx.set_phase("Logging in with the key");
            m.key_file = Some(path.clone());
            let mut s = connect(&m, port, None, &note)
                .context("the key was installed, but logging in with it failed")?;
            let _ = lc_core::Connection::disconnect(&mut s, super::session::LOGOFF_TIMEOUT);
            Ok(KeySetup {
                key_file: path,
                algorithm: kind.algorithm().to_string(),
                installed_in: family.describe().to_string(),
                new_key,
            })
        },
    )
}
