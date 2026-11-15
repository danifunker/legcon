//! The Shell tab's terminal: the controller's screen drawn cell by cell in Menlo, keys turned into
//! `ShellText` / `ShellKey`, and a Connect button while no session runs. Only Cmd+Shift chords
//! leave it for the key map (Win32: Ctrl+Shift), so 1990s programs get every other key.

use std::cell::RefCell;

use legcon::controller::connection::{self as conn, TermKey, TermScreen, DEFAULT_BG};
use legcon::controller::ids;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityTextAreaRole, NSButton, NSColor, NSEvent, NSEventModifierFlags,
    NSPasteboard, NSPasteboardTypeString, NSView,
};
use objc2_foundation::NSRect;

use crate::ui::{self, ns};

const PAD: f64 = 6.0;

#[derive(Default)]
pub struct TermState {
    screen: Option<TermScreen>,
    message: String,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCTermView"]
    #[ivars = RefCell<TermState>]
    pub struct TermView;

    unsafe impl NSObjectProtocol for TermView {}

    impl TermView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            self.setNeedsDisplay(true);
            true
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            self.setNeedsDisplay(true);
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(w) = self.window() {
                w.makeFirstResponder(Some(self));
            }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let focused = self.window().and_then(|w| w.firstResponder()).is_some_and(|r| {
                std::ptr::eq(&*r as *const _ as *const u8, self as *const _ as *const u8)
            });
            let s = self.ivars().borrow();
            draw(&s, self.bounds(), focused);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if let Some(a) = key_action(event) {
                ui::later(Box::new(move || crate::window::conn_action(a)));
            }
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            let pb = NSPasteboard::generalPasteboard();
            // SAFETY: AppKit's own pasteboard type constant.
            let text = pb.stringForType(unsafe { NSPasteboardTypeString });
            if let Some(t) = text.map(|t| t.to_string()).filter(|t| !t.is_empty()) {
                ui::later(Box::new(move || {
                    crate::window::conn_action(conn::Action::ShellText(t.replace('\n', "\r")))
                }));
            }
        }
    }
);

/// What a key press sends to the machine, if anything.
fn key_action(event: &NSEvent) -> Option<conn::Action> {
    let mods = event.modifierFlags();
    if mods.contains(NSEventModifierFlags::Command) {
        return None;
    }
    let special = match event.keyCode() {
        36 | 76 => Some(TermKey::Enter),
        51 => Some(TermKey::Backspace),
        48 => Some(TermKey::Tab),
        53 => Some(TermKey::Escape),
        126 => Some(TermKey::Up),
        125 => Some(TermKey::Down),
        123 => Some(TermKey::Left),
        124 => Some(TermKey::Right),
        115 => Some(TermKey::Home),
        119 => Some(TermKey::End),
        116 => Some(TermKey::PageUp),
        121 => Some(TermKey::PageDown),
        114 => Some(TermKey::Insert),
        117 => Some(TermKey::Delete),
        122 => Some(TermKey::F(1)),
        120 => Some(TermKey::F(2)),
        99 => Some(TermKey::F(3)),
        118 => Some(TermKey::F(4)),
        96 => Some(TermKey::F(5)),
        97 => Some(TermKey::F(6)),
        98 => Some(TermKey::F(7)),
        100 => Some(TermKey::F(8)),
        101 => Some(TermKey::F(9)),
        109 => Some(TermKey::F(10)),
        103 => Some(TermKey::F(11)),
        111 => Some(TermKey::F(12)),
        _ => None,
    };
    if let Some(k) = special {
        return Some(conn::Action::ShellKey(k));
    }
    if mods.contains(NSEventModifierFlags::Control) {
        let c = event
            .charactersIgnoringModifiers()?
            .to_string()
            .chars()
            .next()?;
        if c.is_ascii_alphabetic() || "@[\\]^_".contains(c) {
            return Some(conn::Action::ShellKey(TermKey::Ctrl(c)));
        }
        return None;
    }
    let text: String = event
        .characters()?
        .to_string()
        .chars()
        // Printable text only; arrows and the like come as private-use characters.
        .filter(|c| *c >= ' ' && *c != '\u{7f}' && !('\u{f700}'..='\u{f8ff}').contains(c))
        .collect();
    (!text.is_empty()).then_some(conn::Action::ShellText(text))
}

/// The size of one character cell.
fn cell() -> (f64, f64) {
    let f = ui::mono();
    let w = ui::text_width("M", &f).max(1.0);
    let h = (f.ascender() - f.descender() + f.leading()).ceil().max(1.0);
    (w, h)
}

