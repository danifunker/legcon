//! The main window (mockup: docs/mockups/connection-view.html), screen for screen the Win32
//! one: coloured connection tabs over the Machines page, the Keys page or a connection view
//! (header, two file panes, splitter, terminal area with Shell / Console / Log / Transfers).
//!
//! The view only renders `AppController` and turns input into its actions (CONTRIBUTING.md).
//! Effects run with the controller released, since their dialogs run modal loops.

use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use block2::RcBlock;
use legcon::controller::app::{self, default_dir, AppController, Tab};
use legcon::controller::connection::{self as conn, HeaderInfo, Row, Side, TermTab};
use legcon::controller::keys::{self, Chord, Command, Key, Mods};
use legcon::controller::ssh_keys as sshkeys;
use legcon::controller::{ids, ConnState, Controller, Effect, TabColor};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSBackingStoreType, NSButton, NSColor, NSCursor, NSEvent,
    NSEventMask, NSEventModifierFlags, NSScrollView, NSSegmentedControl,
    NSStandardKeyBindingResponding, NSTextField, NSTextView, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSPoint, NSRect, NSSize};

use crate::effects::{self, Ask, Entry};
use crate::form;
use crate::header::Header;
use crate::table::{Dropped, Icon, Line, Table};
use crate::tabstrip::{TabLook, TabStrip, TAB_H};
use crate::term_view::Term;
use crate::ui::{self, ns};
use crate::wake::MacWake;

/// The strip holds Machines and Keys, then one tab per connection.
const FIRST_CONN: usize = 2;

pub struct Pane {
    bg: Retained<ui::Canvas>,
    places: Retained<NSButton>,
    title: Retained<NSTextField>,
    home: Retained<NSButton>,
    other: Retained<NSButton>,
    path: Retained<NSTextField>,
    list: Rc<Table>,
}

pub struct Widgets {
    tabs: TabStrip,
    status: Retained<NSTextField>,
    m_buttons: Vec<Retained<NSButton>>,
    m_list: Rc<Table>,
    k_buttons: Vec<Retained<NSButton>>,
    k_list: Rc<Table>,
    header: Header,
    local: Pane,
    remote: Pane,
    splitter: Retained<Splitter>,
    term_tabs: Retained<NSSegmentedControl>,
    term: Term,
    term_scroll: Retained<NSScrollView>,
    term_text: Retained<NSTextView>,
    console: Retained<NSTextField>,
    console_send: Retained<NSButton>,
    q_buttons: Vec<(&'static str, Retained<NSButton>)>,
    q_list: Rc<Table>,
}

/// What was last drawn, so a refresh only touches what changed.
#[derive(Default, PartialEq)]
struct Drawn {
    tabs: Vec<TabLook>,
    machines: Vec<String>,
    keys: Vec<String>,
    local: Vec<Row>,
    remote: Vec<Row>,
    header: Option<HeaderInfo>,
    machine: String,
    /// The folders the lists were last filled from: a new one starts at the top.
    local_dir: String,
    remote_dir: String,
    queue_ids: Vec<u64>,
    term_text: String,
}

/// Everything a layout depends on.
type LayoutKey = (i64, i64, i64, u32, bool, i64);

pub struct Shell {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    content: Retained<NSView>,
    app: AppController,
    wake: Arc<MacWake>,
    w: Widgets,
    drawn: Drawn,
    /// Set while the splitter is dragged: the live share.
    drag: Option<f32>,
    split_area: (f64, f64),
    laid: Option<LayoutKey>,
    _delegate: Retained<MainDelegate>,
    _monitor: Option<Retained<AnyObject>>,
}

thread_local! {
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
    /// Remote rows the Finder asked for, by the folder they land in, until the batch is queued.
    static PROMISED: RefCell<Vec<(String, PathBuf)>> = const { RefCell::new(Vec::new()) };
}

/// The Finder redeems a dragged remote row: queue its download into the folder it chose.
fn promised(name: String, dir: PathBuf) {
    let first = PROMISED.with(|p| {
        let mut p = p.borrow_mut();
        p.push((name, dir));
        p.len() == 1
    });
    // The Finder asks once per row, back to back: one batch per folder, so one question.
    if first {
        ui::later(Box::new(|| {
            let mut all = PROMISED.with(|p| std::mem::take(&mut *p.borrow_mut()));
            while let Some((_, dir)) = all.first().cloned() {
                let (here, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|(_, d)| *d == dir);
                all = rest;
                let names = here.into_iter().map(|(n, _)| n).collect();
                conn_action(conn::Action::CopyOut(names, dir));
            }
        }));
    }
}

/// Borrow the shell; `None` while it is already borrowed (a nested call from a modal loop).
fn with<R>(f: impl FnOnce(&mut Shell) -> R) -> Option<R> {
    SHELL.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
}

define_class!(
    /// The main window's delegate: closing it quits (after logging off), resizing lays out.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCMainDelegate"]
    pub struct MainDelegate;

    unsafe impl NSObjectProtocol for MainDelegate {}

    unsafe impl NSWindowDelegate for MainDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn should_close(&self, _sender: &NSWindow) -> bool {
            ui::later(Box::new(quit));
            false
        }

        #[unsafe(method(windowDidResize:))]
        fn did_resize(&self, _note: &NSNotification) {
            refresh();
        }
    }
);

define_class!(
    /// The bar between the files and the terminal area: drag it to share the height.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCSplitter"]
    pub struct Splitter;

    unsafe impl NSObjectProtocol for Splitter {}

    impl Splitter {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            // Its replacement needs macOS 15.
            #[allow(deprecated)]
            let cursor = NSCursor::resizeUpDownCursor();
            self.addCursorRect_cursor(self.bounds(), &cursor);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let (cx, cy) = (b.size.width / 2.0, b.size.height / 2.0);
            ui::fill(ui::rect(cx - 20.0, cy - 1.0, 40.0, 2.0), &NSColor::tertiaryLabelColor());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            split_begin();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            // SAFETY: a plain read of the view's parent.
            if let Some(sv) = unsafe { self.superview() } {
                let p = sv.convertPoint_fromView(event.locationInWindow(), None);
                split_move(p.y);
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            split_end();
        }
    }
);

/// The Transfers buttons in order: widget id and the widest label it shows.
const QUEUE_BUTTONS: [(&str, &str); 9] = [
    (ids::QUEUE_RUN, "Pause queue"),
    (ids::QUEUE_PAUSE, "Pause"),
    (ids::QUEUE_RESUME, "Resume"),
    (ids::QUEUE_RETRY, "Retry"),
    (ids::QUEUE_REMOVE, "Remove"),
    (ids::QUEUE_UP, "Up"),
    (ids::QUEUE_DOWN, "Down"),
    (ids::QUEUE_CLEAR, "Clear finished"),
    (ids::QUEUE_CANCEL, "Cancel all"),
];

const MACHINE_BUTTONS: [(&str, &str, f64); 7] = [
    (ids::MACHINES_ADD, "Add machine...", 130.0),
    (ids::MACHINES_OPEN, "Open", 80.0),
    (ids::MACHINES_REMOVE, "Remove", 90.0),
    (ids::MACHINES_SETTINGS, "Settings...", 100.0),
    (ids::MACHINES_OPTIONS, "Options...", 100.0),
    (ids::MACHINES_DISCONNECT_ALL, "Disconnect all", 130.0),
    (ids::MACHINES_SETUP, "Set up...", 100.0),
];

const KEY_BUTTONS: [(&str, &str, f64); 5] = [
    (ids::KEYS_NEW, "New key...", 110.0),
    (ids::KEYS_IMPORT, "Import...", 100.0),
    (ids::KEYS_EXPORT, "Export...", 100.0),
    (ids::KEYS_COPY, "Copy public key", 140.0),
    (ids::KEYS_DELETE, "Delete", 90.0),
];

