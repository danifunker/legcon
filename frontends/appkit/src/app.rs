//! The application: `NSApplication`, its delegate and the menu bar. Quitting goes through the
//! controller (every machine logs off first), which answers with `Effect::Quit`.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSEventModifierFlags, NSImage, NSMenu, NSMenuItem,
};
use objc2_foundation::{NSData, NSNotification};

use crate::ui::ns;

thread_local! {
    static DELEGATE: RefCell<Option<Retained<Delegate>>> = const { RefCell::new(None) };
    /// Set once the controller said every machine is logged off.
    static QUITTING: Cell<bool> = const { Cell::new(false) };
    /// macOS asked to quit (Cmd+Q, log out, shut down) and waits for our answer.
    static ASKED: Cell<bool> = const { Cell::new(false) };
    static MENU_TARGETS: RefCell<Vec<Retained<crate::ui::Callback>>> = const { RefCell::new(Vec::new()) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCAppDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    impl Delegate {
        /// The Mac is going to sleep: every machine logs off, to connect again when used.
        #[unsafe(method(willSleep:))]
        fn will_sleep(&self, _note: &NSNotification) {
            crate::window::dispatch(legcon::controller::app::Action::Sleep);
        }

        #[unsafe(method(didWake:))]
        fn did_wake(&self, _note: &NSNotification) {
            crate::window::dispatch(legcon::controller::app::Action::Wake);
        }
    }

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _note: &NSNotification) {
            // Set once launched: AppKit sets its own (none outside a bundle) while launching.
            set_icon(&NSApplication::sharedApplication(self.mtm()));
            watch_sleep(self);
            crate::window::start(self.mtm());
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            if QUITTING.with(Cell::get) {
                return NSApplicationTerminateReply::TerminateNow;
            }
            // Log every machine off first; the controller's Quit effect answers macOS.
            ASKED.with(|a| a.set(true));
            crate::ui::later(Box::new(crate::window::quit));
            NSApplicationTerminateReply::TerminateLater
        }
    }
);

/// Hear from NSWorkspace when the Mac sleeps and wakes.
fn watch_sleep(delegate: &Delegate) {
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: the delegate lives for the app's life (DELEGATE), so the observer stays valid.
    unsafe {
        center.addObserver_selector_name_object(
            delegate,
            sel!(willSleep:),
            Some(NSWorkspaceWillSleepNotification),
            None,
        );
        center.addObserver_selector_name_object(
            delegate,
            sel!(didWake:),
            Some(NSWorkspaceDidWakeNotification),
            None,
        );
    }
}

/// The controller logged every machine off: end the process.
pub fn terminate(mtm: MainThreadMarker) {
    QUITTING.with(|q| q.set(true));
    let app = NSApplication::sharedApplication(mtm);
    if ASKED.with(|a| a.replace(false)) {
        app.replyToApplicationShouldTerminate(true);
    } else {
        app.terminate(None);
    }
}

/// A menu item running the shell's command for widget `id` (as its button would).
fn ours(mtm: MainThreadMarker, title: &str, id: &'static str, key: &str) -> Retained<NSMenuItem> {
    let cb = crate::ui::callback(
        mtm,
        Box::new(move |_| crate::ui::later(Box::new(move || crate::window::command(id)))),
    );
    // SAFETY: a plain menu item; its target is kept in MENU_TARGETS for the app's life.
    let it = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &ns(title),
            Some(sel!(fire:)),
            &ns(key),
        )
    };
    // SAFETY: as above.
    unsafe { it.setTarget(Some(&cb)) };
    MENU_TARGETS.with(|t| t.borrow_mut().push(cb));
    it
}

/// The app's icon, so a build run outside its bundle (the flows) shows it in the Dock and alerts.
fn set_icon(app: &NSApplication) {
    use objc2::AllocAnyThread;
    let bytes = include_bytes!("../bundle/icon-512.png");
    let data = NSData::with_bytes(bytes);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        // SAFETY: setting the shared application's icon on the main thread.
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

fn item(mtm: MainThreadMarker, title: &str, action: Sel, key: &str) -> Retained<NSMenuItem> {
    // SAFETY: a plain menu item; its action goes up the responder chain.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &ns(title),
            Some(action),
            &ns(key),
        )
    }
}

fn submenu(mtm: MainThreadMarker, bar: &NSMenu, title: &str, items: Vec<Retained<NSMenuItem>>) {
    let m = NSMenu::initWithTitle(mtm.alloc(), &ns(title));
    for it in items {
        m.addItem(&it);
    }
    let top = NSMenuItem::new(mtm);
    top.setSubmenu(Some(&m));
    bar.addItem(&top);
}

/// The menu bar: the app menu, Edit (so fields take Cmd+C / Cmd+V) and Window.
fn menu_bar(mtm: MainThreadMarker) {
    let bar = NSMenu::new(mtm);
    let hide_others = item(mtm, "Hide Others", sel!(hideOtherApplications:), "h");
    hide_others
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    submenu(
        mtm,
        &bar,
        "legcon",
        vec![
            item(mtm, "About legcon", sel!(orderFrontStandardAboutPanel:), ""),
            NSMenuItem::separatorItem(mtm),
            ours(
                mtm,
                "Set up legcon...",
                legcon::controller::ids::MACHINES_SETUP,
                "",
            ),
            ours(
                mtm,
                "Options...",
                legcon::controller::ids::MACHINES_OPTIONS,
                ",",
            ),
            ours(
                mtm,
                "Disconnect all",
                legcon::controller::ids::MACHINES_DISCONNECT_ALL,
                "",
            ),
            NSMenuItem::separatorItem(mtm),
            item(mtm, "Hide legcon", sel!(hide:), "h"),
            hide_others,
            item(mtm, "Show All", sel!(unhideAllApplications:), ""),
            NSMenuItem::separatorItem(mtm),
            item(mtm, "Quit legcon", sel!(terminate:), "q"),
        ],
    );
    submenu(
        mtm,
        &bar,
        "Edit",
        vec![
            item(mtm, "Undo", sel!(undo:), "z"),
            item(mtm, "Redo", sel!(redo:), "Z"),
            NSMenuItem::separatorItem(mtm),
            item(mtm, "Cut", sel!(cut:), "x"),
            item(mtm, "Copy", sel!(copy:), "c"),
            item(mtm, "Paste", sel!(paste:), "v"),
            item(mtm, "Select All", sel!(selectAll:), "a"),
        ],
    );
    let window = vec![
        item(mtm, "Minimize", sel!(performMiniaturize:), "m"),
        item(mtm, "Zoom", sel!(performZoom:), ""),
    ];
    submenu(mtm, &bar, "Window", window);
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&bar));
}

/// Run the application until it quits.
pub fn run() {
    let mtm = MainThreadMarker::new().expect("main runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate: Retained<Delegate> = {
        let this = mtm.alloc::<Delegate>().set_ivars(());
        // SAFETY: NSObject's plain init.
        unsafe { msg_send![super(this), init] }
    };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|d| *d.borrow_mut() = Some(delegate));
    menu_bar(mtm);
    app.run();
}
