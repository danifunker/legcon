//! The window: the Machines tab, one tab per connection, and the first-run home folder.
//!
//! Closing a connection tab logs that machine off cleanly (PLAN.md §1.4); Quit logs every
//! machine off first. The machine list's Add machine is a short run of prompts until its
//! own dialog is designed (mockup 4).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::connection::{self, ConnectionController};
use super::log::LogStore;
use super::machines::ProbeSummary;
use super::machines::{suggest_id, validate_id, MachineRow, MachinesController, NewMachine};
use super::{not_empty, ChoiceSpec, Controller, Effect, PickSpec, ServiceKind, TabColor, TextSpec};
use crate::model::config;
use crate::model::job::{Job, Notify};
use crate::model::probe::probe_job;
use crate::model::session::Sessions;
use crate::model::settings::Settings;

/// Which tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Machines,
    Connection(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Once, when the window is up: asks for the home folder on the first run.
    Start,
    HomePicked(Option<PathBuf>),
    ShowMachines,
    SelectTab(usize),
    /// Open (or show) a machine's tab.
    OpenMachine(String),
    /// Close a connection tab, logging the machine off.
    CloseTab(usize),
    AddMachine,
    AddAddressAnswered(Option<String>),
    AddServiceChosen(usize),
    /// The answer to what the probe found: add with those services, choose one, cancel.
    AddFoundAnswered(usize),
    AddNameAnswered(Option<String>),
    RemoveMachine(String),
    RemoveConfirmed(bool),
    DisconnectAll,
    /// Give a machine's tab another colour (from its tab's context menu).
    SetColor(String, TabColor),
    Quit,
    /// An action for a connection tab.
    Conn(usize, connection::Action),
}

/// The services Add machine offers, in order.
const ADD_SERVICES: [ServiceKind; 12] = [
    ServiceKind::Ftp,
    ServiceKind::Smb,
    ServiceKind::Ssh,
    ServiceKind::Telnet,
    ServiceKind::Rexec,
    ServiceKind::Rsh,
    ServiceKind::Rlogin,
    ServiceKind::Nfs,
    ServiceKind::Afp,
    ServiceKind::Http,
    ServiceKind::Gopher,
    ServiceKind::Demo,
];

/// A machine being added: its address, then what the probe found or the service chosen.
struct Adding {
    address: String,
    kind: Option<ServiceKind>,
    probe: Option<Job<crate::model::probe::Report>>,
    found: Option<ProbeSummary>,
}

pub struct AppController {
    dir: PathBuf,
    settings: Settings,
    machines: MachinesController,
    sessions: Sessions,
    notify: Arc<dyn Notify>,
    tabs: Vec<ConnectionController>,
    active: Tab,
    log: LogStore,
    closing: Vec<Job<()>>,
    quitting: bool,
    adding: Option<Adding>,
    removing: Option<String>,
}

impl AppController {
    /// Load settings and machines from `dir` (normally [`default_dir`]).
    pub fn open(dir: &Path, notify: Arc<dyn Notify>) -> anyhow::Result<Self> {
        Ok(AppController {
            dir: dir.to_path_buf(),
            settings: Settings::load(dir)?,
            machines: MachinesController::open(dir)?,
            sessions: Sessions::new(),
            notify,
            tabs: Vec::new(),
            active: Tab::Machines,
            log: LogStore::default(),
            closing: Vec::new(),
            quitting: false,
            adding: None,
            removing: None,
        })
    }

    pub fn active(&self) -> Tab {
        self.active
    }

    pub fn tabs(&self) -> &[ConnectionController] {
        &self.tabs
    }

    pub fn tab(&self, i: usize) -> Option<&ConnectionController> {
        self.tabs.get(i)
    }

    pub fn machines(&self) -> Vec<MachineRow> {
        self.machines.rows(&self.sessions)
    }

    pub fn log(&self) -> &LogStore {
        &self.log
    }

    pub fn home_folder(&self) -> Option<&Path> {
        self.settings.home_folder.as_deref()
    }

