//! One-shot operations for the `lc` CLI: the same model calls the window makes, waited on.
//!
//! The CLI is a view; it reaches the model only through here and [`super::machines`].

use std::path::Path;
use std::sync::Arc;

use lc_core::{PathStyle, RemotePath};

use super::connection::Row;
use super::machines::MachinesController;
use crate::model::job::{Job, NoNotify, Notify, Status};
use crate::model::session::Sessions;

fn finish<R: Send + 'static>(job: Job<R>) -> Result<R, String> {
    match job.wait().status {
        Status::Done => job.take_result().ok_or_else(|| "no result".into()),
        Status::Failed(e) => Err(e),
        Status::Cancelled => Err("cancelled".into()),
        Status::Running => unreachable!("wait returns when the job ends"),
    }
}

/// Connect to `machine`, list `path` (its login folder when empty), log off, and return the rows.
pub fn list_remote(
    machines: &MachinesController,
    machine: &str,
    path: &str,
    password: Option<String>,
) -> Result<(String, Vec<Row>), String> {
    let m = machines
        .get(machine)
        .ok_or_else(|| format!("No machine called {machine}."))?
        .clone();
    let password = match password {
        Some(p) => Some(p),
        None => m.password.resolve(&m.id)?,
    };
    let sessions = Sessions::new();
    let quiet: Arc<dyn Notify> = Arc::new(NoNotify);
    let home = finish(sessions.connect(&m, password, Arc::clone(&quiet)))?;
    // Paths follow the machine's own style; `/` works as a separator everywhere for typing ease.
    let sep = home.style.separator();
    let typed = if home.style == PathStyle::Unix {
        path.to_string()
    } else {
        path.replace('/', &sep.to_string())
    };
    // Mac paths read the other way round: `Volume:Folder` is absolute, `:Folder` relative.
    let absolute = match home.style {
        PathStyle::Mac => !typed.starts_with(sep),
        _ => typed.starts_with(sep),
    };
    let typed = match home.style {
        PathStyle::Mac => typed.trim_start_matches(sep).to_string(),
        _ => typed,
    };
    let dir = if typed.is_empty() {
        home
    } else if absolute {
        RemotePath::parse(home.style, &typed)
    } else {
        RemotePath::parse(home.style, &format!("{home}{sep}{typed}"))
    };
    let listed = finish(sessions.list(machine, dir.clone(), Arc::clone(&quiet)));
    let logoff = finish(sessions.disconnect(machine, quiet));
    let entries = listed?;
    logoff?;
    let rows = entries
        .into_iter()
        .map(|e| Row {
            size: if e.is_folder() {
                String::new()
            } else {
                e.size.map(|s| s.to_string()).unwrap_or_default()
            },
            mode: String::new(),
            modified: String::new(),
            is_dir: e.is_folder(),
            name: e.name,
        })
        .collect();
    Ok((dir.to_string(), rows))
}

/// Set up key login on `machine` and remember the key for it; host keys are not checked here.
pub fn set_up_key_login(
    machines: &mut MachinesController,
    dir: &Path,
    machine: &str,
    password: Option<String>,
) -> Result<crate::model::ssh::KeySetup, String> {
    let m = machines
        .get(machine)
        .ok_or_else(|| format!("No machine called {machine}."))?
        .clone();
    let password = match password {
        Some(p) => Some(p),
        None => m.password.resolve(&m.id)?,
    };
    let job = crate::model::ssh::set_up_key_login(
        &m,
        password,
        dir.join("keys"),
        Default::default(),
        Arc::new(NoNotify),
    );
    let s = finish(job)?;
    machines.set_key_login(&m.id, s.key_file.clone(), false)?;
    Ok(s)
}

/// Check rsh on `machine`, and with `install` add this computer to its `~/.rhosts`; the report.
pub fn rsh_trust(
    machines: &MachinesController,
    machine: &str,
    password: Option<String>,
    install: bool,
) -> Result<String, String> {
    let m = machines
        .get(machine)
        .ok_or_else(|| format!("No machine called {machine}."))?
        .clone();
    let password = match password {
        Some(p) => Some(p),
        None => m.password.resolve(&m.id)?,
    };
    let job = crate::model::rsh_trust::rsh_trust(&m, password, install, Arc::new(NoNotify));
    let r = finish(job);
    let failed = !matches!(&r, Ok(t) if t.rsh.is_ok());
    let text = super::connection::rsh_report(&m.name, r);
    if failed {
        Err(text)
    } else {
        Ok(text)
    }
}

/// What `address` offers, found without logging in.
pub fn probe(address: &str) -> Result<super::machines::ProbeSummary, String> {
    let r = crate::model::probe::probe(address, None).map_err(|e| format!("{e:#}"))?;
    Ok((&r).into())
}