fn draw(s: &TermState, b: NSRect, focused: bool) {
    let Some(screen) = &s.screen else {
        ui::fill(b, &NSColor::controlBackgroundColor());
        let font = ui::font();
        let w = ui::text_width(&s.message, &font);
        let x = ((b.size.width - w) / 2.0).max(16.0);
        let r = ui::rect(x, b.size.height / 2.0 - 40.0, b.size.width - x - 16.0, 22.0);
        ui::draw_text(&s.message, r, &font, &NSColor::secondaryLabelColor());
        return;
    };
    ui::fill(b, &ui::rgb(DEFAULT_BG));
    let (cw, ch) = cell();
    let font = ui::mono();
    let bold = objc2_app_kit::NSFontManager::sharedFontManager(
        MainThreadMarker::new().expect("drawn on the main thread"),
    )
    .convertFont_toHaveTrait(&font, objc2_app_kit::NSFontTraitMask::BoldFontMask);
    for (row, runs) in screen.lines.iter().enumerate() {
        let y = PAD + row as f64 * ch;
        let mut col = 0.0;
        for run in runs {
            let n = run.text.chars().count() as f64;
            let x = PAD + col * cw;
            if run.bg != DEFAULT_BG {
                ui::fill(ui::rect(x, y, n * cw, ch), &ui::rgb(run.bg));
            }
            // Cell by cell, so a wide or missing glyph cannot push the rest of the line.
            let f = if run.bold { &bold } else { &font };
            let ink = ui::rgb(run.fg);
            for (i, c) in run.text.chars().enumerate() {
                if c != ' ' {
                    let r = ui::rect(x + i as f64 * cw, y, cw * 2.0, ch);
                    ui::draw_text(&c.to_string(), r, f, &ink);
                }
            }
            if run.underline {
                ui::fill(ui::rect(x, y + ch - 1.0, n * cw, 1.0), &ink);
            }
            col += n;
        }
    }
    if let Some((row, col)) = screen.cursor {
        let x = PAD + f64::from(col) * cw;
        let y = PAD + f64::from(row) * ch;
        if focused {
            ui::fill(
                ui::rect(x, y + ch - ch / 4.0, cw, ch / 4.0),
                &ui::rgb(0xcccccc),
            );
        } else {
            ui::frame(ui::rect(x, y, cw, ch), &ui::rgb(0x808080));
        }
    }
}

pub struct Term {
    pub view: Retained<TermView>,
    pub connect: Retained<NSButton>,
}

impl Term {
    pub fn new(mtm: MainThreadMarker) -> Term {
        let this = mtm
            .alloc::<TermView>()
            .set_ivars(RefCell::new(TermState::default()));
        // SAFETY: NSView's designated initialiser.
        let view: Retained<TermView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        ui::set_id(&view, ids::TERM_BODY);
        view.setAccessibilityElement(true);
        // SAFETY: AppKit's own role constant.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityTextAreaRole }));
        let connect = ui::button(mtm, "Connect", ids::TERM_CONNECT);
        view.addSubview(&connect);
        Term { view, connect }
    }

    /// Show `screen` (or `message` with no session); the screen's text is its accessible value.
    pub fn set(&self, screen: Option<TermScreen>, message: &str) {
        let mut s = self.view.ivars().borrow_mut();
        if s.screen == screen && s.message == message {
            return;
        }
        let text = match &screen {
            Some(sc) => sc
                .lines
                .iter()
                .map(|l| l.iter().map(|r| r.text.as_str()).collect::<String>())
                .map(|l| l.trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            None => message.to_string(),
        };
        s.screen = screen;
        s.message = message.to_string();
        drop(s);
        let value: Retained<AnyObject> =
            Retained::into_super(Retained::into_super(ns(&ui::plain(&text))));
        // SAFETY: an NSString is a valid accessibility value.
        unsafe { self.view.setAccessibilityValue(Some(&value)) };
        self.view.setNeedsDisplay(true);
    }

    /// The terminal's size in cells for its current frame.
    pub fn grid(&self) -> (u16, u16) {
        let b = self.view.bounds();
        let (cw, ch) = cell();
        (
            ((b.size.width - 2.0 * PAD) / cw).max(10.0) as u16,
            ((b.size.height - 2.0 * PAD) / ch).max(2.0) as u16,
        )
    }

    /// The Connect button in the middle, while a session can be opened.
    pub fn place_connect(&self, shown: bool, label: &str) {
        ui::set_visible(&self.connect, shown);
        if shown {
            ui::set_title(&self.connect, label);
            let b = self.view.bounds();
            ui::place(
                &self.connect,
                (
                    (b.size.width - 200.0) / 2.0,
                    b.size.height / 2.0,
                    200.0,
                    28.0,
                ),
            );
        }
    }

    pub fn focused(&self) -> bool {
        let Some(w) = self.view.window() else {
            return false;
        };
        w.firstResponder().is_some_and(|r| {
            std::ptr::eq(
                &*r as *const _ as *const u8,
                &*self.view as *const _ as *const u8,
            )
        })
    }
}
