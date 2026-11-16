//! The setup (D45): on first run, and from Set up... later. One form with a section per topic,
//! every shell drawing it as it draws any form: the home folder, folders on this computer to pin
//! to the Places menu, a starter set of SSH keys for old servers, where passwords are kept, and
//! what the OS will ask for. Then, with no machines yet, an offer to add the first one.

use std::path::{Path, PathBuf};

use lc_ssh::keys::KeyKind;

use super::{ids, ChoiceSpec, Effect, Field, FieldKind, FormSpec, FormValues};
use crate::model::places::{self, Place};
use crate::model::secrets;
use crate::model::settings::Settings;

pub const PURPOSE: &str = "setup";
/// The closing question: add the first machine now, or later.
pub const FIRST_PURPOSE: &str = "setup.first";

/// The starter keys: field id, key name, kind, what it is for.
pub const STARTER_KEYS: [(&str, &str, KeyKind, &str); 3] = [
    (
        ids::SETUP_KEY_RSA,
        "legcon-rsa2048",
        KeyKind::Rsa2048,
        "RSA 2048, SSH-2: OpenSSH 2.5 and later, SSH.com 2.4 and later, Dropbear",
    ),
    (
        ids::SETUP_KEY_DSA,
        "legcon-dsa1024",
        KeyKind::Dsa1024,
        "DSA 1024, SSH-2: OpenSSH before 2.5, SSH.com before 2.4",
    ),
    (
        ids::SETUP_KEY_SSH1,
        "legcon-ssh1",
        KeyKind::Ssh1Rsa1024,
        "RSA 1024, SSH-1: SSH 1.2.x, F-Secure 1.x, OpenSSH with Protocol 1",
    ),
];

/// The folders the form offers to pin, by the field id each got.
pub type Offered = Vec<(&'static str, PathBuf)>;

fn field(id: &'static str, label: &str, kind: FieldKind, hint: &str) -> Field {
    Field {
        id,
        label: label.into(),
        kind,
        hint: hint.into(),
    }
}

/// The home folder a first run suggests.
fn default_home() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join("Retro"))
        .unwrap_or_else(|| PathBuf::from("Retro"))
}

/// What the OS will ask for, in this OS's words.
fn permissions_text() -> String {
    if cfg!(target_os = "macos") {
        "macOS asks once whether legcon may find and connect to devices on your local network: \
         choose Allow, or every machine on your LAN will look unreachable (System Settings > \
         Privacy & Security > Local Network changes it later). Folders under Desktop, Documents \
         and Downloads, and removable volumes, ask the first time legcon opens them. Nothing else \
         is needed: legcon only makes outgoing connections."
            .into()
    } else if cfg!(windows) {
        "Nothing to allow: legcon only makes outgoing connections, so Windows Firewall needs no \
         rule for it."
            .into()
    } else {
        "Nothing to allow for most services. rsh and rlogin connect from a port below 1024, which \
         Linux allows only with the CAP_NET_BIND_SERVICE capability (the package grants it)."
            .into()
    }
}

/// The places worth pinning: the user's folders, mounted volumes, emulators' shared folders.
fn candidates() -> Vec<Place> {
    let mut all = places::system_places();
    all.extend(places::volumes());
    all.extend(places::emulator_folders());
    all.truncate(ids::SETUP_PINS.len());
    all
}

