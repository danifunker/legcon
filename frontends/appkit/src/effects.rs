//! The effects only a shell can carry out: open panels, alerts, the one-line prompt (with
//! Browse... and recent places for a path), popup menus, the pasteboard. Each returns its answer;
//! `window::run` turns it into the controller's action by the effect's purpose.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use legcon::controller::{picks, MenuItem};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadMarker};
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSColor, NSEvent, NSMenu,
    NSMenuItem, NSModalResponseOK, NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSView,
};
use objc2_foundation::{NSPoint, NSURL};

use crate::dialog::Dialog;
use crate::table::{Icon, Line, Table};
use crate::ui::{self, ns};

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&ns(&path.to_string_lossy()))
}

fn open_panel(
    mtm: MainThreadMarker,
    title: &str,
    start: Option<&PathBuf>,
    folder: bool,
) -> Option<PathBuf> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setMessage(Some(&ns(title)));
    panel.setCanChooseFiles(!folder);
    panel.setCanChooseDirectories(folder);
    panel.setCanCreateDirectories(folder);
    panel.setAllowsMultipleSelection(false);
    // Key files live in ~/.ssh, which the panel hides unless asked.
    panel.setShowsHiddenFiles(true);
    if let Some(dir) = start {
        panel.setDirectoryURL(Some(&file_url(dir)));
    }
    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    panel
        .URLs()
        .firstObject()
        .and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))
}

/// The folder picker; a flow's scripted answer when one is set (`LEGCON_TEST_PICKS`).
pub fn pick_folder(
    mtm: MainThreadMarker,
    purpose: &str,
    title: &str,
    start: Option<&PathBuf>,
) -> Option<PathBuf> {
    if let Some(answer) = picks::scripted(purpose) {
        return answer;
    }
    open_panel(mtm, title, start, true)
}

pub fn pick_file(
    mtm: MainThreadMarker,
    purpose: &str,
    title: &str,
    start: Option<&PathBuf>,
) -> Option<PathBuf> {
    if let Some(answer) = picks::scripted(purpose) {
        return answer;
    }
    open_panel(mtm, title, start, false)
}

fn alert(mtm: MainThreadMarker, title: &str, message: &str) -> Retained<NSAlert> {
    let a = NSAlert::new(mtm);
    // The app's own icon: an unbundled build (the flows) would show a folder.
    let icon = objc2_app_kit::NSApplication::sharedApplication(mtm).applicationIconImage();
    // SAFETY: the alert keeps its own reference to the image.
    unsafe { a.setIcon(icon.as_deref()) };
    a.setMessageText(&ns(&ui::plain(title)));
    a.setInformativeText(&ns(&ui::plain(message)));
    a
}

pub fn confirm(
    mtm: MainThreadMarker,
    title: &str,
    message: &str,
    label: &str,
    destructive: bool,
) -> bool {
    let a = alert(mtm, title, message);
    let yes = a.addButtonWithTitle(&ns(label));
    a.addButtonWithTitle(&ns("Cancel"));
    if destructive {
        a.setAlertStyle(NSAlertStyle::Critical);
        yes.setHasDestructiveAction(true);
    }
    a.runModal() == NSAlertFirstButtonReturn
}

/// One of `choices`; Esc (or nothing chosen) gives `dismiss`.
pub fn choose(
    mtm: MainThreadMarker,
    title: &str,
    message: &str,
    choices: &[String],
    dismiss: usize,
) -> usize {
    let a = alert(mtm, title, message);
    for c in choices {
        a.addButtonWithTitle(&ns(c));
    }
    if dismiss > 0 {
        if let Some(b) = a.buttons().iter().nth(dismiss) {
            b.setKeyEquivalent(&ns("\u{1b}"));
        }
    }
    let i = (a.runModal() - NSAlertFirstButtonReturn).max(0) as usize;
    if i < choices.len() {
        i
    } else {
        dismiss
    }
}

pub fn note(mtm: MainThreadMarker, title: &str, message: &str) {
    let a = alert(mtm, title, message);
    a.addButtonWithTitle(&ns("OK"));
    a.runModal();
}

pub fn copy_text(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    // SAFETY: AppKit's own pasteboard type constant.
    pb.setString_forType(&ns(&ui::plain(text)), unsafe { NSPasteboardTypeString });
}

/// A controller's rule for a prompt's text.
type Validator = fn(&str) -> Result<(), String>;

/// What a prompt asks for.
pub struct Ask<'a> {
    pub title: &'a str,
    pub prompt: &'a str,
    pub initial: &'a str,
    pub ok: &'a str,
    pub secret: bool,
    /// Recent places to pick from, and a Browse... button, for a path prompt.
    pub recent: Option<&'a [PathBuf]>,
    pub hint: &'a str,
    pub validator: Option<Validator>,
    pub purpose: &'a str,
}

