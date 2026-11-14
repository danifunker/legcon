//! One-shot operations for the `lc` CLI: the same model calls the window makes, waited on.
//!
//! The CLI is a view; it reaches the model only through here and [`super::machines`].

use std::path::Path;
use std::sync::Arc;

use lc_core::{PathStyle, RemotePath};

use super::connection::Row;
use super::machines::MachinesController;
use crate::model::job::{Job, NoNotify, Notify, Status};
use crate::model::machines::Password;
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
    let password = password.or(match &m.password {
        Password::Saved(p) => Some(p.clone()),
        Password::Ask => None,
    });
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
    let dir = if typed.is_empty() {
        home
    } else if typed.starts_with(sep) {
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
    let password = password.or(match &m.password {
        Password::Saved(p) => Some(p.clone()),
        Password::Ask => None,
    });
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
