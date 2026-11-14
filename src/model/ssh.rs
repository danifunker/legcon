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

fn options(machine: &Machine) -> lc_ssh::Options {
    lc_ssh::Options {
        charset: Charset::from_label(&machine.charset).unwrap_or_default(),
        ..lc_ssh::Options::default()
    }
}

/// Whether the file is an SSH-1 identity (SSH-1 servers take only those).
pub fn is_ssh1_key(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|d| lc_ssh::keyfile::ssh1_identity(&d).ok().flatten())
        .is_some()
}

/// The machine's login key, when it suits the protocol the server speaks.
fn auth(machine: &Machine, password: Option<String>, ssh1: bool) -> lc_ssh::Auth {
    let key = machine
        .key_file
        .as_ref()
        .filter(|p| is_ssh1_key(p) == ssh1)
        .map(|p| lc_ssh::KeyFiles {
            private: p.clone(),
            public: lc_ssh::keys::public_path(p),
        });
    lc_ssh::Auth {
        password: password.or_else(|| key.is_none().then(String::new)),
        key,
    }
}

/// Whether the machine speaks only SSH-1, from its version line.
pub fn speaks_ssh1(machine: &Machine, port: u16) -> anyhow::Result<bool> {
    let banner = lc_ssh::ssh1::banner(&machine.address, port, &options(machine))?;
    Ok(lc_ssh::ssh1::is_ssh1(&banner))
}

/// The host-key check for `machine`: learn a first key, stop on a changed one (see the module doc).
fn verifier<'a>(
    machine: &Machine,
    note: &'a KeySlot,
) -> impl FnMut(&lc_ssh::HostKey) -> lc_core::Result<()> + 'a {
    let known = machine.host_key.clone();
    let check = machine.check_host_key;
    *lock_status(note) = None;
    move |k: &lc_ssh::HostKey| {
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
    }
}