/// Set (or with `None` clear) the app's home folder.
pub fn set_home(dir: &Path, home: Option<&Path>) -> Result<(), String> {
    let mut s = crate::model::settings::Settings::load(dir).map_err(|e| format!("{e:#}"))?;
    if let Some(h) = home {
        if !h.is_dir() {
            return Err(format!("{} is not a folder.", h.display()));
        }
    }
    s.home_folder = home.map(Path::to_path_buf);
    s.save(dir).map_err(|e| format!("{e:#}"))
}

/// The app's home folder, if set.
pub fn home(dir: &Path) -> Result<Option<String>, String> {
    let s = crate::model::settings::Settings::load(dir).map_err(|e| format!("{e:#}"))?;
    Ok(s.home_folder.map(|p| p.display().to_string()))
}

/// A machine's settings form, as `lc machines show` prints it.
pub fn machine_form(machines: &MachinesController, id: &str) -> Result<super::FormSpec, String> {
    machines
        .form(id)
        .ok_or_else(|| format!("No machine called {id}."))
}

/// Change settings by `key=value` (keys as `lc machines show` prints them) and save.
pub fn set_machine(
    machines: &mut MachinesController,
    id: &str,
    pairs: &[String],
) -> Result<(), String> {
    let spec = machine_form(machines, id)?;
    let mut values = spec.initial();
    for p in pairs {
        let (key, value) = p
            .split_once('=')
            .ok_or_else(|| format!("{p:?} is not key=value."))?;
        let key = key.trim();
        let full = spec
            .fields
            .iter()
            .map(|f| f.id)
            .find(|f| *f == key || f.strip_prefix("machine.") == Some(key))
            .ok_or_else(|| format!("No setting called {key}."))?;
        super::machine_form::set_from_text(&spec, &mut values, full, value)?;
    }
    spec.validate(&values)?;
    machines.edit(id, &values).map(|_| ())
}

/// Key kinds by their CLI names, in the order `lc keys new --help` lists them.
pub const KEY_KINDS: [(&str, lc_ssh::keys::KeyKind); 5] = [
    ("rsa2048", lc_ssh::keys::KeyKind::Rsa2048),
    ("rsa1024", lc_ssh::keys::KeyKind::Rsa1024),
    ("dsa1024", lc_ssh::keys::KeyKind::Dsa1024),
    ("ssh1-rsa1024", lc_ssh::keys::KeyKind::Ssh1Rsa1024),
    ("ssh1-rsa2048", lc_ssh::keys::KeyKind::Ssh1Rsa2048),
];

/// The key list as `lc keys list` prints it.
pub fn key_rows(machines: &MachinesController) -> Vec<super::ssh_keys::KeyRow> {
    let keys = super::ssh_keys::KeysController::new(
        machines.keys_dir(),
        std::sync::Arc::new(super::NoNotify),
    );
    keys.rows(machines)
}

/// Make a key and wait for it; its kind label.
pub fn new_key(
    machines: &MachinesController,
    name: &str,
    kind: &str,
    comment: &str,
) -> Result<String, String> {
    let kind = KEY_KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, k)| *k)
        .ok_or_else(|| {
            format!(
                "unknown kind {kind:?}; one of {}",
                KEY_KINDS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let job = crate::model::keystore::generate_job(
        &machines.keys_dir(),
        name,
        kind,
        comment,
        std::sync::Arc::new(super::NoNotify),
    )
    .map_err(|e| format!("{e:#}"))?;
    job.wait();
    super::job_outcome(&job)
        .unwrap_or_else(|| Err("not finished".into()))
        .map(|k| k.kind_label())
}

pub fn import_key(
    machines: &MachinesController,
    file: &Path,
    name: Option<&str>,
) -> Result<String, String> {
    let dir = machines.keys_dir();
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.map_or_else(
        || crate::model::keystore::free_name(&dir, &stem),
        str::to_string,
    );
    crate::model::keystore::import(&dir, file, &name)
        .map(|k| format!("{} ({})", k.name, k.kind_label()))
        .map_err(|e| format!("{e:#}"))
}

fn find_key(
    machines: &MachinesController,
    name: &str,
) -> Result<crate::model::keystore::KeyInfo, String> {
    machines
        .keys()
        .into_iter()
        .find(|k| k.name == name)
        .ok_or_else(|| format!("No key called {name}."))
}

/// The files written.
pub fn export_key(
    machines: &MachinesController,
    name: &str,
    folder: &Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    crate::model::keystore::export(&find_key(machines, name)?, folder).map_err(|e| format!("{e:#}"))
}

/// The public line, then the fingerprints.
pub fn show_key(machines: &MachinesController, name: &str) -> Result<String, String> {
    let k = find_key(machines, name)?;
    Ok(format!(
        "{}\n{}  {}\nMD5 {}\nfile {}\n",
        k.openssh_line(),
        k.kind_label(),
        k.sha256,
        k.md5,
        k.path.display()
    ))
}

pub fn delete_key(machines: &mut MachinesController, name: &str) -> Result<(), String> {
    let k = find_key(machines, name)?;
    crate::model::keystore::delete(&k).map_err(|e| format!("{e:#}"))?;
    machines.drop_key(&k.path)
}
