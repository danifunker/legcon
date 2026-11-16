//! Machine settings (PLAN.md §1.1 Settings, mockup 5) as a form every shell and the CLI share.

use lc_core::Charset;

use super::{ids, Field, FieldKind, FieldValue, FormSpec, FormValues};
use crate::model::keystore::KeyInfo;
use crate::model::machines::{Machine, Password, Service, ServiceKind};
use crate::model::secrets;

pub const PURPOSE: &str = "machines.settings";

/// File services offered in the form, after "(none)".
const FILE_KINDS: [ServiceKind; 8] = [
    ServiceKind::Ftp,
    ServiceKind::Smb,
    ServiceKind::Ssh,
    ServiceKind::Nfs,
    ServiceKind::Afp,
    ServiceKind::Rsh,
    ServiceKind::Http,
    ServiceKind::Gopher,
];

const COMMAND_KINDS: [ServiceKind; 5] = [
    ServiceKind::Telnet,
    ServiceKind::Ssh,
    ServiceKind::Rexec,
    ServiceKind::Rsh,
    ServiceKind::Rlogin,
];

fn kinds(files: bool, m: &Machine) -> Vec<ServiceKind> {
    let mut v: Vec<ServiceKind> = if files {
        FILE_KINDS.to_vec()
    } else {
        COMMAND_KINDS.to_vec()
    };
    // The demo machine keeps its pretend service.
    if files && m.services.iter().any(|s| s.kind == ServiceKind::Demo) {
        v.push(ServiceKind::Demo);
    }
    v
}

/// The NFS version choices and what each stores.
const NFS_VERSIONS: [(&str, Option<u32>); 3] = [("Auto", None), ("3", Some(3)), ("2", Some(2))];

/// Terminal types a shell can ask for; the first is "pick one" (`Machine::terminal_type`).
const TERMINALS: [&str; 10] = [
    "Auto",
    "vt100",
    "vt102",
    "vt220",
    "ansi",
    "xterm",
    "xterm-color",
    "xterm-256color",
    "iris-ansi",
    "dumb",
];

fn choice(options: Vec<String>, selected: usize) -> FieldKind {
    FieldKind::Choice { options, selected }
}

fn field(id: &'static str, label: &str, kind: FieldKind, hint: &str) -> Field {
    Field {
        id,
        label: label.into(),
        kind,
        hint: hint.into(),
    }
}

fn heading(id: &'static str, label: &str) -> Field {
    field(id, label, FieldKind::Heading, "")
}

fn service_fields(m: &Machine, files: bool) -> (FieldKind, FieldKind) {
    let list = kinds(files, m);
    let current = if files {
        m.file_service()
    } else {
        m.command_service()
    };
    let selected = current
        .and_then(|s| list.iter().position(|k| *k == s.kind))
        .map_or(0, |i| i + 1);
    let port = current
        .filter(|s| s.port != s.kind.default_port())
        .map(|s| s.port.to_string())
        .unwrap_or_default();
    let options = std::iter::once("(none)".to_string())
        .chain(list.iter().map(|k| k.label().to_string()))
        .collect();
    (choice(options, selected), FieldKind::Text(port))
}

/// The login keys the form offers after "none": the store's keys, plus the machine's own if elsewhere.
fn key_choices(m: &Machine, keys: &[KeyInfo]) -> Vec<(String, std::path::PathBuf)> {
    let mut v: Vec<(String, std::path::PathBuf)> = keys
        .iter()
        .map(|k| (format!("{} ({})", k.name, k.kind_label()), k.path.clone()))
        .collect();
    if let Some(p) = m
        .key_file
        .as_ref()
        .filter(|p| !v.iter().any(|(_, k)| k == *p))
    {
        v.push((p.display().to_string(), p.clone()));
    }
    v
}