    fn home_or_fallback(&self) -> PathBuf {
        self.settings
            .home_folder
            .clone()
            .or_else(dirs::document_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn save_settings(&mut self) -> Vec<Effect> {
        match self.settings.save(&self.dir) {
            Ok(()) => Vec::new(),
            Err(e) => vec![Effect::Message(format!(
                "Could not save the settings: {e:#}"
            ))],
        }
    }

    fn open_machine(&mut self, id: &str) -> Vec<Effect> {
        if let Some(i) = self.tabs.iter().position(|t| t.machine_id() == id) {
            self.active = Tab::Connection(i);
            return Vec::new();
        }
        let Some(m) = self.machines.get(id).cloned() else {
            return vec![Effect::Message(format!("No machine called {id}."))];
        };
        let home = self.home_or_fallback();
        let tab = ConnectionController::new(
            m,
            &home,
            self.settings.recent_places.clone(),
            self.sessions.clone(),
            Arc::clone(&self.notify),
        )
        .with_keys_dir(self.dir.join("keys"));
        self.tabs.push(tab);
        self.active = Tab::Connection(self.tabs.len() - 1);
        Vec::new()
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let mut tab = self.tabs.remove(i);
        self.closing.extend(tab.begin_shell_close());
        self.closing.extend(tab.begin_disconnect());
        self.active = match self.active {
            Tab::Connection(a) if a == i => {
                if self.tabs.is_empty() {
                    Tab::Machines
                } else {
                    Tab::Connection(i.min(self.tabs.len() - 1))
                }
            }
            Tab::Connection(a) if a > i => Tab::Connection(a - 1),
            other => other,
        };
    }

    fn disconnect_all(&mut self) {
        for tab in &mut self.tabs {
            self.closing.extend(tab.begin_shell_close());
            self.closing.extend(tab.begin_disconnect());
        }
    }

    /// Save what a tab changed (its layout, a place left home to).
    fn absorb(&mut self, i: usize) -> Vec<Effect> {
        let mut out = Vec::new();
        let Some(tab) = self.tabs.get_mut(i) else {
            return out;
        };
        let id = tab.machine_id().to_string();
        let layout = tab.take_layout_change();
        let left = tab.take_left_home();
        if let Some((key, forget)) = tab.take_key_login_change() {
            if let Err(e) = self.machines.set_key_login(&id, key, forget) {
                out.push(Effect::Message(e));
            }
        }
        if let Some(k) = tab.take_host_key_change() {
            if let Err(e) = self.machines.set_host_key(&id, k) {
                out.push(Effect::Message(e));
            }
        }
        if let Some(l) = layout {
            if let Err(e) = self.machines.set_layout(&id, l) {
                out.push(Effect::Message(e));
            }
        }
        if let Some(p) = left {
            self.settings.add_recent(p);
            out.extend(self.save_settings());
        }
        out
    }
}

fn service_choice(why: &str) -> Effect {
    Effect::Choose(ChoiceSpec {
        purpose: "machines.add.service",
        title: "Add machine".into(),
        message: format!("{why}Which service does it offer for files?"),
        choices: ADD_SERVICES
            .iter()
            .map(|k| k.label().to_string())
            .chain(["Cancel".to_string()])
            .collect(),
        dismiss: ADD_SERVICES.len(),
    })
}

impl Controller for AppController {
    type Action = Action;

    fn apply(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Start => {
                if self.settings.home_folder.is_none() {
                    return vec![Effect::PickFolder(PickSpec {
                        purpose: "app.home",
                        title:
                            "Choose the folder on this computer you work from (your home folder)"
                                .into(),
                        start: dirs::document_dir(),
                    })];
                }
            }
            Action::HomePicked(path) => {
                // Cancelled: use Documents for now and ask again next start.
                if let Some(p) = path {
                    self.settings.home_folder = Some(p);
                    return self.save_settings();
                }
            }
            Action::ShowMachines => self.active = Tab::Machines,
            Action::SelectTab(i) if i < self.tabs.len() => self.active = Tab::Connection(i),
            Action::SelectTab(_) => {}
            Action::OpenMachine(id) => return self.open_machine(&id),
            Action::CloseTab(i) => self.close_tab(i),
            Action::AddMachine => {
                self.adding = None;
                return vec![Effect::AskText(TextSpec {
                    purpose: "machines.add.address",
                    title: "Add machine".into(),
                    prompt: "Address (host name or IP):".into(),
                    initial: String::new(),
                    ok_label: "Next".into(),
                    validator: not_empty,
                })];
            }
            Action::AddAddressAnswered(None) => {}
            Action::AddAddressAnswered(Some(addr)) => {
                // Nothing is asked until the probe has looked at what the machine offers.
                let address = addr.trim().to_string();
                self.log
                    .info(format!("Looking at what {address} offers..."));
                self.adding = Some(Adding {
                    probe: Some(probe_job(&address, Arc::clone(&self.notify))),
                    address,
                    kind: None,
                    found: None,
                });
            }
            Action::AddFoundAnswered(choice) => {
                let Some(a) = self.adding.as_mut() else {
                    return Vec::new();
                };
                match (choice, &a.found) {
                    (0, Some(f)) if !f.services.is_empty() => {
                        a.kind = Some(f.services[0].0);
                        return vec![name_prompt(&a.address)];
                    }
                    (1, _) => {
                        a.found = None;
                        return vec![service_choice("")];
                    }
                    _ => self.adding = None,
                }
            }
            Action::AddServiceChosen(i) => {
                let Some(a) = self.adding.as_mut() else {
                    return Vec::new();
                };
                let Some(&kind) = ADD_SERVICES.get(i) else {
                    self.adding = None;
                    return Vec::new();
                };
                a.kind = Some(kind);
                return vec![name_prompt(&a.address)];
            }
            Action::AddNameAnswered(None) => self.adding = None,
            Action::AddNameAnswered(Some(name)) => {
                let Some(Adding {
                    address,
                    kind: Some(kind),
                    found,
                    ..
                }) = self.adding.take()
                else {
                    return Vec::new();
                };
                let services = match &found {
                    Some(f) if f.services.first().map(|s| s.0) == Some(kind) => {
                        f.services.iter().map(|s| s.0).collect()
                    }
                    _ => vec![kind],
                };
                let new = NewMachine {
                    id: name.trim().to_string(),
                    address,
                    services,
                    user: String::new(),
                    password: None,
                };
                let id = new.id.clone();
                let added = self.machines.add(new).and_then(|()| match &found {
                    Some(f) => self.machines.apply_probe(&id, f),
                    None => Ok(()),
                });
                return match added {
                    Ok(()) => {
                        self.log.info(format!("Added {id}."));
                        Vec::new()
                    }
                    Err(e) => vec![Effect::Message(e)],
                };
            }
            Action::RemoveMachine(id) => {
                self.removing = Some(id.clone());
                return vec![Effect::Confirm(super::ConfirmSpec {
                    purpose: "machines.remove",
                    title: "Remove machine".into(),
                    message: format!("Remove {id} and its saved settings and password?"),
                    confirm_label: "Remove".into(),
                    destructive: true,
                })];
            }
            Action::RemoveConfirmed(yes) => {
                let Some(id) = self.removing.take() else {
                    return Vec::new();
                };
                if !yes {
                    return Vec::new();
                }
                if let Some(i) = self.tabs.iter().position(|t| t.machine_id() == id) {
                    self.close_tab(i);
                }
                if let Err(e) = self.machines.remove(&id) {
                    return vec![Effect::Message(e)];
                }
            }
            Action::DisconnectAll => self.disconnect_all(),
            Action::SetColor(id, color) => {
                if let Err(e) = self.machines.set_color(&id, color) {
                    return vec![Effect::Message(e)];
                }
                for t in self.tabs.iter_mut().filter(|t| t.machine_id() == id) {
                    t.set_color(color);
                }
            }
            Action::Quit => {
                self.quitting = true;
                self.disconnect_all();
                if self.closing.is_empty() {
                    return vec![Effect::Quit];
                }
            }
            Action::Conn(i, a) => {
                let Some(tab) = self.tabs.get_mut(i) else {
                    return Vec::new();
                };
                let mut out = tab.apply(a);
                out.extend(self.absorb(i));
                return out;
            }
        }
        Vec::new()
    }

    fn tick(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        for i in 0..self.tabs.len() {
            let effects = self.tabs[i].tick();
            out.extend(effects);
            out.extend(self.absorb(i));
        }
        self.closing.retain(|j| j.is_running());
        out.extend(self.probe_done());
        self.log.extend_from_capture(super::log::capture::drain());
        if self.quitting {
            // Nothing may stop the app closing: problems while logging off go to the log.
            let log = &mut self.log;
            out.retain(|e| match e {
                Effect::Message(m) => {
                    log.warn(m.clone());
                    false
                }
                _ => true,
            });
        }
        if self.quitting && self.closing.is_empty() {
            self.quitting = false;
            out.push(Effect::Quit);
        }
        out
    }
}

impl AppController {
    /// When Add machine's probe finishes: what it found, or the plain service question.
    fn probe_done(&mut self) -> Vec<Effect> {
        let Some(a) = self.adding.as_mut() else {
            return Vec::new();
        };
        let Some(r) = a.probe.as_ref().and_then(super::job_outcome) else {
            return Vec::new();
        };
        a.probe = None;
        match r {
            Ok(report) => {
                let f = ProbeSummary::from(&report);
                if f.services.is_empty() {
                    return vec![service_choice(&format!("{}\n\n", f.message()))];
                }
                let msg = f.message();
                a.found = Some(f);
                vec![Effect::Choose(ChoiceSpec {
                    purpose: "machines.add.found",
                    title: "Add machine".into(),
                    message: msg,
                    choices: vec![
                        "Add with these".into(),
                        "Choose a service".into(),
                        "Cancel".into(),
                    ],
                    dismiss: 2,
                })]
            }
            Err(e) => vec![service_choice(&format!(
                "Could not look at {}: {e}\n\n",
                a.address
            ))],
        }
    }
}

fn name_prompt(address: &str) -> Effect {
    Effect::AskText(TextSpec {
        purpose: "machines.add.name",
        title: "Add machine".into(),
        prompt: "Name for this machine:".into(),
        initial: suggest_id(address),
        ok_label: "Add".into(),
        validator: validate_id,
    })
}

/// The config folder the app uses (`LEGCON_CONFIG_DIR` overrides it).
pub fn default_dir() -> PathBuf {
    config::dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::job::NoNotify;

    fn app(dir: &Path) -> AppController {
        AppController::open(dir, Arc::new(NoNotify)).unwrap()
    }

    #[test]
    fn first_run_asks_for_home_then_remembers_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = app(dir.path());
        assert!(matches!(a.apply(Action::Start)[0], Effect::PickFolder(_)));
        a.apply(Action::HomePicked(Some(PathBuf::from("D:/Retro"))));
        let mut b = app(dir.path());
        assert!(b.apply(Action::Start).is_empty());
        assert_eq!(b.home_folder(), Some(Path::new("D:/Retro")));
    }