pub fn start(mtm: MainThreadMarker) {
    let wake = Arc::new(MacWake::default());
    let app = match AppController::open(&default_dir(), wake.clone()) {
        Ok(a) => a,
        Err(e) => {
            effects::note(
                mtm,
                "legcon",
                &format!("Could not read the settings: {e:#}"),
            );
            NSApplication::sharedApplication(mtm).terminate(None);
            return;
        }
    };
    legcon::controller::log::capture::set_wake(wake.clone());
    // legcon draws its own connection tabs; macOS's window tabs would be a second set.
    NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // SAFETY: the designated initialiser; the shell keeps the window.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1180.0, 760.0)),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the shell owns the window, so AppKit must not release it on close.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns("legcon"));
    window.setMinSize(NSSize::new(760.0, 480.0));
    window.setAccessibilityIdentifier(Some(&ns("main")));
    let content = ui::flipped(mtm);
    window.setContentView(Some(&content));
    let delegate: Retained<MainDelegate> = {
        let this = mtm.alloc::<MainDelegate>().set_ivars(());
        // SAFETY: NSObject's plain init.
        unsafe { msg_send![super(this), init] }
    };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    let w = build(mtm, &content);
    let monitor = key_monitor();
    SHELL.with(|s| {
        *s.borrow_mut() = Some(Shell {
            mtm,
            window: window.clone(),
            content,
            app,
            wake,
            w,
            drawn: Drawn::default(),
            drag: None,
            split_area: (0.0, 0.0),
            laid: None,
            _delegate: delegate,
            _monitor: monitor,
        })
    });
    // Where the user last left it; the first time, centred.
    if !window.setFrameUsingName(&ns("legcon.main")) {
        window.center();
    }
    window.setFrameAutosaveName(&ns("legcon.main"));
    window.makeKeyAndOrderFront(None);
    let ns_app = NSApplication::sharedApplication(mtm);
    if objc2::available!(macos = 14.0) {
        ns_app.activate();
    } else {
        // Its replacement, activate, needs macOS 14.
        #[allow(deprecated)]
        ns_app.activateIgnoringOtherApps(true);
    }
    refresh();
    dispatch(app::Action::Start);
}

pub fn main_window() -> Option<Retained<NSWindow>> {
    with(|s| s.window.clone())
}

fn pane(mtm: MainThreadMarker, content: &NSView, local: bool) -> Pane {
    let (places_id, home_id, path_id, list_id) = if local {
        (
            ids::LOCAL_PLACES,
            ids::LOCAL_HOME,
            ids::LOCAL_PATH,
            ids::LOCAL_LIST,
        )
    } else {
        (
            ids::REMOTE_PLACES,
            ids::REMOTE_HOME,
            ids::REMOTE_PATH,
            ids::REMOTE_LIST,
        )
    };
    let mut cols: Vec<(&str, i32)> = vec![("Name", 220), ("Size", 80), ("Modified", 135)];
    if !local {
        cols.push(("Permissions", 100));
    }
    let bg = ui::canvas(
        mtm,
        Rc::new(|b| {
            ui::fill(b, &NSColor::controlBackgroundColor());
            ui::fill(
                ui::rect(0.0, 0.0, b.size.width, 32.0),
                &NSColor::windowBackgroundColor(),
            );
            ui::fill(ui::rect(0.0, 32.0, b.size.width, 1.0), &ui::separator());
            ui::fill(ui::rect(0.0, 54.0, b.size.width, 1.0), &ui::separator());
            ui::frame(b, &ui::separator());
        }),
    );
    content.addSubview(&bg);
    // The address bar: shows the folder, and goes to whatever path is typed into it.
    let path = ui::path_field(mtm, path_id);
    path.setFont(Some(&ui::mono()));
    path.setPlaceholderString(Some(&ns("Type a path, then Return")));
    let p = Pane {
        bg,
        places: ui::button(mtm, "Places", places_id),
        title: ui::label(mtm, if local { "This computer" } else { "" }),
        home: ui::button(mtm, "Home", home_id),
        other: if local {
            ui::button(mtm, "Go to...", ids::LOCAL_GO_TO)
        } else {
            ui::button(mtm, "Up", "remote.up")
        },
        path,
        list: Rc::new(Table::new(mtm, list_id, &cols, true)),
    };
    p.title.setFont(Some(&ui::bold()));
    for v in [
        &*p.places as &NSView,
        &p.title,
        &p.home,
        &p.other,
        &p.path,
        &p.list.scroll,
    ] {
        content.addSubview(v);
    }
    let side = if local { Side::Local } else { Side::Remote };
    if !local {
        p.list.promise_files(Rc::new(promised));
    }
    p.list.on_activate(Rc::new(move || open_row(side)));
    p.list.on_menu(Rc::new(move || file_menu(side)));
    p.list.on_sort(Rc::new(move |i| {
        if let Some(by) = conn::SORT_COLUMNS.get(i) {
            conn_action(conn::Action::SortBy(side, *by));
        }
    }));
    p.list.drag_and_drop(
        if local { "local" } else { "remote" },
        Rc::new(move |d| match d {
            Dropped::Rows(from) => {
                let from = if from == "local" {
                    Side::Local
                } else {
                    Side::Remote
                };
                if from != side {
                    conn_action(conn::Action::Copy(from, selected_names(from)));
                }
            }
            Dropped::Files(paths) => conn_action(conn::Action::Drop(side, paths)),
        }),
    );
    p
}

