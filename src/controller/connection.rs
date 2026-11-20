//! One connection tab: files on top (this computer | the machine), the terminal area below,
//! a splitter between them and Swap (docs/PLAN.md §1.1, mockups/connection-view.html).
//!
//! The local pane starts in the home folder; each pane's Places menu goes to known, pinned and
//! recent folders, and Go to... takes a typed path. The remote pane connects only when the user
//! says so and logs off cleanly on Disconnect.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lc_core::{Entry, RemotePath};

use super::{
    ids, job_outcome, ConnState, Controller, Effect, Layout, MenuItem, MenuSpec, PasswordSpec,
    PathSpec, ServiceKind, TabColor, TextSpec,
};
use crate::model::job::{Job, Notify};
use crate::model::queue::{Id, OnExisting};
use crate::model::term::TermSession;
use crate::model::transfer::{self, Direction, Pick, Plan};

mod import;
mod rejoin;
mod transfers;
pub use rejoin::{LinkPrefs, REJOIN_PURPOSE, RESUME_PURPOSE};
use transfers::{direction_from, ItemAnswer, Transfers};
pub use transfers::{QueueButton, QueueOp, QueuePrefs, QueueRow, QUEUE_COLUMNS, RENAME_PURPOSE};

use crate::model::local::{list_dir, LocalEntry};
use crate::model::machines::{Machine, Password, Service};
use crate::model::names;
use crate::model::places::{self, Place};
use crate::model::rsh_trust::{self, rsh_trust, TrustReport, Untrusted};
use crate::model::session::Sessions;
use crate::model::settings::add_recent;
use crate::model::ssh::{set_up_key_login, take_note, KeyNote, KeySetup, KeySlot};

/// What a password or an accepted host key is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Login {
    Files,
    Shell,
    Setup,
    /// rsh trust: set it up (`true`) or only look.
    Rsh(bool),
}

/// What a button in the "rsh refused" question does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RshChoice {
    SetUp,
    Check,
    Use(ServiceKind),
    Close,
}
pub use crate::model::term::{TermRun, TermScreen, DEFAULT_BG, DEFAULT_FG};

/// The smallest share either area can be dragged down to.
pub const MIN_SHARE: f32 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// The terminal area's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermTab {
    Shell,
    Console,
    Log,
    Transfers,
}

impl TermTab {
    pub const ALL: [TermTab; 4] = [
        TermTab::Shell,
        TermTab::Console,
        TermTab::Log,
        TermTab::Transfers,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TermTab::Shell => "Shell",
            TermTab::Console => "Console",
            TermTab::Log => "Log",
            TermTab::Transfers => "Transfers",
        }
    }
}

/// The machine header, in parts so a view can lay it out as chips (mockup: connection-view.html).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderInfo {
    pub name: String,
    pub address: String,
    /// `Files: FTP`, or `No file service`.
    pub files: String,
    pub files_state: ConnState,
    /// `Commands: Telnet`, or `No command service`.
    pub commands: String,
    pub commands_state: ConnState,
    /// Passwords or data go unencrypted on some service in use.
    pub plaintext: bool,
    pub color: TabColor,
}

/// A row of either pane's list, as a view draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub is_dir: bool,
    /// Empty for folders.
    pub size: String,
    pub modified: String,
    /// Unix mode as `drwxr-xr-x`, where known.
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Swap,
    /// The files area's share of the height after a splitter drag (clamped).
    SetFilesShare(f32),
    SelectTerm(TermTab),
    LocalHome,
    LocalUp,
    LocalOpen(String),
    /// Open a pane's Places menu.
    Places(Side),
    /// The Places menu's answer: an index into its items; `None` when dismissed.
    PlaceChosen(Side, Option<usize>),
    /// Ask for a folder to go to in a pane.
    GoTo(Side),
    /// The Go to prompts' answers; `None` when cancelled.
    LocalGoToAnswered(Option<PathBuf>),
    RemoteGoToAnswered(Option<String>),
    Connect,
    PasswordAnswered(Option<String>),
    Disconnect,
    RemoteHome,
    RemoteUp,
    RemoteOpen(String),
    Refresh(Side),
    /// Open a shell on the machine's command service (the Shell tab's Connect).
    ShellConnect,
    ShellDisconnect,
    /// Characters typed or pasted into the terminal.
    ShellText(String),
    ShellKey(TermKey),
    /// The terminal's size in character cells after the view was resized.
    ShellResize(u16, u16),
    /// Copy these names from one side's current folder to the other side's.
    Copy(Side, Vec<String>),
    /// A button of "already there" for files dropped onto the local pane (batch or one file).
    CopyConflict(usize),
    /// A new name for a copy whose target is there (`None`: cancelled), queued or local.
    CopyRenameAnswered(Option<String>),
    /// The answer to "host key changed": accept for good, accept once, cancel.
    HostKeyAnswered(usize),
    /// Make (or reuse) a key for this machine, install it over SSH and test it (PLAN.md 5.1).
    SetUpKeyLogin,
    /// After key login works: stop saving the password (0) or keep it.
    KeyLoginPasswordAnswered(usize),
    /// Add this computer to `~/.rhosts` over rexec, FTP or Telnet, then try rsh (PLAN.md 4.2).
    SetUpRshTrust,
    /// Ask the server, over rexec or Telnet, where it sees this computer come from.
    CheckRsh,
    /// The answer to "rsh refused": an index into the buttons it showed.
    RshRefusedAnswered(usize),
    /// Walk these names into the transfer queue without starting it.
    AddToQueue(Side, Vec<String>),
    /// A Transfers button, on the selected rows.
    Queue(QueueOp, Vec<u64>),
    /// "Adding N files to the queue": Add them, Cancel.
    QueueManyAnswered(usize),
    /// "Already there" for a batch: an index into the buttons it showed.
    QueueExistsAnswered(usize),
    /// "Already there" for one queued file: an index into the buttons it showed.
    QueueItemAnswered(usize),
    /// "Transfers are not finished": Finish first, Pause them, Cancel them, Stay connected.
    DisconnectAnswered(usize),
    NewFolder(Side),
    NewFolderAnswered(Option<String>),
    Rename(Side, String),
    RenameAnswered(Option<String>),
    Delete(Side, Vec<String>),
    DeleteConfirmed(bool),
    /// A line typed into the Console tab, sent as the protocol's own command.
    ConsoleSend(String),
    /// Files or folders dropped from the OS (or another program) onto a pane.
    Drop(Side, Vec<PathBuf>),
    /// Download these remote names into a folder other than the local pane's (rows dragged
    /// out to the Finder or Explorer, which names the folder).
    CopyOut(Vec<String>, PathBuf),
    /// "Connect again?" after a lost link or an idle log-off: Connect, Not now.
    RejoinAnswered(usize),
    /// "Transfers are not finished" after a reconnect: Go on, Not now.
    ResumeQueueAnswered(usize),
}

/// A name for a new or renamed file or folder: not empty, no path separators, nothing Windows refuses.
pub fn valid_name(text: &str) -> Result<(), String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(String::new());
    }
    if t == "." || t == ".." {
        return Err("That name is taken by the folder itself.".into());
    }
    if let Some(c) = t.chars().find(|c| {
        matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
    }) {
        return Err(format!("A name cannot contain {c:?}."));
    }
    Ok(())
}

fn not_dots(t: &str) -> Result<&str, String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(String::new());
    }
    if t == "." || t == ".." {
        return Err("That name is taken by the folder itself.".into());
    }
    Ok(t)
}

/// A name on a Unix machine: anything but `/` (and NUL).
pub fn valid_unix_name(text: &str) -> Result<(), String> {
    let t = not_dots(text)?;
    match t.chars().find(|&c| c == '/' || c == '\0') {
        Some(c) => Err(format!("A Unix name cannot contain {c:?}.")),
        None => Ok(()),
    }
}

/// A name on a classic Mac: no `:`, at most 31 characters.
pub fn valid_mac_name(text: &str) -> Result<(), String> {
    let t = not_dots(text)?;
    if t.contains(':') {
        return Err("A Mac name cannot contain ':'.".into());
    }
    let n = t.chars().count();
    if n > 31 {
        return Err(format!(
            "A Mac name has at most 31 characters (this has {n})."
        ));
    }
    Ok(())
}

/// What a Places menu item does.
#[derive(Debug, Clone)]
enum Target {
    Local(PathBuf),
    Remote(RemotePath),
    /// Pin (true) or unpin the pane's current folder.
    Pin(Side, bool),
    GoTo(Side),
}

/// A change to the app-wide local places, for the settings file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlaceEvent {
    /// Files were copied to or from here, or the user went here: a recent place.
    Visited(PathBuf),
    Pinned(PathBuf, bool),
}

/// Picks that share one local folder (a pane's, or the folder files were dropped from).
struct Group {
    local_dir: PathBuf,
    picks: Vec<Pick>,
    sizes: Vec<Option<u64>>,
}

/// What a pending question is about.
enum Pending {
    /// A planned batch over the size that asks first; whether to start the queue.
    QueueMany(Plan, bool),
    /// A planned batch with targets already there, and what each button means (`None`: cancel).
    QueueExists(Plan, bool, Vec<Option<OnExisting>>),
    /// One queued file whose target is there.
    QueueItem(Id, Vec<ItemAnswer>),
    /// The new name for one queued file whose target is there.
    QueueRename(Id),
    /// Disconnect with transfers unfinished.
    QueueDisconnect,
    /// Files dropped from the OS onto the local pane, and the questions about ones already there.
    Import(Box<import::ImportAsk>),
    NewFolder(Side),
    Rename(Side, String),
    Delete(Side, Vec<Pick>),
}

/// Keys that are not plain characters, as a view reports them to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKey {
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 to F12.
    F(u8),
    /// Ctrl plus a letter or one of `@[\]^_`: the control character.
    Ctrl(char),
}

/// The bytes a key sends, as a VT220 does (`app_cursor`: the screen asked for `ESC O` arrows).
pub fn key_bytes(key: TermKey, app_cursor: bool) -> Vec<u8> {
    let csi = |s: &str| format!("\x1b[{s}").into_bytes();
    let arrow = |c: char| {
        if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    match key {
        TermKey::Enter => b"\r".to_vec(),
        // DEL, as most 1990s Unix `stty erase` settings and xterm expect.
        TermKey::Backspace => vec![0x7f],
        TermKey::Tab => b"\t".to_vec(),
        TermKey::Escape => vec![0x1b],
        TermKey::Up => arrow('A'),
        TermKey::Down => arrow('B'),
        TermKey::Right => arrow('C'),
        TermKey::Left => arrow('D'),
        TermKey::Home => csi("1~"),
        TermKey::Insert => csi("2~"),
        TermKey::Delete => csi("3~"),
        TermKey::End => csi("4~"),
        TermKey::PageUp => csi("5~"),
        TermKey::PageDown => csi("6~"),
        TermKey::F(n @ 1..=4) => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
        TermKey::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return Vec::new(),
            };
            csi(&format!("{code}~"))
        }
        TermKey::Ctrl(c) => match c.to_ascii_uppercase() {
            c @ '@'..='_' => vec![c as u8 & 0x1f],
            _ => Vec::new(),
        },
    }
}

pub struct ConnectionController {
    machine: Machine,
    sessions: Sessions,
    notify: Arc<dyn Notify>,
    layout: Layout,
    layout_changed: bool,
    term: TermTab,
    home: PathBuf,
    local_dir: PathBuf,
    local_rows: Vec<LocalEntry>,
    local_job: Option<Job<Vec<LocalEntry>>>,
    local_error: Option<String>,
    recent: Vec<PathBuf>,
    pins: Vec<PathBuf>,
    system: Vec<Place>,
    drives: Vec<PathBuf>,
    drives_job: Option<Job<Vec<PathBuf>>>,
    /// The open Places menu: its side and what each item does.
    menu: Option<(Side, Vec<Option<Target>>)>,
    place_events: Vec<PlaceEvent>,
    remote_places_changed: bool,
    remote_home: Option<RemotePath>,
    remote_dir: Option<RemotePath>,
    remote_rows: Vec<Entry>,
    remote_error: Option<String>,
    connect_job: Option<Job<RemotePath>>,
    list_job: Option<Job<Vec<Entry>>>,
    disconnect_job: Option<Job<()>>,
    session_password: Option<String>,
    /// The password being asked for is for the shell, not the file connection.
    asking_for: Login,
    /// What the shell's SSH connect found out about the host key.
    shell_key_note: KeySlot,
    /// A changed host key waiting for the user: the new key, and whether the shell met it.
    key_question: Option<(String, Login)>,
    host_key_changed: bool,
    key_setup: Option<Job<KeySetup>>,
    key_setup_note: KeySlot,
    /// A login key set up here, and whether to stop saving the password, for the machine store.
    key_login_changed: Option<(PathBuf, bool)>,
    /// Where login keys are made (`keys` in the config folder).
    keys_dir: PathBuf,
    rsh_job: Option<Job<TrustReport>>,
    /// The buttons of the "rsh refused" question being shown.
    rsh_choices: Vec<RshChoice>,
    /// The machine's services after "Use FTP instead", once, for the machine store.
    services_changed: Option<Vec<Service>>,
    shell: Option<TermSession>,
    shell_open: Option<Job<TermSession>>,
    shell_close: Option<Job<()>>,
    shell_error: Option<String>,
    shell_size: (u16, u16),
    xfer: Transfers,
    pending: Option<Pending>,
    ops: Vec<(Side, Job<()>)>,
    console: Vec<String>,
    console_job: Option<Job<String>>,
    /// Lost connections and connecting again (rejoin.rs).
    link: rejoin::Link,
}