    #[test]
    fn add_open_close_and_quit() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut a = app(dir.path());
        a.apply(Action::HomePicked(Some(home.path().to_path_buf())));
        a.apply(Action::AddMachine);
        // The probe runs first; `.invalid` never resolves, so the plain question follows.
        assert!(a
            .apply(Action::AddAddressAnswered(Some("demo.invalid".into())))
            .is_empty());
        let mut asked = Vec::new();
        for _ in 0..600 {
            asked = a.tick();
            if !asked.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            matches!(&asked[..], [Effect::Choose(c)] if c.purpose == "machines.add.service" && c.message.contains("Could not look at demo.invalid")),
            "{asked:?}"
        );
        let demo = ADD_SERVICES
            .iter()
            .position(|k| *k == ServiceKind::Demo)
            .unwrap();
        match &a.apply(Action::AddServiceChosen(demo))[0] {
            Effect::AskText(t) => assert_eq!(t.initial, "demo"),
            e => panic!("{e:?}"),
        }
        a.apply(Action::AddNameAnswered(Some("sparc".into())));
        assert_eq!(a.machines()[0].services, "Demo");
        a.apply(Action::OpenMachine("sparc".into()));
        a.apply(Action::OpenMachine("sparc".into()));
        assert_eq!(a.tabs().len(), 1);
        assert_eq!(a.active(), Tab::Connection(0));
        a.apply(Action::Conn(0, connection::Action::Swap));
        let saved = MachinesController::open(dir.path()).unwrap();
        assert!(saved.get("sparc").unwrap().layout.swapped);
        a.apply(Action::Conn(0, connection::Action::Connect));
        for _ in 0..400 {
            a.tick();
            if a.tab(0).unwrap().can_disconnect() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(a.tab(0).unwrap().can_disconnect());
        assert!(a.apply(Action::Quit).is_empty());
        let mut quit = false;
        for _ in 0..400 {
            if a.tick().contains(&Effect::Quit) {
                quit = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(quit, "quit waits for the log-off, then quits");
        assert_eq!(a.sessions.connected(), Vec::<String>::new());
    }
}
