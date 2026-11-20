//! Small AppKit helpers: views carrying their controller id, frames, text, colours, and the
//! action targets (from rusty-backup's AppKit shell).
//!
//! Every actionable view gets its controller id as its accessibility identifier (`AXIdentifier`);
//! the flow driver finds controls by it. A control's action never runs inside the click or the
//! accessibility press that fired it: it is queued with [`later`], so a dialog it opens does not
//! hold the press (Win32 posts `BM_CLICK` for the same reason).

use std::cell::{OnceCell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSBezierPath, NSButton, NSColor, NSControl, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSLineBreakMode, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSSecureTextField, NSStringDrawing, NSTextAlignment,
    NSTextField, NSView,
};
use objc2_foundation::{NSDictionary, NSNotification, NSPoint, NSRect, NSSize, NSString};

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Text from a remote machine can hold NUL, which a C string would end at: drop it.
pub fn plain(s: &str) -> String {
    s.replace('\0', "")
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w.max(0.0), h.max(0.0)))
}

/// An sRGB colour from 0xRRGGBB.
pub fn rgb(c: u32) -> Retained<NSColor> {
    let ch = |s: u32| f64::from((c >> s) & 0xff) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(ch(16), ch(8), ch(0), 1.0)
}

/// `c` mixed towards a dark grey by `1 - amount`: a tint for dark mode.
pub fn shade(c: u32, amount: f64) -> Retained<NSColor> {
    let base = 30.0 / 255.0;
    let mix = |s: u32| {
        let v = f64::from((c >> s) & 0xff) / 255.0;
        v * amount + base * (1.0 - amount)
    };
    NSColor::colorWithSRGBRed_green_blue_alpha(mix(16), mix(8), mix(0), 1.0)
}

/// `c` mixed towards white by `1 - amount` (Win32's `paint::tint`).
pub fn tint(c: u32, amount: f64) -> Retained<NSColor> {
    let mix = |s: u32| {
        let v = f64::from((c >> s) & 0xff) / 255.0;
        v * amount + (1.0 - amount)
    };
    NSColor::colorWithSRGBRed_green_blue_alpha(mix(16), mix(8), mix(0), 1.0)
}

thread_local! {
    static LATER: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::default();
    static TARGET: OnceCell<Retained<Target>> = const { OnceCell::new() };
}

/// Run `f` from a main-queue block of its own, once the handler on the stack has returned.
pub fn later(f: Box<dyn FnOnce()>) {
    LATER.with(|l| l.borrow_mut().push_back(f));
    dispatch2::DispatchQueue::main().exec_async(|| {
        let next = LATER.with(|l| l.borrow_mut().pop_front());
        if let Some(f) = next {
            f();
        }
    });
}

define_class!(
    /// A container whose y axis grows down, as Win32's client area does.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCFlippedView"]
    pub struct Flipped;

    unsafe impl NSObjectProtocol for Flipped {}

    impl Flipped {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

pub fn flipped(mtm: MainThreadMarker) -> Retained<NSView> {
    let this = mtm.alloc::<Flipped>().set_ivars(());
    // SAFETY: NSView's designated initialiser.
    let v: Retained<Flipped> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
    Retained::into_super(v)
}

/// What a [`Canvas`] draws: called inside `drawRect:` with the view's bounds.
pub type Painter = Rc<dyn Fn(NSRect)>;

define_class!(
    /// A flipped view drawn by a closure; the closure keeps its own state.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCCanvas"]
    #[ivars = RefCell<Option<Painter>>]
    pub struct Canvas;

    unsafe impl NSObjectProtocol for Canvas {}

    impl Canvas {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let p = self.ivars().borrow().clone();
            if let Some(p) = p {
                p(self.bounds());
            }
        }
    }
);

pub fn canvas(mtm: MainThreadMarker, painter: Painter) -> Retained<Canvas> {
    let this = mtm.alloc::<Canvas>().set_ivars(RefCell::new(Some(painter)));
    // SAFETY: NSView's designated initialiser.
    unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
}

define_class!(
    /// The target of every main-window control: it queues the sender's controller id.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCTarget"]
    pub struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(act:))]
        fn act(&self, sender: Option<&AnyObject>) {
            let id = sender
                .and_then(|s| s.downcast_ref::<NSView>())
                .and_then(|v| v.accessibilityIdentifier())
                .map(|s| s.to_string());
            if let Some(id) = id {
                later(Box::new(move || crate::window::command(&id)));
            }
        }
    }
);

