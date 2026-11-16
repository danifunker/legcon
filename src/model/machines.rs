//! The saved machines (`machines.json`), with their services, layout and password.
//!
//! Passwords are saved here by default (PLAN.md D28), the way FileZilla keeps them in
//! `sitemanager.xml`: Base64-encoded, which hides them from a glance but is **not encryption**.
//! The field is tagged so other stores fit later without a format change: `b64:...` saved here,
//! `ask` never saved (asked at connect, kept in memory until the tab closes); `os` the OS
//! credential store (a per-machine checkbox, `model::secrets`). No master password (D28).

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};

use super::settings::DEFAULT_FILES_SHARE;

/// A protocol legcon can use on a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Ftp,
    Telnet,
    Smb,
    Nfs,
    Ssh,
    Rexec,
    /// rsh: files over rcp and one command at a time, trusted through `.rhosts`.
    Rsh,
    /// rlogin: a terminal, trusted through `.rhosts` (or a password typed on screen).
    Rlogin,
    /// HTTP/1.0: a web server's index pages, read-only.
    Http,
    /// Gopher menus, read-only.
    Gopher,
    /// AFP over TCP: AppleShare IP, Mac OS 9 File Sharing, netatalk; resource forks kept.
    Afp,
    /// A built-in machine held in memory, for trying the app without a real one.
    Demo,
}

impl ServiceKind {
    pub const ALL: [ServiceKind; 12] = [
        ServiceKind::Ftp,
        ServiceKind::Telnet,
        ServiceKind::Smb,
        ServiceKind::Nfs,
        ServiceKind::Ssh,
        ServiceKind::Rexec,
        ServiceKind::Rsh,
        ServiceKind::Rlogin,
        ServiceKind::Http,
        ServiceKind::Gopher,
        ServiceKind::Afp,
        ServiceKind::Demo,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::Ftp => "FTP",
            ServiceKind::Telnet => "Telnet",
            ServiceKind::Smb => "SMB",
            ServiceKind::Nfs => "NFS",
            ServiceKind::Ssh => "SSH",
            ServiceKind::Rexec => "rexec",
            ServiceKind::Rsh => "rsh",
            ServiceKind::Rlogin => "rlogin",
            ServiceKind::Http => "HTTP",
            ServiceKind::Gopher => "Gopher",
            ServiceKind::Afp => "AFP",
            ServiceKind::Demo => "Demo",
        }
    }

    /// The short name used in the CLI and in job resources.
    pub fn key(self) -> &'static str {
        match self {
            ServiceKind::Ftp => "ftp",
            ServiceKind::Telnet => "telnet",
            ServiceKind::Smb => "smb",
            ServiceKind::Nfs => "nfs",
            ServiceKind::Ssh => "ssh",
            ServiceKind::Rexec => "rexec",
            ServiceKind::Rsh => "rsh",
            ServiceKind::Rlogin => "rlogin",
            ServiceKind::Http => "http",
            ServiceKind::Gopher => "gopher",
            ServiceKind::Afp => "afp",
            ServiceKind::Demo => "demo",
        }
    }

    pub fn from_key(key: &str) -> Option<ServiceKind> {
        ServiceKind::ALL
            .into_iter()
            .find(|k| k.key().eq_ignore_ascii_case(key))
    }

    pub fn default_port(self) -> u16 {
        match self {
            ServiceKind::Ftp => 21,
            ServiceKind::Telnet => 23,
            ServiceKind::Smb => 139,
            ServiceKind::Nfs => 2049,
            ServiceKind::Ssh => 22,
            ServiceKind::Rexec => 512,
            ServiceKind::Rsh => 514,
            ServiceKind::Rlogin => 513,
            ServiceKind::Http => 80,
            ServiceKind::Gopher => 70,
            ServiceKind::Afp => 548,
            ServiceKind::Demo => 0,
        }
    }

    /// Whether it serves files, commands, or both.
    pub fn serves_files(self) -> bool {
        matches!(
            self,
            ServiceKind::Ftp
                | ServiceKind::Smb
                | ServiceKind::Nfs
                | ServiceKind::Ssh
                | ServiceKind::Rsh
                | ServiceKind::Http
                | ServiceKind::Gopher
                | ServiceKind::Afp
                | ServiceKind::Demo
        )
    }

    pub fn serves_commands(self) -> bool {
        matches!(
            self,
            ServiceKind::Telnet
                | ServiceKind::Ssh
                | ServiceKind::Rexec
                | ServiceKind::Rsh
                | ServiceKind::Rlogin
        )
    }

    /// Whether connecting needs a password up front. Telnet and rlogin ask on screen; rsh and
    /// NFS trust the client; SSH may log in with a key instead (the caller checks that).
    pub fn needs_password(self) -> bool {
        matches!(
            self,
            ServiceKind::Ftp
                | ServiceKind::Smb
                | ServiceKind::Ssh
                | ServiceKind::Rexec
                | ServiceKind::Afp
        )
    }

    /// Whether the password or the commands cross the network unencrypted.
    pub fn plaintext(self) -> bool {
        !matches!(self, ServiceKind::Ssh | ServiceKind::Demo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub kind: ServiceKind,
    pub port: u16,
}

/// How the connection view is arranged for this machine; remembered when the user changes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// The files area's share of the height (0.1 to 0.9).
    pub files_share: f32,
    /// The terminal area is on top.
    pub swapped: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            files_share: DEFAULT_FILES_SHARE,
            swapped: false,
        }
    }
}