fn build(mtm: MainThreadMarker, content: &NSView) -> Widgets {
    let tabs = TabStrip::new(mtm);
    content.addSubview(&tabs.view);
    let status = ui::label(mtm, "");
    status.setTextColor(Some(&NSColor::secondaryLabelColor()));
    ui::set_id(&status, "status");
    content.addSubview(&status);
    let m_buttons: Vec<_> = MACHINE_BUTTONS
        .iter()
        .map(|(id, t, _)| ui::button(mtm, t, id))
        .collect();
    let m_list = Rc::new(Table::new(
        mtm,
        ids::MACHINES_LIST,
        &[
            ("Name", 170),
            ("Address", 170),
            ("Services", 170),
            ("State", 120),
            ("Password", 150),
        ],
        false,
    ));
    m_list.on_activate(Rc::new(|| {
        if let Some(id) = selected_machine() {
            dispatch(app::Action::OpenMachine(id));
        }
    }));
    let k_buttons: Vec<_> = KEY_BUTTONS
        .iter()
        .map(|(id, t, _)| ui::button(mtm, t, id))
        .collect();
    let k_list = Rc::new(Table::new(
        mtm,
        ids::KEYS_LIST,
        &[
            ("Name", 150),
            ("Kind", 140),
            ("Used by", 150),
            ("Comment", 140),
            ("Fingerprint (SHA256)", 380),
        ],
        false,
    ));
    for b in m_buttons.iter().chain(&k_buttons) {
        content.addSubview(b);
    }
    content.addSubview(&m_list.scroll);
    content.addSubview(&k_list.scroll);
    let header = Header::new(mtm);
    content.addSubview(&header.view);
    let local = pane(mtm, content, true);
    let remote = pane(mtm, content, false);
    let splitter: Retained<Splitter> = {
        let this = mtm.alloc::<Splitter>().set_ivars(());
        // SAFETY: NSView's designated initialiser.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    };
    ui::set_id(&splitter, ids::CONN_SPLITTER);
    splitter.setAccessibilityElement(true);
    // SAFETY: AppKit's own role constant.
    splitter.setAccessibilityRole(Some(unsafe { objc2_app_kit::NSAccessibilitySplitterRole }));
    content.addSubview(&splitter);
    let labels: Vec<_> = TermTab::ALL.iter().map(|t| ns(t.label())).collect();
    let labels: Vec<&objc2_foundation::NSString> = labels.iter().map(|l| &**l).collect();
    let t = ui::target(mtm);
    for p in [&local.path, &remote.path] {
        // SAFETY: the target is never dropped.
        unsafe {
            p.setTarget(Some(&t));
            p.setAction(Some(objc2::sel!(act:)));
        }
    }
    // SAFETY: the target is never dropped.
    let term_tabs = unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &objc2_foundation::NSArray::from_slice(&labels),
            objc2_app_kit::NSSegmentSwitchTracking::SelectOne,
            Some(&t),
            Some(objc2::sel!(act:)),
            mtm,
        )
    };
    ui::set_id(&term_tabs, ids::TERM_TABS);
    content.addSubview(&term_tabs);
    let term = Term::new(mtm);
    content.addSubview(&term.view);
    let term_scroll = NSTextView::scrollableTextView(mtm);
    let term_text = term_scroll
        .documentView()
        .and_then(|v| v.downcast::<NSTextView>().ok())
        .expect("a scrollable text view holds a text view");
    term_text.setEditable(false);
    term_text.setFont(Some(&ui::mono()));
    ui::set_id(&term_text, "term.text");
    content.addSubview(&term_scroll);
    let console = ui::field(mtm, "", "console.input", false);
    console.setFont(Some(&ui::mono()));
    // SAFETY: the target is never dropped.
    unsafe {
        console.setTarget(Some(&t));
        console.setAction(Some(objc2::sel!(act:)));
    }
    if let Some(cell) = console.cell() {
        cell.setSendsActionOnEndEditing(false);
    }
    content.addSubview(&console);
    let console_send = ui::button(mtm, "Send", "console.send");
    content.addSubview(&console_send);
    let q_buttons: Vec<_> = QUEUE_BUTTONS
        .iter()
        .map(|(id, label)| (*id, ui::button(mtm, label, id)))
        .collect();
    for (_, b) in &q_buttons {
        content.addSubview(b);
    }
    let q_list = Rc::new(Table::new(mtm, ids::QUEUE_LIST, &conn::QUEUE_COLUMNS, true));
    q_list.on_select(Rc::new(refresh));
    q_list.on_menu(Rc::new(queue_menu));
    content.addSubview(&q_list.scroll);
    Widgets {
        tabs,
        status,
        m_buttons,
        m_list,
        k_buttons,
        k_list,
        header,
        local,
        remote,
        splitter,
        term_tabs,
        term,
        term_scroll,
        term_text,
        console,
        console_send,
        q_buttons,
        q_list,
    }
}

/// Apply an action, run its effects, redraw.
pub fn dispatch(action: app::Action) {
    let effects = with(|s| s.app.apply(action)).unwrap_or_default();
    run(effects);
    refresh();
}

pub fn tick() {
    let effects = with(|s| {
        s.wake.ticked();
        s.app.tick()
    })
    .unwrap_or_default();
    run(effects);
    refresh();
}

/// The window's close button or Cmd+Q: log every machine off, then quit.
pub fn quit() {
    dispatch(app::Action::Quit);
}

fn active_conn() -> Option<usize> {
    with(|s| match s.app.active() {
        Tab::Connection(i) => Some(i),
        Tab::Machines | Tab::Keys => None,
    })
    .flatten()
}

/// The machine of the active connection tab.
fn active_machine() -> Option<String> {
    let i = active_conn()?;
    with(|s| s.app.tab(i).map(|t| t.machine_id().to_string())).flatten()
}

pub fn conn_action(a: conn::Action) {
    if let Some(i) = active_conn() {
        dispatch(app::Action::Conn(i, a));
    }
}

/// Close the tab at strip index `i` (a connection tab only).
pub fn close_tab(i: usize) {
    if i >= FIRST_CONN {
        dispatch(app::Action::CloseTab(i - FIRST_CONN));
    }
}

/// A control's action, by its controller id.
pub fn command(id: &str) {
    let live = |st: ConnState| matches!(st, ConnState::Connected | ConnState::Busy);
    match id {
        ids::TAB_MACHINES => dispatch(app::Action::ShowMachines),
        ids::TAB_KEYS => dispatch(app::Action::ShowKeys),
        _ if id.starts_with(ids::TAB_CONN_PREFIX) => {
            let machine = &id[ids::TAB_CONN_PREFIX.len()..];
            let i = with(|s| s.app.tabs().iter().position(|t| t.machine_id() == machine)).flatten();
            if let Some(i) = i {
                dispatch(app::Action::SelectTab(i));
            }
        }
        ids::MACHINES_ADD => dispatch(app::Action::AddMachine),
        ids::MACHINES_OPEN => {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::OpenMachine(id));
            }
        }
        ids::MACHINES_REMOVE => {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::RemoveMachine(id));
            }
        }
        ids::MACHINES_SETTINGS => {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::EditMachine(id));
            }
        }
        ids::MACHINES_OPTIONS => dispatch(app::Action::EditOptions),
        ids::MACHINES_DISCONNECT_ALL => dispatch(app::Action::DisconnectAll),
        ids::MACHINES_SETUP => dispatch(app::Action::Setup),
        ids::KEYS_NEW => dispatch(app::Action::Keys(sshkeys::Action::New)),
        ids::KEYS_IMPORT => dispatch(app::Action::Keys(sshkeys::Action::Import)),
        ids::KEYS_EXPORT | ids::KEYS_COPY | ids::KEYS_DELETE => {
            let f: fn(String) -> sshkeys::Action = match id {
                ids::KEYS_EXPORT => sshkeys::Action::Export,
                ids::KEYS_COPY => sshkeys::Action::CopyPublic,
                _ => sshkeys::Action::Delete,
            };
            if let Some(name) = selected_key() {
                dispatch(app::Action::Keys(f(name)));
            }
        }
        ids::CONN_FILES => {
            let on = with(|s| s.drawn.header.as_ref().is_some_and(|h| live(h.files_state)));
            conn_action(if on == Some(true) {
                conn::Action::Disconnect
            } else {
                conn::Action::Connect
            });
        }
        ids::CONN_COMMANDS => {
            let on = with(|s| {
                s.drawn
                    .header
                    .as_ref()
                    .is_some_and(|h| live(h.commands_state))
            });
            conn_action(if on == Some(true) {
                conn::Action::ShellDisconnect
            } else {
                conn::Action::ShellConnect
            });
        }
        ids::CONN_SWAP => conn_action(conn::Action::Swap),
        ids::CONN_SETTINGS => {
            if let Some(id) = active_machine() {
                dispatch(app::Action::EditMachine(id));
            }
        }
        ids::CONN_CLOSE => {
            if let Some(i) = active_conn() {
                dispatch(app::Action::CloseTab(i));
            }
        }
        ids::LOCAL_PLACES => conn_action(conn::Action::Places(Side::Local)),
        ids::LOCAL_HOME => conn_action(conn::Action::LocalHome),
        ids::LOCAL_GO_TO => conn_action(conn::Action::GoTo(Side::Local)),
        ids::REMOTE_PLACES => conn_action(conn::Action::Places(Side::Remote)),
        ids::REMOTE_HOME => conn_action(conn::Action::RemoteHome),
        "remote.up" => conn_action(conn::Action::RemoteUp),
        ids::TERM_CONNECT => conn_action(conn::Action::ShellConnect),
        ids::TERM_TABS => {
            let n = with(|s| s.w.term_tabs.selectedSegment()).unwrap_or(-1);
            if let Some(t) = usize::try_from(n).ok().and_then(|n| TermTab::ALL.get(n)) {
                conn_action(conn::Action::SelectTerm(*t));
            }
        }
        "console.send" | "console.input" => send_console(),
        ids::LOCAL_PATH => path_typed(Side::Local),
        ids::REMOTE_PATH => path_typed(Side::Remote),
        _ if QUEUE_BUTTONS.iter().any(|(q, _)| *q == id) => queue_click(id),
        _ => {}
    }
}

