//! The saved machines as every view shows and edits them (the Machines tab, `lc machines`).

use std::path::{Path, PathBuf};

use crate::model::machines::{check_id, Machine, MachineStore, Password, Service};
use crate::model::session::Sessions;

pub use crate::model::machines::{Layout, ServiceKind};
pub use lc_core::ConnState;

/// One row of the machine list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRow {
    pub id: String,
    pub name: String,
    pub address: String,
    /// `FTP, Telnet`.
    pub services: String,
    pub state: ConnState,
    pub password_saved: bool,
}

/// A machine to add, as the CLI's flags or the Add machine prompts give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMachine {
    pub id: String,
    pub address: String,
    pub services: Vec<ServiceKind>,
    pub user: String,
    /// `Some` saves it in `machines.json` (Base64, not encrypted); `None` asks at connect.
    pub password: Option<String>,
}

/// The machine store and its file.
pub struct MachinesController {
    dir: PathBuf,
    store: MachineStore,
}

impl MachinesController {
    /// Load `machines.json` from `dir` (the config folder).
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        Ok(MachinesController {
            dir: dir.to_path_buf(),
            store: MachineStore::load(dir)?,
        })
    }

    pub fn rows(&self, sessions: &Sessions) -> Vec<MachineRow> {
        self.store
            .machines
            .iter()
            .map(|m| MachineRow {
                id: m.id.clone(),
                name: m.name.clone(),
                address: m.address.clone(),
                services: m
                    .services
                    .iter()
                    .map(|s| s.kind.label())
                    .collect::<Vec<_>>()
                    .join(", "),
                state: sessions.state(&m.id),
                password_saved: matches!(m.password, Password::Saved(_)),
            })
            .collect()
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Machine> {
        self.store.get(id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.store.machines.iter().map(|m| m.id.clone()).collect()
    }

    /// Add and save; the message says why not.
    pub fn add(&mut self, new: NewMachine) -> Result<(), String> {
        let mut m = Machine::new(&new.id, &new.address);
        m.services = new
            .services
            .iter()
            .map(|&kind| Service {
                kind,
                port: kind.default_port(),
            })
            .collect();
        m.user = new.user;
        m.password = new.password.map(Password::Saved).unwrap_or(Password::Ask);
        self.store.add(m)?;
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if !self.store.remove(id) {
            return Err(format!("No machine called {id}."));
        }
        self.save()
    }

    /// Remember a machine's layout after the user changed it.
    pub fn set_layout(&mut self, id: &str, layout: Layout) -> Result<(), String> {
        match self.store.get_mut(id) {
            Some(m) if m.layout != layout => {
                m.layout = layout;
                self.save()
            }
            _ => Ok(()),
        }
    }

    fn save(&self) -> Result<(), String> {
        self.store
            .save(&self.dir)
            .map_err(|e| format!("Could not save the machines: {e:#}"))
    }
}

/// The rule for a machine's name, for prompts.
pub fn validate_id(text: &str) -> Result<(), String> {
    check_id(text.trim())
}

/// A name for a machine from its address: `sparc.lan` -> `sparc`, `192.168.1.40` -> `192-168-1-40`.
pub fn suggest_id(address: &str) -> String {
    let a = address.trim();
    let base = if a.chars().all(|c| c.is_ascii_digit() || c == '.') {
        a.replace('.', "-")
    } else {
        a.split('.').next().unwrap_or(a).to_string()
    };
    base.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = MachinesController::open(dir.path()).unwrap();
        c.add(NewMachine {
            id: "sparc".into(),
            address: "192.168.1.40".into(),
            services: vec![ServiceKind::Ftp, ServiceKind::Telnet],
            user: "dani".into(),
            password: Some("pw".into()),
        })
        .unwrap();
        let rows = c.rows(&Sessions::new());
        assert_eq!(rows[0].services, "FTP, Telnet");
        assert!(rows[0].password_saved);
        let again = MachinesController::open(dir.path()).unwrap();
        assert_eq!(again.ids(), ["sparc"]);
        c.remove("sparc").unwrap();
        assert!(c.remove("sparc").is_err());
    }

    #[test]
    fn suggested_ids() {
        assert_eq!(suggest_id("sparc.lan"), "sparc");
        assert_eq!(suggest_id("192.168.1.40"), "192-168-1-40");
        assert!(validate_id(&suggest_id("my box.local")).is_ok());
    }
}
