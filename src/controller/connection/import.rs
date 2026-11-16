//! Files dropped from the OS onto the local pane: the transfer queue's "already there" choices,
//! asked before the copy starts (every clash is known here), then one local copy job.

use std::path::PathBuf;

use super::transfers::{label, ItemAnswer, RENAME_PURPOSE};
use super::{ConnectionController, Pending, Side};
use crate::controller::{ChoiceSpec, Effect, TextSpec};
use crate::model::local::{self, Choices, Existing};
use crate::model::queue::{numbered, OnExisting};

/// Where the questions about a local copy have got to.
pub(super) enum Stage {
    /// The batch question; what each button means (`None`: cancel).
    Batch(Vec<Option<OnExisting>>),
    /// The question about `clashes[at]`.
    Item(Vec<ItemAnswer>),
    /// The new name for `clashes[at]`.
    Rename,
}

pub(super) struct ImportAsk {
    paths: Vec<PathBuf>,
    /// Target files already there, in the order they are asked about.
    clashes: Vec<PathBuf>,
    at: usize,
    /// Asking for each (`Ask`) or naming each (`Rename`).
    each: OnExisting,
    choices: Choices,
    pub(super) stage: Stage,
}

const IF_SAME: &str = "Replace if different leaves a file that is the same (same size and \
contents).";

