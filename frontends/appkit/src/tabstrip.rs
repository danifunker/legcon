//! The tab strip (id `tabs`): Machines, Keys, then one tab per connection wearing its machine's
//! colour and state light (D30). A click selects, the x or a middle click closes (logs off), a
//! right click opens the tab's menu. Each tab is a button, so accessibility presses it.

use std::cell::RefCell;

use legcon::controller::ConnState;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAppearanceCustomization, NSButton, NSColor, NSEvent, NSView};
use objc2_foundation::NSRect;

use crate::header::state_rgb;
use crate::ui::{self, ns};

/// One tab as the strip draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct TabLook {
    pub id: String,
    pub title: String,
    pub color: Option<u32>,
    pub state: Option<ConnState>,
}

#[derive(Debug, Clone)]
pub struct TabState {
    look: TabLook,
    index: usize,
    selected: bool,
}

pub const TAB_W: f64 = 150.0;
pub const TAB_H: f64 = 28.0;
const CLOSE_W: f64 = 20.0;

define_class!(
    #[unsafe(super(NSButton))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCTabButton"]
    #[ivars = RefCell<TabState>]
    pub struct TabButton;

    unsafe impl NSObjectProtocol for TabButton {}

    impl TabButton {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let s = self.ivars().borrow().clone();
            draw(&s, self.bounds(), dark(self));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let (index, conn) = {
                let s = self.ivars().borrow();
                (s.index, s.look.state.is_some())
            };
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            if conn && at.x >= self.bounds().size.width - CLOSE_W {
                ui::later(Box::new(move || crate::window::close_tab(index)));
                return;
            }
            // SAFETY: the button's own tracking, which fires its action.
            let _: () = unsafe { msg_send![super(self), mouseDown: event] };
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {
            let index = self.ivars().borrow().index;
            ui::later(Box::new(move || crate::window::tab_menu(index)));
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, _event: &NSEvent) {
            let (index, conn) = {
                let s = self.ivars().borrow();
                (s.index, s.look.state.is_some())
            };
            if conn {
                ui::later(Box::new(move || crate::window::close_tab(index)));
            }
        }
    }
);

fn dark(v: &NSView) -> bool {
    v.effectiveAppearance().name().to_string().contains("Dark")
}

fn draw(s: &TabState, b: NSRect, dark: bool) {
    let (w, h) = (b.size.width, b.size.height);
    let face: Retained<NSColor> = match (s.look.color, s.selected) {
        (Some(c), true) => ui::tint(c, if dark { 0.55 } else { 0.22 }),
        (Some(c), false) => ui::tint(c, if dark { 0.35 } else { 0.12 }),
        (None, true) => NSColor::windowBackgroundColor(),
        (None, false) => NSColor::quaternaryLabelColor(),
    };
    let r = ui::rect(1.0, 2.0, w - 2.0, h - 2.0);
    ui::pill(r, &face, &NSColor::separatorColor(), 4.0);
    if let Some(c) = s.look.color {
        ui::fill(ui::rect(2.0, 3.0, w - 4.0, 4.0), &ui::rgb(c));
    }
    // A tinted face is light, so its text is dark in either appearance.
    let ink = match s.look.color {
        Some(_) if !dark => ui::rgb(0x1b1b1b),
        _ => NSColor::labelColor(),
    };
    let mut x = 10.0;
    if let Some(st) = s.look.state {
        ui::dot(x, (h - 9.0) / 2.0 + 1.0, 9.0, &ui::rgb(state_rgb(st)));
        x += 16.0;
    }
    let font = if s.selected { ui::bold() } else { ui::font() };
    let right = if s.look.state.is_some() { CLOSE_W } else { 6.0 };
    let th = font.capHeight() + 8.0;
    ui::draw_text(
        &s.look.title,
        ui::rect(x, (h - th) / 2.0 + 1.0, w - x - right, th),
        &font,
        &ink,
    );
    if s.look.state.is_some() {
        ui::draw_text(
            "x",
            ui::rect(w - CLOSE_W + 4.0, (h - th) / 2.0 + 1.0, 12.0, th),
            &ui::font(),
            &NSColor::secondaryLabelColor(),
        );
    }
}

pub struct TabStrip {
    pub view: Retained<NSView>,
    tabs: RefCell<Vec<Retained<TabButton>>>,
}

impl TabStrip {
    pub fn new(mtm: MainThreadMarker) -> TabStrip {
        let view = ui::flipped(mtm);
        ui::set_id(&view, "tabs");
        TabStrip {
            view,
            tabs: RefCell::new(Vec::new()),
        }
    }

    /// Show `looks` with tab `active` selected; buttons are made or dropped as tabs come and go.
    pub fn set(&self, mtm: MainThreadMarker, looks: &[TabLook], active: usize) {
        let mut tabs = self.tabs.borrow_mut();
        while tabs.len() > looks.len() {
            if let Some(t) = tabs.pop() {
                t.removeFromSuperview();
            }
        }
        while tabs.len() < looks.len() {
            let this = mtm.alloc::<TabButton>().set_ivars(RefCell::new(TabState {
                look: looks[tabs.len()].clone(),
                index: tabs.len(),
                selected: false,
            }));
            // SAFETY: NSButton's designated initialiser.
            let b: Retained<TabButton> =
                unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
            b.setBordered(false);
            let t = ui::target(mtm);
            // SAFETY: the target is never dropped.
            unsafe {
                b.setTarget(Some(&t));
                b.setAction(Some(sel!(act:)));
            }
            self.view.addSubview(&b);
            tabs.push(b);
        }
        for (i, (b, look)) in tabs.iter().zip(looks).enumerate() {
            let want = TabState {
                look: look.clone(),
                index: i,
                selected: i == active,
            };
            let changed = {
                let s = b.ivars().borrow();
                s.look != want.look || s.selected != want.selected || s.index != i
            };
            if changed {
                *b.ivars().borrow_mut() = want;
                b.setTitle(&ns(&look.title));
                ui::set_id(b, &look.id);
                let v: &NSView = b;
                v.setNeedsDisplay(true);
            }
            ui::place(b, (4.0 + i as f64 * (TAB_W + 2.0), 0.0, TAB_W, TAB_H));
        }
    }
}