/// What an rsh check or setup on `name` found, in words (the tab and `lc rsh-trust`).
pub(crate) fn rsh_report(name: &str, r: Result<TrustReport, String>) -> String {
    let r = match r {
        Ok(r) => r,
        Err(e) => return format!("{name}: rsh: {e}"),
    };
    if r.via.is_none() && r.rsh.is_ok() {
        return format!("rsh already works on {name}; nothing was changed.");
    }
    let via = r.via.map_or("", |k| k.label());
    let mut lines = Vec::new();
    if !r.added.is_empty() {
        lines.push(format!(
            "Added to ~/.rhosts on {name} (over {via}): {}.",
            r.added.join(", ")
        ));
    }
    if !r.already.is_empty() {
        lines.push(format!("Already in ~/.rhosts: {}.", r.already.join(", ")));
    }
    if let Some(seen) = r.seen {
        match seen.server {
            Some(server) if seen.port_rewritten() => lines.push(format!(
                "{name} sees this computer as {} port {}, but it connected from port {}. Something in between changes the source port (a NAT router, a container's port publishing, or an emulator's user-mode networking such as QEMU's), so rsh's port below 1024 never arrives and rsh is refused whatever .rhosts says. Use bridged networking, or rexec, Telnet or FTP.",
                server.ip(),
                server.port(),
                seen.local.port()
            )),
            Some(server) => lines.push(format!(
                "{name} sees this computer as {} port {}, as it connected; the port arrives intact.",
                server.ip(),
                server.port()
            )),
            None if r.via.is_some_and(rsh_trust::can_look) => lines.push(format!(
                "{name} did not list its connections (no netstat), so the address it sees is not known; this computer connected from {}.",
                seen.local.ip()
            )),
            None => lines.push(format!(
                "{via} cannot ask {name} what it sees; .rhosts names {}, the address this computer connected from.",
                seen.local.ip()
            )),
        }
    }
    let rewritten = r.seen.is_some_and(|s| s.port_rewritten());
    lines.push(match &r.rsh {
        Ok(()) => "rsh works now. Connect to use it.".to_string(),
        Err(_) if rewritten => {
            "rsh is still refused, and will be until the port arrives intact.".to_string()
        }
        Err(e) => format!(
            "rsh is still refused: {e}. If the server checks names, its own DNS or /etc/hosts must name this computer; /etc/hosts.equiv, inetd or a tcp wrapper (hosts.allow) may also stop it."
        ),
    });
    lines.join("\n\n")
}