/// Carry out effects; an answer becomes the matching action (by the effect's purpose).
fn run(effects: Vec<Effect>) {
    let Some(mtm) = with(|s| s.mtm) else {
        return;
    };
    for e in effects {
        match e {
            Effect::PickFolder(spec) => {
                let path =
                    effects::pick_folder(mtm, spec.purpose, &spec.title, spec.start.as_ref());
                match spec.purpose {
                    "app.home" => dispatch(app::Action::HomePicked(path)),
                    "keys.export" => {
                        dispatch(app::Action::Keys(sshkeys::Action::ExportPicked(path)))
                    }
                    _ => {}
                }
            }
            Effect::PickFile(spec) => {
                let path = effects::pick_file(mtm, spec.purpose, &spec.title, spec.start.as_ref());
                if spec.purpose == "keys.import" {
                    dispatch(app::Action::Keys(sshkeys::Action::ImportPicked(path)));
                }
            }
            Effect::AskPath(spec) => {
                let answer = effects::ask(
                    mtm,
                    &Ask {
                        title: &spec.title,
                        prompt: &spec.prompt,
                        initial: &spec.initial,
                        ok: "Go",
                        secret: false,
                        recent: Some(&spec.recent),
                        hint: &spec.hint,
                        validator: Some(spec.validator),
                        purpose: spec.purpose,
                    },
                );
                if spec.purpose == "local.go_to" {
                    conn_action(conn::Action::LocalGoToAnswered(answer.map(PathBuf::from)));
                }
            }
            Effect::Confirm(spec) => {
                let yes = effects::confirm(
                    mtm,
                    &spec.title,
                    &spec.message,
                    &spec.confirm_label,
                    spec.destructive,
                );
                match spec.purpose {
                    "machines.remove" => dispatch(app::Action::RemoveConfirmed(yes)),
                    "conn.delete" => conn_action(conn::Action::DeleteConfirmed(yes)),
                    "keys.delete" => {
                        dispatch(app::Action::Keys(sshkeys::Action::DeleteConfirmed(yes)))
                    }
                    _ => {}
                }
            }
            Effect::AskPassword(spec) => {
                let answer = effects::ask(
                    mtm,
                    &Ask {
                        title: "Password",
                        prompt: &spec.prompt,
                        initial: "",
                        ok: "Connect",
                        secret: true,
                        recent: None,
                        hint: "Kept for this tab only, unless the machine is set to save it.",
                        validator: None,
                        purpose: spec.purpose,
                    },
                );
                if spec.purpose == "conn.password" {
                    conn_action(conn::Action::PasswordAnswered(answer));
                }
            }
            Effect::AskText(spec) => {
                let answer = effects::ask(
                    mtm,
                    &Ask {
                        title: &spec.title,
                        prompt: &spec.prompt,
                        initial: &spec.initial,
                        ok: &spec.ok_label,
                        secret: false,
                        recent: None,
                        hint: "",
                        validator: Some(spec.validator),
                        purpose: spec.purpose,
                    },
                );
                match spec.purpose {
                    "conn.name" => conn_action(conn::Action::NewFolderAnswered(answer)),
                    conn::RENAME_PURPOSE => conn_action(conn::Action::CopyRenameAnswered(answer)),
                    "remote.go_to" => conn_action(conn::Action::RemoteGoToAnswered(answer)),
                    "machines.add.address" => dispatch(app::Action::AddAddressAnswered(answer)),
                    "machines.add.name" => dispatch(app::Action::AddNameAnswered(answer)),
                    "keys.import.name" => dispatch(app::Action::Keys(
                        sshkeys::Action::ImportNameAnswered(answer),
                    )),
                    _ => {}
                }
            }
            Effect::Choose(spec) => {
                let i =
                    effects::choose(mtm, &spec.title, &spec.message, &spec.choices, spec.dismiss);
                match spec.purpose {
                    "machines.add.service" => dispatch(app::Action::AddServiceChosen(i)),
                    "machines.add.found" => dispatch(app::Action::AddFoundAnswered(i)),
                    "conn.copy.conflict" => conn_action(conn::Action::CopyConflict(i)),
                    "conn.hostkey" => conn_action(conn::Action::HostKeyAnswered(i)),
                    "conn.keylogin.password" => {
                        conn_action(conn::Action::KeyLoginPasswordAnswered(i))
                    }
                    "conn.rsh.refused" => conn_action(conn::Action::RshRefusedAnswered(i)),
                    "conn.queue.many" => conn_action(conn::Action::QueueManyAnswered(i)),
                    "conn.queue.exists" => conn_action(conn::Action::QueueExistsAnswered(i)),
                    "conn.queue.item" => conn_action(conn::Action::QueueItemAnswered(i)),
                    "conn.queue.disconnect" => conn_action(conn::Action::DisconnectAnswered(i)),
                    conn::REJOIN_PURPOSE => conn_action(conn::Action::RejoinAnswered(i)),
                    conn::RESUME_PURPOSE => conn_action(conn::Action::ResumeQueueAnswered(i)),
                    legcon::controller::setup::FIRST_PURPOSE => {
                        dispatch(app::Action::SetupFirstAnswered(i))
                    }
                    _ => {}
                }
            }
            Effect::Form(spec) => {
                let answer = form::show(mtm, &spec);
                match spec.purpose {
                    legcon::controller::machine_form::PURPOSE => {
                        dispatch(app::Action::MachineFormAnswered(answer))
                    }
                    legcon::controller::options::PURPOSE => {
                        dispatch(app::Action::OptionsAnswered(answer))
                    }
                    legcon::controller::setup::PURPOSE => {
                        dispatch(app::Action::SetupAnswered(answer))
                    }
                    sshkeys::NEW_PURPOSE => {
                        dispatch(app::Action::Keys(sshkeys::Action::NewAnswered(answer)))
                    }
                    _ => {}
                }
            }
            Effect::Menu(spec) => {
                let anchor: Option<Retained<NSView>> = with(|s| match spec.anchor {
                    ids::LOCAL_PLACES => Some(Retained::into_super(Retained::into_super(
                        s.w.local.places.clone(),
                    ))),
                    ids::REMOTE_PLACES => Some(Retained::into_super(Retained::into_super(
                        s.w.remote.places.clone(),
                    ))),
                    _ => None,
                })
                .flatten();
                let chosen = effects::menu(mtm, anchor.as_deref(), &spec.items);
                match spec.purpose {
                    "local.places" => conn_action(conn::Action::PlaceChosen(Side::Local, chosen)),
                    "remote.places" => conn_action(conn::Action::PlaceChosen(Side::Remote, chosen)),
                    _ => {}
                }
            }
            Effect::Message(text) => {
                set_status(&text);
                effects::note(mtm, "legcon", &text);
            }
            Effect::CopyToClipboard(text) => {
                effects::copy_text(&text);
                set_status("Copied to the clipboard.");
            }
            Effect::Quit => crate::app::terminate(mtm),
        }
    }
}

fn set_status(text: &str) {
    if let Some(status) = with(|s| s.w.status.clone()) {
        ui::set_text(&status, text);
    }
}

fn state_text(s: ConnState) -> &'static str {
    match s {
        ConnState::Disconnected => "Not connected",
        ConnState::Connecting => "Connecting...",
        ConnState::Connected => "Connected",
        ConnState::Busy => "Busy",
        ConnState::Disconnecting => "Logging off...",
        ConnState::Lost => "Connection lost",
        ConnState::Idle => "Logged off while idle",
    }
}