/// The settings form for `m`; the password is read from the OS store when it is kept there.
pub fn form(m: &Machine, keys: &[KeyInfo]) -> FormSpec {
    let (files, files_port) = service_fields(m, true);
    let (commands, commands_port) = service_fields(m, false);
    let (password, os_hint) = match &m.password {
        Password::Saved(p) => (p.clone(), String::new()),
        Password::Ask => (String::new(), String::new()),
        Password::Os => match secrets::get(&m.id) {
            Ok(p) => (p, String::new()),
            Err(e) => (String::new(), format!("Not found: {e}.")),
        },
    };
    let charsets: Vec<String> = Charset::ALL.iter().map(|c| c.label().to_string()).collect();
    let charset = Charset::ALL
        .iter()
        .position(|c| c.label() == m.charset)
        .unwrap_or(0);
    let host_key = m
        .host_key
        .clone()
        .unwrap_or_else(|| "Not seen yet; learned on the first SSH connection.".into());
    let choices = key_choices(m, keys);
    let key_selected = m
        .key_file
        .as_ref()
        .and_then(|p| choices.iter().position(|(_, k)| k == p))
        .map_or(0, |i| i + 1);
    let key_options = std::iter::once("(none: Set up key login makes one)".to_string())
        .chain(choices.into_iter().map(|(label, _)| label))
        .collect();
    let fields = vec![
        heading(ids::MS_GENERAL, "Machine"),
        field(ids::MS_NAME, "Name", FieldKind::Text(m.name.clone()), ""),
        field(
            ids::MS_ADDRESS,
            "Address",
            FieldKind::Text(m.address.clone()),
            "Host name or IP address.",
        ),
        field(ids::MS_FILES, "Files over", files, ""),
        field(
            ids::MS_FILES_PORT,
            "Files port",
            files_port,
            "Empty for the service's usual port.",
        ),
        field(ids::MS_COMMANDS, "Commands over", commands, ""),
        field(
            ids::MS_COMMANDS_PORT,
            "Commands port",
            commands_port,
            "Empty for the service's usual port.",
        ),
        heading(ids::MS_LOGIN, "Login"),
        field(ids::MS_USER, "User", FieldKind::Text(m.user.clone()), ""),
        field(
            ids::MS_PASSWORD,
            "Password",
            FieldKind::Secret(password),
            "Empty asks every time. Kept in machines.json, not encrypted.",
        ),
        field(
            ids::MS_OS_STORE,
            &format!("Keep the password in {}", secrets::store_name()),
            FieldKind::Check(m.password == Password::Os),
            &os_hint,
        ),
        field(
            ids::MS_KEY_FILE,
            "SSH login key",
            choice(key_options, key_selected),
            "Keys are made and imported on the Keys tab.",
        ),
        field(
            ids::MS_USE_AGENT,
            "Also log in as ssh does: ssh-agent and ~/.ssh keys",
            FieldKind::Check(m.use_agent),
            "Tried before the password, which is asked only if no key gets in. Read, never written.",
        ),
        field(
            ids::MS_HOST_KEY,
            "SSH host key",
            FieldKind::Info(host_key),
            "",
        ),
        field(
            ids::MS_CHECK_HOST_KEY,
            "Check the SSH host key",
            FieldKind::Check(m.check_host_key),
            "Off for lab machines that are reinstalled often.",
        ),
        field(
            ids::MS_FORGET_HOST_KEY,
            "Forget the saved host key",
            FieldKind::Check(false),
            "",
        ),
        heading(ids::MS_SYSTEM, "System"),
        field(
            ids::MS_CHARSET,
            "Character set",
            choice(charsets, charset),
            "How the machine spells file names.",
        ),
        field(
            ids::MS_HOME,
            "Folder on this computer",
            FieldKind::Folder(
                m.home_folder
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
            "Where this machine's local pane opens; empty for legcon's home folder.",
        ),
        field(
            ids::MS_REMOTE_FOLDER,
            "Folder on the machine",
            FieldKind::Text(m.remote_folder.clone()),
            "Where the machine's pane opens after connecting, as the machine spells it (/media/fat); \
             empty for the login folder.",
        ),
        field(
            ids::MS_LOGOUT,
            "Logout command",
            FieldKind::Text(m.logout_command.clone()),
            "Sent before a shell closes, such as exit or logout.",
        ),
        field(
            ids::MS_TERMINAL,
            "Terminal type",
            choice(
                TERMINALS.iter().map(|t| t.to_string()).collect(),
                TERMINALS.iter().position(|t| *t == m.terminal).unwrap_or(0),
            ),
            "Auto: xterm-256color on modern SSH servers (colour prompts), vt100 on older ones.",
        ),
        field(
            ids::MS_UID,
            "NFS uid",
            FieldKind::Text(m.uid.map(|v| v.to_string()).unwrap_or_default()),
            "Empty for nobody (65534).",
        ),
        field(
            ids::MS_GID,
            "NFS gid",
            FieldKind::Text(m.gid.map(|v| v.to_string()).unwrap_or_default()),
            "",
        ),
        field(
            ids::MS_NFS_VERSION,
            "NFS version",
            choice(
                NFS_VERSIONS.iter().map(|(l, _)| l.to_string()).collect(),
                NFS_VERSIONS
                    .iter()
                    .position(|(_, v)| *v == m.nfs_version)
                    .unwrap_or(0),
            ),
            "Auto asks for 3 and falls back to 2.",
        ),
        field(
            ids::MS_LOCAL_USER,
            "rsh / rlogin local user",
            FieldKind::Text(m.local_user.clone().unwrap_or_default()),
            "The name .rhosts trusts; empty for the user above.",
        ),
    ];
    FormSpec {
        purpose: PURPOSE,
        title: format!("Settings for {}", m.name),
        fields,
        ok_label: "Save".into(),
        validator: validate,
    }
}

fn port(text: &str) -> Result<Option<u16>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<u16>() {
        Ok(p) if p > 0 => Ok(Some(p)),
        _ => Err(format!("{t} is not a port (1 to 65535).")),
    }
}