/// The one action target; it lives as long as the app.
pub fn target(mtm: MainThreadMarker) -> Retained<Target> {
    TARGET.with(|t| {
        t.get_or_init(|| {
            let this = mtm.alloc::<Target>().set_ivars(());
            // SAFETY: NSObject's plain init.
            unsafe { msg_send![super(this), init] }
        })
        .clone()
    })
}

define_class!(
    /// An action target that runs a closure with the sender, for a dialog's own controls.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCCallback"]
    #[ivars = Box<dyn Fn(&AnyObject)>]
    pub struct Callback;

    unsafe impl NSObjectProtocol for Callback {}

    impl Callback {
        #[unsafe(method(fire:))]
        fn fire(&self, sender: Option<&AnyObject>) {
            if let Some(s) = sender {
                (self.ivars())(s);
            }
        }
    }
);

/// A target running `f`; the caller keeps it as long as the controls that use it.
pub fn callback(mtm: MainThreadMarker, f: Box<dyn Fn(&AnyObject)>) -> Retained<Callback> {
    let this = mtm.alloc::<Callback>().set_ivars(f);
    // SAFETY: NSObject's plain init.
    unsafe { msg_send![super(this), init] }
}

/// Point `control`'s action at `target`.
pub fn hook(control: &NSControl, target: &Callback) {
    // SAFETY: the caller keeps the target alive with the control.
    unsafe {
        control.setTarget(Some(target));
        control.setAction(Some(sel!(fire:)));
    }
}

/// A text field that reports every change, typed or set through accessibility.
pub type OnChange = RefCell<Option<Rc<dyn Fn()>>>;

define_class!(
    #[unsafe(super(NSTextField))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCField"]
    #[ivars = OnChange]
    pub struct Field;

    unsafe impl NSObjectProtocol for Field {}

    impl Field {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, note: &NSNotification) {
            // SAFETY: NSTextField's own handling first.
            let _: () = unsafe { msg_send![super(self), textDidChange: note] };
            changed(self.ivars());
        }

        #[unsafe(method(setAccessibilityValue:))]
        fn set_ax_value(&self, value: Option<&AnyObject>) {
            // SAFETY: NSTextField's own handling first.
            let _: () = unsafe { msg_send![super(self), setAccessibilityValue: value] };
            changed(self.ivars());
        }

        #[unsafe(method(setObjectValue:))]
        fn set_object_value(&self, value: Option<&AnyObject>) {
            // SAFETY: NSTextField's own handling first; accessibility sets the value through here.
            let _: () = unsafe { msg_send![super(self), setObjectValue: value] };
            changed(self.ivars());
        }

        #[unsafe(method(setStringValue:))]
        fn set_string_value(&self, value: &NSString) {
            // SAFETY: NSTextField's own handling first.
            let _: () = unsafe { msg_send![super(self), setStringValue: value] };
            changed(self.ivars());
        }
    }
);

define_class!(
    /// A pane's address bar: typed into, Enter goes there, Escape puts the path back.
    #[unsafe(super(NSTextField))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCPathField"]
    pub struct PathField;

    unsafe impl NSObjectProtocol for PathField {}

    impl PathField {
        #[unsafe(method(textView:doCommandBySelector:))]
        fn do_command(&self, view: &AnyObject, command: objc2::runtime::Sel) -> objc2::runtime::Bool {
            if command == sel!(cancelOperation:) {
                // Drop the edit: the field shows the folder it showed before.
                self.abortEditing();
                if let Some(w) = self.window() {
                    w.makeFirstResponder(None);
                }
                return objc2::runtime::Bool::YES;
            }
            // SAFETY: NSTextField's own handling of every other editing command.
            unsafe { msg_send![super(self), textView: view, doCommandBySelector: command] }
        }
    }
);

