//! The app's Options form: settings that are not about one machine (today, the transfer queue).

use super::{ids, Field, FieldKind, FormSpec, FormValues};
use crate::model::queue::OnExisting;
use crate::model::settings::Settings;

/// The Options choices for files already there, in the order the list shows them.
const ON_EXISTING: [(OnExisting, &str); 5] = [
    (OnExisting::Ask, "Ask"),
    (OnExisting::Overwrite, "Overwrite"),
    (OnExisting::IfDifferent, "Replace if different"),
    (OnExisting::Resume, "Resume where possible, else overwrite"),
    (OnExisting::Skip, "Skip"),
];

pub const PURPOSE: &str = "app.options";

fn ask_over(text: &str) -> Result<usize, String> {
    text.trim()
        .parse::<usize>()
        .map_err(|_| "A whole number, 0 or more.".to_string())
}

fn validate(v: &FormValues) -> Result<(), String> {
    ask_over(v.text(ids::OPT_QUEUE_ASK_OVER)).map(drop)
}

pub fn form(s: &Settings) -> FormSpec {
    FormSpec {
        purpose: PURPOSE,
        title: "Options".into(),
        fields: vec![
            Field {
                id: ids::OPT_SECTION_QUEUE,
                label: "Transfer queue".into(),
                kind: FieldKind::Heading,
                hint: String::new(),
            },
            Field {
                id: ids::OPT_QUEUE_ASK_OVER,
                label: "Ask before adding more files than".into(),
                kind: FieldKind::Text(s.queue_ask_over.to_string()),
                hint: "0 never asks.".into(),
            },
            Field {
                id: ids::OPT_ON_EXISTING,
                label: "When a file is already there".into(),
                kind: FieldKind::Choice {
                    options: ON_EXISTING.iter().map(|o| o.1.to_string()).collect(),
                    selected: ON_EXISTING
                        .iter()
                        .position(|o| o.0 == s.on_existing)
                        .unwrap_or(0),
                },
                hint: "For copies to and from machines and files dropped on this computer's pane."
                    .into(),
            },
            Field {
                id: ids::OPT_CHECKSUM,
                label: "Check copies with a checksum".into(),
                kind: FieldKind::Check(s.verify_checksum),
                hint: "Where the machine can compute one (cksum, XCRC); every copy is checked by size."
                    .into(),
            },
        ],
        ok_label: "Save".into(),
        validator: validate,
    }
}

/// Take the form's answer into `s`.
pub fn apply(s: &mut Settings, v: &FormValues) -> Result<(), String> {
    s.queue_ask_over = ask_over(v.text(ids::OPT_QUEUE_ASK_OVER))?;
    s.verify_checksum = v.check(ids::OPT_CHECKSUM);
    s.on_existing = ON_EXISTING
        .get(v.choice(ids::OPT_ON_EXISTING))
        .map_or(OnExisting::Ask, |o| o.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::FieldValue;

    #[test]
    fn round_trip() {
        let mut s = Settings::default();
        let spec = form(&s);
        let mut v = FormValues(spec.fields.iter().map(|f| (f.id, f.kind.value())).collect());
        assert_eq!(v.text(ids::OPT_QUEUE_ASK_OVER), "100");
        v.set(ids::OPT_QUEUE_ASK_OVER, FieldValue::Text("abc".into()));
        assert!((spec.validator)(&v).is_err());
        v.set(ids::OPT_QUEUE_ASK_OVER, FieldValue::Text(" 10 ".into()));
        v.set(ids::OPT_CHECKSUM, FieldValue::Check(false));
        assert_eq!(v.choice(ids::OPT_ON_EXISTING), 0);
        v.set(ids::OPT_ON_EXISTING, FieldValue::Choice(2));
        apply(&mut s, &v).unwrap();
        assert_eq!((s.queue_ask_over, s.verify_checksum), (10, false));
        assert_eq!(s.on_existing, OnExisting::IfDifferent);
        assert_eq!(form(&s).fields[2].kind.value(), FieldValue::Choice(2));
    }
}