/// Where a machine's password lives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Password {
    /// Saved in `machines.json`, Base64-encoded (not encrypted).
    Saved(String),
    /// Never written; asked at connect.
    #[default]
    Ask,
    /// In the OS credential store under the machine's id (`model::secrets`).
    Os,
}

impl Password {
    fn to_tagged(&self) -> String {
        match self {
            Password::Saved(p) => {
                format!(
                    "b64:{}",
                    base64::engine::general_purpose::STANDARD.encode(p)
                )
            }
            Password::Ask => "ask".to_string(),
            Password::Os => "os".to_string(),
        }
    }

    /// The password to log in with: `None` to ask, an error when the OS store has none.
    pub fn resolve(&self, machine_id: &str) -> Result<Option<String>, String> {
        match self {
            Password::Saved(p) => Ok(Some(p.clone())),
            Password::Ask => Ok(None),
            Password::Os => super::secrets::get(machine_id).map(Some),
        }
    }

    fn from_tagged(s: &str) -> Result<Password, String> {
        if s == "ask" || s.is_empty() {
            return Ok(Password::Ask);
        }
        if s == "os" {
            return Ok(Password::Os);
        }
        if let Some(b) = s.strip_prefix("b64:") {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|e| format!("password: {e}"))?;
            return String::from_utf8(bytes)
                .map(Password::Saved)
                .map_err(|e| format!("password: {e}"));
        }
        Err(format!(
            "password: unknown store {:?}",
            s.split(':').next().unwrap_or("")
        ))
    }
}

impl Serialize for Password {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_tagged())
    }
}

impl<'de> Deserialize<'de> for Password {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Password::from_tagged(&s).map_err(serde::de::Error::custom)
    }
}