/// A pane's address bar with controller id `id`; its action fires on Enter only.
pub fn path_field(mtm: MainThreadMarker, id: &str) -> Retained<NSTextField> {
    // SAFETY: NSTextField's designated initialiser.
    let v: Retained<PathField> =
        unsafe { msg_send![mtm.alloc::<PathField>(), initWithFrame: NSRect::ZERO] };
    let f: Retained<NSTextField> = Retained::into_super(v);
    f.setEditable(true);
    f.setSelectable(true);
    f.setBezeled(true);
    f.setDrawsBackground(true);
    if let Some(cell) = f.cell() {
        cell.setScrollable(true);
        cell.setWraps(false);
        cell.setSendsActionOnEndEditing(false);
    }
    set_id(&f, id);
    f
}

define_class!(
    #[unsafe(super(NSSecureTextField))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCSecureField"]
    #[ivars = OnChange]
    pub struct SecureField;

    unsafe impl NSObjectProtocol for SecureField {}

    impl SecureField {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, note: &NSNotification) {
            // SAFETY: NSSecureTextField's own handling first.
            let _: () = unsafe { msg_send![super(self), textDidChange: note] };
            changed(self.ivars());
        }

        #[unsafe(method(setAccessibilityValue:))]
        fn set_ax_value(&self, value: Option<&AnyObject>) {
            // SAFETY: NSSecureTextField's own handling first.
            let _: () = unsafe { msg_send![super(self), setAccessibilityValue: value] };
            changed(self.ivars());
        }

        #[unsafe(method(setObjectValue:))]
        fn set_object_value(&self, value: Option<&AnyObject>) {
            // SAFETY: NSSecureTextField's own handling first; accessibility sets the value through here.
            let _: () = unsafe { msg_send![super(self), setObjectValue: value] };
            changed(self.ivars());
        }

        #[unsafe(method(setStringValue:))]
        fn set_string_value(&self, value: &NSString) {
            // SAFETY: NSSecureTextField's own handling first.
            let _: () = unsafe { msg_send![super(self), setStringValue: value] };
            changed(self.ivars());
        }
    }
);

fn changed(on: &OnChange) {
    let f = on.borrow().clone();
    if let Some(f) = f {
        f();
    }
}

/// An editable one-line field (secret: shown as dots) with controller id `id`.
pub fn field(mtm: MainThreadMarker, text: &str, id: &str, secret: bool) -> Retained<NSTextField> {
    let f: Retained<NSTextField> = if secret {
        let this = mtm.alloc::<SecureField>().set_ivars(RefCell::new(None));
        // SAFETY: NSTextField's designated initialiser.
        let v: Retained<SecureField> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        Retained::into_super(v.into_super())
    } else {
        let this = mtm.alloc::<Field>().set_ivars(RefCell::new(None));
        // SAFETY: NSTextField's designated initialiser.
        let v: Retained<Field> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        Retained::into_super(v)
    };
    f.setStringValue(&ns(&plain(text)));
    f.setEditable(true);
    f.setSelectable(true);
    f.setBezeled(true);
    f.setDrawsBackground(true);
    if let Some(cell) = f.cell() {
        cell.setScrollable(true);
        cell.setWraps(false);
    }
    set_id(&f, id);
    f
}

/// Run `f` whenever the field made by [`field`] changes.
pub fn on_change(field: &NSTextField, f: Rc<dyn Fn()>) {
    if let Some(x) = field.downcast_ref::<Field>() {
        *x.ivars().borrow_mut() = Some(f);
    } else if let Some(x) = field.downcast_ref::<SecureField>() {
        *x.ivars().borrow_mut() = Some(f);
    }
}

/// Give `view` its controller id as its accessibility identifier.
pub fn set_id(view: &NSView, id: &str) {
    if !id.is_empty() {
        view.setAccessibilityIdentifier(Some(&ns(id)));
    }
}

/// A push button whose click queues `window::command(id)`.
pub fn button(mtm: MainThreadMarker, title: &str, id: &str) -> Retained<NSButton> {
    let t = target(mtm);
    // SAFETY: the target outlives every control (it is never dropped).
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&ns(title), Some(&t), Some(sel!(act:)), mtm)
    };
    set_id(&b, id);
    b
}