fn number(text: &str, what: &str) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("The {what} is a number."))
}

/// The form's rule: what must be filled in and well formed.
pub fn validate(v: &FormValues) -> Result<(), String> {
    if v.text(ids::MS_NAME).trim().is_empty() {
        return Err("The machine needs a name.".into());
    }
    if v.text(ids::MS_ADDRESS).trim().is_empty() {
        return Err("The machine needs an address.".into());
    }
    if v.choice(ids::MS_FILES) == 0 && v.choice(ids::MS_COMMANDS) == 0 {
        return Err("Choose a service for files or for commands.".into());
    }
    port(v.text(ids::MS_FILES_PORT))?;
    port(v.text(ids::MS_COMMANDS_PORT))?;
    number(v.text(ids::MS_UID), "uid")?;
    number(v.text(ids::MS_GID), "gid")?;
    if v.check(ids::MS_OS_STORE) && v.text(ids::MS_PASSWORD).is_empty() {
        return Err(format!(
            "Type the password to keep it in {}.",
            secrets::store_name()
        ));
    }
    Ok(())
}

fn service(m: &Machine, files: bool, choice: usize, port_text: &str) -> Option<Service> {
    let kind = *kinds(files, m).get(choice.checked_sub(1)?)?;
    let port = port(port_text)
        .ok()
        .flatten()
        .unwrap_or(kind.default_port());
    Some(Service { kind, port })
}