/// One old computer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    /// Stable id, also the name the CLI uses (`lc ls sparc:/usr`).
    pub id: String,
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: Password,
    /// A character set label from `lc_core::Charset` (`ISO-8859-1`, `MacRoman`, ...).
    #[serde(default = "default_charset")]
    pub charset: String,
    /// This machine's own home folder instead of the app's.
    #[serde(default)]
    pub home_folder: Option<PathBuf>,
    #[serde(default)]
    pub layout: Layout,
    /// The colour of its tab and header, so machines are told apart at a glance.
    #[serde(default)]
    pub color: TabColor,
    /// Sent before a shell session closes (`exit`, `logout`); empty sends nothing.
    #[serde(default)]
    pub logout_command: String,
    /// Log in over SSH as `ssh` does too: the keys in the user's ssh-agent and `~/.ssh`.
    #[serde(default = "yes")]
    pub use_agent: bool,
    /// The terminal type a shell asks for (`vt100`, `xterm-256color`); empty picks one
    /// ([`Machine::terminal_type`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal: String,
    /// The SSH host key seen last time (`ssh-dss AAAA...`), learned on the first connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    /// Check the SSH host key; off for lab machines that are reinstalled all the time.
    #[serde(default = "yes")]
    pub check_host_key: bool,
    /// The SSH login key (traditional PEM; its public half is next to it as `.pub`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// The uid and gid NFS sends; the server trusts them (nobody, 65534, when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// NFS version 3 or 2; unset tries 3, then 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfs_version: Option<u32>,
    /// The name on this computer that rsh / rlogin present (`.rhosts` lists it); the user when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_user: Option<String>,
    /// Folders on the machine the user pinned to its Places menu, as the machine spells them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_folders: Vec<String>,
    /// Folders on the machine files were last copied to or from, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_folders: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Machine {
    /// The terminal type a shell asks for: the machine's own setting, else `xterm-256color` for
    /// a modern SSH server (OpenSSH 4 and later, Dropbear), whose shells colour their prompts only
    /// for xterm-like terminals, and `vt100` for everything older, which knows nothing newer.
    pub fn terminal_type(&self, ssh_banner: Option<&str>) -> String {
        if !self.terminal.trim().is_empty() {
            return self.terminal.trim().to_string();
        }
        let modern = ssh_banner.is_some_and(|b| {
            let soft = b.split('-').nth(2).unwrap_or("");
            let openssh = soft
                .strip_prefix("OpenSSH_")
                .and_then(|v| v.split(['.', 'p']).next())
                .and_then(|v| v.parse::<u32>().ok())
                .is_some_and(|major| major >= 4);
            openssh || soft.starts_with("dropbear")
        });
        if modern { "xterm-256color" } else { "vt100" }.to_string()
    }
}

/// A machine's tab colour, from a fixed palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabColor {
    #[default]
    Gray,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
}

impl TabColor {
    pub const ALL: [TabColor; 9] = [
        TabColor::Gray,
        TabColor::Red,
        TabColor::Orange,
        TabColor::Yellow,
        TabColor::Green,
        TabColor::Teal,
        TabColor::Blue,
        TabColor::Purple,
        TabColor::Pink,
    ];

    /// The colour a new machine gets: the palette in turn, skipping gray.
    pub fn nth(n: usize) -> TabColor {
        TabColor::ALL[1 + n % (TabColor::ALL.len() - 1)]
    }

    pub fn label(self) -> &'static str {
        match self {
            TabColor::Gray => "Gray",
            TabColor::Red => "Red",
            TabColor::Orange => "Orange",
            TabColor::Yellow => "Yellow",
            TabColor::Green => "Green",
            TabColor::Teal => "Teal",
            TabColor::Blue => "Blue",
            TabColor::Purple => "Purple",
            TabColor::Pink => "Pink",
        }
    }

    /// The colour as 0xRRGGBB, readable on light and dark backgrounds.
    pub fn rgb(self) -> u32 {
        match self {
            TabColor::Gray => 0x8a8f96,
            TabColor::Red => 0xd13438,
            TabColor::Orange => 0xe8741c,
            TabColor::Yellow => 0xd9a400,
            TabColor::Green => 0x2e8b3a,
            TabColor::Teal => 0x038387,
            TabColor::Blue => 0x0067c0,
            TabColor::Purple => 0x8764b8,
            TabColor::Pink => 0xc239b3,
        }
    }
}

fn default_charset() -> String {
    lc_core::Charset::default().label().to_string()
}

impl Machine {
    pub fn new(id: &str, address: &str) -> Machine {
        Machine {
            id: id.to_string(),
            name: id.to_string(),
            address: address.to_string(),
            services: Vec::new(),
            user: String::new(),
            password: Password::Ask,
            charset: default_charset(),
            home_folder: None,
            layout: Layout::default(),
            color: TabColor::default(),
            logout_command: String::new(),
            terminal: String::new(),
            use_agent: true,
            host_key: None,
            check_host_key: true,
            key_file: None,
            uid: None,
            gid: None,
            nfs_version: None,
            local_user: None,
            pinned_folders: Vec::new(),
            recent_folders: Vec::new(),
        }
    }