impl ConnectionController {
    /// A tab for `machine`, its local pane in `home` (the machine's own home wins).
    pub(crate) fn new(
        machine: Machine,
        app_home: &Path,
        recent: Vec<PathBuf>,
        pins: Vec<PathBuf>,
        sessions: Sessions,
        notify: Arc<dyn Notify>,
    ) -> Self {
        // A machine's own folder that is not on this computer (moved, or another computer's
        // path) falls back to the app's, rather than an error in the pane.
        let home = machine
            .home_folder
            .clone()
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| app_home.to_path_buf());
        let mut c = ConnectionController {
            layout: machine.layout.clone(),
            machine,
            sessions,
            notify,
            layout_changed: false,
            term: TermTab::Shell,
            local_dir: home.clone(),
            home,
            local_rows: Vec::new(),
            local_job: None,
            local_error: None,
            recent,
            pins,
            system: places::system_places(),
            drives: Vec::new(),
            drives_job: None,
            menu: None,
            place_events: Vec::new(),
            remote_places_changed: false,
            remote_home: None,
            remote_dir: None,
            remote_rows: Vec::new(),
            remote_error: None,
            connect_job: None,
            list_job: None,
            disconnect_job: None,
            session_password: None,
            asking_for: Login::Files,
            shell_key_note: KeySlot::default(),
            key_question: None,
            host_key_changed: false,
            key_setup: None,
            key_setup_note: KeySlot::default(),
            key_login_changed: None,
            keys_dir: crate::model::config::dir().join("keys"),
            rsh_job: None,
            rsh_choices: Vec::new(),
            services_changed: None,
            shell: None,
            shell_open: None,
            shell_close: None,
            shell_error: None,
            shell_size: (80, 24),
            xfer: Transfers::default(),
            pending: None,
            ops: Vec::new(),
            console: Vec::new(),
            console_job: None,
            link: rejoin::Link::default(),
        };
        c.push_upkeep();
        c.list_local();
        c.drives_job = Some(Job::spawn(
            "Find drives".to_string(),
            Arc::clone(&c.notify),
            |_| Ok(places::drives()),
        ));
        c
    }

    pub fn machine_id(&self) -> &str {
        &self.machine.id
    }

    pub fn title(&self) -> String {
        self.machine.name.clone()
    }

    pub fn tab_id(&self) -> String {
        format!("{}{}", ids::TAB_CONN_PREFIX, self.machine.id)
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The layout once, after the user changed it, for the app to save.
    pub(crate) fn take_layout_change(&mut self) -> Option<Layout> {
        std::mem::take(&mut self.layout_changed).then(|| self.layout.clone())
    }

    /// A host key learned or accepted, once, for the machine store.
    pub(crate) fn take_host_key_change(&mut self) -> Option<String> {
        std::mem::take(&mut self.host_key_changed)
            .then(|| self.machine.host_key.clone())
            .flatten()
    }

    /// Act on what an SSH connect found out about the host key; a question if it changed.
    fn key_note(&mut self, note: Option<KeyNote>, login: Login) -> Option<Effect> {
        match note? {
            KeyNote::Learned(line) => {
                self.machine.host_key = Some(line);
                self.host_key_changed = true;
                None
            }
            KeyNote::Changed { line, md5, sha256 } => {
                let algorithm = line.split(' ').next().unwrap_or("").to_string();
                self.key_question = Some((line, login));
                Some(Effect::Choose(super::ChoiceSpec {
                    purpose: "conn.hostkey",
                    title: "Host key changed".into(),
                    message: format!(
                        "{} ({}) shows a different SSH host key than last time.\n\n\
                         New key: {algorithm}\nMD5: {md5}\nSHA-256:\n{}\n\n\
                         This is expected after the machine was reinstalled. If it was not, \
                         someone may be pretending to be it. Nothing has been sent yet.",
                        self.machine.name,
                        self.machine.address,
                        sha256.trim_start_matches("SHA256:")
                    ),
                    choices: vec![
                        "Accept the new key".into(),
                        "Accept once".into(),
                        "Cancel".into(),
                    ],
                    dismiss: 2,
                }))
            }
        }
    }

    /// Reconnect after a changed host key was accepted, for good or for this connection only.
    fn host_key_answered(&mut self, choice: usize) {
        let Some((line, login)) = self.key_question.take() else {
            return;
        };
        if choice > 1 {
            return;
        }
        let old = self.machine.host_key.replace(line);
        if choice == 0 {
            self.host_key_changed = true;
        }
        let password = match self.machine.password.resolve(&self.machine.id) {
            Ok(Some(p)) => Some(p),
            _ => self.session_password.clone(),
        };
        self.log_in(login, password);
        if choice == 1 {
            // The job took its own copy of the machine; the stored key stays as it was.
            self.machine.host_key = old;
        }
    }

    fn log_in(&mut self, login: Login, password: Option<String>) {
        match login {
            Login::Files => self.connect(password),
            Login::Shell => self.open_shell(password),
            Login::Rsh(install) => {
                self.rsh_job = Some(rsh_trust(
                    &self.machine,
                    password,
                    install,
                    Arc::clone(&self.notify),
                ))
            }
            Login::Setup => {
                self.key_setup = Some(set_up_key_login(
                    &self.machine,
                    password,
                    self.keys_dir.clone(),
                    Arc::clone(&self.key_setup_note),
                    Arc::clone(&self.notify),
                ))
            }
        }
    }

    /// The password to log in with, or the question for it; `None` when there is nothing to ask.
    fn password_for(&mut self, login: Login, needed: bool) -> Result<Option<String>, Box<Effect>> {
        // The OS store may have lost it; then the password typed this session, then ask.
        if let Ok(Some(p)) = self.machine.password.resolve(&self.machine.id) {
            return Ok(Some(p));
        }
        match &self.session_password {
            Some(p) => Ok(Some(p.clone())),
            None if needed => {
                self.asking_for = login;
                Err(Box::new(self.ask_password()))
            }
            None => Ok(None),
        }
    }

    /// The machine has SSH, so a login key can be set up for it.
    pub fn can_set_up_key_login(&self) -> bool {
        self.machine
            .services
            .iter()
            .any(|s| s.kind == ServiceKind::Ssh)
            && self.key_setup.is_none()
    }

    fn start_key_setup(&mut self) -> Vec<Effect> {
        if !self.can_set_up_key_login() {
            return Vec::new();
        }
        match self.password_for(Login::Setup, true) {
            Ok(p) => self.log_in(Login::Setup, p),
            Err(ask) => return vec![*ask],
        }
        Vec::new()
    }

    /// The machine has rsh and a service to log in with a password, so `.rhosts` can be set up.
    pub fn can_set_up_rsh_trust(&self) -> bool {
        self.has(ServiceKind::Rsh)
            && rsh_trust::via(&self.machine).is_some()
            && self.rsh_job.is_none()
    }

    /// The machine has rsh and rexec or Telnet, so the server can say what it sees.
    pub fn can_check_rsh(&self) -> bool {
        self.can_set_up_rsh_trust()
            && rsh_trust::via(&self.machine).is_some_and(rsh_trust::can_look)
    }

    fn has(&self, kind: ServiceKind) -> bool {
        self.machine.services.iter().any(|s| s.kind == kind)
    }

    fn start_rsh(&mut self, install: bool) -> Vec<Effect> {
        let ok = if install {
            self.can_set_up_rsh_trust()
        } else {
            self.can_check_rsh()
        };
        if !ok {
            return Vec::new();
        }
        match self.password_for(Login::Rsh(install), true) {
            Ok(p) => self.log_in(Login::Rsh(install), p),
            Err(ask) => return vec![*ask],
        }
        Vec::new()
    }

    /// The machine's services after a change made here, once, for the machine store.
    pub(crate) fn take_services_change(&mut self) -> Option<Vec<Service>> {
        self.services_changed.take()
    }

    /// rsh refused the file connection: say why, and offer the ways on.
    fn rsh_refused(&mut self, why: Untrusted) -> Effect {
        let name = &self.machine.name;
        let mut choices = Vec::new();
        let mut message = match &why {
            Untrusted::NoLowPort => format!(
                "{name}: rsh needs a source port below 1024, and {}.",
                rsh_trust::NO_LOW_PORT
            ),
            Untrusted::Refused { local, said } => {
                let said = if said.is_empty() {
                    "it closed the connection".to_string()
                } else {
                    format!("it said \"{said}\"")
                };
                if self.can_set_up_rsh_trust() {
                    choices.push(RshChoice::SetUp);
                }
                if self.can_check_rsh() {
                    choices.push(RshChoice::Check);
                }
                format!(
                    "{name} would not let rsh in: {said}.\n\nrsh has no password: the server must list this computer in ~/.rhosts (or /etc/hosts.equiv), and the connection must arrive from a port below 1024. This computer connected from {} port {}.",
                    local.ip(),
                    local.port()
                )
            }
        };
        let others: Vec<ServiceKind> = self
            .machine
            .services
            .iter()
            .map(|s| s.kind)
            .filter(|k| k.serves_files() && *k != ServiceKind::Rsh)
            .collect();
        if let Some(&k) = others.first() {
            choices.push(RshChoice::Use(k));
        }
        if choices.contains(&RshChoice::SetUp) {
            let via = rsh_trust::via(&self.machine).map_or("", |k| k.label());
            message += &format!(
                "\n\nSet up rsh trust logs in once over {via} with the password, adds this computer to ~/.rhosts and tries rsh again."
            );
        }
        if choices.contains(&RshChoice::Check) {
            message +=
                " Check asks the server which address and port it sees, without changing anything.";
        }
        if let Some(k) = others.first() {
            message += &format!(
                "\n\n{name} also has {}; it can serve the files instead.",
                k.label()
            );
        }
        choices.push(RshChoice::Close);
        let labels = choices
            .iter()
            .map(|c| match c {
                RshChoice::SetUp => "Set up rsh trust".to_string(),
                RshChoice::Check => "Check".to_string(),
                RshChoice::Use(k) => format!("Use {} instead", k.label()),
                RshChoice::Close => "Close".to_string(),
            })
            .collect();
        let dismiss = choices.len() - 1;
        self.rsh_choices = choices;
        Effect::Choose(super::ChoiceSpec {
            purpose: "conn.rsh.refused",
            title: "rsh refused".into(),
            message,
            choices: labels,
            dismiss,
        })
    }

    fn rsh_refused_answered(&mut self, i: usize) -> Vec<Effect> {
        match std::mem::take(&mut self.rsh_choices).get(i) {
            Some(RshChoice::SetUp) => self.start_rsh(true),
            Some(RshChoice::Check) => self.start_rsh(false),
            Some(&RshChoice::Use(kind)) => {
                let services = &mut self.machine.services;
                if let (Some(from), Some(to)) = (
                    services.iter().position(|s| s.kind == kind),
                    services.iter().position(|s| s.kind == ServiceKind::Rsh),
                ) {
                    if from > to {
                        let s = services.remove(from);
                        services.insert(to, s);
                    }
                }
                self.services_changed = Some(self.machine.services.clone());
                if !self.can_connect() {
                    return Vec::new();
                }
                match self.password_for(Login::Files, kind.needs_password()) {
                    Ok(p) => self.connect(p),
                    Err(ask) => return vec![*ask],
                }
                Vec::new()
            }
            Some(RshChoice::Close) | None => Vec::new(),
        }
    }

    /// The key set up for this machine, and whether to stop saving its password, once.
    pub(crate) fn take_key_login_change(&mut self) -> Option<(PathBuf, bool)> {
        self.key_login_changed.take()
    }

    fn key_setup_done(&mut self, r: Result<KeySetup, String>) -> Vec<Effect> {
        let s = match r {
            Ok(s) => s,
            Err(e) => {
                return vec![Effect::Message(format!(
                    "{}: could not set up key login: {e}",
                    self.machine.name
                ))]
            }
        };
        self.machine.key_file = Some(s.key_file.clone());
        self.key_login_changed = Some((s.key_file.clone(), false));
        let made = if s.new_key {
            format!("legcon made a new {} key", s.algorithm)
        } else {
            format!("legcon reused its {} key", s.algorithm)
        };
        let what = format!(
            "Key login works on {}. {made} ({}) and installed it in {} there.",
            self.machine.name,
            s.key_file.display(),
            s.installed_in
        );
        if self.machine.password != Password::Ask {
            vec![Effect::Choose(super::ChoiceSpec {
                purpose: "conn.keylogin.password",
                title: "Key login".into(),
                message: format!("{what}\n\nThe password is still saved. Stop saving it?"),
                choices: vec!["Stop saving the password".into(), "Keep it".into()],
                dismiss: 1,
            })]
        } else {
            vec![Effect::Message(what)]
        }
    }

    /// Make login keys in `dir` instead of the default config folder's `keys`.
    pub(crate) fn with_keys_dir(mut self, dir: PathBuf) -> Self {
        self.keys_dir = dir;
        self
    }

    /// Pins and recent places changed here, once, for the settings file.
    pub(crate) fn take_place_events(&mut self) -> Vec<PlaceEvent> {
        std::mem::take(&mut self.place_events)
    }

    /// The machine's pinned and recent folders after a change, once, for the machine store.
    pub(crate) fn take_remote_places(&mut self) -> Option<(Vec<String>, Vec<String>)> {
        std::mem::take(&mut self.remote_places_changed).then(|| {
            (
                self.machine.pinned_folders.clone(),
                self.machine.recent_folders.clone(),
            )
        })
    }

    /// The app-wide pins and recent places, after any tab changed them.
    pub(crate) fn set_local_places(&mut self, pins: Vec<PathBuf>, recent: Vec<PathBuf>) {
        self.pins = pins;
        self.recent = recent;
    }

    pub fn term_tab(&self) -> TermTab {
        self.term
    }

    pub fn color(&self) -> TabColor {
        self.machine.color
    }

    pub(crate) fn set_color(&mut self, color: TabColor) {
        self.machine.color = color;
    }

    /// Take the machine's edited settings; true when a live connection still uses the old ones.
    pub(crate) fn machine_edited(&mut self, m: Machine) -> bool {
        let old = std::mem::replace(&mut self.machine, m);
        self.push_upkeep();
        let m = &self.machine;
        let changed = (
            &old.address,
            &old.services,
            &old.user,
            &old.charset,
            old.uid,
            old.gid,
        ) != (&m.address, &m.services, &m.user, &m.charset, m.uid, m.gid)
            || old.local_user != m.local_user;
        changed && self.tab_state() != ConnState::Disconnected
    }

    /// The tab's state light: connected if either service is.
    pub fn tab_state(&self) -> ConnState {
        let (f, c) = (self.state(), self.shell_state());
        for s in [
            ConnState::Connected,
            ConnState::Busy,
            ConnState::Connecting,
            ConnState::Disconnecting,
            ConnState::Lost,
            ConnState::Idle,
        ] {
            if f == s || c == s {
                return s;
            }
        }
        ConnState::Disconnected
    }

    /// The machine header in parts.
    pub fn header_info(&self) -> HeaderInfo {
        let label = |s: Option<&crate::model::machines::Service>, what: &str, none: &str| {
            s.map(|s| format!("{what}: {}", s.kind.label()))
                .unwrap_or_else(|| none.to_string())
        };
        let plain = |k: Option<ServiceKind>| k.is_some_and(ServiceKind::plaintext);
        HeaderInfo {
            name: self.machine.name.clone(),
            address: self.machine.address.clone(),
            files: label(self.machine.file_service(), "Files", "No file service"),
            files_state: self.state(),
            commands: label(
                self.machine.command_service(),
                "Commands",
                "No command service",
            ),
            commands_state: self.shell_state(),
            plaintext: plain(self.machine.file_service().map(|s| s.kind))
                || plain(self.machine.command_service().map(|s| s.kind)),
            color: self.machine.color,
        }
    }

    pub fn shell_state(&self) -> ConnState {
        if self.shell_open.is_some() {
            return ConnState::Connecting;
        }
        if self.shell_close.is_some() {
            return ConnState::Disconnecting;
        }
        self.shell
            .as_ref()
            .map_or(ConnState::Disconnected, TermSession::state)
    }

    pub fn has_command_service(&self) -> bool {
        self.machine.command_service().is_some()
    }

    pub fn can_shell_connect(&self) -> bool {
        self.has_command_service() && self.shell_state() == ConnState::Disconnected
    }

    pub fn can_shell_disconnect(&self) -> bool {
        self.shell_state() == ConnState::Connected
    }

    /// The Shell tab's Connect button label, such as `Connect with Telnet`.
    pub fn shell_connect_label(&self) -> String {
        self.machine
            .command_service()
            .map(|s| format!("Connect with {}", s.kind.label()))
            .unwrap_or_else(|| "Connect".into())
    }

    /// The terminal size the controller last heard from the view, in cells.
    pub fn shell_size(&self) -> (u16, u16) {
        self.shell_size
    }

    /// The live terminal, once a shell is open (and after it ended, until the next one).
    pub fn shell_screen(&self) -> Option<TermScreen> {
        self.shell.as_ref().map(TermSession::screen)
    }

    /// The Shell tab's message while no terminal shows: what it offers, or why it ended.
    pub fn shell_message(&self) -> String {
        if let Some(e) = &self.shell_error {
            return e.clone();
        }
        if let Some(why) = self.shell.as_ref().and_then(TermSession::ended) {
            return why;
        }
        match self.machine.command_service() {
            Some(s) => format!(
                "{} offers {}. Nothing is opened until you connect; the file connection is not affected.",
                self.machine.name,
                s.kind.label()
            ),
            None => format!("{} has no command service set up.", self.machine.name),
        }
    }

    fn shell_connect(&mut self) -> Vec<Effect> {
        if !self.can_shell_connect() {
            return Vec::new();
        }
        // Telnet logs in on the screen, SSH may have a key; the others need the password up front.
        let kind = self.machine.command_service().map(|s| s.kind);
        let key = kind == Some(ServiceKind::Ssh)
            && (self.machine.key_file.is_some() || self.machine.use_agent);
        let needed = !key && kind.is_some_and(ServiceKind::needs_password);
        match self.password_for(Login::Shell, needed) {
            Ok(p) => self.open_shell(p),
            Err(ask) => return vec![*ask],
        }
        Vec::new()
    }

    fn open_shell(&mut self, password: Option<String>) {
        self.shell_error = None;
        let (cols, rows) = self.shell_size;
        self.shell_open = Some(TermSession::open(
            &self.machine,
            password,
            (cols, rows),
            Arc::clone(&self.shell_key_note),
            Arc::clone(&self.notify),
            Some(self.sessions.trace_log(&self.machine.id)),
            self.keepalive(),
        ));
        self.link.shell_closed = false;
    }

    fn ask_password(&self) -> Effect {
        let user = if self.machine.user.is_empty() {
            "anonymous"
        } else {
            &self.machine.user
        };
        Effect::AskPassword(PasswordSpec {
            purpose: "conn.password",
            prompt: format!("Password for {user}@{}:", self.machine.address),
        })
    }

    /// Close the shell (logout command first); also used when the tab closes.
    pub(crate) fn begin_shell_close(&mut self) -> Option<Job<()>> {
        self.link.shell_closed = true;
        let shell = self.shell.as_ref()?;
        if shell.state() != ConnState::Connected || self.shell_close.is_some() {
            return None;
        }
        let job = shell.close(&self.machine.logout_command, Arc::clone(&self.notify));
        self.shell_close = Some(job.clone());
        Some(job)
    }

    fn shell_send(&mut self, bytes: &[u8]) {
        if let Some(s) = self
            .shell
            .as_ref()
            .filter(|s| s.state() == ConnState::Connected)
        {
            if let Err(e) = s.send(bytes) {
                self.shell_error = Some(format!("Could not send: {e}"));
            }
        }
    }

    pub fn state(&self) -> ConnState {
        if self.connect_job.is_some() {
            return ConnState::Connecting;
        }
        if self.disconnect_job.is_some() {
            return ConnState::Disconnecting;
        }
        self.sessions.state(&self.machine.id)
    }

    /// The machine header's service line: `Files: FTP` or what is missing.
    pub fn header(&self) -> String {
        let files = self
            .machine
            .file_service()
            .map(|s| format!("Files: {}", s.kind.label()))
            .unwrap_or_else(|| "No file service".into());
        let cmds = self
            .machine
            .command_service()
            .map(|s| format!("Commands: {}", s.kind.label()))
            .unwrap_or_else(|| "No command service".into());
        format!(
            "{}  -  {}  -  {}  -  {}",
            self.machine.name, self.machine.address, files, cmds
        )
    }

    /// Whether the file service sends passwords in the clear.
    pub fn plaintext(&self) -> bool {
        self.machine
            .file_service()
            .is_some_and(|s| s.kind.plaintext())
    }

    pub fn can_connect(&self) -> bool {
        matches!(self.state(), ConnState::Disconnected | ConnState::Lost)
    }

    pub fn can_disconnect(&self) -> bool {
        matches!(self.state(), ConnState::Connected | ConnState::Busy)
    }

    pub fn local_path(&self) -> String {
        self.local_dir.display().to_string()
    }

    pub fn home_path(&self) -> String {
        self.home.display().to_string()
    }

    pub fn local_rows(&self) -> Vec<Row> {
        self.local_rows
            .iter()
            .map(|e| Row {
                name: e.name.clone(),
                is_dir: e.is_dir,
                size: if e.is_dir {
                    String::new()
                } else {
                    size_text(e.size)
                },
                modified: e.modified.map(date_text).unwrap_or_default(),
                mode: String::new(),
            })
            .collect()
    }

    pub fn local_error(&self) -> Option<&str> {
        self.local_error.as_deref()
    }

    pub fn remote_path(&self) -> String {
        self.remote_dir
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_default()
    }

    pub fn remote_rows(&self) -> Vec<Row> {
        self.remote_rows
            .iter()
            .map(|e| Row {
                name: e.name.clone(),
                is_dir: e.is_folder(),
                size: if e.is_folder() {
                    String::new()
                } else {
                    e.size.map(size_text).unwrap_or_default()
                },
                modified: e.modified.map(|t| date_text(t.unix)).unwrap_or_default(),
                mode: e
                    .mode
                    .map(|m| mode_text(m, e.is_folder()))
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// Why the remote pane is empty or stale: a failed listing, or the connection lost or idle.
    pub fn remote_error(&self) -> Option<&str> {
        self.remote_error.as_deref().or(self.link.note.as_deref())
    }

    /// What the terminal area shows until its features arrive (phase 1 and later).
    pub fn term_text(&self) -> String {
        match self.term {
            TermTab::Shell => match self.machine.command_service() {
                Some(s) => format!(
                    "{} offers {}. Shell sessions arrive in phase 1; nothing is opened until you connect.",
                    self.machine.name,
                    s.kind.label()
                ),
                None => format!("{} has no command service set up.", self.machine.name),
            },
            TermTab::Console => {
                if self.console.is_empty() {
                    let what = match self.machine.file_service().map(|s| s.kind) {
                        Some(ServiceKind::Ftp) => "FTP commands (SITE CHMOD 755 file, STAT, SYST, HELP)",
                        _ => "the file protocol's own commands",
                    };
                    format!("Type {what} below; they go to {} on the file connection.", self.machine.name)
                } else {
                    self.console.join("\n")
                }
            }
            TermTab::Log => {
                let lines = self.sessions.trace(&self.machine.id);
                if lines.is_empty() {
                    format!("No protocol trace yet. {}", self.status_line())
                } else {
                    // The recent end: a view shows the last screenful.
                    lines[lines.len().saturating_sub(500)..].join("\n")
                }
            }
            TermTab::Transfers => self.transfers_text(),
        }
    }

    pub fn can_console(&self) -> bool {
        self.can_disconnect() && self.console_job.is_none()
    }

    fn picks(&self, side: Side, names: &[String]) -> (Vec<Pick>, Vec<Option<u64>>) {
        let mut picks = Vec::new();
        let mut sizes = Vec::new();
        for n in names {
            match side {
                Side::Local => {
                    if let Some(e) = self.local_rows.iter().find(|e| &e.name == n) {
                        picks.push(Pick {
                            name: e.name.clone(),
                            is_dir: e.is_dir,
                        });
                        sizes.push(Some(e.size));
                    }
                }
                Side::Remote => {
                    if let Some(e) = self.remote_rows.iter().find(|e| &e.name == n) {
                        picks.push(Pick {
                            name: e.name.clone(),
                            is_dir: e.is_folder(),
                        });
                        sizes.push(e.size);
                    }
                }
            }
        }
        (picks, sizes)
    }

    /// Walk the picks into the queue; `start` runs the queue (Copy), otherwise it only fills.
    fn copy(&mut self, from: Side, names: Vec<String>, start: bool) -> Vec<Effect> {
        let dir = self.local_dir.clone();
        self.copy_into(from, names, dir, start)
    }

    /// As [`Self::copy`], with `local_dir` as the folder on this computer.
    fn copy_into(
        &mut self,
        from: Side,
        names: Vec<String>,
        local_dir: PathBuf,
        start: bool,
    ) -> Vec<Effect> {
        if !self.can_disconnect() || self.remote_dir.is_none() {
            return vec![Effect::Message(format!(
                "Connect to {} first.",
                self.machine.name
            ))];
        }
        let (picks, sizes) = self.picks(from, &names);
        if picks.is_empty() {
            return Vec::new();
        }
        let group = Group {
            local_dir,
            picks,
            sizes,
        };
        self.start_plan(direction_from(from), group, start);
        Vec::new()
    }

    /// The queue's settings (the app's, and again when they change).
    pub(crate) fn set_queue_prefs(&mut self, prefs: QueuePrefs) {
        self.xfer.prefs = prefs;
    }

    fn charset(&self) -> lc_core::Charset {
        lc_core::Charset::from_label(&self.machine.charset).unwrap_or_default()
    }

    fn remote_style(&self) -> lc_core::PathStyle {
        self.remote_dir
            .as_ref()
            .map_or(lc_core::PathStyle::Unix, |d| d.style)
    }

    /// Files dropped onto a pane: uploaded to the machine, or copied into the local folder.
    fn drop_files(&mut self, side: Side, paths: Vec<PathBuf>) -> Vec<Effect> {
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
        if paths.is_empty() {
            return Vec::new();
        }
        let name = |p: &PathBuf| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        match side {
            Side::Local => {
                // Dropping files onto the folder they are already in does nothing.
                let paths: Vec<PathBuf> = paths
                    .into_iter()
                    .filter(|p| p.parent() != Some(self.local_dir.as_path()))
                    .collect();
                self.import(paths)
            }
            Side::Remote => {
                if !self.can_disconnect() || self.remote_dir.is_none() {
                    return vec![Effect::Message(format!(
                        "Connect to {} first.",
                        self.machine.name
                    ))];
                }
                let mut groups: Vec<Group> = Vec::new();
                for p in &paths {
                    let parent = p.parent().map(Path::to_path_buf).unwrap_or_default();
                    let pick = Pick {
                        name: name(p),
                        is_dir: p.is_dir(),
                    };
                    let size = std::fs::metadata(p)
                        .ok()
                        .filter(|m| m.is_file())
                        .map(|m| m.len());
                    match groups.iter_mut().find(|g| g.local_dir == parent) {
                        Some(g) => {
                            g.picks.push(pick);
                            g.sizes.push(size);
                        }
                        None => groups.push(Group {
                            local_dir: parent,
                            picks: vec![pick],
                            sizes: vec![size],
                        }),
                    }
                }
                for g in groups {
                    self.start_plan(Direction::Upload, g, true);
                }
                Vec::new()
            }
        }
    }
}

impl ConnectionController {
    fn run_op(
        &mut self,
        side: Side,
        label: String,
        f: impl FnOnce(&mut dyn lc_core::FileSession, &RemotePath) -> lc_core::Result<()>
            + Send
            + 'static,
        local: impl FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
    ) {
        match side {
            Side::Remote => {
                let Some(dir) = self.remote_dir.clone() else {
                    return;
                };
                let job = self.sessions.run(
                    &self.machine.id,
                    label,
                    Arc::clone(&self.notify),
                    move |fs, _| Ok(f(fs, &dir)?),
                );
                self.ops.push((side, job));
            }
            Side::Local => {
                let dir = self.local_dir.clone();
                let job = Job::spawn(label, Arc::clone(&self.notify), move |_| Ok(local(&dir)?));
                self.ops.push((side, job));
            }
        }
    }

    /// The rule a new name follows on this side: Windows' here, the machine's own over there.
    fn name_rule(&self, side: Side) -> fn(&str) -> Result<(), String> {
        match (side, self.remote_style()) {
            (Side::Local, _) | (Side::Remote, lc_core::PathStyle::Dos) => valid_name,
            (Side::Remote, lc_core::PathStyle::Unix) => valid_unix_name,
            (Side::Remote, lc_core::PathStyle::Mac) => valid_mac_name,
        }
    }

    fn answer_pending(&mut self, answer: Option<String>) -> Vec<Effect> {
        let Some(name) = answer.map(|a| a.trim().to_string()) else {
            self.pending = None;
            return Vec::new();
        };
        let side = match &self.pending {
            Some(Pending::NewFolder(s)) | Some(Pending::Rename(s, _)) => *s,
            _ => Side::Local,
        };
        let mut checked = (self.name_rule(side))(&name);
        if checked.is_ok() && side == Side::Remote {
            let max = (self.machine.file_service().map(|s| s.kind) == Some(ServiceKind::Afp))
                .then_some(31);
            if let Some(why) = names::remote_problem(&name, self.charset(), max) {
                checked = Err(format!("{name}: {why}."));
            }
        }
        if let Err(e) = checked {
            self.pending = None;
            return vec![Effect::Message(e)];
        }
        match self.pending.take() {
            Some(Pending::NewFolder(side)) => {
                let (n1, n2) = (name.clone(), name.clone());
                self.run_op(
                    side,
                    format!("New folder {name}"),
                    move |fs, dir| fs.mkdir(&dir.join(&n1)),
                    move |dir| std::fs::create_dir(dir.join(&n2)),
                );
            }
            Some(Pending::Rename(side, old)) if old != name => {
                let (o1, n1, o2, n2) = (old.clone(), name.clone(), old.clone(), name.clone());
                self.run_op(
                    side,
                    format!("Rename {old} to {name}"),
                    move |fs, dir| fs.rename(&dir.join(&o1), &dir.join(&n1)),
                    move |dir| std::fs::rename(dir.join(&o2), dir.join(&n2)),
                );
            }
            _ => {}
        }
        Vec::new()
    }

    /// The running job's phase, or the last error, for the status line.
    pub fn status_line(&self) -> String {
        let running = [
            self.connect_job.as_ref().map(|j| j.snapshot()),
            self.list_job.as_ref().map(|j| j.snapshot()),
            self.disconnect_job.as_ref().map(|j| j.snapshot()),
            self.rsh_job.as_ref().map(|j| j.snapshot()),
        ]
        .into_iter()
        .flatten()
        .next();
        if let Some(snap) = running {
            return if snap.phase.is_empty() {
                snap.label
            } else {
                snap.phase
            };
        }
        if let Some(e) = self.remote_error.as_ref().or(self.local_error.as_ref()) {
            return e.clone();
        }
        let state = match self.state() {
            ConnState::Connected => format!("Connected to {}.", self.machine.name),
            _ => "Not connected.".into(),
        };
        match self.queue_summary() {
            q if q.is_empty() => state,
            q => format!("{state} {q}."),
        }
    }

    fn list_local(&mut self) {
        let dir = self.local_dir.clone();
        self.local_error = None;
        self.local_job = Some(Job::spawn(
            format!("List {}", dir.display()),
            Arc::clone(&self.notify),
            move |_| Ok(list_dir(&dir)?),
        ));
    }

    /// A local folder for the recent list (and the settings file).
    fn note_local(&mut self, dir: PathBuf) {
        if self.recent.first() != Some(&dir) {
            add_recent(&mut self.recent, dir.clone());
            self.place_events.push(PlaceEvent::Visited(dir));
        }
    }

    /// A folder on the machine for its recent list.
    fn note_remote(&mut self, dir: &RemotePath) {
        let text = dir.to_string();
        if self.machine.recent_folders.first() != Some(&text) {
            add_recent(&mut self.machine.recent_folders, text);
            self.remote_places_changed = true;
        }
    }

    /// The Places menu for one pane: what it shows and what each line does.
    fn places_menu(&mut self, side: Side) -> Vec<Effect> {
        let mut lines: Vec<(MenuItem, Option<Target>)> = Vec::new();
        match side {
            Side::Local => {
                let here = &self.local_dir;
                lines.push(line(
                    format!("Home: {}", self.home.display()),
                    Some(Target::Local(self.home.clone())),
                ));
                for p in &self.system {
                    lines.push(line(p.label.clone(), Some(Target::Local(p.path.clone()))));
                }
                if !self.drives.is_empty() {
                    lines.push((MenuItem::Separator, None));
                }
                for d in &self.drives {
                    lines.push(line(
                        d.display().to_string(),
                        Some(Target::Local(d.clone())),
                    ));
                }
                let local = |p: &PathBuf| (p.display().to_string(), Target::Local(p.clone()));
                group(&mut lines, "Pinned", self.pins.iter().map(local));
                group(
                    &mut lines,
                    "Recent",
                    self.recent
                        .iter()
                        .filter(|p| !self.pins.contains(p))
                        .map(local),
                );
                let pinned = self.pins.contains(here);
                lines.push((MenuItem::Separator, None));
                lines.push(line(
                    pin_label(pinned),
                    Some(Target::Pin(Side::Local, !pinned)),
                ));
                lines.push(line(
                    "Go to folder...".into(),
                    Some(Target::GoTo(Side::Local)),
                ));
            }
            Side::Remote => {
                let dir = self.remote_dir.clone();
                let on = |t: Target| dir.as_ref().map(|_| t);
                let style = dir.as_ref().map(|d| d.style).unwrap_or_default();
                let home = self
                    .remote_home
                    .clone()
                    .unwrap_or_else(|| RemotePath::root(style));
                let top = RemotePath::root(style);
                lines.push(line(
                    format!("Login folder: {home}"),
                    on(Target::Remote(home)),
                ));
                lines.push(line(format!("Top level: {top}"), on(Target::Remote(top))));
                let pins = &self.machine.pinned_folders;
                if dir.is_some() {
                    let remote =
                        |p: &String| (p.clone(), Target::Remote(RemotePath::parse(style, p)));
                    group(&mut lines, "Pinned", pins.iter().map(remote));
                    group(
                        &mut lines,
                        "Recent",
                        self.machine
                            .recent_folders
                            .iter()
                            .filter(|p| !pins.contains(p))
                            .map(remote),
                    );
                }
                let pinned = dir.as_ref().is_some_and(|d| pins.contains(&d.to_string()));
                lines.push((MenuItem::Separator, None));
                lines.push(line(
                    pin_label(pinned),
                    on(Target::Pin(Side::Remote, !pinned)),
                ));
                lines.push(line(
                    "Go to folder...".into(),
                    on(Target::GoTo(Side::Remote)),
                ));
            }
        }
        let (items, targets) = lines.into_iter().unzip();
        self.menu = Some((side, targets));
        vec![Effect::Menu(MenuSpec {
            purpose: match side {
                Side::Local => "local.places",
                Side::Remote => "remote.places",
            },
            anchor: match side {
                Side::Local => ids::LOCAL_PLACES,
                Side::Remote => ids::REMOTE_PLACES,
            },
            items,
        })]
    }

    fn go_to_target(&mut self, t: Target) -> Vec<Effect> {
        match t {
            Target::Local(p) if p.is_dir() => self.go_local(p),
            Target::Local(p) => {
                return vec![Effect::Message(format!(
                    "{} is not there any more.",
                    p.display()
                ))]
            }
            Target::Remote(p) => {
                if self.remote_dir.is_some() {
                    self.go_remote(p);
                }
            }
            Target::Pin(Side::Local, on) => {
                let dir = self.local_dir.clone();
                self.pins.retain(|p| p != &dir);
                if on {
                    self.pins.push(dir.clone());
                }
                self.place_events.push(PlaceEvent::Pinned(dir, on));
            }
            Target::Pin(Side::Remote, on) => {
                if let Some(dir) = &self.remote_dir {
                    let text = dir.to_string();
                    self.machine.pinned_folders.retain(|p| p != &text);
                    if on {
                        self.machine.pinned_folders.push(text);
                    }
                    self.remote_places_changed = true;
                }
            }
            Target::GoTo(side) => return self.apply(Action::GoTo(side)),
        }
        Vec::new()
    }

    fn go_local(&mut self, dir: PathBuf) {
        self.local_dir = dir;
        self.list_local();
    }

    fn list_remote(&mut self) {
        if let Some(dir) = self.remote_dir.clone() {
            self.remote_error = None;
            self.list_job = Some(self.sessions.list(
                &self.machine.id,
                dir,
                Arc::clone(&self.notify),
            ));
        }
    }

    fn go_remote(&mut self, dir: RemotePath) {
        self.remote_dir = Some(dir);
        self.list_remote();
    }

    /// Connect the file service, asking for the password first where it is needed.
    fn start_connect(&mut self) -> Vec<Effect> {
        let kind = self.machine.file_service().map(|s| s.kind);
        // No password for the demo or NFS, nor up front for SSH with keys to try.
        let key = kind == Some(ServiceKind::Ssh)
            && (self.machine.key_file.is_some() || self.machine.use_agent);
        let needed = !key && kind.is_some_and(ServiceKind::needs_password);
        match self.password_for(Login::Files, needed) {
            Ok(p) => self.connect(p),
            Err(ask) => return vec![*ask],
        }
        Vec::new()
    }

    fn connect(&mut self, password: Option<String>) {
        self.remote_error = None;
        self.connect_job = Some(self.sessions.connect(
            &self.machine,
            password,
            Arc::clone(&self.notify),
        ));
    }

    /// Start a clean log-off; also used when the tab closes.
    pub(crate) fn begin_disconnect(&mut self) -> Option<Job<()>> {
        if !self.can_disconnect() {
            return None;
        }
        self.queue_on_disconnect();
        let job = self
            .sessions
            .disconnect(&self.machine.id, Arc::clone(&self.notify));
        self.disconnect_job = Some(job.clone());
        Some(job)
    }

    fn set_share(&mut self, share: f32) {
        let share = share.clamp(MIN_SHARE, 1.0 - MIN_SHARE);
        if (share - self.layout.files_share).abs() > f32::EPSILON {
            self.layout.files_share = share;
            self.layout_changed = true;
        }
    }
}

/// A local Go to answer must be an existing folder.
fn existing_folder(text: &str) -> Result<(), String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(String::new());
    }
    if Path::new(t).is_dir() {
        Ok(())
    } else {
        Err("No such folder on this computer.".into())
    }
}

fn not_empty(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err(String::new())
    } else {
        Ok(())
    }
}