/// The one-line prompt (ids `prompt.*`, as on Win32); the text entered, or `None` for Cancel.
pub fn ask(mtm: MainThreadMarker, a: &Ask) -> Option<String> {
    let w = 460.0;
    let m = 14.0;
    let row = 24.0;
    let recent_h = if a.recent.is_some() { 110.0 } else { 0.0 };
    let hint_h = if a.hint.is_empty() { 0.0 } else { 20.0 };
    let h = m + 20.0 + 6.0 + row + 8.0 + recent_h + hint_h + 20.0 + 12.0 + 30.0 + 12.0;
    let d = Dialog::new(mtm, a.title, w, h);
    let mut y = m;
    let label = ui::label(mtm, a.prompt);
    d.body.addSubview(&label);
    ui::place(&label, (m, y, w - 2.0 * m, 20.0));
    y += 26.0;
    let field = ui::field(mtm, a.initial, "prompt.text", a.secret);
    field.setAccessibilityLabel(Some(&ns(&ui::plain(a.prompt))));
    d.body.addSubview(&field);
    let field_w = if a.recent.is_some() {
        w - 3.0 * m - 96.0
    } else {
        w - 2.0 * m
    };
    ui::place(&field, (m, y, field_w, row));
    let recent: Option<Rc<Table>> = a.recent.map(|places| {
        let f = field.clone();
        let purpose = a.purpose.to_string();
        let b = d.button(
            "Browse...",
            "prompt.browse",
            Box::new(move || {
                let text = f.stringValue().to_string();
                let start = (!text.trim().is_empty()).then(|| PathBuf::from(text.trim()));
                if let Some(p) = pick_folder(mtm, &purpose, "Choose a folder", start.as_ref()) {
                    f.setStringValue(&ns(&p.to_string_lossy()));
                    // A pick from the panel is taken as typed.
                    // SAFETY: an NSString is a valid accessibility value.
                    let v: Retained<AnyObject> =
                        Retained::into_super(Retained::into_super(ns(&p.to_string_lossy())));
                    unsafe { f.setAccessibilityValue(Some(&v)) };
                }
            }),
        );
        ui::place(&b, (w - m - 96.0, y - 2.0, 96.0, row + 4.0));
        let t = Rc::new(Table::new(
            mtm,
            "prompt.recent",
            &[("Recent places", 400)],
            false,
        ));
        t.set_rows(
            places
                .iter()
                .map(|p| Line {
                    icon: Icon::File {
                        name: String::new(),
                        dir: true,
                    },
                    cells: vec![p.display().to_string()],
                })
                .collect(),
        );
        d.body.addSubview(&t.scroll);
        ui::place(&t.scroll, (m, y + row + 8.0, w - 2.0 * m, recent_h - 8.0));
        t
    });
    y += row + 8.0 + recent_h;
    if !a.hint.is_empty() {
        let hint = ui::label(mtm, a.hint);
        hint.setTextColor(Some(&NSColor::secondaryLabelColor()));
        d.body.addSubview(&hint);
        ui::place(&hint, (m, y, w - 2.0 * m, 18.0));
        y += hint_h;
    }
    let error = ui::label(mtm, "");
    error.setTextColor(Some(&NSColor::systemRedColor()));
    d.body.addSubview(&error);
    ui::place(&error, (m, y, w - 2.0 * m, 18.0));

    let validator = a.validator;
    let answer: Rc<RefCell<Option<String>>> = Rc::default();
    let ok = {
        let (field, answer) = (field.clone(), answer.clone());
        Rc::new(move || {
            let text = field.stringValue().to_string();
            if validator.is_some_and(|v| v(&text).is_err()) {
                return false;
            }
            *answer.borrow_mut() = Some(text);
            true
        })
    };
    let okb = d.ok_cancel(a.ok, "prompt.ok", "prompt.cancel", ok);
    let check: Rc<dyn Fn()> = {
        let (field, error, okb) = (field.clone(), error.clone(), okb.clone());
        Rc::new(move || {
            let verdict = validator.map_or(Ok(()), |v| v(&field.stringValue().to_string()));
            ui::set_text(&error, verdict.as_ref().err().map_or("", |e| e.as_str()));
            ui::set_enabled(&okb, verdict.is_ok());
        })
    };
    ui::on_change(&field, check.clone());
    if let Some(t) = &recent {
        // A click on a recent place puts it in the box; a double-click goes there.
        let places: Vec<String> = a
            .recent
            .unwrap_or_default()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let (tt, f, c) = (t.clone(), field.clone(), check.clone());
        let pick: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(p) = tt.selected().and_then(|i| places.get(i)) {
                f.setStringValue(&ns(p));
                c();
            }
        });
        t.on_select(pick.clone());
        let ok_now = okb.clone();
        t.on_activate(Rc::new(move || {
            pick();
            // SAFETY: a plain click on the dialog's own button.
            unsafe { ok_now.performClick(None) };
        }));
    }
    check();
    if d.run(Some(&field)) {
        answer.take()
    } else {
        None
    }
}

