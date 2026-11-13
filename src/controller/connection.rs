//! One connection tab: files on top (this computer | the machine), the terminal area below,
//! a splitter between them and Swap (docs/PLAN.md §1.1, mockups/connection-view.html).
//!
//! The local pane starts in the home folder; Leave home... is the only way out of it (a typed
//! path, Browse..., or a recent place) and Home is the way back. The remote pane connects only
//! when the user says so and logs off cleanly on Disconnect.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lc_core::{Entry, RemotePath};

use super::{
    ids, job_outcome, ConnState, Controller, Effect, Layout, PasswordSpec, PathSpec, ServiceKind,
    TabColor,
};
use crate::model::job::{Job, Notify};
use crate::model::term::TermSession;
use crate::model::transfer::{self, Direction, Pick, Summary};

use crate::model::local::{list_dir, LocalEntry};
use crate::model::machines::{Machine, Password};
use crate::model::session::Sessions;
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
    ToggleTree(Side),
    SelectTerm(TermTab),
    LocalHome,
    LocalUp,
    LocalOpen(String),
    LeaveHome,
    /// The Leave home prompt's answer; `None` when cancelled.
    LeaveHomeAnswered(Option<PathBuf>),
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
    /// The answer to "these already exist": Overwrite, Skip existing, Cancel.
    CopyConflict(usize),
    CancelTransfers,
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

/// One copy batch as the Transfers tab lists it.
struct Batch {
    label: String,
    job: Job<Summary>,
    /// The finished line, once it ended.
    result: Option<String>,
    to: Side,
}

/// Picks that share one local folder (a pane's, or the folder files were dropped from).
struct Group {
    local_dir: PathBuf,
    picks: Vec<Pick>,
    sizes: Vec<Option<u64>>,
}

/// What a pending question is about.
enum Pending {
    Copy(Direction, Vec<Group>),
    /// Files dropped from the OS onto the local pane, to copy into its folder.
    Import(Vec<PathBuf>),
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
    left_home_to: Option<PathBuf>,
    remote_home: Option<RemotePath>,
    remote_dir: Option<RemotePath>,
    remote_rows: Vec<Entry>,
    remote_error: Option<String>,
    connect_job: Option<Job<RemotePath>>,
    list_job: Option<Job<Vec<Entry>>>,
    disconnect_job: Option<Job<()>>,
    session_password: Option<String>,
    /// The password being asked for is for the shell, not the file connection.
    asking_for_shell: bool,
    shell: Option<TermSession>,
    shell_open: Option<Job<TermSession>>,
    shell_close: Option<Job<()>>,
    shell_error: Option<String>,
    shell_size: (u16, u16),
    batches: Vec<Batch>,
    pending: Option<Pending>,
    ops: Vec<(Side, Job<()>)>,
    console: Vec<String>,
    console_job: Option<Job<String>>,
}

impl ConnectionController {
    /// A tab for `machine`, its local pane in `home` (the machine's own home wins).
    pub(crate) fn new(
        machine: Machine,
        app_home: &Path,
        recent: Vec<PathBuf>,
        sessions: Sessions,
        notify: Arc<dyn Notify>,
    ) -> Self {
        let home = machine
            .home_folder
            .clone()
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
            left_home_to: None,
            remote_home: None,
            remote_dir: None,
            remote_rows: Vec::new(),
            remote_error: None,
            connect_job: None,
            list_job: None,
            disconnect_job: None,
            session_password: None,
            asking_for_shell: false,
            shell: None,
            shell_open: None,
            shell_close: None,
            shell_error: None,
            shell_size: (80, 24),
            batches: Vec::new(),
            pending: None,
            ops: Vec::new(),
            console: Vec::new(),
            console_job: None,
        };
        c.list_local();
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

