//! The dialog that draws any controller form (`Effect::Form`): a tab per section, label left,
//! control right, hints under them, the whole-form rule run as the user edits. Ids as on Win32:
//! a field's id is its control's, `<purpose>.sections` / `.ok` / `.cancel` / `<field>.browse`.

use std::path::PathBuf;
use std::rc::Rc;

use legcon::controller::{FieldKind, FieldValue, FormSpec, FormValues};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSPopUpButton, NSTabView, NSTabViewItem, NSTextField, NSView,
};

use crate::dialog::Dialog;
use crate::effects::pick_folder;
use crate::ui::{self, ns};

#[derive(Clone)]
enum Control {
    Text(Retained<NSTextField>),
    Check(Retained<NSButton>),
    Choice(Retained<NSPopUpButton>),
    Info,
}

struct Row {
    id: &'static str,
    control: Control,
}

fn values(rows: &[Row]) -> FormValues {
    FormValues(
        rows.iter()
            .map(|r| {
                let v = match &r.control {
                    Control::Text(f) => FieldValue::Text(f.stringValue().to_string()),
                    Control::Check(b) => FieldValue::Check(b.state() == NSControlStateValueOn),
                    Control::Choice(p) => {
                        FieldValue::Choice(p.indexOfSelectedItem().max(0) as usize)
                    }
                    Control::Info => FieldValue::None,
                };
                (r.id, v)
            })
            .collect(),
    )
}

const W: f64 = 580.0;
const M: f64 = 14.0;
const LABEL_W: f64 = 170.0;
const ROW_H: f64 = 24.0;
const HINT_H: f64 = 30.0;

