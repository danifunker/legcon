//! The Keys tab (PLAN.md §5.1, D36): make, import, export, copy and delete SSH login keys.

use std::path::PathBuf;
use std::sync::Arc;

use lc_ssh::keys::KeyKind;

use super::machines::MachinesController;
use super::{ids, ConfirmSpec, Effect, Field, FieldKind, FormSpec, FormValues, PickSpec, TextSpec};
use crate::model::job::{Job, Notify};
use crate::model::keystore::{self, KeyInfo};

pub const NEW_PURPOSE: &str = "keys.new";

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Reread the key folder (the tab was shown).
    Refresh,
    New,
    NewAnswered(Option<FormValues>),
    Import,
    ImportPicked(Option<PathBuf>),
    ImportNameAnswered(Option<String>),
    Export(String),
    ExportPicked(Option<PathBuf>),
    CopyPublic(String),
    Delete(String),
    DeleteConfirmed(bool),
}

/// One row of the keys list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub name: String,
    pub kind: String,
    pub fingerprint: String,
    pub comment: String,
    /// The machines that log in with it, or why it is not for logging in.
    pub used_by: String,
}

pub struct KeysController {
    dir: PathBuf,
    notify: Arc<dyn Notify>,
    keys: Vec<KeyInfo>,
    making: Option<Job<KeyInfo>>,
    importing: Option<PathBuf>,
    exporting: Option<String>,
    deleting: Option<String>,
    /// Lines for the Log tab, taken by the app controller.
    notes: Vec<String>,
}

fn new_form(name: String, kind: usize, comment: String) -> FormSpec {
    FormSpec {
        purpose: NEW_PURPOSE,
        title: "New SSH key".into(),
        fields: vec![
            Field {
                id: ids::KEY_NAME,
                label: "Name".into(),
                kind: FieldKind::Text(name),
                hint: "Letters, digits, - _ and .".into(),
            },
            Field {
                id: ids::KEY_KIND,
                label: "Kind".into(),
                kind: FieldKind::Choice {
                    options: KeyKind::ALL.iter().map(|k| k.label().to_string()).collect(),
                    selected: kind,
                },
                hint: "SSH-1 keys are for machines that speak only SSH-1.".into(),
            },
            Field {
                id: ids::KEY_COMMENT,
                label: "Comment".into(),
                kind: FieldKind::Text(comment),
                hint: "Shown after the key in authorized_keys.".into(),
            },
        ],
        ok_label: "Make key".into(),
        validator: |v| keystore::check_name(v.text(ids::KEY_NAME)),
    }
}

impl KeysController {
    pub fn new(dir: PathBuf, notify: Arc<dyn Notify>) -> Self {
        let keys = keystore::list(&dir);
        KeysController {
            dir,
            notify,
            keys,
            making: None,
            importing: None,
            exporting: None,
            deleting: None,
            notes: Vec::new(),
        }
    }

    pub fn rows(&self, machines: &MachinesController) -> Vec<KeyRow> {
        self.keys
            .iter()
            .map(|k| KeyRow {
                name: k.name.clone(),
                kind: k.kind_label(),
                fingerprint: k.sha256.clone(),
                comment: k.comment.clone(),
                used_by: machines.using_key(&k.path).join(", "),
            })
            .collect()
    }

    pub(crate) fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// What is being done in the background, for the status line.
    pub fn busy(&self) -> Option<String> {
        self.making
            .as_ref()
            .map(|j| j.snapshot().phase)
            .filter(|p| !p.is_empty())
    }

    fn get(&self, name: &str) -> Option<&KeyInfo> {
        self.keys.iter().find(|k| k.name == name)
    }

    fn reload(&mut self) {
        self.keys = keystore::list(&self.dir);
    }