fn lines(rows: &[Row], perms: bool) -> Vec<Line> {
    rows.iter()
        .map(|r| {
            let mut v = vec![r.name.clone(), r.size.clone(), r.modified.clone()];
            if perms {
                v.push(r.mode.clone());
            }
            Line {
                icon: Icon::File {
                    name: r.name.clone(),
                    dir: r.is_dir,
                },
                cells: v,
            }
        })
        .collect()
}

/// Bring every control in line with the controller.
pub fn refresh() {
    let mut resize = None;
    let status = with(|s| {
        let mtm = s.mtm;
        let looks: Vec<TabLook> = [("Machines", ids::TAB_MACHINES), ("Keys", ids::TAB_KEYS)]
            .into_iter()
            .map(|(t, id)| TabLook {
                id: id.into(),
                title: t.into(),
                color: None,
                state: None,
            })
            .chain(s.app.tabs().iter().map(|t| TabLook {
                id: format!("{}{}", ids::TAB_CONN_PREFIX, t.machine_id()),
                title: t.title(),
                color: Some(t.color().rgb()),
                state: Some(t.tab_state()),
            }))
            .collect();
        let active = s.app.active();
        let at = match active {
            Tab::Machines => 0,
            Tab::Keys => 1,
            Tab::Connection(i) => i + FIRST_CONN,
        };
        s.w.tabs.set(mtm, &looks, at);
        s.drawn.tabs = looks;
        let on_machines = active == Tab::Machines;
        let on_keys = active == Tab::Keys;
        let on_conn = matches!(active, Tab::Connection(_));
        for b in &s.w.k_buttons {
            ui::set_visible(b, on_keys);
        }
        ui::set_visible(&s.w.k_list.scroll, on_keys);
        let keys = s.app.keys();
        s.drawn.keys = keys.iter().map(|k| k.name.clone()).collect();
        s.w.k_list.set_rows(
            keys.into_iter()
                .map(|k| Line {
                    icon: Icon::File {
                        name: "key.pem".into(),
                        dir: false,
                    },
                    cells: vec![k.name, k.kind, k.used_by, k.comment, k.fingerprint],
                })
                .collect(),
        );
        for b in &s.w.m_buttons {
            ui::set_visible(b, on_machines);
        }
        ui::set_visible(&s.w.m_list.scroll, on_machines);
        for p in [&s.w.local, &s.w.remote] {
            for v in [
                &*p.places as &NSView,
                &p.title,
                &p.home,
                &p.other,
                &p.path,
                &p.list.scroll,
            ] {
                ui::set_visible(v, on_conn);
            }
            ui::set_visible(&p.bg, on_conn);
        }
        for v in [&*s.w.header.view, &s.w.term_tabs, &s.w.splitter] {
            ui::set_visible(v, on_conn);
        }
        let machines = s.app.machines();
        s.drawn.machines = machines.iter().map(|m| m.id.clone()).collect();
        s.w.m_list.set_rows(
            machines
                .into_iter()
                .map(|m| {
                    let pw = if m.password_saved {
                        "Saved (encoded)"
                    } else {
                        "Asks"
                    };
                    Line {
                        icon: Icon::Color(m.color.rgb()),
                        cells: vec![
                            m.name,
                            m.address,
                            m.services,
                            state_text(m.state).into(),
                            pw.into(),
                        ],
                    }
                })
                .collect(),
        );
        let Tab::Connection(i) = active else {
            for v in [
                &*s.w.term.view as &NSView,
                &s.w.term_scroll,
                &s.w.console,
                &s.w.console_send,
                &s.w.q_list.scroll,
            ] {
                ui::set_visible(v, false);
            }
            for (_, b) in &s.w.q_buttons {
                ui::set_visible(b, false);
            }
            s.drawn.header = None;
            if on_keys {
                return s.app.keys_busy().unwrap_or_else(|| {
                    format!(
                        "{} keys, kept in legcon's config folder. Export copies one out; nothing is written to ~/.ssh.",
                        s.drawn.keys.len()
                    )
                });
            }
            return format!(
                "{} machines saved. Double-click one to open it; right-click a tab to colour it.",
                s.drawn.machines.len()
            );
        };
        let Some(t) = s.app.tab(i) else {
            return String::new();
        };
        if s.drawn.machine != t.machine_id() {
            s.drawn.local.clear();
            s.drawn.remote.clear();
            s.drawn.machine = t.machine_id().to_string();
            // Empty the lists too: a tab with no rows would otherwise keep the last tab's.
            s.w.local.list.set_rows(Vec::new());
            s.w.remote.list.set_rows(Vec::new());
            s.w.q_list.set_rows(Vec::new());
            s.drawn.queue_ids.clear();
        }
        let info = t.header_info();
        if s.drawn.header.as_ref() != Some(&info) {
            s.w.header.update(&info);
            s.drawn.header = Some(info);
        }
        ui::set_text(&s.w.remote.title, &t.title());
        // Not while the user is typing a path there.
        for (field, path) in [
            (&s.w.local.path, t.local_path()),
            (&s.w.remote.path, t.remote_path()),
        ] {
            if field.currentEditor().is_none() {
                ui::set_text(field, &path);
            }
        }
        ui::set_enabled(&s.w.remote.home, t.can_disconnect());
        ui::set_enabled(&s.w.remote.other, t.can_disconnect());
        for (list, side) in [
            (&s.w.local.list, Side::Local),
            (&s.w.remote.list, Side::Remote),
        ] {
            let sort = t.sort(side);
            if let Some(col) = conn::SORT_COLUMNS.iter().position(|b| *b == sort.by) {
                list.set_sort(col, sort.descending);
            }
        }
        s.w.local.list.set_dir(Some(PathBuf::from(t.local_path())));
        let local = t.local_rows();
        if local != s.drawn.local {
            s.w.local.list.set_rows(lines(&local, false));
            if s.drawn.local_dir != t.local_path() {
                s.w.local.list.to_top();
                s.drawn.local_dir = t.local_path();
            }
            s.drawn.local = local;
        }
        let remote = t.remote_rows();
        if remote != s.drawn.remote {
            s.w.remote.list.set_rows(lines(&remote, true));
            if s.drawn.remote_dir != t.remote_path() {
                s.w.remote.list.to_top();
                s.drawn.remote_dir = t.remote_path();
            }
            s.drawn.remote = remote;
        }
        let term = t.term_tab();
        if let Some(n) = TermTab::ALL.iter().position(|x| *x == term) {
            if s.w.term_tabs.selectedSegment() != n as isize {
                s.w.term_tabs.setSelectedSegment(n as isize);
            }
        }
        let on_shell = term == TermTab::Shell;
        ui::set_visible(&s.w.term.view, on_shell);
        let on_queue = term == TermTab::Transfers;
        ui::set_visible(&s.w.term_scroll, !on_shell && !on_queue);
        ui::set_visible(&s.w.q_list.scroll, on_queue);
        for (_, b) in &s.w.q_buttons {
            ui::set_visible(b, on_queue);
        }
        if on_queue {
            let rows = t.queue_rows();
            let picked = selected_in(&s.w.q_list, &s.drawn.queue_ids);
            let ids: Vec<u64> = rows.iter().map(|r| r.id).collect();
            let lines: Vec<Line> = rows
                .iter()
                .map(|r| Line {
                    icon: Icon::File {
                        name: r.name.clone(),
                        dir: r.name.ends_with('/'),
                    },
                    cells: vec![
                        r.name.clone(),
                        r.size.clone(),
                        r.direction.clone(),
                        r.status.clone(),
                        r.progress.clone(),
                        r.from.clone(),
                        r.to.clone(),
                    ],
                })
                .collect();
            s.w.q_list.set_rows(lines);
            if ids != s.drawn.queue_ids {
                // Rows moved or went: keep the same entries selected.
                let keep: Vec<usize> = ids
                    .iter()
                    .enumerate()
                    .filter(|(_, id)| picked.contains(id))
                    .map(|(i, _)| i)
                    .collect();
                s.w.q_list.select_rows(&keep);
                s.drawn.queue_ids = ids;
            }
            let picked = selected_in(&s.w.q_list, &s.drawn.queue_ids);
            for b in t.queue_buttons(&picked) {
                if let Some((_, h)) = s.w.q_buttons.iter().find(|(id, _)| *id == b.id) {
                    ui::set_title(h, b.label);
                    ui::set_enabled(h, b.enabled);
                }
            }
        }
        let on_console = term == TermTab::Console;
        ui::set_visible(&s.w.console, on_console);
        ui::set_visible(&s.w.console_send, on_console);
        ui::set_enabled(&s.w.console, t.can_console());
        ui::set_enabled(&s.w.console_send, t.can_console());
        if !on_shell && !on_queue {
            let text = t.term_text();
            if s.drawn.term_text != text {
                s.w.term_text.setString(&ns(&ui::plain(&text)));
                // Show the newest lines, as a log viewer does.
                // SAFETY: a plain scroll of our own text view.
                unsafe { s.w.term_text.scrollToEndOfDocument(None) };
                s.drawn.term_text = text;
            }
        }
        let screen = if t.shell_state() == ConnState::Connected {
            t.shell_screen()
        } else {
            None
        };
        s.w.term.set(screen, &t.shell_message());
        // On every connection tab, not just Shell: a shell opened from another tab starts at
        // the size it is shown at, rather than a default it is squeezed from later.
        if s.w.term.view.bounds().size.width > 0.0 {
            resize = Some(t.shell_size());
        }
        t.status_line()
    });
    if let Some(text) = status {
        set_status(&text);
    }
    layout();
    place_term_connect();
    // The terminal's size follows its area, also when the area changed while another tab showed.
    if let Some(had) = resize {
        if let Some(grid) = with(|s| s.w.term.grid()) {
            if grid != had {
                conn_action(conn::Action::ShellResize(grid.0, grid.1));
            }
        }
    }
}