/// Show the form modally; the values when saved, `None` for Cancel.
pub fn show(mtm: MainThreadMarker, spec: &FormSpec) -> Option<FormValues> {
    // Sections become tabs; a form without headings is one page.
    let mut pages: Vec<(String, Vec<&legcon::controller::Field>)> = Vec::new();
    for f in &spec.fields {
        match (&f.kind, pages.last_mut()) {
            (FieldKind::Heading, _) => pages.push((f.label.clone(), Vec::new())),
            (_, Some(p)) => p.1.push(f),
            (_, None) => pages.push((String::new(), vec![f])),
        }
    }
    let tabbed = spec.fields.iter().any(|f| f.kind == FieldKind::Heading);
    let page_h = |fields: &[&legcon::controller::Field]| -> f64 {
        fields
            .iter()
            .map(|f| ROW_H + 8.0 + if f.hint.is_empty() { 0.0 } else { HINT_H })
            .sum::<f64>()
            + 12.0
    };
    let content_h = pages.iter().map(|p| page_h(&p.1)).fold(0.0, f64::max);
    let area_h = if tabbed { content_h + 44.0 } else { content_h };
    let h = M + area_h + 10.0 + 20.0 + 10.0 + 30.0 + 12.0;
    let d = Rc::new(Dialog::new(mtm, &spec.title, W, h));

    let tabs = tabbed.then(|| {
        let t = NSTabView::initWithFrame(mtm.alloc(), ui::rect(M, M, W - 2.0 * M, area_h));
        ui::set_id(&t, &format!("{}.sections", spec.purpose));
        d.body.addSubview(&t);
        t
    });
    let mut rows: Vec<Row> = Vec::new();
    let mut checks: Vec<Retained<NSView>> = Vec::new();
    let mut first: Option<Retained<NSView>> = None;
    let page_w = W - 2.0 * M - if tabbed { 20.0 } else { 0.0 };
    for (title, fields) in &pages {
        let page = ui::flipped(mtm);
        page.setFrame(ui::rect(0.0, 0.0, page_w, content_h));
        let control_x = 8.0 + LABEL_W;
        let control_w = page_w - control_x - 8.0;
        let mut y = 8.0;
        for f in fields {
            let label = |text: &str| {
                let l = ui::label(mtm, text);
                l.setAlignment(objc2_app_kit::NSTextAlignment::Right);
                page.addSubview(&l);
                ui::place(&l, (8.0, y + 3.0, LABEL_W - 10.0, 18.0));
            };
            let control = match &f.kind {
                FieldKind::Heading => unreachable!(),
                FieldKind::Text(t) | FieldKind::Secret(t) | FieldKind::Folder(t) => {
                    label(&f.label);
                    let secret = matches!(f.kind, FieldKind::Secret(_));
                    let field = ui::field(mtm, t, f.id, secret);
                    field.setAccessibilityLabel(Some(&ns(&f.label)));
                    page.addSubview(&field);
                    let browse = matches!(f.kind, FieldKind::Folder(_));
                    let fw = if browse { control_w - 100.0 } else { control_w };
                    ui::place(&field, (control_x, y, fw, ROW_H));
                    if browse {
                        let (fld, purpose) = (field.clone(), spec.purpose);
                        let b = d.button(
                            "Browse...",
                            &format!("{}.browse", f.id),
                            Box::new(move || {
                                let start = PathBuf::from(fld.stringValue().to_string());
                                let start = start.is_dir().then_some(start);
                                if let Some(p) =
                                    pick_folder(mtm, purpose, "Choose a folder", start.as_ref())
                                {
                                    ui::set_text(&fld, &p.display().to_string());
                                    // Run the form's rule, as typing would.
                                    let v: Retained<objc2::runtime::AnyObject> =
                                        Retained::into_super(Retained::into_super(ns(&p
                                            .display()
                                            .to_string())));
                                    // SAFETY: an NSString is a valid accessibility value.
                                    unsafe { fld.setAccessibilityValue(Some(&v)) };
                                }
                            }),
                        );
                        b.removeFromSuperview();
                        page.addSubview(&b);
                        ui::place(
                            &b,
                            (control_x + control_w - 94.0, y - 2.0, 94.0, ROW_H + 4.0),
                        );
                    }
                    if first.is_none() {
                        first = Some(Retained::into_super(Retained::into_super(field.clone())));
                    }
                    Control::Text(field)
                }
                FieldKind::Info(t) => {
                    label(&f.label);
                    let l = ui::label(mtm, t);
                    l.setSelectable(true);
                    ui::set_id(&l, f.id);
                    page.addSubview(&l);
                    ui::place(&l, (control_x, y + 3.0, control_w, 18.0));
                    Control::Info
                }
                FieldKind::Check(on) => {
                    let cb = d.callback(Box::new(|_| {}));
                    // SAFETY: the dialog keeps the callback as long as the box.
                    let b = unsafe {
                        NSButton::checkboxWithTitle_target_action(
                            &ns(&f.label),
                            Some(&cb),
                            Some(objc2::sel!(fire:)),
                            mtm,
                        )
                    };
                    b.setState(if *on {
                        NSControlStateValueOn
                    } else {
                        NSControlStateValueOff
                    });
                    ui::set_id(&b, f.id);
                    page.addSubview(&b);
                    ui::place(&b, (control_x, y + 2.0, control_w, 20.0));
                    checks.push(Retained::into_super(Retained::into_super(b.clone())));
                    Control::Check(b)
                }
                FieldKind::Choice { options, selected } => {
                    label(&f.label);
                    let p = NSPopUpButton::initWithFrame_pullsDown(
                        mtm.alloc(),
                        ui::rect(0.0, 0.0, control_w, ROW_H),
                        false,
                    );
                    for o in options {
                        p.addItemWithTitle(&ns(o));
                    }
                    p.selectItemAtIndex(*selected as isize);
                    ui::set_id(&p, f.id);
                    page.addSubview(&p);
                    ui::place(&p, (control_x - 3.0, y - 1.0, control_w + 6.0, ROW_H + 2.0));
                    Control::Choice(p)
                }
            };
            y += ROW_H + 8.0;
            if !f.hint.is_empty() {
                let hint = ui::label(mtm, &f.hint);
                hint.setTextColor(Some(&NSColor::secondaryLabelColor()));
                hint.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(
                    objc2_app_kit::NSFont::smallSystemFontSize(),
                )));
                hint.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByWordWrapping);
                hint.setMaximumNumberOfLines(2);
                page.addSubview(&hint);
                ui::place(&hint, (control_x, y - 6.0, control_w, HINT_H));
                y += HINT_H;
            }
            rows.push(Row { id: f.id, control });
        }
        match &tabs {
            Some(t) => {
                let id: Retained<objc2::runtime::AnyObject> =
                    Retained::into_super(Retained::into_super(ns(title)));
                // SAFETY: an NSString identifier, kept by the item.
                let item = unsafe { NSTabViewItem::initWithIdentifier(mtm.alloc(), Some(&id)) };
                item.setLabel(&ns(title));
                item.setView(Some(&page));
                t.addTabViewItem(&item);
            }
            None => {
                d.body.addSubview(&page);
                ui::place(&page, (M, M, page_w, content_h));
            }
        }
    }

    let error = ui::label(mtm, "");
    error.setTextColor(Some(&NSColor::systemRedColor()));
    d.body.addSubview(&error);
    ui::place(&error, (M, M + area_h + 8.0, W - 2.0 * M, 20.0));

    let rows = Rc::new(rows);
    let validator = spec.validator;
    let answer: Rc<std::cell::RefCell<Option<FormValues>>> = Rc::default();
    let ok = {
        let (rows, answer) = (rows.clone(), answer.clone());
        Rc::new(move || {
            let v = values(&rows);
            if validator(&v).is_err() {
                return false;
            }
            *answer.borrow_mut() = Some(v);
            true
        })
    };
    let okb = d.ok_cancel(
        &spec.ok_label,
        &format!("{}.ok", spec.purpose),
        &format!("{}.cancel", spec.purpose),
        ok,
    );
    let check: Rc<dyn Fn()> = {
        let (rows, error, okb) = (rows.clone(), error.clone(), okb.clone());
        Rc::new(move || {
            let verdict = validator(&values(&rows));
            ui::set_text(&error, verdict.as_ref().err().map_or("", |e| e.as_str()));
            ui::set_enabled(&okb, verdict.is_ok());
        })
    };
    for r in rows.iter() {
        match &r.control {
            Control::Text(f) => ui::on_change(f, check.clone()),
            Control::Check(b) => {
                let c = check.clone();
                let cb = d.callback(Box::new(move |_| c()));
                ui::hook(b, &cb);
            }
            Control::Choice(p) => {
                let c = check.clone();
                let cb = d.callback(Box::new(move |_| c()));
                ui::hook(p, &cb);
            }
            Control::Info => {}
        }
    }
    drop(checks);
    check();
    if d.run(first.as_deref()) {
        answer.take()
    } else {
        None
    }
}