    pub fn apply(&mut self, action: Action, machines: &mut MachinesController) -> Vec<Effect> {
        match action {
            Action::Refresh => self.reload(),
            Action::New => {
                return vec![Effect::Form(new_form(
                    keystore::free_name(&self.dir, "vintage"),
                    0,
                    "legcon".into(),
                ))]
            }
            Action::NewAnswered(None) => {}
            Action::NewAnswered(Some(v)) => {
                let kind = KeyKind::ALL
                    .get(v.choice(ids::KEY_KIND))
                    .copied()
                    .unwrap_or(KeyKind::Rsa2048);
                if self.making.is_some() {
                    return vec![Effect::Message("A key is already being made.".into())];
                }
                match keystore::generate_job(
                    &self.dir,
                    v.text(ids::KEY_NAME),
                    kind,
                    v.text(ids::KEY_COMMENT),
                    Arc::clone(&self.notify),
                ) {
                    Ok(job) => self.making = Some(job),
                    Err(e) => {
                        let again = new_form(
                            v.text(ids::KEY_NAME).into(),
                            v.choice(ids::KEY_KIND),
                            v.text(ids::KEY_COMMENT).into(),
                        );
                        return vec![Effect::Message(format!("{e:#}")), Effect::Form(again)];
                    }
                }
            }
            Action::Import => {
                let ssh = dirs::home_dir()
                    .map(|h| h.join(".ssh"))
                    .filter(|d| d.is_dir());
                return vec![Effect::PickFile(PickSpec {
                    purpose: "keys.import",
                    title: "Import a private key (OpenSSH, PEM, PuTTY .ppk or SSH-1 identity)"
                        .into(),
                    start: ssh,
                })];
            }
            Action::ImportPicked(None) => {}
            Action::ImportPicked(Some(file)) => {
                let stem = file
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.importing = Some(file);
                return vec![Effect::AskText(TextSpec {
                    purpose: "keys.import.name",
                    title: "Import key".into(),
                    prompt: "Name for the key in legcon:".into(),
                    initial: keystore::free_name(&self.dir, &stem),
                    ok_label: "Import".into(),
                    validator: keystore::check_name,
                })];
            }
            Action::ImportNameAnswered(None) => self.importing = None,
            Action::ImportNameAnswered(Some(name)) => {
                let Some(file) = self.importing.take() else {
                    return Vec::new();
                };
                let r = keystore::import(&self.dir, &file, &name);
                self.reload();
                return vec![Effect::Message(match r {
                    Ok(k) => format!("Imported {} ({}).", k.name, k.kind_label()),
                    Err(e) => format!("Could not import {}: {e:#}", file.display()),
                })];
            }
            Action::Export(name) => {
                if self.get(&name).is_none() {
                    return Vec::new();
                }
                self.exporting = Some(name);
                return vec![Effect::PickFolder(PickSpec {
                    purpose: "keys.export",
                    title: "Export the key to this folder".into(),
                    start: dirs::document_dir(),
                })];
            }
            Action::ExportPicked(folder) => {
                let (Some(name), Some(folder)) = (self.exporting.take(), folder) else {
                    return Vec::new();
                };
                let Some(k) = self.get(&name) else {
                    return Vec::new();
                };
                return vec![Effect::Message(match keystore::export(k, &folder) {
                    Ok(files) => format!(
                        "Exported {name} to {}: {}. The private key has no passphrase; keep it private.",
                        folder.display(),
                        files
                            .iter()
                            .filter_map(|f| f.file_name())
                            .map(|f| f.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Err(e) => format!("Could not export {name}: {e:#}"),
                })];
            }
            Action::CopyPublic(name) => {
                if let Some(k) = self.get(&name) {
                    return vec![Effect::CopyToClipboard(k.openssh_line())];
                }
            }
            Action::Delete(name) => {
                let Some(k) = self.get(&name) else {
                    return Vec::new();
                };
                let users = machines.using_key(&k.path);
                let tail = if users.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\n{} log in with it and will ask for a password again.",
                        users.join(", ")
                    )
                };
                self.deleting = Some(name.clone());
                return vec![Effect::Confirm(ConfirmSpec {
                    purpose: "keys.delete",
                    title: "Delete key".into(),
                    message: format!(
                        "Delete the key {name}? Machines it was installed on keep it in their authorized keys.{tail}"
                    ),
                    confirm_label: "Delete".into(),
                    destructive: true,
                })];
            }
            Action::DeleteConfirmed(yes) => {
                let Some(name) = self.deleting.take().filter(|_| yes) else {
                    return Vec::new();
                };
                let Some(k) = self.get(&name).cloned() else {
                    return Vec::new();
                };
                let r = keystore::delete(&k)
                    .map_err(|e| format!("{e:#}"))
                    .and_then(|()| machines.drop_key(&k.path));
                self.reload();
                if let Err(e) = r {
                    return vec![Effect::Message(format!("Could not delete {name}: {e}"))];
                }
            }
        }
        Vec::new()
    }

    /// Collect a finished key; a message when it is ready or failed.
    pub fn tick(&mut self) -> Vec<Effect> {
        let Some(r) = self.making.as_ref().and_then(super::job_outcome) else {
            return Vec::new();
        };
        self.making = None;
        self.reload();
        match r {
            Ok(k) => {
                self.notes
                    .push(format!("Made the key {} ({}).", k.name, k.kind_label()));
                Vec::new()
            }
            Err(e) => vec![Effect::Message(format!("Could not make the key: {e}"))],
        }
    }
}