/// A push button running `cb`, for dialogs.
pub fn dialog_button(
    mtm: MainThreadMarker,
    title: &str,
    id: &str,
    cb: &Callback,
) -> Retained<NSButton> {
    // SAFETY: the caller keeps the callback as long as the button.
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&ns(title), Some(cb), Some(sel!(fire:)), mtm)
    };
    set_id(&b, id);
    b
}

/// A one-line label; an overlong line loses its end.
pub fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(&plain(text)), mtm);
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l
}

/// Set a field's text only when it changed: a blind set moves the caret mid-typing.
pub fn set_text(field: &NSTextField, text: &str) {
    let text = plain(text);
    if field.stringValue().to_string() != text {
        field.setStringValue(&ns(&text));
    }
}

pub fn set_title(b: &NSButton, text: &str) {
    if b.title().to_string() != text {
        b.setTitle(&ns(text));
    }
}

pub fn set_visible(view: &NSView, on: bool) {
    if view.isHidden() == on {
        view.setHidden(!on);
    }
}

pub fn set_enabled(c: &NSControl, on: bool) {
    if c.isEnabled() != on {
        c.setEnabled(on);
    }
}

/// Place `view` at (x, y, w, h) in its flipped parent.
pub fn place(view: &NSView, r: (f64, f64, f64, f64)) {
    let frame = rect(r.0, r.1, r.2, r.3);
    if view.frame() != frame {
        view.setFrame(frame);
    }
}

pub fn fill(r: NSRect, color: &NSColor) {
    color.setFill();
    NSBezierPath::fillRect(r);
}

pub fn frame(r: NSRect, color: &NSColor) {
    color.setStroke();
    let inset = rect(
        r.origin.x + 0.5,
        r.origin.y + 0.5,
        r.size.width - 1.0,
        r.size.height - 1.0,
    );
    NSBezierPath::strokeRect(inset);
}

pub fn dot(x: f64, y: f64, d: f64, color: &NSColor) {
    color.setFill();
    NSBezierPath::bezierPathWithOvalInRect(rect(x, y, d, d)).fill();
}

/// A rounded rectangle filled with `bg` and outlined with `line`.
pub fn pill(r: NSRect, bg: &NSColor, line: &NSColor, radius: f64) {
    let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, radius, radius);
    bg.setFill();
    p.fill();
    line.setStroke();
    p.stroke();
}

/// Attributes for drawing text in `font` and `color`, cut with an ellipsis where it does not fit.
pub fn attrs(font: &NSFont, color: &NSColor) -> Retained<NSDictionary<NSString, AnyObject>> {
    let style = NSMutableParagraphStyle::new();
    style.setAlignment(NSTextAlignment::Left);
    style.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    // SAFETY: AppKit's attribute-name constants.
    let keys = unsafe {
        [
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
        ]
    };
    let values: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), style.as_ref()];
    NSDictionary::from_slices(&keys, &values)
}

pub fn draw_text(text: &str, r: NSRect, font: &NSFont, color: &NSColor) {
    let a = attrs(font, color);
    // SAFETY: drawing inside drawRect: with attributes of the documented types.
    unsafe { ns(&plain(text)).drawInRect_withAttributes(r, Some(&a)) };
}

pub fn text_width(text: &str, font: &NSFont) -> f64 {
    let a = attrs(font, &NSColor::labelColor());
    // SAFETY: measuring with attributes of the documented types.
    unsafe { ns(&plain(text)).sizeWithAttributes(Some(&a)) }.width
}

pub fn separator() -> Retained<NSColor> {
    NSColor::separatorColor()
}

/// Whether `v` draws in a dark appearance.
pub fn dark(v: &NSView) -> bool {
    use objc2_app_kit::NSAppearanceCustomization;
    v.effectiveAppearance().name().to_string().contains("Dark")
}

pub fn font() -> Retained<NSFont> {
    NSFont::systemFontOfSize(NSFont::systemFontSize())
}

pub fn bold() -> Retained<NSFont> {
    NSFont::boldSystemFontOfSize(NSFont::systemFontSize())
}

/// The terminal's and the path lines' font.
pub fn mono() -> Retained<NSFont> {
    NSFont::fontWithName_size(&ns("Menlo"), 12.0)
        .unwrap_or_else(|| NSFont::userFixedPitchFontOfSize(12.0).unwrap_or_else(font))
}
