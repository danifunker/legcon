//! The saved machines as every view shows and edits them (the Machines tab, `lc machines`).

use std::path::{Path, PathBuf};

use crate::model::machines::{check_id, Machine, MachineStore, Password, Service};
use crate::model::session::Sessions;

pub use crate::model::machines::{Layout, ServiceKind, TabColor};
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
    pub color: TabColor,
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

/// What a probe found, as the views show it and Add machine uses it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeSummary {
    pub address: String,
    /// One line per service that answered.
    pub lines: Vec<String>,
    /// A guess such as `Windows NT (IIS)`.
    pub system: Option<String>,
    /// The services to set up, best file service first, with their ports.
    pub services: Vec<(ServiceKind, u16)>,
    pub charset: String,
}

impl From<&crate::model::probe::Report> for ProbeSummary {
    fn from(r: &crate::model::probe::Report) -> Self {
        ProbeSummary {
            address: r.address.clone(),
            lines: r.lines(),
            system: r.system.clone(),
            services: r.services(),
            charset: r.charset.clone(),
        }
    }
}

impl ProbeSummary {
    /// The message Add machine shows.
    pub fn message(&self) -> String {
        if self.lines.is_empty() {
            return format!(
                "Nothing on {} answered on the usual ports. It may be switched off, behind a \
                 firewall, or offer only what legcon cannot see without a login.",
                self.address
            );
        }
        let mut m = format!("Found on {}:\n", self.address);
        for l in &self.lines {
            m.push_str(&format!("  {l}\n"));
        }
        if let Some(s) = &self.system {
            m.push_str(&format!("\nLooks like {s}; names in {}.", self.charset));
        }
        m
    }
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
                password_saved: m.password != Password::Ask,
                color: m.color,
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
        // Classic Mac servers name files in MacRoman.
        if m.file_service().is_some_and(|s| s.kind == ServiceKind::Afp) {
            m.charset = lc_core::Charset::MacRoman.label().to_string();
        }
        self.store.add(m)?;
        self.save()
    }

    /// Use the probe's ports and character set for a machine just added.
    pub fn apply_probe(&mut self, id: &str, p: &ProbeSummary) -> Result<(), String> {
        let Some(m) = self.store.get_mut(id) else {
            return Err(format!("No machine called {id}."));
        };
        for s in m.services.iter_mut() {
            if let Some((_, port)) = p.services.iter().find(|(k, _)| *k == s.kind) {
                s.port = *port;
            }
        }
        if !p.charset.is_empty() {
            m.charset = p.charset.clone();
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if self
            .store
            .get(id)
            .is_some_and(|m| m.password == Password::Os)
        {
            // A store that cannot be reached must not keep the machine from going.
            let _ = crate::model::secrets::delete(id);
        }
        if !self.store.remove(id) {
            return Err(format!("No machine called {id}."));
        }
        self.save()
    }

    /// Save a machine settings form; the edited machine, or why not.
    pub fn edit(&mut self, id: &str, v: &super::FormValues) -> Result<Machine, String> {
        let m = self
            .store
            .get_mut(id)
            .ok_or_else(|| format!("No machine called {id}."))?;
        let mut edited = m.clone();
        super::machine_form::apply(&mut edited, v)?;
        *m = edited.clone();
        self.save()?;
        Ok(edited)
    }

    /// The uid and gid NFS sends for this machine.
    pub fn set_unix_ids(
        &mut self,
        id: &str,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), String> {
        let Some(m) = self.store.get_mut(id) else {
            return Err(format!("No machine called {id}."));
        };
        m.uid = uid;
        m.gid = gid;
        self.save()
    }

    /// Give a machine's tab another colour.
    pub fn set_color(&mut self, id: &str, color: TabColor) -> Result<(), String> {
        match self.store.get_mut(id) {
            Some(m) if m.color != color => {
                m.color = color;
                self.save()
            }
            Some(_) => Ok(()),
            None => Err(format!("No machine called {id}.")),
        }
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

    /// Remember the SSH host key a machine showed (learned, or accepted after a change).
    pub fn set_host_key(&mut self, id: &str, key: String) -> Result<(), String> {
        match self.store.get_mut(id) {
            Some(m) if m.host_key.as_deref() != Some(&key) => {
                m.host_key = Some(key);
                self.save()
            }
            _ => Ok(()),
        }
    }

    /// Log in to this machine with `key` from now on; `forget_password` stops saving the password.
    pub fn set_key_login(
        &mut self,
        id: &str,
        key: std::path::PathBuf,
        forget_password: bool,
    ) -> Result<(), String> {
        let Some(m) = self.store.get_mut(id) else {
            return Ok(());
        };
        m.key_file = Some(key);
        if forget_password {
            if m.password == Password::Os {
                crate::model::secrets::delete(id)?;
            }
            m.password = Password::Ask;
        }
        self.save()
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