/// Write the answer into `m`, the password into the OS store when asked; the form's error else.
pub fn apply(m: &mut Machine, v: &FormValues, keys: &[KeyInfo]) -> Result<(), String> {
    validate(v)?;
    let key = v
        .choice(ids::MS_KEY_FILE)
        .checked_sub(1)
        .and_then(|i| key_choices(m, keys).into_iter().nth(i))
        .map(|(_, p)| p);
    let files = service(m, true, v.choice(ids::MS_FILES), v.text(ids::MS_FILES_PORT));
    let commands = service(
        m,
        false,
        v.choice(ids::MS_COMMANDS),
        v.text(ids::MS_COMMANDS_PORT),
    );
    let mut services: Vec<Service> = files.into_iter().collect();
    if let Some(c) = commands {
        // One service can do both (SSH, rsh); keep it once, with the files port.
        if !services.iter().any(|s| s.kind == c.kind) {
            services.push(c);
        }
    }
    let password = v.text(ids::MS_PASSWORD).to_string();
    let new_password = match (password.is_empty(), v.check(ids::MS_OS_STORE)) {
        (true, _) => Password::Ask,
        (false, true) => {
            secrets::set(&m.id, &password)?;
            Password::Os
        }
        (false, false) => Password::Saved(password),
    };
    if m.password == Password::Os && new_password != Password::Os {
        secrets::delete(&m.id)?;
    }
    m.password = new_password;
    m.name = v.text(ids::MS_NAME).trim().to_string();
    m.address = v.text(ids::MS_ADDRESS).trim().to_string();
    m.services = services;
    m.user = v.text(ids::MS_USER).trim().to_string();
    if let Some(c) = Charset::ALL.get(v.choice(ids::MS_CHARSET)) {
        m.charset = c.label().to_string();
    }
    let home = v.text(ids::MS_HOME).trim();
    m.home_folder = (!home.is_empty()).then(|| home.into());
    m.remote_folder = v.text(ids::MS_REMOTE_FOLDER).trim().to_string();
    m.logout_command = v.text(ids::MS_LOGOUT).trim().to_string();
    m.terminal = match v.choice(ids::MS_TERMINAL) {
        0 => String::new(),
        i => TERMINALS.get(i).map_or_else(String::new, |t| t.to_string()),
    };
    m.key_file = key;
    m.use_agent = v.check(ids::MS_USE_AGENT);
    m.check_host_key = v.check(ids::MS_CHECK_HOST_KEY);
    if v.check(ids::MS_FORGET_HOST_KEY) {
        m.host_key = None;
    }
    m.uid = number(v.text(ids::MS_UID), "uid")?;
    m.gid = number(v.text(ids::MS_GID), "gid")?;
    if let Some((_, n)) = NFS_VERSIONS.get(v.choice(ids::MS_NFS_VERSION)) {
        m.nfs_version = *n;
    }
    let local = v.text(ids::MS_LOCAL_USER).trim();
    m.local_user = (!local.is_empty()).then(|| local.to_string());
    Ok(())
}

/// The form again with the user's answer in it, after a failed save.
pub fn refill(mut spec: FormSpec, v: &FormValues) -> FormSpec {
    for f in &mut spec.fields {
        let Some((_, value)) = v.0.iter().find(|(id, _)| *id == f.id) else {
            continue;
        };
        match (&mut f.kind, value) {
            (
                FieldKind::Text(t) | FieldKind::Secret(t) | FieldKind::Folder(t),
                FieldValue::Text(n),
            ) => *t = n.clone(),
            (FieldKind::Check(b), FieldValue::Check(n)) => *b = *n,
            (FieldKind::Choice { selected, .. }, FieldValue::Choice(n)) => *selected = *n,
            _ => {}
        }
    }
    spec
}