/// The setup form, and the folders it offers by field id.
pub fn form(settings: &Settings, has_key: impl Fn(&str) -> bool) -> (FormSpec, Offered) {
    let home = settings
        .home_folder
        .clone()
        .unwrap_or_else(default_home)
        .display()
        .to_string();
    let mut fields =
        vec![
        field(ids::SETUP_SECTION_HOME, "Home", FieldKind::Heading, ""),
        field(
            ids::SETUP_HOME,
            "Home folder",
            FieldKind::Folder(home),
            "Where every connection's local pane starts; made if it is not there. A machine can \
             have its own.",
        ),
        field(ids::SETUP_SECTION_FOLDERS, "Folders", FieldKind::Heading, ""),
    ];
    let mut offered = Vec::new();
    for (id, p) in ids::SETUP_PINS.iter().zip(candidates()) {
        let pinned = settings.pinned_places.contains(&p.path);
        fields.push(field(
            id,
            &format!("Pin {}", p.label),
            FieldKind::Check(pinned),
            &p.path.display().to_string(),
        ));
        offered.push((*id, p.path));
    }
    fields.push(field(
        ids::SETUP_SECTION_KEYS,
        "Keys",
        FieldKind::Heading,
        "",
    ));
    for (id, name, _, what) in STARTER_KEYS {
        let made = has_key(name);
        fields.push(field(
            id,
            &format!("Make {name}"),
            FieldKind::Check(!made && id != ids::SETUP_KEY_SSH1),
            if made { "Made already." } else { what },
        ));
    }
    fields.push(field(
        ids::SETUP_SECTION_PASSWORDS,
        "Passwords",
        FieldKind::Heading,
        "",
    ));
    fields.push(field(
        ids::SETUP_PASSWORDS,
        "Keep passwords",
        FieldKind::Choice {
            options: vec![
                "In legcon's machines file (encoded, not encrypted)".into(),
                format!("In the {}", secrets::store_name()),
            ],
            selected: usize::from(settings.passwords_in_os_store),
        },
        "The default for each machine's settings; old machines' passwords are often throw-away.",
    ));
    fields.push(field(
        ids::SETUP_SECTION_PERMISSIONS,
        "Permissions",
        FieldKind::Heading,
        "",
    ));
    fields.push(field(
        ids::SETUP_PERMISSIONS,
        "What the system asks",
        FieldKind::Info(permissions_text()),
        "",
    ));
    let spec = FormSpec {
        purpose: PURPOSE,
        title: "Set up legcon".into(),
        fields,
        ok_label: "Finish".into(),
        validator,
    };
    (spec, offered)
}

fn validator(v: &FormValues) -> Result<(), String> {
    let home = v.text(ids::SETUP_HOME).trim();
    if home.is_empty() {
        return Err("Choose a home folder.".into());
    }
    if !Path::new(home).is_absolute() {
        return Err("The home folder needs a full path.".into());
    }
    Ok(())
}

/// What the answered form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub home: PathBuf,
    pub pin: Vec<PathBuf>,
    pub unpin: Vec<PathBuf>,
    pub keys: Vec<(&'static str, KeyKind)>,
    pub passwords_in_os_store: bool,
}

pub fn read(v: &FormValues, offered: &Offered) -> Answer {
    let (pin, unpin) = offered
        .iter()
        .map(|(id, p)| (v.check(id), p.clone()))
        .partition::<Vec<_>, _>(|(on, _)| *on);
    Answer {
        home: PathBuf::from(v.text(ids::SETUP_HOME).trim()),
        pin: pin.into_iter().map(|(_, p)| p).collect(),
        unpin: unpin.into_iter().map(|(_, p)| p).collect(),
        keys: STARTER_KEYS
            .iter()
            .filter(|(id, ..)| v.check(id))
            .map(|(_, name, kind, _)| (*name, *kind))
            .collect(),
        passwords_in_os_store: v.choice(ids::SETUP_PASSWORDS) == 1,
    }
}

/// The closing question when there are no machines yet.
pub fn first_machine() -> Effect {
    Effect::Choose(ChoiceSpec {
        purpose: FIRST_PURPOSE,
        title: "Set up legcon".into(),
        message: "legcon is set up. Add your first machine now? Type its address and legcon \
                  finds out what it speaks."
            .into(),
        choices: vec!["Add a machine...".into(), "Later".into()],
        dismiss: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::FieldValue;

    #[test]
    fn the_form_offers_folders_keys_and_reads_back() {
        let settings = Settings::default();
        let (spec, offered) = form(&settings, |n| n == "legcon-dsa1024");
        assert!(spec.fields.iter().any(|f| f.id == ids::SETUP_HOME));
        // A key made already is not made again.
        let dsa = spec
            .fields
            .iter()
            .find(|f| f.id == ids::SETUP_KEY_DSA)
            .unwrap();
        assert_eq!(dsa.kind, FieldKind::Check(false));
        let mut v = spec.initial();
        assert!(spec.validate(&v).is_ok());
        v.set(ids::SETUP_HOME, FieldValue::Text("relative".into()));
        assert!(spec.validate(&v).is_err());
        v.set(ids::SETUP_HOME, FieldValue::Text("/tmp/retro".into()));
        v.set(ids::SETUP_PASSWORDS, FieldValue::Choice(1));
        if let Some((id, _)) = offered.first() {
            v.set(id, FieldValue::Check(true));
        }
        let a = read(&v, &offered);
        assert_eq!(a.home, PathBuf::from("/tmp/retro"));
        assert!(a.passwords_in_os_store);
        assert_eq!(a.keys, vec![("legcon-rsa2048", KeyKind::Rsa2048)]);
        assert_eq!(a.pin.len() + a.unpin.len(), offered.len());
    }
}