fn existing(on: OnExisting) -> Existing {
    match on {
        OnExisting::IfDifferent => Existing::IfDifferent,
        OnExisting::Skip => Existing::Skip,
        _ => Existing::Overwrite,
    }
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl ImportAsk {
    /// Whether a copy named `name` would land on a file: there already, or another rename's.
    fn taken(&self, name: &str) -> bool {
        let Some(dst) = self.clashes.get(self.at) else {
            return false;
        };
        let new = dst.with_file_name(name);
        new.exists()
            || self
                .choices
                .each
                .iter()
                .any(|(d, e)| matches!(e, Existing::Rename(n) if d.with_file_name(n) == new))
    }
}

impl ConnectionController {
    /// Files dropped onto the local pane, not already in its folder.
    pub(super) fn import(&mut self, paths: Vec<PathBuf>) -> Vec<Effect> {
        let clashes: Vec<PathBuf> = local::clashes(&paths, &self.local_dir)
            .into_iter()
            .map(|(_, dst)| dst)
            .collect();
        let preset = self.xfer.prefs.on_existing;
        if clashes.is_empty() || !matches!(preset, OnExisting::Ask | OnExisting::Rename) {
            self.start_import(paths, Choices::all(existing(preset)));
            return Vec::new();
        }
        let many = clashes.len() > 1;
        let first = file_name(&clashes[0]);
        let mut message = if many {
            format!(
                "{} of these are already in {}, such as {first}.",
                clashes.len(),
                self.local_dir.display()
            )
        } else {
            format!("{first} is already in {}.", self.local_dir.display())
        };
        message.push(' ');
        message += IF_SAME;
        let mut on = vec![
            OnExisting::Overwrite,
            OnExisting::IfDifferent,
            OnExisting::Rename,
            OnExisting::Skip,
        ];
        if many {
            on.push(OnExisting::Ask);
        }
        let mut choices: Vec<String> = on.iter().map(|o| label(*o, many).to_string()).collect();
        let mut answers: Vec<Option<OnExisting>> = on.into_iter().map(Some).collect();
        choices.push("Cancel".into());
        answers.push(None);
        let dismiss = choices.len() - 1;
        self.pending = Some(Pending::Import(Box::new(ImportAsk {
            paths,
            clashes,
            at: 0,
            each: OnExisting::Ask,
            choices: Choices::all(Existing::Overwrite),
            stage: Stage::Batch(answers),
        })));
        vec![Effect::Choose(ChoiceSpec {
            purpose: "conn.copy.conflict",
            title: "Copy".into(),
            message,
            choices,
            dismiss,
        })]
    }

    /// The next question, or the copy once every clash has its answer.
    fn import_next(&mut self, mut ask: Box<ImportAsk>) -> Vec<Effect> {
        let Some(dst) = ask.clashes.get(ask.at).cloned() else {
            let ImportAsk { paths, choices, .. } = *ask;
            self.start_import(paths, choices);
            return Vec::new();
        };
        let name = file_name(&dst);
        if ask.each == OnExisting::Rename {
            return self.import_ask_name(ask, None);
        }
        let mut answers = vec![
            ItemAnswer::This(OnExisting::Overwrite),
            ItemAnswer::This(OnExisting::IfDifferent),
            ItemAnswer::This(OnExisting::Rename),
        ];
        let skip = answers.len();
        answers.extend([
            ItemAnswer::This(OnExisting::Skip),
            ItemAnswer::Rest(OnExisting::Overwrite),
            ItemAnswer::Rest(OnExisting::Skip),
        ]);
        let choices = answers
            .iter()
            .map(|a| match a {
                ItemAnswer::This(o) => label(*o, false).to_string(),
                ItemAnswer::Rest(OnExisting::Overwrite) => "Overwrite all".into(),
                ItemAnswer::Rest(_) => "Skip all".into(),
            })
            .collect();
        let folder = dst
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        ask.stage = Stage::Item(answers);
        self.pending = Some(Pending::Import(ask));
        vec![Effect::Choose(ChoiceSpec {
            purpose: "conn.copy.conflict",
            title: "Copy".into(),
            message: format!("{name} is already in {folder}. {IF_SAME}"),
            choices,
            dismiss: skip,
        })]
    }

    fn import_ask_name(&mut self, mut ask: Box<ImportAsk>, taken: Option<&str>) -> Vec<Effect> {
        let dst = ask.clashes[ask.at].clone();
        let name = file_name(&dst);
        let initial = (2..)
            .map(|n| numbered(&name, n))
            .find(|c| !ask.taken(c))
            .expect("some number is free");
        let prompt = match taken {
            Some(n) => format!("{n} is also there. Copy {name} as:"),
            None => format!("{name} is already there. Copy it as:"),
        };
        ask.stage = Stage::Rename;
        self.pending = Some(Pending::Import(ask));
        vec![Effect::AskText(TextSpec {
            purpose: RENAME_PURPOSE,
            title: "Copy under another name".into(),
            prompt,
            initial,
            ok_label: "Copy".into(),
            validator: self.name_rule(Side::Local),
        })]
    }

    /// A button of the batch or per-file question.
    pub(super) fn import_answered(&mut self, choice: usize) -> Vec<Effect> {
        let Some(Pending::Import(mut ask)) = self.pending.take() else {
            return Vec::new();
        };
        match std::mem::replace(&mut ask.stage, Stage::Rename) {
            Stage::Batch(answers) => match answers.get(choice).copied().flatten() {
                None => Vec::new(),
                Some(each @ (OnExisting::Ask | OnExisting::Rename)) => {
                    ask.each = each;
                    self.import_next(ask)
                }
                Some(on) => {
                    let ImportAsk { paths, .. } = *ask;
                    self.start_import(paths, Choices::all(existing(on)));
                    Vec::new()
                }
            },
            Stage::Item(answers) => {
                let dst = ask.clashes[ask.at].clone();
                match answers
                    .get(choice)
                    .copied()
                    .unwrap_or(ItemAnswer::This(OnExisting::Skip))
                {
                    ItemAnswer::This(OnExisting::Rename) => self.import_ask_name(ask, None),
                    ItemAnswer::This(on) => {
                        ask.choices.each.insert(dst, existing(on));
                        ask.at += 1;
                        self.import_next(ask)
                    }
                    ItemAnswer::Rest(on) => {
                        ask.choices.rest = existing(on);
                        ask.at = ask.clashes.len();
                        self.import_next(ask)
                    }
                }
            }
            Stage::Rename => Vec::new(),
        }
    }

    /// The new name for one local copy; no name goes back to the question about that file.
    pub(super) fn import_rename_answered(&mut self, answer: Option<String>) -> Vec<Effect> {
        let Some(Pending::Import(mut ask)) = self.pending.take() else {
            return Vec::new();
        };
        match answer
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
        {
            Some(n) if ask.taken(&n) => self.import_ask_name(ask, Some(&n)),
            Some(n) => {
                let dst = ask.clashes[ask.at].clone();
                ask.choices.each.insert(dst, Existing::Rename(n));
                ask.at += 1;
                self.import_next(ask)
            }
            None => {
                ask.each = OnExisting::Ask;
                self.import_next(ask)
            }
        }
    }
}