/// A typed remote folder: absolute as the machine spells it, or relative to `dir`.
fn remote_target(dir: &RemotePath, text: &str) -> RemotePath {
    let sep = dir.style.separator();
    // A classic Mac path is relative when it starts with ':'; others when it lacks the leading separator.
    let relative = match dir.style {
        lc_core::PathStyle::Mac => text.starts_with(':'),
        _ => !text.starts_with(sep),
    };
    if relative {
        RemotePath::parse(dir.style, &format!("{dir}{sep}{text}"))
    } else {
        RemotePath::parse(dir.style, text)
    }
}

fn pin_label(pinned: bool) -> String {
    if pinned {
        "Unpin this folder"
    } else {
        "Pin this folder"
    }
    .into()
}

/// A menu line; it is greyed when it has nothing to do.
fn line(label: String, target: Option<Target>) -> (MenuItem, Option<Target>) {
    (
        MenuItem::Item {
            label,
            enabled: target.is_some(),
        },
        target,
    )
}

/// A headed group of places (Pinned, Recent) with a separator above; nothing when empty.
fn group(
    lines: &mut Vec<(MenuItem, Option<Target>)>,
    heading: &str,
    places: impl Iterator<Item = (String, Target)>,
) {
    let mut places = places.peekable();
    if places.peek().is_none() {
        return;
    }
    lines.push((MenuItem::Separator, None));
    lines.push((MenuItem::Heading(heading.into()), None));
    lines.extend(places.map(|(l, t)| line(l, Some(t))));
}

impl Controller for ConnectionController {
    type Action = Action;

