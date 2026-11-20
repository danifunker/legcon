//! The app's Options form: settings that are not about one machine (today, the transfer queue).

use super::{ids, Field, FieldKind, FormSpec, FormValues};
use crate::model::queue::OnExisting;
use crate::model::settings::{Auto, Settings};

/// The Options choices for files already there, in the order the list shows them.
const ON_EXISTING: [(OnExisting, &str); 5] = [
    (OnExisting::Ask, "Ask"),
    (OnExisting::Overwrite, "Overwrite"),
    (OnExisting::IfDifferent, "Replace if different"),
    (OnExisting::Resume, "Resume where possible, else overwrite"),
    (OnExisting::Skip, "Skip"),
];

pub const PURPOSE: &str = "app.options";

/// The order the reconnect and queue choices are listed in.
pub const AUTO: [Auto; 3] = [Auto::Always, Auto::Ask, Auto::Never];
pub const RECONNECT_LABELS: [&str; 3] = [
    "Connect again when I use the machine",
    "Ask before connecting again",
    "Only show Reconnect",
];
pub const RESUME_LABELS: [&str; 3] = [
    "Go on with them",
    "Ask whether to go on",
    "Wait for Start queue",
];
pub const RECONNECT_LABEL: &str = "When a connection was dropped or logged off";
pub const RECONNECT_HINT: &str =
    "Going to a folder, refreshing or copying on a machine whose server dropped the link, or that \
     legcon logged off while idle, connects again and carries on.";
pub const RESUME_LABEL: &str = "After connecting again, unfinished transfers";
pub const KEEPALIVE_LABEL: &str = "Keep idle connections up, pinging every (seconds)";
pub const KEEPALIVE_HINT: &str =
    "0 lets servers drop idle connections; they come back when used. Shells are kept up too.";
pub const IDLE_LABEL: &str = "Log off machines unused for (minutes)";
pub const IDLE_HINT: &str = "0 never does. A running transfer queue keeps its machine connected.";

/// The list index of `a` in [`AUTO`].
pub fn auto_index(a: Auto) -> usize {
    AUTO.iter().position(|x| *x == a).unwrap_or(0)
}

/// A whole number of seconds or minutes, 0 or more.
pub fn count(text: &str) -> Result<u64, String> {
    text.trim()
        .parse::<u64>()
        .map_err(|_| "A whole number, 0 or more.".to_string())
}

fn ask_over(text: &str) -> Result<usize, String> {
    text.trim()
        .parse::<usize>()
        .map_err(|_| "A whole number, 0 or more.".to_string())
}

fn validate(v: &FormValues) -> Result<(), String> {
    ask_over(v.text(ids::OPT_QUEUE_ASK_OVER))?;
    count(v.text(ids::OPT_KEEPALIVE))?;
    count(v.text(ids::OPT_IDLE_LOGOFF)).map(drop)
}

fn labels(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| s.to_string()).collect()
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
            Field {
                id: ids::OPT_SECTION_LINK,
                label: "Connections".into(),
                kind: FieldKind::Heading,
                hint: String::new(),
            },
            Field {
                id: ids::OPT_RECONNECT,
                label: RECONNECT_LABEL.into(),
                kind: FieldKind::Choice {
                    options: labels(&RECONNECT_LABELS),
                    selected: auto_index(s.reconnect),
                },
                hint: RECONNECT_HINT.into(),
            },
            Field {
                id: ids::OPT_RESUME_QUEUE,
                label: RESUME_LABEL.into(),
                kind: FieldKind::Choice {
                    options: labels(&RESUME_LABELS),
                    selected: auto_index(s.resume_queue),
                },
                hint: String::new(),
            },
            Field {
                id: ids::OPT_KEEPALIVE,
                label: KEEPALIVE_LABEL.into(),
                kind: FieldKind::Text(s.keepalive_secs.to_string()),
                hint: KEEPALIVE_HINT.into(),
            },
            Field {
                id: ids::OPT_IDLE_LOGOFF,
                label: IDLE_LABEL.into(),
                kind: FieldKind::Text(s.idle_logoff_mins.to_string()),
                hint: IDLE_HINT.into(),
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
    s.reconnect = AUTO
        .get(v.choice(ids::OPT_RECONNECT))
        .copied()
        .unwrap_or_default();
    s.resume_queue = AUTO
        .get(v.choice(ids::OPT_RESUME_QUEUE))
        .copied()
        .unwrap_or(Auto::Ask);
    s.keepalive_secs = count(v.text(ids::OPT_KEEPALIVE))?;
    s.idle_logoff_mins = count(v.text(ids::OPT_IDLE_LOGOFF))?;
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
        assert_eq!(v.choice(ids::OPT_RESUME_QUEUE), 1, "asks by default");
        v.set(ids::OPT_RECONNECT, FieldValue::Choice(2));
        v.set(ids::OPT_KEEPALIVE, FieldValue::Text("x".into()));
        assert!((spec.validator)(&v).is_err());
        v.set(ids::OPT_KEEPALIVE, FieldValue::Text("60".into()));
        v.set(ids::OPT_IDLE_LOGOFF, FieldValue::Text("15".into()));
        apply(&mut s, &v).unwrap();
        assert_eq!(s.reconnect, Auto::Never);
        assert_eq!((s.keepalive_secs, s.idle_logoff_mins), (60, 15));
    }
}