thread_local! {
    static PICKED: std::cell::Cell<Option<isize>> = const { std::cell::Cell::new(None) };
}

/// One line of a popup menu the shell builds itself.
pub enum Entry {
    Item {
        label: String,
        enabled: bool,
        checked: bool,
        /// A shortcut shown beside the label: the key equivalent and whether it takes Cmd.
        key: Option<(&'static str, bool)>,
    },
    Heading(String),
    Separator,
}

impl Entry {
    pub fn item(label: impl Into<String>, enabled: bool) -> Entry {
        Entry::Item {
            label: label.into(),
            enabled,
            checked: false,
            key: None,
        }
    }
}

/// The key equivalents of the shortcuts menus show.
pub const F2: &str = "\u{F705}";
pub const F5: &str = "\u{F708}";
pub const F6: &str = "\u{F709}";
pub const DEL: &str = "\u{F728}";

/// Pop up a controller menu under `anchor` (or at the mouse); the index chosen, `None` when dismissed.
pub fn menu(mtm: MainThreadMarker, anchor: Option<&NSView>, items: &[MenuItem]) -> Option<usize> {
    let entries: Vec<Entry> = items
        .iter()
        .map(|i| match i {
            MenuItem::Item { label, enabled } => Entry::item(label.clone(), *enabled),
            MenuItem::Heading(l) => Entry::Heading(l.clone()),
            MenuItem::Separator => Entry::Separator,
        })
        .collect();
    pop(mtm, anchor, &entries)
}

/// Pop up `entries` under `anchor` (or at the mouse) and wait; the index chosen.
pub fn pop(mtm: MainThreadMarker, anchor: Option<&NSView>, entries: &[Entry]) -> Option<usize> {
    let target = ui::callback(
        mtm,
        Box::new(|sender: &AnyObject| {
            if let Some(item) = sender.downcast_ref::<NSMenuItem>() {
                PICKED.with(|p| p.set(Some(item.tag())));
            }
        }),
    );
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for (i, e) in entries.iter().enumerate() {
        let it = match e {
            Entry::Separator => NSMenuItem::separatorItem(mtm),
            Entry::Item {
                label,
                enabled,
                checked,
                key,
            } => {
                // SAFETY: a plain menu item; its action goes to `target`, kept until the menu closes.
                let it = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        mtm.alloc(),
                        &ns(&ui::plain(label)),
                        Some(sel!(fire:)),
                        &ns(key.map_or("", |k| k.0)),
                    )
                };
                if let Some((_, cmd)) = key {
                    it.setKeyEquivalentModifierMask(if *cmd {
                        objc2_app_kit::NSEventModifierFlags::Command
                    } else {
                        objc2_app_kit::NSEventModifierFlags::empty()
                    });
                }
                // SAFETY: as above.
                unsafe { it.setTarget(Some(&target)) };
                it.setTag(i as isize);
                it.setEnabled(*enabled);
                if *checked {
                    it.setState(objc2_app_kit::NSControlStateValueOn);
                }
                it
            }
            Entry::Heading(label) => {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&ns(&ui::plain(label)));
                it.setEnabled(false);
                it
            }
        };
        menu.addItem(&it);
    }
    PICKED.with(|p| p.set(None));
    match anchor {
        Some(a) => {
            let b = a.bounds();
            let y = if a.isFlipped() {
                b.size.height + 2.0
            } else {
                -2.0
            };
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0.0, y), Some(a));
        }
        None => match crate::window::main_window().and_then(|w| w.contentView().map(|v| (w, v))) {
            // At the mouse, but in the window's content view, so accessibility lists the menu.
            Some((w, v)) => {
                let at = w.convertPointFromScreen(NSEvent::mouseLocation());
                let at = v.convertPoint_fromView(at, None);
                menu.popUpMenuPositioningItem_atLocation_inView(None, at, Some(&v));
            }
            None => {
                menu.popUpMenuPositioningItem_atLocation_inView(
                    None,
                    NSEvent::mouseLocation(),
                    None,
                );
            }
        },
    }
    drop(target);
    PICKED.with(std::cell::Cell::take).map(|p| p as usize)
}