    /// A place the user left home to, once, for the app's recent list.
    pub(crate) fn take_left_home(&mut self) -> Option<PathBuf> {
        self.left_home_to.take()
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

    /// The tab's state light: connected if either service is.
    pub fn tab_state(&self) -> ConnState {
        let (f, c) = (self.state(), self.shell_state());
        for s in [
            ConnState::Connected,
            ConnState::Busy,
            ConnState::Connecting,
            ConnState::Disconnecting,
            ConnState::Lost,
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
        let plain = |k: Option<ServiceKind>| {
            matches!(
                k,
                Some(
                    ServiceKind::Ftp | ServiceKind::Telnet | ServiceKind::Rexec | ServiceKind::Smb
                )
            )
        };
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
        // Telnet logs in on the screen; the others need the password up front.
        let needs_password =
            self.machine.command_service().map(|s| s.kind) != Some(ServiceKind::Telnet);
        let password = match (&self.machine.password, &self.session_password) {
            (Password::Saved(p), _) | (Password::Ask, Some(p)) => Some(p.clone()),
            (Password::Ask, None) if needs_password => {
                self.asking_for_shell = true;
                return vec![self.ask_password()];
            }
            (Password::Ask, None) => None,
        };
        self.open_shell(password);
        Vec::new()
    }

    fn open_shell(&mut self, password: Option<String>) {
        self.shell_error = None;
        let (cols, rows) = self.shell_size;
        self.shell_open = Some(TermSession::open(
            &self.machine,
            password,
            cols,
            rows,
            Arc::clone(&self.notify),
        ));
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
        matches!(
            self.machine.file_service().map(|s| s.kind),
            Some(ServiceKind::Ftp | ServiceKind::Telnet | ServiceKind::Rexec | ServiceKind::Smb)
        )
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

    /// The local pane is outside the home folder.
    pub fn away_from_home(&self) -> bool {
        !self.local_dir.starts_with(&self.home)
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

    /// The folders from the home (or the place left to) down to the current one, for the tree.
    pub fn local_trail(&self) -> Vec<String> {
        let root = if self.away_from_home() {
            self.local_dir
                .ancestors()
                .last()
                .unwrap_or(&self.local_dir)
                .to_path_buf()
        } else {
            self.home.clone()
        };
        let mut trail = vec![root.display().to_string()];
        if let Ok(rest) = self.local_dir.strip_prefix(&root) {
            trail.extend(rest.iter().map(|p| p.to_string_lossy().into_owned()));
        }
        trail
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

    /// The remote folders from the login folder down, for the tree.
    pub fn remote_trail(&self) -> Vec<String> {
        let (Some(home), Some(dir)) = (&self.remote_home, &self.remote_dir) else {
            return Vec::new();
        };
        let root = if dir.starts_with(home) {
            home.clone()
        } else {
            RemotePath::root(dir.style)
        };
        let mut trail = vec![root.to_string()];
        trail.extend(dir.parts()[root.parts().len()..].iter().cloned());
        trail
    }

    pub fn remote_error(&self) -> Option<&str> {
        self.remote_error.as_deref()
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

    /// The Transfers tab: every batch of this session, newest first.
    pub fn transfers_text(&self) -> String {
        if self.batches.is_empty() {
            return "No transfers yet. Select files and press F6, or right-click and choose Copy."
                .into();
        }
        self.batches
            .iter()
            .rev()
            .map(|b| match &b.result {
                Some(r) => format!("{}\n    {r}", b.label),
                None => {
                    let s = b.job.snapshot();
                    format!("{}\n    {}   {}", b.label, s.phase, s.detail())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether a copy is running (its Cancel shows).
    pub fn copying(&self) -> bool {
        self.batches.iter().any(|b| b.result.is_none())
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

    fn start_copy(&mut self, dir: Direction, g: Group, overwrite: bool) {
        let Group {
            local_dir,
            picks,
            sizes,
        } = g;
        let Some(remote_dir) = self.remote_dir.clone() else {
            return;
        };
        let what = if picks.len() == 1 {
            picks[0].name.clone()
        } else {
            format!("{} items", picks.len())
        };
        let label = match dir {
            Direction::Download => format!(
                "Download {what} from {} to {}",
                self.machine.name,
                local_dir.display()
            ),
            Direction::Upload => format!("Upload {what} to {}:{}", self.machine.name, remote_dir),
        };
        let job = self.sessions.run(
            &self.machine.id,
            label.clone(),
            Arc::clone(&self.notify),
            move |fs, ctx| {
                transfer::copy(
                    fs,
                    &transfer::Batch {
                        dir,
                        remote_dir: &remote_dir,
                        local_dir: &local_dir,
                        picks: &picks,
                        sizes: &sizes,
                        overwrite,
                    },
                    ctx,
                )
            },
        );
        let to = match dir {
            Direction::Download => Side::Local,
            Direction::Upload => Side::Remote,
        };
        self.batches.push(Batch {
            label,
            job,
            result: None,
            to,
        });
        if self.shell_state() != ConnState::Connected {
            self.term = TermTab::Transfers;
        }
    }

    fn copy(&mut self, from: Side, names: Vec<String>) -> Vec<Effect> {
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
        let dir = match from {
            Side::Local => Direction::Upload,
            Side::Remote => Direction::Download,
        };
        let clashes: Vec<&str> = picks
            .iter()
            .filter(|p| match dir {
                Direction::Download => self.local_dir.join(&p.name).exists(),
                Direction::Upload => self.remote_rows.iter().any(|e| e.name == p.name),
            })
            .map(|p| p.name.as_str())
            .collect();
        let group = Group {
            local_dir: self.local_dir.clone(),
            picks: picks.clone(),
            sizes,
        };
        if clashes.is_empty() {
            self.start_copy(dir, group, true);
            return Vec::new();
        }
        self.pending = Some(Pending::Copy(dir, vec![group]));
        conflict_question(&clashes)
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
                let clashes: Vec<String> = paths
                    .iter()
                    .map(name)
                    .filter(|n| self.local_dir.join(n).exists())
                    .collect();
                if clashes.is_empty() {
                    self.start_import(paths, true);
                    return Vec::new();
                }
                self.pending = Some(Pending::Import(paths));
                conflict_question(&clashes.iter().map(String::as_str).collect::<Vec<_>>())
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
                let clashes: Vec<String> = paths
                    .iter()
                    .map(name)
                    .filter(|n| self.remote_rows.iter().any(|e| &e.name == n))
                    .collect();
                if clashes.is_empty() {
                    for g in groups {
                        self.start_copy(Direction::Upload, g, true);
                    }
                    return Vec::new();
                }
                self.pending = Some(Pending::Copy(Direction::Upload, groups));
                conflict_question(&clashes.iter().map(String::as_str).collect::<Vec<_>>())
            }
        }
    }

    fn start_import(&mut self, paths: Vec<PathBuf>, overwrite: bool) {
        let dest = self.local_dir.clone();
        let what = if paths.len() == 1 {
            paths[0]
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            format!("{} items", paths.len())
        };
        let label = format!("Copy {what} into {}", dest.display());
        let job = Job::spawn(label.clone(), Arc::clone(&self.notify), move |ctx| {
            crate::model::local::copy_into(&paths, &dest, overwrite, ctx)
        });
        self.batches.push(Batch {
            label,
            job,
            result: None,
            to: Side::Local,
        });
        if self.shell_state() != ConnState::Connected {
            self.term = TermTab::Transfers;
        }
    }
}

/// "These already exist": Overwrite, Skip existing, Cancel.
fn conflict_question(clashes: &[&str]) -> Vec<Effect> {
    let message = if clashes.len() == 1 {
        format!("{} already exists there.", clashes[0])
    } else {
        format!(
            "{} of these already exist there, such as {}.",
            clashes.len(),
            clashes[0]
        )
    };
    vec![Effect::Choose(super::ChoiceSpec {
        purpose: "conn.copy.conflict",
        title: "Copy".into(),
        message,
        choices: vec!["Overwrite".into(), "Skip existing".into(), "Cancel".into()],
        dismiss: 2,
    })]
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

    fn answer_pending(&mut self, answer: Option<String>) -> Vec<Effect> {
        let Some(name) = answer.map(|a| a.trim().to_string()) else {
            self.pending = None;
            return Vec::new();
        };
        if let Err(e) = valid_name(&name) {
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
        match self.state() {
            ConnState::Connected => format!("Connected to {}.", self.machine.name),
            _ => "Not connected.".into(),
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

/// A Leave home answer must be an existing folder.
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

impl Controller for ConnectionController {
    type Action = Action;

    fn apply(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Swap => {
                self.layout.swapped = !self.layout.swapped;
                self.layout_changed = true;
            }
            Action::SetFilesShare(share) => self.set_share(share),
            Action::ToggleTree(side) => {
                let t = match side {
                    Side::Local => &mut self.layout.local_tree,
                    Side::Remote => &mut self.layout.remote_tree,
                };
                *t = !*t;
                self.layout_changed = true;
            }
            Action::SelectTerm(t) => self.term = t,
            Action::LocalHome => self.go_local(self.home.clone()),
            Action::LocalUp => {
                // Up stops at the home folder; leaving it is Leave home's job.
                let parent = self.local_dir.parent().map(Path::to_path_buf);
                match parent {
                    Some(p) if self.away_from_home() || p.starts_with(&self.home) => {
                        self.go_local(p)
                    }
                    _ => {}
                }
            }
            Action::LocalOpen(name) => {
                let target = self.local_dir.join(&name);
                if target.is_dir() {
                    self.go_local(target);
                }
            }
            Action::LeaveHome => {
                return vec![Effect::AskPath(PathSpec {
                    purpose: "local.leave_home",
                    title: "Leave home".into(),
                    prompt: "Go to a folder on this computer:".into(),
                    initial: self
                        .recent
                        .first()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    recent: self.recent.clone(),
                    hint: format!(
                        "Home ({}) stays one click away with the Home button.",
                        self.home.display()
                    ),
                    validator: existing_folder,
                })];
            }
            Action::LeaveHomeAnswered(None) => {}
            Action::LeaveHomeAnswered(Some(path)) => {
                if let Err(e) = existing_folder(&path.to_string_lossy()) {
                    return vec![Effect::Message(e)];
                }
                self.recent.retain(|p| p != &path);
                self.recent.insert(0, path.clone());
                self.left_home_to = Some(path.clone());
                self.go_local(path);
            }
            Action::Connect => {
                if !self.can_connect() {
                    return Vec::new();
                }
                let needs_password =
                    self.machine.file_service().map(|s| s.kind) != Some(ServiceKind::Demo);
                match (&self.machine.password, &self.session_password) {
                    (Password::Saved(p), _) => self.connect(Some(p.clone())),
                    (Password::Ask, Some(p)) => self.connect(Some(p.clone())),
                    (Password::Ask, None) if needs_password => {
                        self.asking_for_shell = false;
                        return vec![self.ask_password()];
                    }
                    (Password::Ask, None) => self.connect(None),
                }
            }
            Action::PasswordAnswered(None) => {}
            Action::PasswordAnswered(Some(p)) => {
                // Kept for this tab only, so a reconnect does not ask again.
                self.session_password = Some(p.clone());
                if std::mem::take(&mut self.asking_for_shell) {
                    self.open_shell(Some(p));
                } else {
                    self.connect(Some(p));
                }
            }
            Action::Disconnect => {
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
            Action::Copy(from, names) => return self.copy(from, names),
            Action::CopyConflict(choice) => match self.pending.take() {
                Some(Pending::Copy(dir, groups)) if choice < 2 => {
                    for g in groups {
                        self.start_copy(dir, g, choice == 0);
                    }
                }
                Some(Pending::Import(paths)) if choice < 2 => self.start_import(paths, choice == 0),
                _ => {}
            },
            Action::Drop(side, paths) => return self.drop_files(side, paths),
            Action::CancelTransfers => {
                for b in self.batches.iter().filter(|b| b.result.is_none()) {
                    b.job.cancel();
                }
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
                    validator: valid_name,
                })];
            }
            Action::NewFolderAnswered(a) | Action::RenameAnswered(a) => {
                return self.answer_pending(a)
            }
            Action::Rename(side, old) => {
                self.pending = Some(Pending::Rename(side, old.clone()));
                return vec![Effect::AskText(super::TextSpec {
                    purpose: "conn.name",
                    title: "Rename".into(),
                    prompt: format!("New name for {old}:"),
                    initial: old,
                    ok_label: "Rename".into(),
                    validator: valid_name,
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
            match r {
                Ok(home) => {
                    self.remote_home = Some(home.clone());
                    self.go_remote(home);
                }
                Err(e) => {
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
                Ok(rows) => self.remote_rows = rows,
                Err(e) => {
                    self.remote_error = Some(format!("Could not list {}: {e}", self.remote_path()))
                }
            }
        }
        let mut refresh = (false, false);
        for b in self.batches.iter_mut().filter(|b| b.result.is_none()) {
            if let Some(r) = job_outcome(&b.job) {
                b.result = Some(match r {
                    Ok(s) => {
                        let mut line = format!("Done: {} file(s), {}", s.files, size_text(s.bytes));
                        if !s.skipped.is_empty() {
                            line += &format!("; skipped {} that already existed", s.skipped.len());
                        }
                        if let Some((name, why)) = s.failed.first() {
                            line += &format!("; {} failed (first: {name}: {why})", s.failed.len());
                        }
                        line
                    }
                    Err(e) => format!("Stopped: {e}"),
                });
                match b.to {
                    Side::Local => refresh.0 = true,
                    Side::Remote => refresh.1 = true,
                }
            }
        }
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
                out.push(Effect::Message(e));
            }
        }
        if let Some(r) = self.console_job.as_ref().and_then(job_outcome) {
            self.console_job = None;
            match r {
                Ok(text) => self.console.extend(text.lines().map(|l| format!("< {l}"))),
                Err(e) => self.console.push(format!("  {e}")),
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
        if let Some(r) = self.shell_open.as_ref().and_then(job_outcome) {
            self.shell_open = None;
            match r {
                Ok(s) => self.shell = Some(s),
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
            self.remote_rows.clear();
            self.remote_dir = None;
            self.remote_home = None;
            if let Err(e) = r {
                out.push(Effect::Message(format!("{}: {e}", self.machine.name)));
            }
        }
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

    fn tab(home: &Path) -> ConnectionController {
        let mut m = Machine::new("sparc", "demo");
        m.services.push(Service {
            kind: ServiceKind::Demo,
            port: 0,
        });
        ConnectionController::new(m, home, Vec::new(), Sessions::new(), Arc::new(NoNotify))
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
    fn swap_split_and_trees_are_remembered_once() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        assert_eq!(c.take_layout_change(), None);
        c.apply(Action::Swap);
        c.apply(Action::SetFilesShare(0.99));
        c.apply(Action::ToggleTree(Side::Remote));
        let l = c.take_layout_change().unwrap();
        assert!(l.swapped && !l.remote_tree && l.local_tree);
        assert!((l.files_share - (1.0 - MIN_SHARE)).abs() < 1e-6);
        assert_eq!(c.take_layout_change(), None);
    }

    #[test]
    fn up_stops_at_home_and_leave_home_goes_anywhere() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("sparc")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        settle(&mut c);
        c.apply(Action::LocalOpen("sparc".into()));
        settle(&mut c);
        assert!(c.local_path().ends_with("sparc"));
        c.apply(Action::LocalUp);
        c.apply(Action::LocalUp);
        settle(&mut c);
        assert_eq!(c.local_path(), home.path().display().to_string());
        assert!(matches!(c.apply(Action::LeaveHome)[0], Effect::AskPath(_)));
        c.apply(Action::LeaveHomeAnswered(Some(
            elsewhere.path().to_path_buf(),
        )));
        settle(&mut c);
        assert!(c.away_from_home());
        assert_eq!(c.take_left_home().as_deref(), Some(elsewhere.path()));
        c.apply(Action::LocalHome);
        settle(&mut c);
        assert!(!c.away_from_home());
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
        assert_eq!(c.remote_trail(), ["/export/home/dani", "src"]);
        assert_eq!(c.remote_rows()[2].mode, "-rw-r--r--");
        c.apply(Action::Disconnect);
        settle(&mut c);
        assert_eq!(c.state(), ConnState::Disconnected);
        assert!(c.remote_rows().is_empty());
    }

    #[test]
    fn copy_both_ways_with_the_conflict_question_and_file_ops() {
        let home = tempfile::tempdir().unwrap();
        let mut c = tab(home.path());
        c.apply(Action::Connect);
        settle(&mut c);
        // Download a folder and a file.
        assert!(c
            .apply(Action::Copy(
                Side::Remote,
                vec!["src".into(), ".profile".into()]
            ))
            .is_empty());
        settle(&mut c);
        assert!(home.path().join("src/Makefile.local").is_file());
        assert!(
            c.transfers_text().contains("Done: 3 file(s)"),
            "{}",
            c.transfers_text()
        );
        // Again: everything exists now, so it asks.
        let e = c.apply(Action::Copy(Side::Remote, vec![".profile".into()]));
        assert!(matches!(&e[0], Effect::Choose(s) if s.purpose == "conn.copy.conflict"));
        c.apply(Action::CopyConflict(1));
        settle(&mut c);
        assert!(
            c.transfers_text().contains("skipped 1"),
            "{}",
            c.transfers_text()
        );
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
        // A second drop asks first; Skip existing copies nothing.
        let e = c.apply(Action::Drop(Side::Local, dropped));
        assert!(matches!(&e[0], Effect::Choose(s) if s.purpose == "conn.copy.conflict"));
        c.apply(Action::CopyConflict(1));
        settle(&mut c);
        assert!(
            c.transfers_text().contains("Done: 0 file(s)"),
            "{}",
            c.transfers_text()
        );
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
}
