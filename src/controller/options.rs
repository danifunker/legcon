//! The app's Options form: settings that are not about one machine (today, the transfer queue).

use super::{ids, Field, FieldKind, FormSpec, FormValues};
use crate::model::settings::Settings;

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
        apply(&mut s, &v).unwrap();
        assert_eq!((s.queue_ask_over, s.verify_checksum), (10, false));
    }
}