    fn apply(&mut self, action: Action) -> Vec<Effect> {
        if let Some(out) = self.gate(&action) {
            return out;
        }
        if matches!(action, Action::ShellText(_) | Action::ShellKey(_)) {
            if let Some(out) = self.shell_gate() {
                return out;
            }
        }
        match action {
            Action::Swap => {
                // The areas trade places and sizes: the one on top keeps the larger share.
                self.layout.swapped = !self.layout.swapped;
                self.layout.files_share = 1.0 - self.layout.files_share;
                self.layout_changed = true;
            }
            Action::SetFilesShare(share) => self.set_share(share),
            Action::SelectTerm(t) => self.term = t,
            Action::LocalHome => self.go_local(self.home.clone()),
            Action::LocalUp => {
                if let Some(p) = self.local_dir.parent().map(Path::to_path_buf) {
                    self.go_local(p);
                }
            }
            Action::LocalOpen(name) => {
                let target = self.local_dir.join(&name);
                if target.is_dir() {
                    self.go_local(target);
                }
            }
            Action::Places(side) => return self.places_menu(side),
            Action::PlaceChosen(side, i) => {
                let target = match (self.menu.take(), i) {
                    (Some((s, targets)), Some(i)) if s == side => targets.get(i).cloned().flatten(),
                    _ => None,
                };
                if let Some(t) = target {
                    return self.go_to_target(t);
                }
            }
            Action::GoTo(Side::Local) => {
                return vec![Effect::AskPath(PathSpec {
                    purpose: "local.go_to",
                    title: "Go to folder".into(),
                    prompt: "Go to a folder on this computer:".into(),
                    initial: self.local_path(),
                    recent: self.recent.clone(),
                    hint: "Pinned and recent folders are also in the Places menu.".into(),
                    validator: existing_folder,
                })];
            }
            Action::GoTo(Side::Remote) => {
                let Some(dir) = &self.remote_dir else {
                    return vec![Effect::Message("Connect first.".into())];
                };
                return vec![Effect::AskText(TextSpec {
                    purpose: "remote.go_to",
                    title: "Go to folder".into(),
                    prompt: format!("Go to a folder on {}:", self.machine.name),
                    initial: dir.to_string(),
                    ok_label: "Go".into(),
                    validator: not_empty,
                })];
            }
            Action::LocalGoToAnswered(None) | Action::RemoteGoToAnswered(None) => {}
            Action::LocalGoToAnswered(Some(path)) => {
                if let Err(e) = existing_folder(&path.to_string_lossy()) {
                    return vec![Effect::Message(e)];
                }
                self.note_local(path.clone());
                self.go_local(path);
            }
            Action::RemoteGoToAnswered(Some(text)) => {
                if let Some(dir) = &self.remote_dir {
                    let target = remote_target(dir, text.trim());
                    self.go_remote(target);
                }
            }
            Action::Connect => {
                if matches!(self.state(), ConnState::Lost | ConnState::Idle) {
                    return self.rejoin(None);
                }
                if !self.can_connect() {
                    return Vec::new();
                }
                return self.start_connect();
            }
            Action::RejoinAnswered(choice) => return self.rejoin_answered(choice),
            Action::ResumeQueueAnswered(choice) => return self.resume_answered(choice),
            Action::PasswordAnswered(None) => {}
            Action::PasswordAnswered(Some(p)) => {
                // Kept for this tab only, so a reconnect does not ask again.
                self.session_password = Some(p.clone());
                self.log_in(self.asking_for, Some(p));
            }
            Action::Disconnect => {
                self.link.rejoin = None;
                if matches!(self.state(), ConnState::Lost | ConnState::Idle) {
                    // Nothing to log off; the pane empties and nothing connects again.
                    self.link.cut.clear();
                    self.link.queue_was_running = false;
                    let job = self
                        .sessions
                        .disconnect(&self.machine.id, Arc::clone(&self.notify));
                    self.disconnect_job = Some(job);
                    return Vec::new();
                }
                if let Some(ask) = self.ask_disconnect() {
                    return ask;
                }
                self.begin_disconnect();
            }
            Action::RemoteHome => {
                if let Some(h) = self.remote_home.clone() {
                    self.go_remote(h);
                }
            }
            Action::RemoteUp => {
                if let Some(p) = self.remote_dir.as_ref().and_then(RemotePath::parent) {
                    self.go_remote(p);
                }
            }
            Action::RemoteOpen(name) => {
                let is_dir = self
                    .remote_rows
                    .iter()
                    .any(|e| e.name == name && e.is_folder());
                if let (true, Some(dir)) = (is_dir, self.remote_dir.clone()) {
                    self.go_remote(dir.join(&name));
                }
            }
            Action::Refresh(Side::Local) => self.list_local(),
            Action::Refresh(Side::Remote) => self.list_remote(),
            Action::ShellConnect => {
                self.term = TermTab::Shell;
                return self.shell_connect();
            }
            Action::ShellDisconnect => {
                self.begin_shell_close();
            }
            Action::ShellText(text) => {
                if let Some(s) = self
                    .shell
                    .as_ref()
                    .filter(|s| s.state() == ConnState::Connected)
                {
                    if let Err(e) = s.send_text(&text) {
                        self.shell_error = Some(format!("Could not send: {e}"));
                    }
                }
            }
            Action::ShellKey(key) => {
                let app = self.shell.as_ref().is_some_and(|s| s.screen().app_cursor);
                self.shell_send(&key_bytes(key, app));
            }
            Action::ShellResize(cols, rows) => {
                let (cols, rows) = (cols.clamp(10, 500), rows.clamp(2, 300));
                self.shell_size = (cols, rows);
                if let Some(s) = self
                    .shell
                    .as_ref()
                    .filter(|s| s.state() == ConnState::Connected)
                {
                    let _ = s.resize(cols, rows);
                }
            }
            Action::Copy(from, names) => return self.copy(from, names, true),
            Action::HostKeyAnswered(choice) => self.host_key_answered(choice),
            Action::SetUpKeyLogin => return self.start_key_setup(),
            Action::SetUpRshTrust => return self.start_rsh(true),
            Action::CheckRsh => return self.start_rsh(false),
            Action::RshRefusedAnswered(i) => return self.rsh_refused_answered(i),
            Action::KeyLoginPasswordAnswered(choice) => {
                // The key itself was stored when the setup finished; this only drops the password.
                if let (0, Some(key)) = (choice, self.machine.key_file.clone()) {
                    self.key_login_changed = Some((key, true));
                    self.machine.password = Password::Ask;
                }
            }
            Action::CopyConflict(choice) => return self.import_answered(choice),
            Action::CopyRenameAnswered(answer) => {
                return match self.pending {
                    Some(Pending::QueueRename(_)) => {
                        let out = self.queue_rename_answered(answer);
                        self.nudge_queue(out)
                    }
                    Some(Pending::Import(_)) => self.import_rename_answered(answer),
                    _ => Vec::new(),
                };
            }
            Action::Drop(side, paths) => return self.drop_files(side, paths),
            Action::AddToQueue(side, names) => return self.copy(side, names, false),
            Action::CopyOut(names, dir) => return self.copy_into(Side::Remote, names, dir, true),
            Action::Queue(op, ids) => {
                let out = self.queue_op(op, &ids);
                return self.nudge_queue(out);
            }
            Action::QueueManyAnswered(choice) => {
                let out = self.queue_many_answered(choice);
                return self.nudge_queue(out);
            }
            Action::QueueExistsAnswered(choice) => {
                self.queue_exists_answered(choice);
                return self.nudge_queue(Vec::new());
            }
            Action::QueueItemAnswered(choice) => {
                self.queue_item_answered(choice);
                return self.nudge_queue(Vec::new());
            }
            Action::DisconnectAnswered(choice) => {
                self.disconnect_answered(choice);
                return self.nudge_queue(Vec::new());
            }
            Action::NewFolder(side) => {
                if side == Side::Remote && !self.can_disconnect() {
                    return Vec::new();
                }
                self.pending = Some(Pending::NewFolder(side));
                return vec![Effect::AskText(super::TextSpec {
                    purpose: "conn.name",
                    title: "New folder".into(),
                    prompt: format!(
                        "Name of the new folder in {}:",
                        match side {
                            Side::Local => self.local_path(),
                            Side::Remote => self.remote_path(),
                        }
                    ),
                    initial: String::new(),
                    ok_label: "Create".into(),
                    validator: self.name_rule(side),
                })];
            }
            Action::NewFolderAnswered(a) | Action::RenameAnswered(a) => {
                return self.answer_pending(a)
            }
            Action::Rename(side, old) => {
                self.pending = Some(Pending::Rename(side, old.clone()));
                let validator = self.name_rule(side);
                return vec![Effect::AskText(super::TextSpec {
                    purpose: "conn.name",
                    title: "Rename".into(),
                    prompt: format!("New name for {old}:"),
                    initial: old,
                    ok_label: "Rename".into(),
                    validator,
                })];
            }
            Action::Delete(side, names) => {
                let (picks, _) = self.picks(side, &names);
                if picks.is_empty() {
                    return Vec::new();
                }
                let place = match side {
                    Side::Local => "this computer".to_string(),
                    Side::Remote => self.machine.name.clone(),
                };
                let folders = picks.iter().any(|p| p.is_dir);
                let what = if picks.len() == 1 {
                    picks[0].name.clone()
                } else {
                    format!("{} items", picks.len())
                };
                let tail = if folders {
                    " Folders are deleted with everything in them."
                } else {
                    ""
                };
                self.pending = Some(Pending::Delete(side, picks));
                return vec![Effect::Confirm(super::ConfirmSpec {
                    purpose: "conn.delete",
                    title: "Delete".into(),
                    message: format!("Delete {what} on {place}? This cannot be undone.{tail}"),
                    confirm_label: "Delete".into(),
                    destructive: true,
                })];
            }
            Action::DeleteConfirmed(yes) => {
                if let Some(Pending::Delete(side, picks)) = self.pending.take() {
                    if yes {
                        let p2 = picks.clone();
                        let label = format!("Delete {} item(s)", picks.len());
                        self.run_op(
                            side,
                            label,
                            move |fs, dir| {
                                for p in &picks {
                                    transfer::remove_tree(fs, &dir.join(&p.name), p.is_dir)?;
                                }
                                Ok(())
                            },
                            move |dir| {
                                for p in &p2 {
                                    let path = dir.join(&p.name);
                                    if p.is_dir {
                                        std::fs::remove_dir_all(path)?;
                                    } else {
                                        std::fs::remove_file(path)?;
                                    }
                                }
                                Ok(())
                            },
                        );
                    }
                }
            }
            Action::ConsoleSend(line) => {
                let line = line.trim().to_string();
                if line.is_empty() || !self.can_console() {
                    return Vec::new();
                }
                self.console.push(format!("> {line}"));
                let l = line.clone();
                let Some(dir) = self.remote_dir.clone() else {
                    return Vec::new();
                };
                self.console_job = Some(self.sessions.run(
                    &self.machine.id,
                    format!("Console: {line}"),
                    Arc::clone(&self.notify),
                    move |fs, _| Ok(fs.quote(&dir, &l)?),
                ));
            }
        }
        Vec::new()
    }

    fn tick(&mut self) -> Vec<Effect> {
        let mut out = Vec::new();
        if let Some(r) = self.drives_job.as_ref().and_then(job_outcome) {
            self.drives_job = None;
            self.drives = r.unwrap_or_default();
        }
        if let Some(r) = self.local_job.as_ref().and_then(job_outcome) {
            self.local_job = None;
            match r {
                Ok(rows) => self.local_rows = rows,
                Err(e) => {
                    self.local_rows.clear();
                    self.local_error =
                        Some(format!("Could not list {}: {e}", self.local_dir.display()));
                }
            }
        }
        if let Some(r) = self.connect_job.as_ref().and_then(job_outcome) {
            self.connect_job = None;
            let note = self.sessions.take_key_note(&self.machine.id);
            let asked = self.key_note(note, Login::Files);
            let changed = asked.is_some();
            out.extend(asked);
            match r {
                Ok(home) => {
                    self.remote_home = Some(home.clone());
                    if let Some(more) = self.rejoined(&home) {
                        out.extend(more);
                    } else {
                        // The machine's own start folder, when set; else where the login put us.
                        let start = match self.machine.remote_folder.trim() {
                            "" => home,
                            f => remote_target(&home, f),
                        };
                        self.go_remote(start);
                    }
                }
                Err(e) if changed => {
                    self.rejoin_failed();
                    self.remote_error = Some(format!("Could not connect: {e}"))
                }
                // No key got in and no password was given: ask for it, as ssh does, then retry.
                Err(e)
                    if self.session_password.is_none() && crate::model::ssh::keys_refused(&e) =>
                {
                    self.asking_for = Login::Files;
                    out.push(self.ask_password());
                }
                Err(e) => {
                    self.rejoin_failed();
                    self.remote_error = Some(format!("Could not connect: {e}"));
                    if let Some(why) = self.sessions.take_rsh_note(&self.machine.id) {
                        out.push(self.rsh_refused(why));
                        return out;
                    }
                    self.remote_error = Some(format!("Could not connect: {e}"));
                    out.push(Effect::Message(format!(
                        "{}: could not connect: {e}",
                        self.machine.name
                    )));
                }
            }
        }
        if let Some(r) = self.list_job.as_ref().and_then(job_outcome) {
            self.list_job = None;
            match r {
                Ok(rows) => {
                    self.link_worked();
                    self.link.listed = self.remote_dir.clone();
                    self.remote_rows = rows
                }
                Err(e) => {
                    self.link.failed = true;
                    self.remote_error = Some(format!("Could not list {}: {e}", self.remote_path()))
                }
            }
        }
        let mut refresh = (false, false);
        self.tick_transfers(&mut out, &mut refresh);
        let mut finished = Vec::new();
        self.ops.retain(|(side, job)| match job_outcome(job) {
            Some(r) => {
                finished.push((*side, r));
                false
            }
            None => true,
        });
        for (side, r) in finished {
            match side {
                Side::Local => refresh.0 = true,
                Side::Remote => refresh.1 = true,
            }
            if let Err(e) = r {
                // A lost link is said once, by the reconnect, not by every job it failed.
                if side == Side::Remote && self.state() == ConnState::Lost {
                    self.link.failed = true;
                    log::warn!("{}: {e}", self.machine.name);
                } else {
                    out.push(Effect::Message(e));
                }
            } else if side == Side::Remote {
                self.link_worked();
            }
        }
        if let Some(r) = self.console_job.as_ref().and_then(job_outcome) {
            self.console_job = None;
            match r {
                Ok(text) => {
                    self.link_worked();
                    self.console.extend(text.lines().map(|l| format!("< {l}")))
                }
                Err(e) => {
                    self.link.failed = true;
                    self.console.push(format!("  {e}"))
                }
            }
            // A command may have changed the folder or its files.
            refresh.1 = true;
        }
        if refresh.0 {
            self.list_local();
        }
        if refresh.1 && self.list_job.is_none() {
            self.list_remote();
        }
        if let Some(r) = self.rsh_job.as_ref().and_then(job_outcome) {
            self.rsh_job = None;
            out.push(Effect::Message(rsh_report(&self.machine.name, r)));
        }
        if let Some(r) = self.key_setup.as_ref().and_then(job_outcome) {
            self.key_setup = None;
            let note = take_note(&self.key_setup_note);
            match self.key_note(note, Login::Setup) {
                Some(ask) => out.push(ask),
                None => out.extend(self.key_setup_done(r)),
            }
        }
        if let Some(r) = self.shell_open.as_ref().and_then(job_outcome) {
            self.shell_open = None;
            let note = take_note(&self.shell_key_note);
            out.extend(self.key_note(note, Login::Shell));
            match r {
                Ok(s) => {
                    // The machine closed the last one, not the user: say when it came back.
                    let again = self.shell.as_ref().is_some_and(|o| o.ended().is_some());
                    if again && !self.link.shell_closed {
                        s.note(&format!("{} Connected again.", rejoin::clock()));
                    }
                    self.shell = Some(s)
                }
                Err(e)
                    if self.session_password.is_none() && crate::model::ssh::keys_refused(&e) =>
                {
                    self.asking_for = Login::Shell;
                    out.push(self.ask_password());
                }
                Err(e) => self.shell_error = Some(format!("Could not open a shell: {e}")),
            }
        }
        if let Some(r) = self.shell_close.as_ref().and_then(job_outcome) {
            self.shell_close = None;
            if let Err(e) = r {
                out.push(Effect::Message(format!("{}: {e}", self.machine.name)));
            }
        }
        if let Some(r) = self.disconnect_job.as_ref().and_then(job_outcome) {
            self.disconnect_job = None;
            self.link.listed = None;
            self.remote_rows.clear();
            self.remote_dir = None;
            self.remote_home = None;
            if let Err(e) = r {
                out.push(Effect::Message(format!("{}: {e}", self.machine.name)));
            }
        }
        self.watch_link(&mut out);
        out
    }
}