/// Set one field from text, as the CLI's `field=value` does: a choice by its label, a check by yes / no.
pub fn set_from_text(
    spec: &FormSpec,
    v: &mut FormValues,
    id: &str,
    text: &str,
) -> Result<(), String> {
    let f = spec
        .fields
        .iter()
        .find(|f| f.id == id)
        .ok_or_else(|| format!("No setting called {id}."))?;
    let value = match &f.kind {
        FieldKind::Text(_) | FieldKind::Secret(_) | FieldKind::Folder(_) => {
            FieldValue::Text(text.to_string())
        }
        FieldKind::Check(_) => match text.to_ascii_lowercase().as_str() {
            "yes" | "on" | "true" | "1" => FieldValue::Check(true),
            "no" | "off" | "false" | "0" => FieldValue::Check(false),
            _ => return Err(format!("{} is yes or no.", f.label)),
        },
        FieldKind::Choice { options, .. } => options
            .iter()
            .position(|o| o.eq_ignore_ascii_case(text))
            .map(FieldValue::Choice)
            .ok_or_else(|| format!("{} is one of: {}.", f.label, options.join(", ")))?,
        FieldKind::Heading | FieldKind::Info(_) => {
            return Err(format!("{} cannot be changed here.", f.label))
        }
    };
    v.set(f.id, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        let mut m = Machine::new("form-test", "10.0.0.5");
        m.services = vec![
            Service {
                kind: ServiceKind::Ftp,
                port: 21,
            },
            Service {
                kind: ServiceKind::Telnet,
                port: 2323,
            },
        ];
        m.password = Password::Saved("pw".into());
        m
    }

    #[test]
    fn opening_and_saving_unchanged_keeps_the_machine() {
        let m = machine();
        let spec = form(&m, &[]);
        let mut edited = m.clone();
        apply(&mut edited, &spec.initial(), &[]).unwrap();
        assert_eq!(edited, m);
    }

    #[test]
    fn changes_land_in_the_machine() {
        secrets::use_memory_store();
        let mut m = machine();
        let spec = form(&m, &[]);
        let mut v = spec.initial();
        set_from_text(&spec, &mut v, ids::MS_FILES, "SSH").unwrap();
        set_from_text(&spec, &mut v, ids::MS_COMMANDS, "ssh").unwrap();
        set_from_text(&spec, &mut v, ids::MS_FILES_PORT, "2222").unwrap();
        set_from_text(&spec, &mut v, ids::MS_CHARSET, "MacRoman").unwrap();
        set_from_text(&spec, &mut v, ids::MS_UID, "100").unwrap();
        set_from_text(&spec, &mut v, ids::MS_NFS_VERSION, "2").unwrap();
        set_from_text(&spec, &mut v, ids::MS_OS_STORE, "yes").unwrap();
        apply(&mut m, &v, &[]).unwrap();
        assert_eq!(
            m.services,
            vec![Service {
                kind: ServiceKind::Ssh,
                port: 2222
            }]
        );
        assert_eq!(m.charset, "MacRoman");
        assert_eq!(m.uid, Some(100));
        assert_eq!(m.nfs_version, Some(2));
        assert_eq!(m.password, Password::Os);
        assert_eq!(secrets::get("form-test").unwrap(), "pw");
        // Unticking moves it back into the file and out of the store.
        let spec = form(&m, &[]);
        let mut v = spec.initial();
        assert_eq!(v.text(ids::MS_PASSWORD), "pw");
        set_from_text(&spec, &mut v, ids::MS_OS_STORE, "no").unwrap();
        apply(&mut m, &v, &[]).unwrap();
        assert_eq!(m.password, Password::Saved("pw".into()));
        assert!(secrets::get("form-test").is_err());
    }

    #[test]
    fn bad_answers_are_refused_with_a_reason() {
        let m = machine();
        let spec = form(&m, &[]);
        let mut v = spec.initial();
        v.set(ids::MS_FILES_PORT, FieldValue::Text("99999".into()));
        assert!(spec.validate(&v).unwrap_err().contains("port"));
        let mut v = spec.initial();
        v.set(ids::MS_FILES, FieldValue::Choice(0));
        v.set(ids::MS_COMMANDS, FieldValue::Choice(0));
        assert!(spec.validate(&v).is_err());
        let mut v = spec.initial();
        v.set(ids::MS_PASSWORD, FieldValue::Text(String::new()));
        v.set(ids::MS_OS_STORE, FieldValue::Check(true));
        assert!(spec.validate(&v).is_err());
        assert!(set_from_text(&spec, &mut v, ids::MS_CHARSET, "EBCDIC").is_err());
        assert!(set_from_text(&spec, &mut v, ids::MS_HOST_KEY, "x").is_err());
    }

    #[test]
    fn an_empty_password_means_ask() {
        let mut m = machine();
        let spec = form(&m, &[]);
        let mut v = spec.initial();
        v.set(ids::MS_PASSWORD, FieldValue::Text(String::new()));
        apply(&mut m, &v, &[]).unwrap();
        assert_eq!(m.password, Password::Ask);
    }
}