/// Show the terminal's Connect button while no session runs and one can be opened.
fn place_term_connect() {
    with(|s| {
        let (show, label) = match s.app.active() {
            Tab::Connection(i) => s.app.tab(i).map_or((false, String::new()), |t| {
                (
                    t.can_shell_connect() && t.term_tab() == TermTab::Shell,
                    t.shell_connect_label(),
                )
            }),
            Tab::Machines | Tab::Keys => (false, String::new()),
        };
        s.w.term.place_connect(show, &label);
    });
}

/// Place every control for the current size, tab and layout; skipped when nothing moved.
fn layout() {
    with(|s| {
        let b = s.content.bounds();
        let (w, h) = (b.size.width, b.size.height);
        let key: LayoutKey = match s.app.active() {
            Tab::Machines => (w as i64, h as i64, -1, 0, false, -1),
            Tab::Keys => (w as i64, h as i64, -3, 0, false, -1),
            Tab::Connection(i) => match s.app.tab(i) {
                Some(t) => {
                    let l = t.layout();
                    let share = s.drag.unwrap_or(l.files_share);
                    (
                        w as i64,
                        h as i64,
                        i as i64,
                        share.to_bits(),
                        l.swapped,
                        TermTab::ALL
                            .iter()
                            .position(|x| *x == t.term_tab())
                            .unwrap_or(0) as i64,
                    )
                }
                None => (w as i64, h as i64, -2, 0, false, -1),
            },
        };
        if s.laid == Some(key) {
            return;
        }
        s.laid = Some(key);
        let m = 8.0;
        let status_h = 22.0;
        ui::place(&s.w.status, (m, h - status_h, w - 2.0 * m, status_h - 4.0));
        ui::place(&s.w.tabs.view, (0.0, 6.0, w, TAB_H));
        let top = 6.0 + TAB_H;
        let bottom = h - status_h - 2.0;
        let btn = 28.0;
        let row = |buttons: &[Retained<NSButton>], widths: &[f64], list: &Table| {
            let mut x = m;
            for (b, bw) in buttons.iter().zip(widths) {
                ui::place(b, (x, top + m, *bw, btn));
                x += bw + 4.0;
            }
            ui::place(
                &list.scroll,
                (
                    m,
                    top + btn + 2.0 * m,
                    w - 2.0 * m,
                    bottom - top - btn - 2.0 * m,
                ),
            );
        };
        match s.app.active() {
            Tab::Machines => {
                let widths: Vec<f64> = MACHINE_BUTTONS.iter().map(|b| b.2).collect();
                return row(&s.w.m_buttons, &widths, &s.w.m_list);
            }
            Tab::Keys => {
                let widths: Vec<f64> = KEY_BUTTONS.iter().map(|b| b.2).collect();
                return row(&s.w.k_buttons, &widths, &s.w.k_list);
            }
            Tab::Connection(_) => {}
        }
        let Tab::Connection(i) = s.app.active() else {
            return;
        };
        let Some(t) = s.app.tab(i) else {
            return;
        };
        let layout = t.layout().clone();
        let on_console = t.term_tab() == TermTab::Console;
        let header_h = 40.0;
        ui::place(&s.w.header.view, (0.0, top + 2.0, w, header_h));
        if let Some(info) = s.drawn.header.clone() {
            s.w.header.update(&info);
        }
        let area_top = top + 2.0 + header_h;
        let area_h = bottom - area_top;
        let split_h = 8.0;
        let share = f64::from(s.drag.unwrap_or(layout.files_share));
        let files_h = ((area_h - split_h) * share).floor();
        let term_h = area_h - split_h - files_h;
        let (files_y, split_y, term_y) = if layout.swapped {
            (area_top + term_h + split_h, area_top + term_h, area_top)
        } else {
            (area_top, area_top + files_h, area_top + files_h + split_h)
        };
        ui::place(&s.w.splitter, (0.0, split_y, w, split_h));
        s.split_area = (area_top, area_h - split_h);
        // Two panes, each a box: a header band, a path row, then the list.
        let pane_w = ((w - 3.0 * m) / 2.0).floor();
        let band = 32.0;
        let path_h = 22.0;
        for (k, p) in [&s.w.local, &s.w.remote].into_iter().enumerate() {
            let px = m + k as f64 * (pane_w + m);
            ui::place(&p.bg, (px, files_y, pane_w, files_h));
            let by = files_y + (band - 26.0) / 2.0;
            ui::place(&p.places, (px + 4.0, by, 70.0, 26.0));
            let other_w = if k == 0 { 80.0 } else { 56.0 };
            let right = px + pane_w - 4.0;
            ui::place(&p.other, (right - other_w, by, other_w, 26.0));
            ui::place(&p.home, (right - other_w - 4.0 - 66.0, by, 66.0, 26.0));
            let tx = px + 4.0 + 70.0 + 8.0;
            ui::place(
                &p.title,
                (tx, files_y + 8.0, right - other_w - 74.0 - tx, 18.0),
            );
            ui::place(
                &p.path,
                (px + 6.0, files_y + band + 3.0, pane_w - 12.0, 17.0),
            );
            let body_y = files_y + band + path_h + 1.0;
            ui::place(
                &p.list.scroll,
                (
                    px + 1.0,
                    body_y,
                    pane_w - 2.0,
                    files_h - band - path_h - 2.0,
                ),
            );
        }
        ui::place(&s.w.term_tabs, (m, term_y, 360.0, 24.0));
        let body = (m, term_y + 28.0, w - 2.0 * m, term_h - 28.0);
        ui::place(&s.w.term.view, body);
        let crow = if on_console { 34.0 } else { 0.0 };
        ui::place(&s.w.term_scroll, (body.0, body.1, body.2, body.3 - crow));
        if on_console {
            let y = body.1 + body.3 - crow + 6.0;
            ui::place(&s.w.console, (body.0, y, body.2 - 90.0, 24.0));
            ui::place(
                &s.w.console_send,
                (body.0 + body.2 - 84.0, y - 2.0, 84.0, 28.0),
            );
        }
        // Transfers: a row of buttons, the queue list under it.
        let mut x = body.0;
        for (id, b) in &s.w.q_buttons {
            let bw = QUEUE_BUTTONS
                .iter()
                .find(|(i, _)| i == id)
                .map_or(80.0, |(_, l)| 28.0 + 7.5 * l.len() as f64);
            ui::place(b, (x, body.1, bw, 26.0));
            x += bw + 4.0;
        }
        ui::place(
            &s.w.q_list.scroll,
            (body.0, body.1 + 32.0, body.2, body.3 - 32.0),
        );
    });
}