    /// The service used for files: the first that serves them.
    pub fn file_service(&self) -> Option<&Service> {
        self.services.iter().find(|s| s.kind.serves_files())
    }

    /// The service used for commands: the first that serves them.
    pub fn command_service(&self) -> Option<&Service> {
        self.services.iter().find(|s| s.kind.serves_commands())
    }
}

/// A machine id: letters, digits, `-`, `_` and `.`, so it works as a CLI prefix (`sparc:/usr`).
pub fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err(String::new());
    }
    if id.len() > 40 {
        return Err("At most 40 characters.".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("Use letters, digits, '-', '_' or '.' only.".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MachineStore {
    #[serde(default)]
    pub machines: Vec<Machine>,
}

impl MachineStore {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("machines.json")
    }

    /// Read `machines.json` from `dir`; a missing file is an empty store.
    pub fn load(dir: &Path) -> anyhow::Result<MachineStore> {
        match std::fs::read(Self::path_in(dir)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MachineStore::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        super::config::write_atomic(&Self::path_in(dir), &serde_json::to_vec_pretty(self)?)
    }

    pub fn get(&self, id: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Machine> {
        self.machines.iter_mut().find(|m| m.id == id)
    }

    /// Add a machine; refuses a bad or taken id.
    pub fn add(&mut self, machine: Machine) -> Result<(), String> {
        check_id(&machine.id).map_err(|e| {
            if e.is_empty() {
                "No name given.".into()
            } else {
                e
            }
        })?;
        if self.get(&machine.id).is_some() {
            return Err(format!("A machine called {} already exists.", machine.id));
        }
        let mut machine = machine;
        if machine.color == TabColor::Gray {
            machine.color = TabColor::nth(self.machines.len());
        }
        self.machines.push(machine);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|m| m.id != id);
        self.machines.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_base64_on_disk_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = MachineStore::default();
        let mut m = Machine::new("sparc", "192.168.1.40");
        m.services.push(Service {
            kind: ServiceKind::Ftp,
            port: 21,
        });
        m.user = "dani".into();
        m.password = Password::Saved("s3cret".into());
        store.add(m).unwrap();
        store.add(Machine::new("win95", "192.168.1.41")).unwrap();
        store.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(MachineStore::path_in(dir.path())).unwrap();
        assert!(text.contains("\"b64:czNjcmV0\""), "{text}");
        assert!(!text.contains("s3cret"));
        assert!(text.contains("\"ask\""));
        assert_eq!(MachineStore::load(dir.path()).unwrap(), store);
    }

    #[test]
    fn ids_are_checked_and_unique() {
        let mut store = MachineStore::default();
        assert!(store.add(Machine::new("a b", "x")).is_err());
        store.add(Machine::new("indy", "x")).unwrap();
        assert!(store.add(Machine::new("indy", "y")).is_err());
        assert!(store.remove("indy"));
        assert!(!store.remove("indy"));
    }

    #[test]
    fn an_unknown_password_store_is_an_error_not_a_silent_ask() {
        let r: Result<Password, _> = serde_json::from_str("\"enc:abc\"");
        assert!(r.is_err());
        let os: Password = serde_json::from_str("\"os\"").unwrap();
        assert_eq!(os, Password::Os);
        assert_eq!(serde_json::to_string(&Password::Os).unwrap(), "\"os\"");
    }

    #[test]
    fn terminal_type_is_xterm_for_modern_ssh_and_vt100_before() {
        let mut m = Machine::new("pi", "192.168.99.143");
        assert_eq!(
            m.terminal_type(Some("SSH-2.0-OpenSSH_8.6")),
            "xterm-256color"
        );
        assert_eq!(
            m.terminal_type(Some("SSH-2.0-dropbear_2022.83")),
            "xterm-256color"
        );
        assert_eq!(m.terminal_type(Some("SSH-2.0-OpenSSH_3.4p1")), "vt100");
        assert_eq!(m.terminal_type(Some("SSH-1.5-1.2.27")), "vt100");
        assert_eq!(m.terminal_type(None), "vt100");
        m.terminal = "vt220".into();
        assert_eq!(m.terminal_type(Some("SSH-2.0-OpenSSH_9.2")), "vt220");
    }
}