/// Connect and log in to `machine`'s SSH service on `port` over SSH-2.
pub fn connect(
    machine: &Machine,
    port: u16,
    password: Option<String>,
    note: &KeySlot,
) -> anyhow::Result<lc_ssh::SshSession> {
    let s = lc_ssh::connect(
        &machine.address,
        port,
        &machine.user,
        &auth(machine, password, false),
        &options(machine),
        &mut verifier(machine, note),
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

/// Connect and log in over SSH-1, ready for one command.
pub fn connect1(
    machine: &Machine,
    port: u16,
    password: Option<String>,
    note: &KeySlot,
) -> anyhow::Result<lc_ssh::ssh1::Conn> {
    let c = lc_ssh::ssh1::Conn::open(
        &machine.address,
        port,
        &machine.user,
        &auth(machine, password, true),
        &options(machine),
        &mut verifier(machine, note),
    )?;
    log_ssh1(machine, &c.info);
    Ok(c)
}

fn log_ssh1(machine: &Machine, i: &lc_ssh::ssh1::Ssh1Info) {
    log::info!(
        "{}: {} (SSH-1, 3DES, host key {} bits, server key {} bits; weak algorithms)",
        machine.name,
        i.banner,
        i.host_bits,
        i.server_bits
    );
}

/// The machine's files over SSH: SFTP or SCP on SSH-2, SCP and shell commands on SSH-1.
pub fn open_files(
    machine: &Machine,
    port: u16,
    password: Option<String>,
    note: &KeySlot,
) -> anyhow::Result<Box<dyn lc_core::FileSession>> {
    if speaks_ssh1(machine, port)? {
        let f = lc_ssh::ssh1::Ssh1Files::open(
            &machine.address,
            port,
            &machine.user,
            &auth(machine, password, true),
            &options(machine),
            &mut verifier(machine, note),
        )?;
        log_ssh1(machine, &f.info);
        return Ok(Box::new(f));
    }
    Ok(Box::new(connect(machine, port, password, note)?))
}

/// A terminal over SSH-1 or SSH-2, whichever the machine speaks.
pub fn open_pty(
    machine: &Machine,
    port: u16,
    password: Option<String>,
    note: &KeySlot,
    cols: u16,
    rows: u16,
) -> anyhow::Result<lc_core::Pty> {
    if speaks_ssh1(machine, port)? {
        let poll = options(machine).poll;
        return Ok(connect1(machine, port, password, note)?.into_pty("vt100", cols, rows, poll)?);
    }
    Ok(connect(machine, port, password, note)?.into_pty("vt100", cols, rows)?)
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
        lc_ssh::keys::KeyKind::Dsa1024 => "dsa",
        _ => "rsa",
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

/// "Set up key login" on an SSH-1 machine: an SSH-1 RSA key in `~/.ssh/authorized_keys`.
fn set_up_ssh1(
    m: &mut Machine,
    port: u16,
    password: Option<String>,
    chosen: Option<PathBuf>,
    keys_dir: &Path,
    note: &KeySlot,
    ctx: &super::job::JobCtx,
) -> anyhow::Result<KeySetup> {
    if let Some(p) = chosen.as_ref().filter(|p| !is_ssh1_key(p)) {
        bail!(
            "{} speaks only SSH-1, and its key {} is an SSH-2 key; choose an SSH-1 key in its settings, or none to have one made",
            m.name,
            p.display()
        );
    }
    let kind = lc_ssh::keys::KeyKind::Ssh1Rsa1024;
    let path = chosen.unwrap_or_else(|| keys_dir.join(format!("{}_rsa1", m.id)));
    let new_key = !path.exists();
    if new_key {
        ctx.set_phase("Making an SSH-1 RSA key");
        let (identity, line) = lc_ssh::keys::generate_ssh1(kind, &format!("legcon-{}", m.id))?;
        lc_ssh::keys::save_ssh1(&identity, &line, &path)?;
    }
    let line = std::fs::read_to_string(lc_ssh::keys::public_path(&path))?;
    ctx.set_phase("Installing it in ~/.ssh/authorized_keys (SSH-1)");
    let r = connect1(m, port, password, note)?.exec(
        lc_ssh::keys::install_command_ssh1(line.trim()).as_bytes(),
        b"",
    )?;
    if r.exit_status.unwrap_or(0) != 0 {
        bail!(
            "installing the key failed: {}",
            String::from_utf8_lossy(&r.errors).trim()
        );
    }
    ctx.set_phase("Logging in with the key");
    m.key_file = Some(path.clone());
    connect1(m, port, None, note)
        .context("the key was installed, but logging in with it failed")?
        .exec(b"true", b"")?;
    Ok(KeySetup {
        key_file: path,
        algorithm: "rsa1".into(),
        installed_in: "~/.ssh/authorized_keys (SSH-1)".into(),
        new_key,
    })
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
    // A key chosen in the machine's settings is used as it is; otherwise the machine gets its own.
    let chosen = m.key_file.clone().filter(|p| p.is_file());
    Job::spawn(
        format!("Set up key login on {}", m.name),
        notify,
        move |ctx| {
            let port = port(&m);
            ctx.set_phase("Logging in with the password");
            m.key_file = None;
            if speaks_ssh1(&m, port)? {
                return set_up_ssh1(&mut m, port, password, chosen, &keys_dir, &note, ctx);
            }
            if chosen.as_deref().is_some_and(is_ssh1_key) {
                bail!("{} speaks SSH-2, and this machine's key is an SSH-1 key; choose another in its settings, or none to have one made", m.name);
            }
            let mut s = connect(&m, port, password, &note)?;
            let banner = s.negotiated().banner.clone();
            let family = lc_ssh::keys::Family::of(&banner);
            let kind = lc_ssh::keys::KeyKind::for_banner(&banner);
            if let Some(p) = &chosen {
                let alg = lc_ssh::keyfile::describe(&read_blob(p)?)?.algorithm;
                if kind == lc_ssh::keys::KeyKind::Dsa1024 && alg != "ssh-dss" {
                    bail!(
                        "{banner} takes only ssh-dss (DSA) keys, and this machine's key is {alg}; choose a DSA key in its settings, or none to have one made"
                    );
                }
            }
            let path = chosen
                .clone()
                .unwrap_or_else(|| key_path(&m, kind, &keys_dir));
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
            let algorithm = lc_ssh::keyfile::describe(&blob)?.algorithm;
            Ok(KeySetup {
                key_file: path,
                algorithm,
                installed_in: family.describe().to_string(),
                new_key,
            })
        },
    )
}