fn size_text(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.0} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / K / K)
    } else {
        format!("{:.1} GB", f / K / K / K)
    }
}

/// `YYYY-MM-DD` from seconds since 1970 (UTC; zones come with the listing parsers).
fn date_text(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn mode_text(mode: u32, dir: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::job::NoNotify;
    use crate::model::machines::Service;
    use transfers::RENAME_PURPOSE;

    fn tab(home: &Path) -> ConnectionController {
        let mut m = Machine::new("sparc", "demo");
        m.services.push(Service {
            kind: ServiceKind::Demo,
            port: 0,
        });
        ConnectionController::new(
            m,
            home,
            Vec::new(),
            Vec::new(),
            Sessions::new(),
            Arc::new(NoNotify),
        )
    }

    #[test]
    fn host_keys_are_learned_and_a_change_is_asked_about() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        assert!(c
            .key_note(Some(KeyNote::Learned("ssh-dss AAAA".into())), Login::Files)
            .is_none());
        assert_eq!(c.take_host_key_change().as_deref(), Some("ssh-dss AAAA"));
        let changed = || KeyNote::Changed {
            line: "ssh-dss BBBB".into(),
            md5: "ab:cd".into(),
            sha256: "SHA256:xyz".into(),
        };
        let e = c.key_note(Some(changed()), Login::Files).unwrap();
        assert!(
            matches!(&e, Effect::Choose(s) if s.purpose == "conn.hostkey" && s.message.contains("xyz"))
        );
        c.apply(Action::HostKeyAnswered(1));
        assert_eq!(c.machine.host_key.as_deref(), Some("ssh-dss AAAA"));
        assert_eq!(c.take_host_key_change(), None);
        settle(&mut c);
        c.key_note(Some(changed()), Login::Files);
        c.apply(Action::HostKeyAnswered(0));
        assert_eq!(c.take_host_key_change().as_deref(), Some("ssh-dss BBBB"));
        settle(&mut c);
    }

    #[test]
    fn rsh_refusal_offers_trust_a_check_and_another_service() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.machine.services = [ServiceKind::Rsh, ServiceKind::Rexec, ServiceKind::Ftp]
            .into_iter()
            .map(|kind| Service {
                kind,
                port: kind.default_port(),
            })
            .collect();
        c.machine.password = Password::Saved("pw".into());
        let why = Untrusted::Refused {
            local: "192.168.1.10:1023".parse().unwrap(),
            said: "Permission denied.".into(),
        };
        let Effect::Choose(q) = c.rsh_refused(why) else {
            panic!("not a question")
        };
        assert_eq!(
            q.choices,
            ["Set up rsh trust", "Check", "Use FTP instead", "Close"]
        );
        assert!(
            q.message.contains("192.168.1.10 port 1023"),
            "{}",
            q.message
        );
        assert!(q.message.contains("over rexec"), "{}", q.message);
        // Use FTP: it moves before rsh, is saved, and a connect starts.
        c.apply(Action::RshRefusedAnswered(2));
        let kinds: Vec<_> = c
            .take_services_change()
            .unwrap()
            .iter()
            .map(|s| s.kind)
            .collect();
        assert_eq!(
            kinds,
            [ServiceKind::Ftp, ServiceKind::Rsh, ServiceKind::Rexec]
        );
        assert!(c.connect_job.is_some());
        // A computer that hands out no low port gets only the other service.
        let Effect::Choose(q) = c.rsh_refused(Untrusted::NoLowPort) else {
            panic!("not a question")
        };
        assert_eq!(q.choices, ["Use FTP instead", "Close"]);
    }

    #[test]
    fn rsh_report_explains_a_rewritten_port() {
        let seen = lc_rcmd::Seen {
            local: "192.168.1.10:51000".parse().unwrap(),
            server: Some("172.17.0.1:40112".parse().unwrap()),
        };
        let r = TrustReport {
            via: Some(ServiceKind::Rexec),
            seen: Some(seen),
            added: vec!["172.17.0.1 dani".into()],
            already: Vec::new(),
            rsh: Err("closed".into()),
        };
        let text = rsh_report("irix", Ok(r));
        assert!(
            text.contains("Added to ~/.rhosts on irix (over rexec): 172.17.0.1 dani."),
            "{text}"
        );
        assert!(text.contains("changes the source port"), "{text}");
        assert!(
            text.contains("will be until the port arrives intact"),
            "{text}"
        );
    }

    #[test]
    fn key_login_offers_to_stop_saving_the_password() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.machine.password = Password::Saved("pw".into());
        let e = c.key_setup_done(Ok(KeySetup {
            key_file: PathBuf::from("keys/sparc_rsa"),
            algorithm: "ssh-rsa".into(),
            installed_in: "~/.ssh/authorized_keys".into(),
            new_key: true,
        }));
        assert!(matches!(&e[0], Effect::Choose(s) if s.purpose == "conn.keylogin.password"));
        let key = PathBuf::from("keys/sparc_rsa");
        assert_eq!(c.take_key_login_change(), Some((key.clone(), false)));
        c.apply(Action::KeyLoginPasswordAnswered(0));
        assert_eq!(c.take_key_login_change(), Some((key, true)));
        assert_eq!(c.machine.password, Password::Ask);
    }

    /// Tick until a question with this purpose comes up.
    fn settle_ask(c: &mut ConnectionController, purpose: &str) -> super::super::ChoiceSpec {
        for _ in 0..400 {
            for e in c.tick() {
                if let Effect::Choose(s) = e {
                    assert_eq!(s.purpose, purpose, "{}", s.message);
                    return s;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no {purpose} question");
    }

    #[test]
    fn a_ten_file_tree_goes_up_and_comes_back_even_reordered() {
        let home = tempfile::tempdir().unwrap();
        let files = [
            "tree/a1.txt",
            "tree/a2.txt",
            "tree/a3.txt",
            "tree/sub/b1.txt",
            "tree/sub/b2.txt",
            "tree/sub/b3.txt",
            "tree/sub/deep/c1.txt",
            "tree/sub/deep/c2.txt",
            "tree/other/d1.txt",
            "tree/other/d2.txt",
        ];
        for f in files {
            let p = home.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("contents of {f}")).unwrap();
        }
        std::fs::create_dir_all(home.path().join("tree/empty")).unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        c.apply(Action::Refresh(Side::Local));
        settle(&mut c);
        // Upload: ten rows (folders are not rows), waiting until Start.
        c.apply(Action::AddToQueue(Side::Local, vec!["tree".into()]));
        settle(&mut c);
        let rows = c.queue_rows();
        assert_eq!(rows.len(), 10, "{}", c.transfers_text());
        assert!(rows.iter().all(|r| r.status == "Waiting"));
        // The deepest file to the top, above the folders it needs.
        let c1 = rows.iter().find(|r| r.name == "c1.txt").unwrap().id;
        for _ in 0..12 {
            c.apply(Action::Queue(QueueOp::Up, vec![c1]));
        }
        assert_eq!(c.queue_rows()[0].name, "c1.txt");
        c.apply(Action::Queue(QueueOp::Start, Vec::new()));
        settle(&mut c);
        let done = c
            .queue_rows()
            .iter()
            .filter(|r| r.status.starts_with("Done"))
            .count();
        assert_eq!(done, 10, "{}", c.transfers_text());
        // Back down into a clean folder: same bytes, empty folder included.
        std::fs::remove_dir_all(home.path().join("tree")).unwrap();
        c.apply(Action::Queue(QueueOp::ClearFinished, Vec::new()));
        c.apply(Action::Refresh(Side::Remote));
        settle(&mut c);
        c.apply(Action::Copy(Side::Remote, vec!["tree".into()]));
        settle(&mut c);
        for f in files {
            assert_eq!(
                std::fs::read_to_string(home.path().join(f)).unwrap(),
                format!("contents of {f}"),
                "{f}"
            );
        }
        assert!(home.path().join("tree/empty").is_dir());
    }

    #[test]
    fn disconnect_with_transfers_left_asks_and_can_finish_first() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        std::fs::write(home.path().join("a.txt"), b"aaa").unwrap();
        c.apply(Action::Refresh(Side::Local));
        settle(&mut c);
        c.apply(Action::AddToQueue(Side::Local, vec!["a.txt".into()]));
        settle(&mut c);
        let e = c.apply(Action::Disconnect);
        assert!(
            matches!(&e[0], Effect::Choose(s) if s.purpose == "conn.queue.disconnect"),
            "{e:?}"
        );
        assert_eq!(c.state(), ConnState::Connected);
        c.apply(Action::DisconnectAnswered(0));
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Disconnected);
        assert!(c
            .queue_rows()
            .iter()
            .any(|r| r.name == "a.txt" && r.status.starts_with("Done")));
        // Nothing left: Disconnect just logs off.
        c.apply(Action::Connect);
        settle(&mut c);
        assert!(c.apply(Action::Disconnect).is_empty());
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Disconnected);
    }

    fn settle(c: &mut ConnectionController) {
        for _ in 0..400 {
            c.tick();
            if c.local_job.is_none()
                && c.connect_job.is_none()
                && c.list_job.is_none()
                && c.disconnect_job.is_none()
                && c.ops.is_empty()
                && c.console_job.is_none()
                && !c.copying()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("jobs did not finish");
    }

    #[test]
    fn swap_and_split_are_remembered_once() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        assert_eq!(c.take_layout_change(), None);
        let before = c.layout().files_share;
        c.apply(Action::Swap);
        // The terminal comes up with the files' size, and swapping back restores both.
        assert!((c.layout().files_share - (1.0 - before)).abs() < 1e-6);
        c.apply(Action::Swap);
        assert!(!c.layout().swapped);
        assert!((c.layout().files_share - before).abs() < 1e-6);
        c.apply(Action::Swap);
        c.apply(Action::SetFilesShare(0.99));
        let l = c.take_layout_change().unwrap();
        assert!(l.swapped);
        assert!((l.files_share - (1.0 - MIN_SHARE)).abs() < 1e-6);
        assert_eq!(c.take_layout_change(), None);
    }

    /// Open a pane's Places menu and choose the line labelled `label`.
    fn choose_place(c: &mut ConnectionController, side: Side, label: &str) -> Vec<Effect> {
        let effects = c.apply(Action::Places(side));
        let [Effect::Menu(m)] = &effects[..] else {
            panic!("no menu: {effects:?}");
        };
        let i = m
            .items
            .iter()
            .position(|it| matches!(it, MenuItem::Item { label: l, enabled: true } if l == label))
            .unwrap_or_else(|| panic!("no enabled {label:?} in {:?}", m.items));
        c.apply(Action::PlaceChosen(side, Some(i)))
    }

    fn menu_labels(c: &mut ConnectionController, side: Side) -> Vec<String> {
        let effects = c.apply(Action::Places(side));
        let [Effect::Menu(m)] = &effects[..] else {
            panic!("no menu");
        };
        let labels = m
            .items
            .iter()
            .filter_map(|it| match it {
                MenuItem::Item { label, .. } | MenuItem::Heading(label) => Some(label.clone()),
                MenuItem::Separator => None,
            })
            .collect();
        c.apply(Action::PlaceChosen(side, None));
        labels
    }

    #[test]
    fn up_leaves_home_and_places_pin_and_go_back() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("sparc")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        settle(&mut c);
        c.apply(Action::LocalOpen("sparc".into()));
        c.apply(Action::LocalUp);
        c.apply(Action::LocalUp);
        settle(&mut c);
        // No fence at the home folder any more.
        assert_eq!(
            c.local_path(),
            home.path().parent().unwrap().display().to_string()
        );
        assert!(matches!(
            c.apply(Action::GoTo(Side::Local))[0],
            Effect::AskPath(_)
        ));
        c.apply(Action::LocalGoToAnswered(Some(elsewhere.path().into())));
        settle(&mut c);
        assert_eq!(c.local_path(), elsewhere.path().display().to_string());
        assert!(choose_place(&mut c, Side::Local, "Pin this folder").is_empty());
        assert_eq!(
            c.take_place_events(),
            [
                PlaceEvent::Visited(elsewhere.path().into()),
                PlaceEvent::Pinned(elsewhere.path().into(), true)
            ]
        );
        c.apply(Action::LocalHome);
        settle(&mut c);
        let pinned = elsewhere.path().display().to_string();
        let labels = menu_labels(&mut c, Side::Local);
        assert!(labels.contains(&"Pinned".to_string()) && labels.contains(&pinned));
        // A pinned place is not listed again under Recent.
        assert!(!labels.contains(&"Recent".to_string()), "{labels:?}");
        choose_place(&mut c, Side::Local, &pinned);
        settle(&mut c);
        assert_eq!(c.local_path(), pinned);
        assert!(menu_labels(&mut c, Side::Local).contains(&"Unpin this folder".to_string()));
        // A dismissed menu does nothing; an answer for a menu no longer open is ignored.
        c.apply(Action::Places(Side::Local));
        assert!(c
            .apply(Action::PlaceChosen(Side::Remote, Some(0)))
            .is_empty());
        assert!(c
            .apply(Action::PlaceChosen(Side::Local, Some(0)))
            .is_empty());
    }

    #[test]
    fn remote_places_need_a_connection_and_are_kept_per_machine() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        let effects = c.apply(Action::Places(Side::Remote));
        let [Effect::Menu(m)] = &effects[..] else {
            panic!("no menu");
        };
        assert!(m
            .items
            .iter()
            .all(|it| !matches!(it, MenuItem::Item { enabled: true, .. })));
        c.apply(Action::Connect);
        settle(&mut c);
        c.apply(Action::RemoteOpen("src".into()));
        settle(&mut c);
        choose_place(&mut c, Side::Remote, "Pin this folder");
        assert_eq!(
            c.take_remote_places(),
            Some((vec!["/export/home/dani/src".to_string()], Vec::new()))
        );
        choose_place(&mut c, Side::Remote, "Top level: /");
        settle(&mut c);
        assert_eq!(c.remote_path(), "/");
        choose_place(&mut c, Side::Remote, "/export/home/dani/src");
        settle(&mut c);
        assert_eq!(c.remote_path(), "/export/home/dani/src");
        // Go to takes a path relative to the current folder, or an absolute one.
        c.apply(Action::RemoteGoToAnswered(Some("..".into())));
        settle(&mut c);
        assert_eq!(c.remote_path(), "/export/home/dani");
        c.apply(Action::RemoteGoToAnswered(Some("/tmp".into())));
        assert_eq!(c.remote_path(), "/tmp");
    }

    #[test]
    fn a_machine_opens_in_its_own_folders_or_falls_back() {
        let home = tempfile::tempdir().unwrap();
        let mut m = Machine::new("sparc", "demo");
        m.services.push(Service {
            kind: ServiceKind::Demo,
            port: 0,
        });
        m.remote_folder = "src".into();
        m.home_folder = Some(PathBuf::from("/no/such/folder/on/this/computer"));
        let mut c = ConnectionController::new(
            m,
            home.path(),
            Vec::new(),
            Vec::new(),
            Sessions::new(),
            Arc::new(NoNotify),
        );
        // The local folder is not here: the app's home folder instead.
        assert_eq!(c.local_path(), home.path().display().to_string());
        c.apply(Action::Connect);
        settle(&mut c);
        // Relative to the login folder, as the machine spells it.
        assert_eq!(c.remote_path(), "/export/home/dani/src");
    }

    #[test]
    fn connect_browse_and_disconnect_the_demo_machine() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        assert!(c.apply(Action::Connect).is_empty());
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Connected);
        assert_eq!(c.remote_path(), "/export/home/dani");
        c.apply(Action::RemoteOpen("src".into()));
        settle(&mut c);
        let names: Vec<_> = c.remote_rows().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["gcc-2.95.3", "bash-2.05b.tar.gz", "Makefile.local"]);
        assert_eq!(c.remote_path(), "/export/home/dani/src");
        assert_eq!(c.remote_rows()[2].mode, "-rw-r--r--");
        c.apply(Action::Disconnect);
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Disconnected);
        assert!(c.remote_rows().is_empty());
    }

    #[test]
    fn rows_dragged_out_download_into_the_folder_they_land_in() {
        let home = tempfile::tempdir().unwrap();
        let away = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        let to = away.path().to_path_buf();
        assert!(c
            .apply(Action::CopyOut(vec!["src".into(), ".profile".into()], to))
            .is_empty());
        settle(&mut c);
        assert!(away.path().join(".profile").is_file());
        assert!(away.path().join("src/Makefile.local").is_file());
        assert!(
            !home.path().join(".profile").exists(),
            "not into the local pane"
        );
        // The local pane's own copy still lands in the local folder.
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        settle(&mut c);
        assert!(home.path().join(".profile").is_file());
    }

    #[test]
    fn copy_both_ways_with_the_conflict_question_and_file_ops() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        // Download a folder and a file: one queue entry per file, each checked.
        assert!(c
            .apply(Action::Copy(
                Side::Remote,
                vec!["src".into(), ".profile".into()]
            ))
            .is_empty());
        settle(&mut c);
        assert!(home.path().join("src/Makefile.local").is_file());
        let done = |c: &ConnectionController, what: &str| {
            c.queue_rows()
                .iter()
                .filter(|r| r.status.starts_with(what))
                .count()
        };
        assert_eq!(
            done(&c, "Done, CRC-32 matches"),
            3,
            "{}",
            c.transfers_text()
        );
        // Again: it is there now, so the batch asks; Skip them leaves it.
        c.apply(Action::Queue(QueueOp::ClearFinished, Vec::new()));
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        let s = settle_ask(&mut c, "conn.queue.exists");
        let skip = s.choices.iter().position(|b| b == "Skip").unwrap();
        c.apply(Action::QueueExistsAnswered(skip));
        settle(&mut c);
        assert_eq!(done(&c, "Skipped"), 1, "{}", c.transfers_text());
        // Ask for each: the queue stops at each file that is there; Skip all answers the rest.
        c.apply(Action::Copy(Side::Remote, vec!["src".into()]));
        let s = settle_ask(&mut c, "conn.queue.exists");
        let each = s.choices.iter().position(|b| b == "Ask for each").unwrap();
        c.apply(Action::QueueExistsAnswered(each));
        let s = settle_ask(&mut c, "conn.queue.item");
        assert!(
            s.message.contains("already on this computer"),
            "{}",
            s.message
        );
        let all = s.choices.iter().position(|b| b == "Skip all").unwrap();
        c.apply(Action::QueueItemAnswered(all));
        settle(&mut c);
        assert_eq!(done(&c, "Skipped"), 3, "{}", c.transfers_text());
        // Over the limit, the batch asks before it is added; Cancel adds nothing.
        c.apply(Action::Queue(QueueOp::ClearFinished, Vec::new()));
        c.set_queue_prefs(QueuePrefs {
            ask_over: 1,
            checksum: false,
            ..Default::default()
        });
        c.apply(Action::Copy(Side::Remote, vec!["src".into()]));
        let s = settle_ask(&mut c, "conn.queue.many");
        assert!(s.message.starts_with("Adding 2 files"), "{}", s.message);
        c.apply(Action::QueueManyAnswered(1));
        settle(&mut c);
        assert!(c.queue_rows().is_empty(), "{}", c.transfers_text());
        // Add to queue fills it without starting; Start runs it.
        std::fs::write(home.path().join("later.txt"), b"later").unwrap();
        c.apply(Action::Refresh(Side::Local));
        settle(&mut c);
        c.apply(Action::AddToQueue(Side::Local, vec!["later.txt".into()]));
        settle(&mut c);
        assert_eq!(done(&c, "Waiting"), 1, "{}", c.transfers_text());
        assert!(c.transfers_text().contains("the queue is paused"));
        let ids: Vec<u64> = c.queue_rows().iter().map(|r| r.id).collect();
        let buttons = c.queue_buttons(&ids);
        assert!(buttons
            .iter()
            .any(|b| b.op == QueueOp::Start && b.enabled && b.id == ids::QUEUE_RUN));
        assert!(!buttons.iter().any(|b| b.op == QueueOp::Resume && b.enabled));
        c.apply(Action::Queue(QueueOp::Start, Vec::new()));
        settle(&mut c);
        assert_eq!(done(&c, "Done, size matches"), 1, "{}", c.transfers_text());
        assert!(c.remote_rows().iter().any(|r| r.name == "later.txt"));
        c.apply(Action::Queue(QueueOp::ClearFinished, Vec::new()));
        // Upload a new local file; it shows in the remote listing.
        std::fs::write(home.path().join("hello.txt"), b"hi there").unwrap();
        c.apply(Action::Refresh(Side::Local));
        settle(&mut c);
        c.apply(Action::Copy(Side::Local, vec!["hello.txt".into()]));
        settle(&mut c);
        assert!(c.remote_rows().iter().any(|r| r.name == "hello.txt"));
        // New folder, rename and delete on the machine.
        assert!(matches!(
            c.apply(Action::NewFolder(Side::Remote))[0],
            Effect::AskText(_)
        ));
        c.apply(Action::NewFolderAnswered(Some("docs".into())));
        settle(&mut c);
        assert!(c.remote_rows().iter().any(|r| r.name == "docs" && r.is_dir));
        c.apply(Action::Rename(Side::Remote, "hello.txt".into()));
        c.apply(Action::RenameAnswered(Some("hi.txt".into())));
        settle(&mut c);
        assert!(c.remote_rows().iter().any(|r| r.name == "hi.txt"));
        assert!(matches!(
            c.apply(Action::Delete(
                Side::Remote,
                vec!["hi.txt".into(), "docs".into()]
            ))[0],
            Effect::Confirm(_)
        ));
        c.apply(Action::DeleteConfirmed(true));
        settle(&mut c);
        assert!(!c
            .remote_rows()
            .iter()
            .any(|r| r.name == "hi.txt" || r.name == "docs"));
        // Local delete of a folder with its files.
        c.apply(Action::Delete(Side::Local, vec!["src".into()]));
        c.apply(Action::DeleteConfirmed(true));
        settle(&mut c);
        assert!(!home.path().join("src").exists());
        // The demo machine has no console; the answer says so.
        c.apply(Action::ConsoleSend("STAT".into()));
        settle(&mut c);
        assert!(c.console.last().unwrap().contains("no command console"));
        assert_eq!(valid_name("a/b"), Err("A name cannot contain '/'.".into()));
        assert_eq!(valid_unix_name("What?"), Ok(()));
        assert!(valid_unix_name("a/b").is_err());
        assert_eq!(valid_mac_name("Q1/Q2 notes?"), Ok(()));
        assert!(valid_mac_name("a:b").is_err());
        assert!(valid_mac_name(&"x".repeat(32)).unwrap_err().contains("31"));
    }

    #[test]
    fn files_dropped_from_the_os_upload_or_land_in_the_local_folder() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("readme.1st"), b"read me").unwrap();
        std::fs::create_dir(outside.path().join("disks")).unwrap();
        std::fs::write(outside.path().join("disks/boot.img"), vec![0u8; 1440]).unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        // Onto the machine: uploaded, folder and all.
        let dropped = vec![
            outside.path().join("readme.1st"),
            outside.path().join("disks"),
        ];
        assert!(c
            .apply(Action::Drop(Side::Remote, dropped.clone()))
            .is_empty());
        settle(&mut c);
        let names: Vec<String> = c.remote_rows().into_iter().map(|r| r.name).collect();
        assert!(
            names.contains(&"readme.1st".to_string()) && names.contains(&"disks".to_string()),
            "{names:?}"
        );
        // Onto the local pane: copied into the home folder.
        assert!(c
            .apply(Action::Drop(Side::Local, dropped.clone()))
            .is_empty());
        settle(&mut c);
        assert_eq!(
            std::fs::read(home.path().join("disks/boot.img"))
                .unwrap()
                .len(),
            1440
        );
        // A second drop asks first; Skip them copies nothing.
        let e = c.apply(Action::Drop(Side::Local, dropped));
        let Effect::Choose(s) = &e[0] else {
            panic!("{e:?}")
        };
        assert_eq!(s.purpose, "conn.copy.conflict");
        c.apply(Action::CopyConflict(
            s.choices.iter().position(|b| b == "Skip them").unwrap(),
        ));
        settle(&mut c);
        assert!(
            c.transfers_text().contains("0 file(s)"),
            "{}",
            c.transfers_text()
        );
    }

    /// Tick until the queue asks for a name; the prompt.
    fn settle_name(c: &mut ConnectionController) -> super::super::TextSpec {
        for _ in 0..400 {
            for e in c.tick() {
                match e {
                    Effect::AskText(t) => return t,
                    Effect::Choose(s) => panic!("asked {}: {}", s.purpose, s.message),
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no name asked for");
    }

    #[test]
    fn files_already_there_are_renamed_compared_or_left() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        let profile = home.path().join(".profile");
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        settle(&mut c);
        let original = std::fs::read(&profile).unwrap();
        let pick = |s: &super::super::ChoiceSpec, b: &str| {
            s.choices.iter().position(|c| c == b).unwrap_or_else(|| {
                panic!("no {b} in {:?}", s.choices);
            })
        };
        let status = |c: &ConnectionController| {
            let rows = c.queue_rows();
            let r = rows.iter().rev().find(|r| r.direction.starts_with("To "));
            r.unwrap().status.clone()
        };
        // The same file: Replace if different leaves it, by the machine's checksum.
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        let s = settle_ask(&mut c, "conn.queue.exists");
        assert_eq!(
            s.choices,
            [
                "Overwrite",
                "Replace if different",
                "Rename...",
                "Skip",
                "Cancel"
            ]
        );
        c.apply(Action::QueueExistsAnswered(pick(
            &s,
            "Replace if different",
        )));
        settle(&mut c);
        assert_eq!(status(&c), "Same as there, not copied (CRC-32 matches)");
        // Same size, other bytes: the checksum differs, so it is copied.
        let mut changed = original.clone();
        changed[0] ^= 1;
        std::fs::write(&profile, &changed).unwrap();
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        let s = settle_ask(&mut c, "conn.queue.exists");
        c.apply(Action::QueueExistsAnswered(pick(
            &s,
            "Replace if different",
        )));
        settle(&mut c);
        assert!(status(&c).starts_with("Done"), "{}", c.transfers_text());
        assert_eq!(std::fs::read(&profile).unwrap(), original);
        // Rename...: a name to edit, offered free; one that is there is asked again.
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        let s = settle_ask(&mut c, "conn.queue.exists");
        let e = c.apply(Action::QueueExistsAnswered(pick(&s, "Rename...")));
        let t = match &e[..] {
            [Effect::AskText(t)] => t.clone(),
            _ => settle_name(&mut c),
        };
        assert_eq!(
            (t.purpose, t.initial.as_str()),
            (RENAME_PURPOSE, ".profile (2)")
        );
        assert!(t.validate("a/b").is_err());
        let again = c.apply(Action::CopyRenameAnswered(Some(".profile".into())));
        assert!(
            matches!(&again[..], [Effect::AskText(t)] if t.prompt.starts_with(".profile is also")),
            "{again:?}"
        );
        c.apply(Action::CopyRenameAnswered(Some("profile.old".into())));
        settle(&mut c);
        assert_eq!(
            std::fs::read(home.path().join("profile.old")).unwrap(),
            original
        );
        // Files dropped on the local pane get the same choices; Rename... names the copy.
        let outside = tempfile::tempdir().unwrap();
        let a = outside.path().join("a.txt");
        std::fs::write(&a, b"one").unwrap();
        std::fs::write(home.path().join("a.txt"), b"two").unwrap();
        let e = c.apply(Action::Drop(Side::Local, vec![a.clone()]));
        let Effect::Choose(s) = &e[0] else {
            panic!("{e:?}")
        };
        let e = c.apply(Action::CopyConflict(pick(s, "Rename...")));
        let [Effect::AskText(t)] = &e[..] else {
            panic!("{e:?}")
        };
        assert_eq!(t.initial, "a (2).txt");
        c.apply(Action::CopyRenameAnswered(Some(t.initial.clone())));
        settle(&mut c);
        assert_eq!(
            std::fs::read(home.path().join("a (2).txt")).unwrap(),
            b"one"
        );
        assert_eq!(std::fs::read(home.path().join("a.txt")).unwrap(), b"two");
        // The Options default answers without asking.
        c.set_queue_prefs(QueuePrefs {
            on_existing: OnExisting::Skip,
            ..Default::default()
        });
        c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        settle(&mut c);
        assert_eq!(status(&c), "Skipped, already there");
        assert!(c.apply(Action::Drop(Side::Local, vec![a])).is_empty());
        settle(&mut c);
        assert_eq!(std::fs::read(home.path().join("a.txt")).unwrap(), b"two");
    }

    #[test]
    fn keys_send_vt220_sequences() {
        assert_eq!(key_bytes(TermKey::Up, false), b"\x1b[A");
        assert_eq!(key_bytes(TermKey::Up, true), b"\x1bOA");
        assert_eq!(key_bytes(TermKey::F(1), false), b"\x1bOP");
        assert_eq!(key_bytes(TermKey::F(5), false), b"\x1b[15~");
        assert_eq!(key_bytes(TermKey::Ctrl('c'), false), [3]);
        assert_eq!(key_bytes(TermKey::Enter, false), b"\r");
    }

    #[test]
    fn a_telnet_shell_logs_in_and_logs_out() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            s.write_all(b"SunOS 5.8\r\n\r\nlogin: ").unwrap();
            let mut heard = Vec::new();
            let wait_for = |s: &mut std::net::TcpStream, want: &[u8], heard: &mut Vec<u8>| {
                let mut buf = [0u8; 256];
                while !heard.windows(want.len()).any(|w| w == want) {
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "client hung up early");
                    heard.extend_from_slice(&buf[..n]);
                }
            };
            wait_for(&mut s, b"dani\r\n", &mut heard);
            s.write_all(b"dani\r\n$ ").unwrap();
            wait_for(&mut s, b"exit\r\n", &mut heard);
        });
        let home = tempfile::tempdir().unwrap();
        let mut m = Machine::new("sparc", "127.0.0.1");
        m.services.push(Service {
            kind: ServiceKind::Telnet,
            port,
        });
        m.logout_command = "exit".into();
        let mut c = ConnectionController::new(
            m,
            home.path(),
            Vec::new(),
            Vec::new(),
            Sessions::new(),
            Arc::new(NoNotify),
        );
        assert!(c.can_shell_connect());
        c.apply(Action::ShellConnect);
        let text = |c: &ConnectionController| {
            c.shell_screen()
                .map(|s| {
                    s.lines
                        .iter()
                        .flatten()
                        .map(|r| r.text.clone())
                        .collect::<String>()
                })
                .unwrap_or_default()
        };
        let wait = |c: &mut ConnectionController, want: &str| {
            for _ in 0..400 {
                c.tick();
                if text(c).contains(want) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("never saw {want:?} in {:?}", text(c));
        };
        wait(&mut c, "login:");
        assert_eq!(c.shell_state(), ConnState::Connected);
        assert_eq!(c.tab_state(), ConnState::Connected);
        c.apply(Action::ShellText("dani".into()));
        c.apply(Action::ShellKey(TermKey::Enter));
        wait(&mut c, "$");
        c.apply(Action::ShellDisconnect);
        server.join().unwrap();
        for _ in 0..400 {
            c.tick();
            if c.shell_state() == ConnState::Disconnected {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.shell_state(), ConnState::Disconnected);
    }

    #[test]
    fn dates_and_sizes() {
        assert_eq!(date_text(0), "1970-01-01");
        assert_eq!(date_text(951_782_400), "2000-02-29");
        assert_eq!(size_text(812 * 1024), "812 KB");
    }

    /// A connected demo tab whose server the test controls (it can drop the link).
    fn droppable(home: &Path) -> (ConnectionController, lc_core::memory::MemoryFs) {
        let mut c = tab(home);
        c.apply(Action::Connect);
        settle(&mut c);
        let fs = crate::model::session::demo_machine();
        c.sessions
            .adopt("sparc", Box::new(fs.clone()), Arc::new(NoNotify));
        (c, fs)
    }

    fn drop_link(fs: &lc_core::memory::MemoryFs) {
        fs.dropped.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn settle_out(c: &mut ConnectionController) -> Vec<Effect> {
        let mut out = Vec::new();
        for _ in 0..400 {
            out.extend(c.tick());
            if c.connect_job.is_none() && c.list_job.is_none() && c.ops.is_empty() && !c.copying() {
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("jobs did not finish");
    }

    #[test]
    fn a_dropped_link_connects_again_and_carries_on() {
        let home = tempfile::tempdir().unwrap();
        let (mut c, fs) = droppable(home.path());
        assert_eq!(c.remote_path(), "/export/home/dani");
        drop_link(&fs);
        let out = c.apply(Action::RemoteOpen("src".into()));
        assert!(out.is_empty());
        let out = settle_out(&mut c);
        assert!(
            !out.iter()
                .any(|e| matches!(e, Effect::Message(_) | Effect::Choose(_))),
            "{out:?}"
        );
        assert_eq!(c.state(), ConnState::Connected);
        assert_eq!(c.remote_path(), "/export/home/dani/src");
        assert!(c.remote_rows.iter().any(|e| e.name == "Makefile.local"));
        assert_eq!(c.remote_error(), None);
        // The Console says when it went and when it came back.
        assert!(
            c.console[0].starts_with("-- ") && c.console[0].contains("lost"),
            "{:?}",
            c.console
        );
        assert!(
            c.console[1].contains("Connected again to sparc"),
            "{:?}",
            c.console
        );
    }

    #[test]
    fn an_idle_machine_asks_or_waits_as_set() {
        let home = tempfile::tempdir().unwrap();
        let (mut c, _fs) = droppable(home.path());
        c.apply(Action::RemoteOpen("src".into()));
        settle(&mut c);
        c.sessions.suspend("sparc", Arc::new(NoNotify)).wait();
        c.tick();
        assert_eq!(c.state(), ConnState::Idle);
        assert!(c.remote_error().unwrap().contains("slept"));
        c.machine.reconnect = Some(crate::model::settings::Auto::Never);
        let out = c.apply(Action::Refresh(Side::Remote));
        assert!(matches!(&out[..], [Effect::Message(m)] if m.contains("Reconnect")));
        assert_eq!(c.state(), ConnState::Idle);
        c.machine.reconnect = Some(crate::model::settings::Auto::Ask);
        let out = c.apply(Action::RemoteUp);
        assert!(matches!(&out[..], [Effect::Choose(s)] if s.purpose == REJOIN_PURPOSE));
        assert!(c.apply(Action::RejoinAnswered(0)).is_empty());
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Connected);
        assert_eq!(c.remote_path(), "/export/home/dani", "went up from src");
    }

    #[test]
    fn the_header_connect_on_a_lost_machine_goes_back_to_its_folder() {
        let home = tempfile::tempdir().unwrap();
        let (mut c, fs) = droppable(home.path());
        c.apply(Action::RemoteOpen("bin".into()));
        settle(&mut c);
        // A keepalive found the link gone: nothing the user did failed, so nothing reconnects.
        drop_link(&fs);
        c.sessions.age("sparc", crate::model::session::CHECK_AFTER);
        c.apply(Action::Queue(QueueOp::ClearFinished, Vec::new()));
        c.sessions.suspend("sparc", Arc::new(NoNotify)).wait();
        c.tick();
        assert!(c.can_connect() || c.state() == ConnState::Idle);
        c.apply(Action::Connect);
        settle(&mut c);
        assert_eq!(c.remote_path(), "/export/home/dani/bin");
        // Disconnect from Idle empties the pane; using it no longer connects.
        c.sessions.suspend("sparc", Arc::new(NoNotify)).wait();
        c.tick();
        c.apply(Action::Disconnect);
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Disconnected);
        assert!(c.apply(Action::Refresh(Side::Remote)).is_empty());
        assert_eq!(c.state(), ConnState::Disconnected);
    }

    #[test]
    fn a_queue_the_sleep_stopped_asks_to_go_on_after_waking() {
        let home = tempfile::tempdir().unwrap();
        let (mut c, _fs) = droppable(home.path());
        c.apply(Action::AddToQueue(Side::Remote, vec![".profile".into()]));
        settle(&mut c);
        assert_eq!(c.xfer.queue.unfinished(), 1);
        c.xfer.queue.running = true;
        for j in c.begin_suspend() {
            j.wait();
        }
        c.tick();
        assert_eq!(c.state(), ConnState::Idle);
        let out = c.woke();
        assert!(out.is_empty(), "{out:?}");
        let s = settle_ask(&mut c, RESUME_PURPOSE);
        assert!(s.message.contains("1 file(s)"));
        c.apply(Action::ResumeQueueAnswered(0));
        settle(&mut c);
        assert_eq!(c.xfer.queue.unfinished(), 0);
        assert!(home.path().join(".profile").exists());
    }

    #[test]
    fn a_link_lost_under_the_queue_reconnects_and_asks_to_go_on() {
        let home = tempfile::tempdir().unwrap();
        let (mut c, fs) = droppable(home.path());
        c.apply(Action::AddToQueue(Side::Remote, vec![".profile".into()]));
        settle(&mut c);
        drop_link(&fs);
        c.apply(Action::Queue(QueueOp::Start, Vec::new()));
        let s = settle_ask(&mut c, RESUME_PURPOSE);
        assert!(s.message.contains("1 file(s)"), "{}", s.message);
        assert!(c.can_disconnect(), "connected again");
        c.apply(Action::ResumeQueueAnswered(0));
        settle(&mut c);
        assert!(home.path().join(".profile").exists());
        assert!(c.link.cut.is_empty());
    }

    #[test]
    fn console_commands_are_not_replayed_and_a_double_wake_asks_once() {
        let home = tempfile::tempdir().unwrap();
        let (mut c, _fs) = droppable(home.path());
        c.apply(Action::ConsoleSend("SITE CHMOD 644 x".into()));
        assert!(
            c.link.last.is_none(),
            "a console command is never sent twice"
        );
        settle(&mut c);
        c.apply(Action::AddToQueue(Side::Remote, vec![".profile".into()]));
        settle(&mut c);
        c.xfer.queue.running = true;
        for j in c.begin_suspend() {
            j.wait();
        }
        c.tick();
        c.machine.reconnect = Some(crate::model::settings::Auto::Ask);
        let first = c.woke();
        assert!(matches!(&first[..], [Effect::Choose(s)] if s.purpose == REJOIN_PURPOSE));
        assert!(c.woke().is_empty(), "the second wake does not ask again");
    }

    #[test]
    fn a_shell_the_machine_closed_says_when_and_opens_again_with_a_note() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // The first shell is hung up on (an idle timeout); the second stays.
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"$ ").unwrap();
            drop(s);
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"$ ").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        let home = tempfile::tempdir().unwrap();
        let mut m = Machine::new("sparc", "127.0.0.1");
        m.services.push(Service {
            kind: ServiceKind::Telnet,
            port,
        });
        let mut c = ConnectionController::new(
            m,
            home.path(),
            Vec::new(),
            Vec::new(),
            Sessions::new(),
            Arc::new(NoNotify),
        );
        c.apply(Action::ShellConnect);
        for _ in 0..400 {
            c.tick();
            if c.shell.as_ref().is_some_and(|s| s.ended().is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let msg = c.shell_message();
        // A clean close or a reset, depending on the timing: either way, with the time.
        assert!(msg.starts_with("-- ") && msg.as_bytes()[5] == b':', "{msg}");
        // Typing into it opens a new shell, which starts with the time it came back.
        c.apply(Action::ShellText("x".into()));
        let screen = |c: &ConnectionController| {
            c.shell_screen()
                .map(|s| {
                    s.lines
                        .iter()
                        .flatten()
                        .map(|r| r.text.clone())
                        .collect::<String>()
                })
                .unwrap_or_default()
        };
        for _ in 0..400 {
            c.tick();
            if screen(&c).contains("Connected again.") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(screen(&c).contains("Connected again."), "{:?}", screen(&c));
        server.join().unwrap();
    }
}