fn split_begin() {
    with(|s| {
        let share = match s.app.active() {
            Tab::Connection(i) => s.app.tab(i).map_or(0.75, |t| t.layout().files_share),
            Tab::Machines | Tab::Keys => 0.75,
        };
        s.drag = Some(share);
    });
}

fn split_move(y: f64) {
    let moved = with(|s| {
        s.drag?;
        let (top, h) = s.split_area;
        let swapped = match s.app.active() {
            Tab::Connection(i) => s.app.tab(i).is_some_and(|t| t.layout().swapped),
            Tab::Machines | Tab::Keys => false,
        };
        let f = (((y - top) / h.max(1.0)) as f32).clamp(conn::MIN_SHARE, 1.0 - conn::MIN_SHARE);
        s.drag = Some(if swapped { 1.0 - f } else { f });
        Some(())
    })
    .flatten();
    if moved.is_some() {
        layout();
    }
}

fn split_end() {
    if let Some(share) = with(|s| s.drag.take()).flatten() {
        conn_action(conn::Action::SetFilesShare(share));
    }
}

/// The names selected in one side's list.
fn selected_names(side: Side) -> Vec<String> {
    with(|s| {
        let (list, rows) = match side {
            Side::Local => (&s.w.local.list, &s.drawn.local),
            Side::Remote => (&s.w.remote.list, &s.drawn.remote),
        };
        list.selected_all()
            .into_iter()
            .filter_map(|i| rows.get(i).map(|r| r.name.clone()))
            .collect()
    })
    .unwrap_or_default()
}

/// Return or a double-click in a pane: go into the folder.
fn open_row(side: Side) {
    let row = with(|s| {
        let (list, rows) = match side {
            Side::Local => (&s.w.local.list, &s.drawn.local),
            Side::Remote => (&s.w.remote.list, &s.drawn.remote),
        };
        list.selected().and_then(|i| rows.get(i).cloned())
    })
    .flatten();
    if let Some(r) = row.filter(|r| r.is_dir) {
        conn_action(match side {
            Side::Local => conn::Action::LocalOpen(r.name),
            Side::Remote => conn::Action::RemoteOpen(r.name),
        });
    }
}

/// The queue entries selected in the Transfers list.
fn selected_in(list: &Table, ids: &[u64]) -> Vec<u64> {
    list.selected_all()
        .into_iter()
        .filter_map(|i| ids.get(i).copied())
        .collect()
}

/// What the Transfers buttons offer for the current selection.
fn queue_buttons() -> Option<(Vec<conn::QueueButton>, Vec<u64>)> {
    with(|s| {
        let i = match s.app.active() {
            Tab::Connection(i) => i,
            Tab::Machines | Tab::Keys => return None,
        };
        let picked = selected_in(&s.w.q_list, &s.drawn.queue_ids);
        s.app.tab(i).map(|t| (t.queue_buttons(&picked), picked))
    })
    .flatten()
}

fn queue_click(id: &str) {
    let Some((buttons, picked)) = queue_buttons() else {
        return;
    };
    if let Some(b) = buttons.into_iter().find(|b| b.id == id && b.enabled) {
        conn_action(conn::Action::Queue(b.op, picked));
    }
}

/// Right-click on the Transfers list: the same choices as its buttons.
fn queue_menu() {
    let (Some((buttons, picked)), Some(mtm)) = (queue_buttons(), with(|s| s.mtm)) else {
        return;
    };
    let mut entries = Vec::new();
    let mut map = Vec::new();
    for b in &buttons {
        if b.op == conn::QueueOp::ClearFinished {
            entries.push(Entry::Separator);
            map.push(None);
        }
        entries.push(Entry::item(b.label, b.enabled));
        map.push(Some(b.op));
    }
    if let Some(op) = effects::pop(mtm, None, &entries).and_then(|i| map.get(i).copied().flatten())
    {
        conn_action(conn::Action::Queue(op, picked));
    }
}

/// Return in an address bar: go to what was typed, and give the keyboard back to the list.
fn path_typed(side: Side) {
    let Some((text, list, window)) = with(|s| {
        let p = match side {
            Side::Local => &s.w.local,
            Side::Remote => &s.w.remote,
        };
        (
            p.path.stringValue().to_string(),
            p.list.view.clone(),
            s.window.clone(),
        )
    }) else {
        return;
    };
    window.makeFirstResponder(Some(&list));
    conn_action(conn::Action::PathTyped(side, text));
}

/// Put the cursor in a pane's address bar, its text selected to type over.
fn edit_path(side: Side) {
    let Some((path, window)) = with(|s| {
        let p = match side {
            Side::Local => &s.w.local,
            Side::Remote => &s.w.remote,
        };
        (p.path.clone(), s.window.clone())
    }) else {
        return;
    };
    window.makeFirstResponder(Some(&path));
}

fn send_console() {
    let Some(field) = with(|s| s.w.console.clone()) else {
        return;
    };
    let line = field.stringValue().to_string();
    if !line.trim().is_empty() {
        field.setStringValue(&ns(""));
        conn_action(conn::Action::ConsoleSend(line));
    }
}

/// The right-click menu of a file list: what can be done with its selection.
fn file_menu(side: Side) {
    let Some((mtm, other)) = with(|s| {
        let other = match s.app.active() {
            Tab::Connection(i) => s.app.tab(i).map(|t| match side {
                Side::Local => format!("Copy to {}", t.title()),
                Side::Remote => "Copy to this computer".to_string(),
            }),
            Tab::Machines | Tab::Keys => None,
        };
        (s.mtm, other)
    }) else {
        return;
    };
    let names = selected_names(side);
    let item = |label: String, on: bool, key: Option<(&'static str, bool)>| Entry::Item {
        label,
        enabled: on,
        checked: false,
        key,
    };
    let entries = vec![
        item(
            other.unwrap_or_else(|| "Copy".into()),
            !names.is_empty(),
            Some((effects::F6, false)),
        ),
        item("Add to queue".into(), !names.is_empty(), None),
        Entry::Separator,
        item("New folder...".into(), true, Some(("n", true))),
        item(
            "Rename...".into(),
            names.len() == 1,
            Some((effects::F2, false)),
        ),
        item(
            "Delete...".into(),
            !names.is_empty(),
            Some((effects::DEL, false)),
        ),
        Entry::Separator,
        item("Refresh".into(), true, Some((effects::F5, false))),
    ];
    match effects::pop(mtm, None, &entries) {
        Some(0) => conn_action(conn::Action::Copy(side, names)),
        Some(1) => conn_action(conn::Action::AddToQueue(side, names)),
        Some(3) => conn_action(conn::Action::NewFolder(side)),
        Some(4) => conn_action(conn::Action::Rename(side, names[0].clone())),
        Some(5) => conn_action(conn::Action::Delete(side, names)),
        Some(7) => conn_action(conn::Action::Refresh(side)),
        _ => {}
    }
}

