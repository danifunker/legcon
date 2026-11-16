//! The machine header (mockup: connection-view.html): the colour strip, the state light, name
//! and address, the plaintext badge, a chip per service with its Connect / Disconnect, then
//! Settings..., Swap and Close tab. Shapes are drawn; text is labels, so accessibility reads it.

use std::cell::RefCell;
use std::rc::Rc;

use legcon::controller::connection::HeaderInfo;
use legcon::controller::{ids, ConnState};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSButton, NSColor, NSTextField, NSView,
};
use objc2_foundation::NSRect;

use crate::ui;

pub const OK: u32 = 0x2e8b3a;
pub const BUSY: u32 = 0x0067c0;
pub const WARN: u32 = 0xc98a00;
pub const OFF: u32 = 0x9aa0a6;
pub const BAD: u32 = 0xc42b1c;

pub fn state_rgb(s: ConnState) -> u32 {
    match s {
        ConnState::Connected => OK,
        ConnState::Busy => BUSY,
        ConnState::Connecting | ConnState::Disconnecting => WARN,
        ConnState::Disconnected => OFF,
        ConnState::Lost => BAD,
    }
}

fn button_label(s: ConnState) -> &'static str {
    match s {
        ConnState::Connected | ConnState::Busy => "Disconnect",
        ConnState::Connecting => "Connecting...",
        ConnState::Disconnecting => "Logging off...",
        ConnState::Disconnected | ConnState::Lost => "Connect",
    }
}

/// What the background draws, worked out when the header is laid out.
#[derive(Default, Clone)]
struct Shapes {
    color: u32,
    light: u32,
    badge: Option<NSRect>,
    chips: Vec<(NSRect, u32)>,
}

pub struct Header {
    pub view: Retained<NSView>,
    name: Retained<NSTextField>,
    address: Retained<NSTextField>,
    badge: Retained<NSTextField>,
    chips: [Retained<NSTextField>; 2],
    pub files: Retained<NSButton>,
    pub commands: Retained<NSButton>,
    pub swap: Retained<NSButton>,
    pub close: Retained<NSButton>,
    pub settings: Retained<NSButton>,
    shapes: Rc<RefCell<Shapes>>,
}

impl Header {
    pub fn new(mtm: MainThreadMarker) -> Header {
        let shapes = Rc::new(RefCell::new(Shapes::default()));
        let s = shapes.clone();
        let view = ui::canvas(
            mtm,
            Rc::new(move |b| {
                let s = s.borrow();
                ui::fill(
                    ui::rect(0.0, 4.0, 5.0, b.size.height - 8.0),
                    &ui::rgb(s.color),
                );
                ui::dot(14.0, (b.size.height - 10.0) / 2.0, 10.0, &ui::rgb(s.light));
                if let Some(r) = s.badge {
                    ui::pill(r, &ui::rgb(0xfff4ce), &ui::rgb(0xf0d78a), 9.0);
                }
                for (r, st) in &s.chips {
                    let bg = NSColor::controlBackgroundColor();
                    ui::pill(*r, &bg, &ui::separator(), r.size.height / 2.0);
                    let y = r.origin.y + (r.size.height - 8.0) / 2.0;
                    ui::dot(r.origin.x + 10.0, y, 8.0, &ui::rgb(*st));
                }
            }),
        );
        let view: Retained<NSView> = Retained::into_super(view);
        ui::set_id(&view, ids::CONN_HEADER);
        view.setAccessibilityElement(true);
        // SAFETY: AppKit's own role constant.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        let name = ui::label(mtm, "");
        name.setFont(Some(&ui::bold()));
        let address = ui::label(mtm, "");
        address.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let badge = ui::label(mtm, "plaintext");
        badge.setTextColor(Some(&ui::rgb(0x7a5a00)));
        let chips = [ui::label(mtm, ""), ui::label(mtm, "")];
        let h = Header {
            files: ui::button(mtm, "Connect", ids::CONN_FILES),
            commands: ui::button(mtm, "Connect", ids::CONN_COMMANDS),
            swap: ui::button(mtm, "Swap", ids::CONN_SWAP),
            close: ui::button(mtm, "Close tab", ids::CONN_CLOSE),
            settings: ui::button(mtm, "Settings...", ids::CONN_SETTINGS),
            view,
            name,
            address,
            badge,
            chips,
            shapes,
        };
        for v in [&h.name, &h.address, &h.badge, &h.chips[0], &h.chips[1]] {
            h.view.addSubview(v);
        }
        for b in [&h.files, &h.commands, &h.swap, &h.close, &h.settings] {
            h.view.addSubview(b);
        }
        h
    }

    /// Lay the header out for `info` at its current width (Win32's `header::geometry`).
    pub fn update(&self, info: &HeaderInfo) {
        let b = self.view.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let font = ui::font();
        let (top, height) = (4.0, h - 8.0);
        let mut x = w - 8.0;
        for (btn, bw) in [
            (&self.close, 80.0),
            (&self.swap, 70.0),
            (&self.settings, 92.0),
        ] {
            x -= bw;
            ui::place(btn, (x, top + 2.0, bw, height - 4.0));
            x -= 6.0;
        }
        let mut chips = Vec::new();
        let parts = [
            (1usize, &info.commands, info.commands_state, &self.commands),
            (0usize, &info.files, info.files_state, &self.files),
        ];
        for (i, label, state, btn) in parts {
            let bw = 104.0;
            let tw = ui::text_width(label, &font) + 4.0;
            let cw = 24.0 + tw + 8.0 + bw + 4.0;
            x -= cw + 6.0;
            let chip = ui::rect(x, top, cw, height);
            chips.push((chip, state_rgb(state)));
            ui::place(&self.chips[i], (x + 24.0, (h - 17.0) / 2.0, tw, 17.0));
            ui::set_text(&self.chips[i], label);
            ui::place(btn, (x + cw - bw - 4.0, top + 2.0, bw, height - 4.0));
            ui::set_title(btn, button_label(state));
            let usable = !label.starts_with("No ")
                && matches!(
                    state,
                    ConnState::Connected | ConnState::Disconnected | ConnState::Lost
                );
            ui::set_enabled(btn, usable);
        }
        let name_w = ui::text_width(&info.name, &ui::bold()) + 6.0;
        ui::set_text(&self.name, &info.name);
        ui::place(&self.name, (30.0, (h - 17.0) / 2.0, name_w, 17.0));
        let ax = 30.0 + name_w + 10.0;
        let addr_w = (ui::text_width(&info.address, &font) + 6.0).min((x - ax - 100.0).max(0.0));
        ui::set_text(&self.address, &info.address);
        ui::place(&self.address, (ax, (h - 17.0) / 2.0, addr_w, 17.0));
        ui::set_visible(&self.badge, info.plaintext);
        let badge = info.plaintext.then(|| {
            let bw = ui::text_width("plaintext", &font) + 16.0;
            let r = ui::rect(ax + addr_w + 10.0, h / 2.0 - 10.0, bw, 20.0);
            ui::place(
                &self.badge,
                (r.origin.x + 8.0, (h - 17.0) / 2.0, bw - 10.0, 17.0),
            );
            r
        });
        let live = |s: ConnState| matches!(s, ConnState::Connected);
        let light = if live(info.files_state) || live(info.commands_state) {
            ConnState::Connected
        } else {
            info.files_state
        };
        *self.shapes.borrow_mut() = Shapes {
            color: info.color.rgb(),
            light: state_rgb(light),
            badge,
            chips,
        };
        self.view.setNeedsDisplay(true);
    }
}
