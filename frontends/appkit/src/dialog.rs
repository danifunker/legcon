//! A modal dialog window the prompt and the form are built in: a flipped body, OK (Return) and
//! Cancel (Esc), run with `runModalForWindow`. Its controls keep their callbacks here.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};

use crate::ui::{self, ns, Callback};

define_class!(
    /// Closing the dialog's window is Cancel.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCDialogDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowShouldClose:))]
        fn should_close(&self, _sender: &NSWindow) -> bool {
            NSApplication::sharedApplication(self.mtm()).stopModalWithCode(0);
            false
        }
    }
);

pub struct Dialog {
    mtm: MainThreadMarker,
    pub window: Retained<NSWindow>,
    pub body: Retained<NSView>,
    keep: RefCell<Vec<Retained<Callback>>>,
    _delegate: Retained<Delegate>,
}

impl Dialog {
    pub fn new(mtm: MainThreadMarker, title: &str, w: f64, h: f64) -> Dialog {
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        // SAFETY: the designated initialiser; `Dialog` keeps the window.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                ui::rect(0.0, 0.0, w, h),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: `Dialog` owns the window, so AppKit must not release it on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns(&ui::plain(title)));
        let body = ui::flipped(mtm);
        window.setContentView(Some(&body));
        let delegate: Retained<Delegate> = {
            let this = mtm.alloc::<Delegate>().set_ivars(());
            // SAFETY: NSObject's plain init.
            unsafe { msg_send![super(this), init] }
        };
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Dialog {
            mtm,
            window,
            body,
            keep: RefCell::new(Vec::new()),
            _delegate: delegate,
        }
    }

    /// A callback kept as long as the dialog.
    pub fn callback(&self, f: Box<dyn Fn(&AnyObject)>) -> Retained<Callback> {
        let cb = ui::callback(self.mtm, f);
        self.keep.borrow_mut().push(cb.clone());
        cb
    }

    /// A button in the body running `f`.
    pub fn button(&self, title: &str, id: &str, f: Box<dyn Fn()>) -> Retained<NSButton> {
        let cb = self.callback(Box::new(move |_| f()));
        let b = ui::dialog_button(self.mtm, title, id, &cb);
        self.body.addSubview(&b);
        b
    }

    /// OK and Cancel at the bottom right of a `w` x `h` body; OK ends with `ok()`'s verdict.
    pub fn ok_cancel(
        &self,
        ok_label: &str,
        ok_id: &str,
        cancel_id: &str,
        ok: Rc<dyn Fn() -> bool>,
    ) -> Retained<NSButton> {
        let mtm = self.mtm;
        let b = self.body.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let okb = self.button(
            ok_label,
            ok_id,
            Box::new(move || {
                if ok() {
                    NSApplication::sharedApplication(mtm).stopModalWithCode(1);
                }
            }),
        );
        okb.setKeyEquivalent(&ns("\r"));
        let cancel = self.button(
            "Cancel",
            cancel_id,
            Box::new(move || NSApplication::sharedApplication(mtm).stopModalWithCode(0)),
        );
        cancel.setKeyEquivalent(&ns("\u{1b}"));
        ui::place(&cancel, (w - 12.0 - 96.0, h - 12.0 - 30.0, 96.0, 30.0));
        ui::place(
            &okb,
            (w - 12.0 - 2.0 * 96.0 - 8.0, h - 12.0 - 30.0, 96.0, 30.0),
        );
        okb
    }

    /// Show the dialog over the main window until OK or Cancel; true for OK.
    pub fn run(&self, first: Option<&NSView>) -> bool {
        let app = NSApplication::sharedApplication(self.mtm);
        if let Some(main) = crate::window::main_window() {
            let p = main.frame();
            let s = self.window.frame();
            let x = p.origin.x + (p.size.width - s.size.width) / 2.0;
            let y = p.origin.y + (p.size.height - s.size.height) * 2.0 / 3.0;
            self.window
                .setFrameOrigin(objc2_foundation::NSPoint::new(x, y));
        } else {
            self.window.center();
        }
        self.window.makeKeyAndOrderFront(None);
        if let Some(v) = first {
            self.window.makeFirstResponder(Some(v));
        }
        let code = app.runModalForWindow(&self.window);
        self.window.orderOut(None);
        code == 1
    }
}