/// A right-click on tab `tab` of the strip: its colour, key login, rsh trust, settings, close.
pub fn tab_menu(tab: usize) {
    let Some((mtm, id, current, keys, rsh)) = with(|s| {
        let t = s.app.tab(tab.checked_sub(FIRST_CONN)?)?;
        Some((
            s.mtm,
            t.machine_id().to_string(),
            t.color(),
            t.can_set_up_key_login(),
            (t.can_set_up_rsh_trust(), t.can_check_rsh()),
        ))
    })
    .flatten() else {
        return;
    };
    let mut entries: Vec<Entry> = TabColor::ALL
        .iter()
        .map(|c| Entry::Item {
            label: c.label().into(),
            enabled: true,
            checked: *c == current,
            key: None,
        })
        .collect();
    let n = entries.len();
    entries.extend([
        Entry::Separator,
        Entry::item("Set up key login...", keys),
        Entry::item("Set up rsh trust...", rsh.0),
        Entry::item("Check rsh from the machine's side", rsh.1),
        Entry::item("Settings...", true),
        Entry::item("Close tab (logs off)", true),
    ]);
    let i = tab - FIRST_CONN;
    match effects::pop(mtm, None, &entries) {
        Some(c) if c < n => dispatch(app::Action::SetColor(id, TabColor::ALL[c])),
        Some(c) if c == n + 1 => dispatch(app::Action::Conn(i, conn::Action::SetUpKeyLogin)),
        Some(c) if c == n + 2 => dispatch(app::Action::Conn(i, conn::Action::SetUpRshTrust)),
        Some(c) if c == n + 3 => dispatch(app::Action::Conn(i, conn::Action::CheckRsh)),
        Some(c) if c == n + 4 => dispatch(app::Action::EditMachine(id)),
        Some(c) if c == n + 5 => dispatch(app::Action::CloseTab(i)),
        _ => {}
    }
}

fn selected_key() -> Option<String> {
    with(|s| {
        s.w.k_list
            .selected()
            .and_then(|i| s.drawn.keys.get(i).cloned())
    })
    .flatten()
}

fn selected_machine() -> Option<String> {
    with(|s| {
        s.w.m_list
            .selected()
            .and_then(|i| s.drawn.machines.get(i).cloned())
    })
    .flatten()
}

/// Watch key presses in the main window before its controls see them: the key map's chords.
fn key_monitor() -> Option<Retained<AnyObject>> {
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the monitor a valid event for the duration of the call.
        let e = unsafe { event.as_ref() };
        if key(e) {
            std::ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    // SAFETY: the block lives as long as the monitor, which the shell keeps.
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block) }
}

/// The chord for a key press: the key without modifiers, Cmd as Primary.
fn chord(e: &NSEvent) -> Option<Chord> {
    let flags = e.modifierFlags();
    let key = match e.keyCode() {
        48 => Key::Tab,
        36 | 76 => Key::Enter,
        51 => Key::Backspace,
        117 => Key::Delete,
        126 => Key::Up,
        125 => Key::Down,
        122 => Key::F(1),
        120 => Key::F(2),
        99 => Key::F(3),
        118 => Key::F(4),
        96 => Key::F(5),
        97 => Key::F(6),
        98 => Key::F(7),
        100 => Key::F(8),
        101 => Key::F(9),
        109 => Key::F(10),
        103 => Key::F(11),
        111 => Key::F(12),
        _ => {
            // The key as typed without modifiers, so Shift+1 is still 1.
            let c = e
                .charactersByApplyingModifiers(NSEventModifierFlags::empty())?
                .to_string()
                .chars()
                .next()?;
            match c {
                '0'..='9' => Key::Digit(c as u8 - b'0'),
                c if c.is_ascii_alphabetic() => Key::Char(c.to_ascii_uppercase()),
                '[' | ']' => Key::Char(c),
                _ => return None,
            }
        }
    };
    Some(Chord {
        key,
        mods: Mods {
            primary: flags.contains(NSEventModifierFlags::Command),
            shift: flags.contains(NSEventModifierFlags::Shift),
            alt: false,
        },
    })
}

/// A key press in the main window: the key map's command, if any. True when it was used.
fn key(e: &NSEvent) -> bool {
    let Some((main, in_lists, in_term, remote_focused)) = with(|s| {
        (
            s.window.clone(),
            s.w.local.list.focused() || s.w.remote.list.focused() || s.w.m_list.focused(),
            s.w.term.focused(),
            s.w.remote.list.focused(),
        )
    }) else {
        return false;
    };
    if !e
        .window(main.mtm())
        .is_some_and(|w| std::ptr::eq(&*w, &*main))
    {
        return false;
    }
    let Some(c) = chord(e) else {
        return false;
    };
    let Some(cmd) = keys::command_for(c) else {
        return false;
    };
    // The terminal keeps every key a 1990s program might want; only Cmd+Shift chords escape it.
    if in_term && !(c.mods.primary && c.mods.shift) {
        return false;
    }
    if !c.mods.primary && !in_lists {
        return false;
    }
    let side = if remote_focused {
        Side::Remote
    } else {
        Side::Local
    };
    // Run from a block of its own: the command may open a dialog.
    ui::later(Box::new(move || run_command(cmd, side)));
    true
}

fn run_command(cmd: Command, side: Side) {
    let tabs = with(|s| s.app.tabs().len()).unwrap_or(0);
    let current = active_conn();
    match cmd {
        Command::Swap => conn_action(conn::Action::Swap),
        Command::LocalPlaces => conn_action(conn::Action::Places(Side::Local)),
        Command::RemotePlaces => conn_action(conn::Action::Places(Side::Remote)),
        Command::LocalHome => conn_action(conn::Action::LocalHome),
        Command::GoTo => conn_action(conn::Action::GoTo(side)),
        Command::EditPath => edit_path(side),
        Command::Refresh => conn_action(conn::Action::Refresh(side)),
        Command::Up => conn_action(match side {
            Side::Local => conn::Action::LocalUp,
            Side::Remote => conn::Action::RemoteUp,
        }),
        Command::CloseTab => {
            if let Some(i) = current {
                dispatch(app::Action::CloseTab(i));
            }
        }
        Command::NextTab | Command::PrevTab => {
            let n = tabs + FIRST_CONN;
            let on_keys = with(|s| s.app.active() == Tab::Keys).unwrap_or(false);
            let at = current.map_or(usize::from(on_keys), |i| i + FIRST_CONN);
            let next = if cmd == Command::NextTab {
                (at + 1) % n
            } else {
                (at + n - 1) % n
            };
            dispatch(match next {
                0 => app::Action::ShowMachines,
                1 => app::Action::ShowKeys,
                n => app::Action::SelectTab(n - FIRST_CONN),
            });
        }
        Command::ShowMachines => dispatch(app::Action::ShowMachines),
        Command::Copy => conn_action(conn::Action::Copy(side, selected_names(side))),
        Command::Rename => {
            if let [one] = selected_names(side).as_slice() {
                conn_action(conn::Action::Rename(side, one.clone()));
            }
        }
        Command::Delete => {
            let names = selected_names(side);
            if !names.is_empty() {
                conn_action(conn::Action::Delete(side, names));
            }
        }
        Command::NewFolder => conn_action(conn::Action::NewFolder(side)),
    }
}
