//! The main window (mockup: docs/mockups/connection-view.html): coloured connection tabs over
//! the Machines page or a connection view (header, two file panes, splitter, terminal area).
//!
//! The view only renders `AppController` and turns input into its actions (CONTRIBUTING.md).
//! Effects run with the controller released, since their dialogs pump messages.

use std::cell::RefCell;
use std::sync::Arc;

use legcon::controller::app::{self, default_dir, AppController, Tab};
use legcon::controller::connection::{self as conn, HeaderInfo, Row, Side, TermScreen, TermTab};
use legcon::controller::keys::{self, Chord, Command, Key, Mods};
use legcon::controller::ssh_keys as sshkeys;
use legcon::controller::{ids, ConnState, Controller, Effect, TabColor};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, RedrawWindow, ScreenToClient, SetBkColor, HBRUSH, HDC,
    PAINTSTRUCT, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_SCROLLCARET, EM_SETSEL, LVN_BEGINDRAG, LVN_ITEMACTIVATE, NMHDR, NM_RCLICK,
    ODS_SELECTED, SB_SETTEXTW, TCN_SELCHANGE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetCursor, ShowWindow, TrackPopupMenu, BN_CLICKED, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_MULTILINE,
    ES_READONLY, HMENU, IDC_ARROW, IDC_HAND, IDC_SIZENS, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM,
    WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONUP, WM_MOUSEMOVE, WM_NOTIFY, WM_PAINT,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WNDCLASSW, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW,
    WS_TABSTOP, WS_VSCROLL,
};

use crate::controls;
use crate::effects::{self, Ask};
use crate::form;
use crate::header::{self, Header};
use crate::icons;
use crate::paint;
use crate::term_view::{self, TermView};
use crate::ui;
use crate::wake::{Win32Wake, WM_APP_JOB};

/// Posted by the tab strip: close tab `wParam` (0 is Machines).
const WM_APP_CLOSE_TAB: u32 = WM_APP + 3;
/// The tab strip holds Machines and Keys, then one tab per connection.
const FIRST_CONN: usize = 2;
/// Posted by the tab strip: the colour menu for tab `wParam`.
const WM_APP_TAB_MENU: u32 = WM_APP + 4;

pub struct Pane {
    places: HWND,
    title: HWND,
    home: HWND,
    other: HWND,
    path: HWND,
    list: HWND,
}

pub struct Widgets {
    tabs: HWND,
    status: HWND,
    m_add: HWND,
    m_open: HWND,
    m_remove: HWND,
    m_settings: HWND,
    m_options: HWND,
    m_disconnect_all: HWND,
    m_setup: HWND,
    m_list: HWND,
    k_new: HWND,
    k_import: HWND,
    k_export: HWND,
    k_copy: HWND,
    k_delete: HWND,
    k_list: HWND,
    header: Header,
    local: Pane,
    remote: Pane,
    term_tabs: HWND,
    term: TermView,
    term_text: HWND,
    console: HWND,
    console_send: HWND,
    /// The Transfers tab: its buttons (by widget id) and the queue list.
    q_buttons: Vec<(&'static str, HWND)>,
    q_list: HWND,
}

/// One tab as the strip paints it.
#[derive(Clone, PartialEq)]
struct TabLook {
    title: String,
    color: Option<u32>,
    state: Option<ConnState>,
}

/// What was last drawn, so a refresh only touches what changed.
#[derive(Default, PartialEq)]
struct Drawn {
    tabs: Vec<TabLook>,
    machines: Vec<(i32, Vec<String>)>,
    keys: Vec<(i32, Vec<String>)>,
    local: Vec<Row>,
    remote: Vec<Row>,
    header: Option<HeaderInfo>,
    screen: Option<TermScreen>,
    machine: String,
    queue: Vec<(i32, Vec<String>)>,
    queue_ids: Vec<u64>,
}

/// Everything a layout depends on.
type LayoutKey = (i32, i32, i64, u32, bool, i64);

pub struct Shell {
    window: HWND,
    app: AppController,
    wake: Arc<Win32Wake>,
    w: Widgets,
    drawn: Drawn,
    /// Set while the splitter is dragged: the live share.
    drag: Option<f32>,
    splitter: RECT,
    split_area: (i32, i32),
    panes: [RECT; 2],
    laid: Option<LayoutKey>,
    /// Rows being dragged from one pane, until the button comes up.
    file_drag: Option<(Side, Vec<String>)>,
}

thread_local! {
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
    static BRUSHES: RefCell<Vec<(u32, isize)>> = const { RefCell::new(Vec::new()) };
}

/// Borrow the shell; `None` while it is already borrowed (a nested message during a call).
fn with<R>(f: impl FnOnce(&mut Shell) -> R) -> Option<R> {
    SHELL.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
}

fn brush(rgb: u32) -> HBRUSH {
    BRUSHES.with(|b| {
        if let Some((_, h)) = b.borrow().iter().find(|(c, _)| *c == rgb) {
            return HBRUSH(*h as _);
        }
        // SAFETY: a solid brush kept for the life of the process.
        let h = unsafe { CreateSolidBrush(paint::colorref(rgb)) };
        b.borrow_mut().push((rgb, h.0 as isize));
        h
    })
}

pub fn start() {
    let wake = Arc::new(Win32Wake::default());
    legcon::controller::log::capture::init();
    let app = match AppController::open(&default_dir(), wake.clone()) {
        Ok(a) => a,
        Err(e) => {
            effects::note(
                HWND::default(),
                "legcon",
                &format!("Could not read the settings: {e:#}"),
            );
            return;
        }
    };
    // SAFETY: registering our class and creating the main window on the UI thread.
    let window = unsafe {
        let inst = GetModuleHandleW(None).expect("module handle");
        let class = w!("LegconMain");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("legcon"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            Some(HMENU(std::ptr::null_mut())),
            Some(inst.into()),
            None,
        )
        .expect("the main window")
    };
    wake.set_window(window);
    legcon::controller::log::capture::set_wake(wake.clone());
    let w = build(window);
    SHELL.with(|s| {
        *s.borrow_mut() = Some(Shell {
            window,
            app,
            wake,
            w,
            drawn: Drawn::default(),
            drag: None,
            splitter: RECT::default(),
            split_area: (0, 0),
            panes: [RECT::default(); 2],
            laid: None,
            file_drag: None,
        })
    });
    // SAFETY: showing the window we created, and taking files dropped from Explorer.
    unsafe {
        let _ = ShowWindow(window, SW_SHOWNORMAL);
    }
    crate::ole_drop::register(window);
    refresh();
    dispatch(app::Action::Start);
}

fn pane(window: HWND, local: bool) -> Pane {
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
    let mut cols: Vec<(&str, i32)> = vec![("Name", 220), ("Size", 80), ("Modified", 90)];
    if !local {
        cols.push(("Permissions", 90));
    }
    let path = ui::child(
        window,
        w!("EDIT"),
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32 | ES_READONLY as u32),
        WINDOW_EX_STYLE(0),
        path_id,
    );
    ui::set_font(path, ui::mono_font_for(window));
    Pane {
        places: ui::button(window, "Places", places_id),
        title: ui::label(window, if local { "This computer" } else { "" }),
        home: ui::button(window, "Home", home_id),
        other: if local {
            ui::button(window, "Go to...", ids::LOCAL_GO_TO)
        } else {
            ui::button(window, "Up", "remote.up")
        },
        path,
        list: controls::list(window, list_id, &cols, icons::system_list(), false),
    }
}

fn build(window: HWND) -> Widgets {
    let term_text = ui::child(
        window,
        w!("EDIT"),
        "",
        WINDOW_STYLE(ES_MULTILINE as u32 | ES_READONLY as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        "term.text",
    );
    ui::set_font(term_text, ui::mono_font_for(window));
    let term_tabs = controls::tabs(window, ids::TERM_TABS, false);
    controls::set_tabs(
        term_tabs,
        &TermTab::ALL
            .iter()
            .map(|t| t.label().to_string())
            .collect::<Vec<_>>(),
    );
    let tabs = controls::tabs(window, "tabs", true);
    let dpi = ui::dpi(window);
    controls::set_tab_size(tabs, ui::scale(150, dpi), ui::scale(28, dpi));
    // SAFETY: subclassing our own tab strip for close and colour clicks.
    unsafe {
        let _ = SetWindowSubclass(tabs, Some(tabs_proc), 1, 0);
    }
    let swatch = ui::scale(14, ui::dpi(window));
    let colors: Vec<u32> = TabColor::ALL.iter().map(|c| c.rgb()).collect();
    Widgets {
        tabs,
        status: ui::child(
            window,
            w!("msctls_statusbar32"),
            "",
            WINDOW_STYLE(0),
            WINDOW_EX_STYLE(0),
            "status",
        ),
        m_add: ui::button(window, "Add machine...", ids::MACHINES_ADD),
        m_open: ui::button(window, "Open", ids::MACHINES_OPEN),
        m_remove: ui::button(window, "Remove", ids::MACHINES_REMOVE),
        m_settings: ui::button(window, "Settings...", ids::MACHINES_SETTINGS),
        m_options: ui::button(window, "Options...", ids::MACHINES_OPTIONS),
        m_disconnect_all: ui::button(window, "Disconnect all", ids::MACHINES_DISCONNECT_ALL),
        m_setup: ui::button(window, "Set up...", ids::MACHINES_SETUP),
        m_list: controls::list(
            window,
            ids::MACHINES_LIST,
            &[
                ("Name", 150),
                ("Address", 160),
                ("Services", 160),
                ("State", 110),
                ("Password", 140),
            ],
            icons::color_list(&colors, swatch),
            true,
        ),
        k_new: ui::button(window, "New key...", ids::KEYS_NEW),
        k_import: ui::button(window, "Import...", ids::KEYS_IMPORT),
        k_export: ui::button(window, "Export...", ids::KEYS_EXPORT),
        k_copy: ui::button(window, "Copy public key", ids::KEYS_COPY),
        k_delete: ui::button(window, "Delete", ids::KEYS_DELETE),
        k_list: controls::list(
            window,
            ids::KEYS_LIST,
            &[
                ("Name", 150),
                ("Kind", 120),
                ("Used by", 150),
                ("Comment", 130),
                ("Fingerprint (SHA256)", 340),
            ],
            icons::system_list(),
            true,
        ),
        header: header::create(window),
        local: pane(window, true),
        remote: pane(window, false),
        term_tabs,
        term: term_view::create(window),
        term_text,
        console: {
            let e = ui::child(
                window,
                w!("EDIT"),
                "",
                WINDOW_STYLE(ES_AUTOHSCROLL as u32 | WS_TABSTOP.0),
                windows::Win32::UI::WindowsAndMessaging::WS_EX_CLIENTEDGE,
                "console.input",
            );
            ui::set_font(e, ui::mono_font_for(window));
            e
        },
        console_send: ui::button(window, "Send", "console.send"),
        q_buttons: QUEUE_BUTTONS
            .iter()
            .map(|(id, label)| (*id, ui::button(window, label, id)))
            .collect(),
        q_list: controls::list(
            window,
            ids::QUEUE_LIST,
            &conn::QUEUE_COLUMNS,
            icons::system_list(),
            false,
        ),
    }
}

/// Apply an action, run its effects, redraw.
pub fn dispatch(action: app::Action) {
    let effects = with(|s| s.app.apply(action)).unwrap_or_default();
    run(effects);
    refresh();
}

/// The machine of the active connection tab.
fn active_machine() -> Option<String> {
    let i = active_conn()?;
    with(|s| s.app.tab(i).map(|t| t.machine_id().to_string())).flatten()
}

fn active_conn() -> Option<usize> {
    with(|s| match s.app.active() {
        Tab::Connection(i) => Some(i),
        Tab::Machines | Tab::Keys => None,
    })
    .flatten()
}

pub fn conn_action(a: conn::Action) {
    if let Some(i) = active_conn() {
        dispatch(app::Action::Conn(i, a));
    }
}

/// The live terminal of the showing tab, as last drawn.
pub fn shell_screen() -> Option<TermScreen> {
    with(|s| s.drawn.screen.clone()).flatten()
}

pub fn shell_message() -> String {
    with(|s| match s.app.active() {
        Tab::Connection(i) => s.app.tab(i).map(|t| t.shell_message()).unwrap_or_default(),
        Tab::Machines | Tab::Keys => String::new(),
    })
    .unwrap_or_default()
}

pub fn header_info() -> Option<HeaderInfo> {
    with(|s| s.drawn.header.clone()).flatten()
}

/// A click on one of the header's buttons.
pub fn header_button(from: HWND) {
    let Some((buttons, files_on, cmds_on)) = with(|s| {
        let h = &s.w.header;
        let info = s.drawn.header.clone();
        let live = |st: ConnState| matches!(st, ConnState::Connected | ConnState::Busy);
        (
            [h.files, h.commands, h.swap, h.close, h.settings],
            info.as_ref().is_some_and(|i| live(i.files_state)),
            info.as_ref().is_some_and(|i| live(i.commands_state)),
        )
    }) else {
        return;
    };
    let [files, commands, swap, close, settings] = buttons;
    match from {
        h if h == files => conn_action(if files_on {
            conn::Action::Disconnect
        } else {
            conn::Action::Connect
        }),
        h if h == commands => conn_action(if cmds_on {
            conn::Action::ShellDisconnect
        } else {
            conn::Action::ShellConnect
        }),
        h if h == swap => conn_action(conn::Action::Swap),
        h if h == settings => {
            if let Some(id) = active_machine() {
                dispatch(app::Action::EditMachine(id));
            }
        }
        h if h == close => {
            if let Some(i) = active_conn() {
                dispatch(app::Action::CloseTab(i));
            }
        }
        _ => {}
    }
}

/// Show the terminal's Connect button while no session runs and one can be opened.
pub fn place_term_connect() {
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
        term_view::place_connect(&s.w.term, show, &label);
    });
}

/// Carry out effects; an answer becomes the matching action (by the effect's purpose).
fn run(effects: Vec<Effect>) {
    let Some(owner) = with(|s| s.window) else {
        return;
    };
    for e in effects {
        match e {
            Effect::PickFolder(spec) => {
                let path =
                    effects::pick_folder(owner, spec.purpose, &spec.title, spec.start.as_ref());
                match spec.purpose {
                    "app.home" => dispatch(app::Action::HomePicked(path)),
                    "keys.export" => {
                        dispatch(app::Action::Keys(sshkeys::Action::ExportPicked(path)))
                    }
                    _ => {}
                }
            }
            Effect::PickFile(spec) => {
                let path =
                    effects::pick_file(owner, spec.purpose, &spec.title, spec.start.as_ref());
                if spec.purpose == "keys.import" {
                    dispatch(app::Action::Keys(sshkeys::Action::ImportPicked(path)));
                }
            }
            Effect::AskPath(spec) => {
                let answer = effects::ask(
                    owner,
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
                    conn_action(conn::Action::LocalGoToAnswered(answer.map(Into::into)));
                }
            }
            Effect::Confirm(spec) => {
                let yes = effects::confirm(
                    owner,
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
                    owner,
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
                    owner,
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
                let i = effects::choose(
                    owner,
                    &spec.title,
                    &spec.message,
                    &spec.choices,
                    spec.dismiss,
                );
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
                    legcon::controller::setup::FIRST_PURPOSE => {
                        dispatch(app::Action::SetupFirstAnswered(i))
                    }
                    _ => {}
                }
            }
            Effect::Form(spec) => {
                let answer = form::show(owner, &spec);
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
                let anchor = with(|s| match spec.anchor {
                    ids::LOCAL_PLACES => Some(s.w.local.places),
                    ids::REMOTE_PLACES => Some(s.w.remote.places),
                    _ => None,
                })
                .flatten();
                let chosen = effects::menu(owner, anchor, &spec.items);
                match spec.purpose {
                    "local.places" => conn_action(conn::Action::PlaceChosen(Side::Local, chosen)),
                    "remote.places" => conn_action(conn::Action::PlaceChosen(Side::Remote, chosen)),
                    _ => {}
                }
            }
            Effect::Message(text) => {
                set_status(&text);
                effects::note(owner, "legcon", &text);
            }
            Effect::CopyToClipboard(text) => {
                effects::copy_text(owner, &text);
                set_status("Copied to the clipboard.");
            }
            Effect::Quit => {
                // SAFETY: destroying our own main window ends the message loop.
                let _ = unsafe { DestroyWindow(owner) };
            }
        }
    }
}

fn set_status(text: &str) {
    if let Some(status) = with(|s| s.w.status) {
        let t = ui::wide(text);
        // SAFETY: `t` outlives the call.
        unsafe {
            SendMessageW(
                status,
                SB_SETTEXTW,
                Some(WPARAM(0)),
                Some(LPARAM(t.as_ptr() as isize)),
            );
        }
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
    }
}

fn cells(rows: &[Row], perms: bool) -> Vec<(i32, Vec<String>)> {
    rows.iter()
        .map(|r| {
            let mut v = vec![r.name.clone(), r.size.clone(), r.modified.clone()];
            if perms {
                v.push(r.mode.clone());
            }
            (icons::file_icon(&r.name, r.is_dir), v)
        })
        .collect()
}

/// Bring every control in line with the controller.
fn refresh() {
    let mut resize = None;
    let status = with(|s| {
        let looks: Vec<TabLook> = ["Machines", "Keys"]
            .into_iter()
            .map(|t| TabLook {
                title: t.into(),
                color: None,
                state: None,
            })
            .chain(s.app.tabs().iter().map(|t| TabLook {
                title: t.title(),
                color: Some(t.color().rgb()),
                state: Some(t.tab_state()),
            }))
            .collect();
        if looks
            .iter()
            .map(|l| &l.title)
            .ne(s.drawn.tabs.iter().map(|l| &l.title))
        {
            let labels: Vec<String> = looks.iter().map(|l| l.title.clone()).collect();
            controls::set_tabs(s.w.tabs, &labels);
        }
        if looks != s.drawn.tabs {
            s.drawn.tabs = looks;
            term_view::invalidate(s.w.tabs);
        }
        let active = s.app.active();
        controls::select_tab(
            s.w.tabs,
            match active {
                Tab::Machines => 0,
                Tab::Keys => 1,
                Tab::Connection(i) => i + FIRST_CONN,
            },
        );
        let on_machines = active == Tab::Machines;
        let on_keys = active == Tab::Keys;
        let on_conn = matches!(active, Tab::Connection(_));
        for h in [
            s.w.k_new,
            s.w.k_import,
            s.w.k_export,
            s.w.k_copy,
            s.w.k_delete,
            s.w.k_list,
        ] {
            ui::set_visible(h, on_keys);
        }
        let keys: Vec<(i32, Vec<String>)> = s
            .app
            .keys()
            .into_iter()
            .map(|k| {
                (
                    icons::file_icon("key.pem", false),
                    vec![k.name, k.kind, k.used_by, k.comment, k.fingerprint],
                )
            })
            .collect();
        if keys != s.drawn.keys {
            controls::fill(s.w.k_list, &keys);
            s.drawn.keys = keys;
        }
        for h in [
            s.w.m_add,
            s.w.m_open,
            s.w.m_remove,
            s.w.m_settings,
            s.w.m_options,
            s.w.m_disconnect_all,
            s.w.m_setup,
            s.w.m_list,
        ] {
            ui::set_visible(h, on_machines);
        }
        for p in [&s.w.local, &s.w.remote] {
            for h in [p.places, p.title, p.home, p.other, p.path, p.list] {
                ui::set_visible(h, on_conn);
            }
        }
        for h in [s.w.header.hwnd, s.w.term_tabs] {
            ui::set_visible(h, on_conn);
        }
        let machines: Vec<(i32, Vec<String>)> = s
            .app
            .machines()
            .into_iter()
            .map(|m| {
                let icon = TabColor::ALL
                    .iter()
                    .position(|c| *c == m.color)
                    .unwrap_or(0) as i32;
                let pw = if m.password_saved {
                    "Saved (encoded)"
                } else {
                    "Asks"
                };
                (
                    icon,
                    vec![
                        m.name,
                        m.address,
                        m.services,
                        state_text(m.state).into(),
                        pw.into(),
                    ],
                )
            })
            .collect();
        if machines != s.drawn.machines {
            controls::fill(s.w.m_list, &machines);
            s.drawn.machines = machines;
        }
        let Tab::Connection(i) = active else {
            for h in [
                s.w.term.hwnd,
                s.w.term_text,
                s.w.console,
                s.w.console_send,
                s.w.q_list,
            ] {
                ui::set_visible(h, false);
            }
            for (_, h) in &s.w.q_buttons {
                ui::set_visible(*h, false);
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
            // Empty the controls too: a tab with no rows would otherwise keep the last tab's.
            controls::fill(s.w.local.list, &[]);
            controls::fill(s.w.remote.list, &[]);
            controls::fill(s.w.q_list, &[]);
            s.drawn.queue.clear();
            s.drawn.queue_ids.clear();
        }
        let info = t.header_info();
        if s.drawn.header.as_ref() != Some(&info) {
            header::update(&s.w.header, &info);
            s.drawn.header = Some(info);
        }
        ui::set_text(s.w.remote.title, &t.title());
        ui::set_text(s.w.local.path, &format!(" {}", t.local_path()));
        ui::set_text(s.w.remote.path, &format!(" {}", t.remote_path()));
        ui::set_enabled(s.w.remote.home, t.can_disconnect());
        ui::set_enabled(s.w.remote.other, t.can_disconnect());
        let local = t.local_rows();
        if local != s.drawn.local {
            controls::fill(s.w.local.list, &cells(&local, false));
            s.drawn.local = local;
        }
        let remote = t.remote_rows();
        if remote != s.drawn.remote {
            controls::fill(s.w.remote.list, &cells(&remote, true));
            s.drawn.remote = remote;
        }
        let term = t.term_tab();
        if let Some(n) = TermTab::ALL.iter().position(|x| *x == term) {
            controls::select_tab(s.w.term_tabs, n);
        }
        let on_shell = term == TermTab::Shell;
        ui::set_visible(s.w.term.hwnd, on_shell);
        let on_queue = term == TermTab::Transfers;
        ui::set_visible(s.w.term_text, !on_shell && !on_queue);
        ui::set_visible(s.w.q_list, on_queue);
        for (_, h) in &s.w.q_buttons {
            ui::set_visible(*h, on_queue);
        }
        if on_queue {
            let rows = t.queue_rows();
            let cells: Vec<(i32, Vec<String>)> = rows
                .iter()
                .map(|r| {
                    (
                        icons::file_icon(&r.name, r.name.ends_with('/')),
                        vec![
                            r.name.clone(),
                            r.size.clone(),
                            r.direction.clone(),
                            r.status.clone(),
                            r.progress.clone(),
                            r.from.clone(),
                            r.to.clone(),
                        ],
                    )
                })
                .collect();
            let ids: Vec<u64> = rows.iter().map(|r| r.id).collect();
            let picked = selected_in(s.w.q_list, &s.drawn.queue_ids);
            if cells != s.drawn.queue || ids != s.drawn.queue_ids {
                controls::update(s.w.q_list, &s.drawn.queue, &cells);
                if ids != s.drawn.queue_ids {
                    // Rows moved or went: keep the same entries selected.
                    let keep: Vec<usize> = ids
                        .iter()
                        .enumerate()
                        .filter(|(_, id)| picked.contains(id))
                        .map(|(i, _)| i)
                        .collect();
                    controls::select_rows(s.w.q_list, &keep);
                }
                s.drawn.queue = cells;
                s.drawn.queue_ids = ids;
            }
            let picked = selected_in(s.w.q_list, &s.drawn.queue_ids);
            for b in t.queue_buttons(&picked) {
                if let Some((_, h)) = s.w.q_buttons.iter().find(|(id, _)| *id == b.id) {
                    ui::set_text(*h, b.label);
                    ui::set_enabled(*h, b.enabled);
                }
            }
        }
        let on_console = term == TermTab::Console;
        ui::set_visible(s.w.console, on_console);
        ui::set_visible(s.w.console_send, on_console);
        ui::set_enabled(s.w.console, t.can_console());
        ui::set_enabled(s.w.console_send, t.can_console());
        if !on_shell {
            let text = t.term_text().replace('\n', "\r\n");
            if ui::text(s.w.term_text) != text {
                ui::set_text(s.w.term_text, &text);
                // Show the newest lines, as a log viewer does.
                let end = text.encode_utf16().count();
                // SAFETY: plain edit-control messages.
                unsafe {
                    SendMessageW(
                        s.w.term_text,
                        EM_SETSEL,
                        Some(WPARAM(end)),
                        Some(LPARAM(end as isize)),
                    );
                    SendMessageW(s.w.term_text, EM_SCROLLCARET, None, None);
                }
            }
        }
        // On every connection tab, not just Shell: a shell opened from another tab starts at
        // the size it is shown at, rather than a default it is squeezed from later.
        if ui::client(s.w.term.hwnd).right > 0 {
            let grid = term_view::grid(s.w.term.hwnd);
            if grid != t.shell_size() {
                resize = Some(grid);
            }
        }
        let screen = if t.shell_state() == ConnState::Connected {
            t.shell_screen()
        } else {
            None
        };
        if screen != s.drawn.screen {
            s.drawn.screen = screen;
            term_view::invalidate(s.w.term.hwnd);
        }
        t.status_line()
    });
    if let Some(text) = status {
        set_status(&text);
    }
    layout();
    place_term_connect();
    // The terminal's size follows its area, also when the area changed while another tab showed.
    if let Some((cols, rows)) = resize {
        conn_action(conn::Action::ShellResize(cols, rows));
    }
}

/// Place every control for the current size, tab and layout; skipped when nothing moved.
fn layout() {
    let moved = with(|s| {
        let r = ui::client(s.window);
        let (w, h) = (r.right, r.bottom);
        let key: LayoutKey = match s.app.active() {
            Tab::Machines => (w, h, -1, 0, false, -1),
            Tab::Keys => (w, h, -3, 0, false, -1),
            Tab::Connection(i) => match s.app.tab(i) {
                Some(t) => {
                    let l = t.layout();
                    let share = s.drag.unwrap_or(l.files_share);
                    (
                        w,
                        h,
                        i as i64,
                        share.to_bits(),
                        l.swapped,
                        TermTab::ALL
                            .iter()
                            .position(|x| *x == t.term_tab())
                            .unwrap_or(0) as i64,
                    )
                }
                None => (w, h, -2, 0, false, -1),
            },
        };
        if s.laid == Some(key) {
            return false;
        }
        s.laid = Some(key);
        let dpi = ui::dpi(s.window);
        let sc = |v| ui::scale(v, dpi);
        // SAFETY: the status bar sizes itself on WM_SIZE.
        unsafe { SendMessageW(s.w.status, WM_SIZE, None, None) };
        let status_h = {
            let mut sr = RECT::default();
            // SAFETY: a plain query of our own control.
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowRect(s.w.status, &mut sr)
            };
            sr.bottom - sr.top
        };
        let m = sc(6);
        let tab_h = sc(30);
        ui::place(s.w.tabs, (0, sc(4), w, tab_h));
        let top = sc(4) + tab_h;
        let bottom = h - status_h - m;
        let btn = sc(26);
        if s.app.active() == Tab::Machines {
            let mut x = m;
            for (b, bw) in [
                (s.w.m_add, 110),
                (s.w.m_open, 80),
                (s.w.m_remove, 80),
                (s.w.m_settings, 90),
                (s.w.m_options, 90),
                (s.w.m_disconnect_all, 110),
                (s.w.m_setup, 90),
            ] {
                ui::place(b, (x, top + m, sc(bw), btn));
                x += sc(bw) + m;
            }
            ui::place(
                s.w.m_list,
                (m, top + btn + 2 * m, w - 2 * m, bottom - top - btn - 2 * m),
            );
            return true;
        }
        if s.app.active() == Tab::Keys {
            let mut x = m;
            for (b, bw) in [
                (s.w.k_new, 100),
                (s.w.k_import, 90),
                (s.w.k_export, 90),
                (s.w.k_copy, 130),
                (s.w.k_delete, 80),
            ] {
                ui::place(b, (x, top + m, sc(bw), btn));
                x += sc(bw) + m;
            }
            ui::place(
                s.w.k_list,
                (m, top + btn + 2 * m, w - 2 * m, bottom - top - btn - 2 * m),
            );
            return true;
        }
        let Tab::Connection(i) = s.app.active() else {
            return true;
        };
        let Some(t) = s.app.tab(i) else {
            return true;
        };
        let layout = t.layout().clone();
        let header_h = sc(40);
        ui::place(s.w.header.hwnd, (0, top, w, header_h));
        if let Some(info) = s.drawn.header.as_ref() {
            header::update(&s.w.header, info);
        }
        let area_top = top + header_h;
        let area_h = bottom - area_top;
        let split_h = sc(8);
        let share = s.drag.unwrap_or(layout.files_share);
        let files_h = ((area_h - split_h) as f32 * share) as i32;
        let term_h = area_h - split_h - files_h;
        let (files_y, split_y, term_y) = if layout.swapped {
            (area_top + term_h + split_h, area_top + term_h, area_top)
        } else {
            (area_top, area_top + files_h, area_top + files_h + split_h)
        };
        s.splitter = paint::rect(0, split_y, w, split_h);
        s.split_area = (area_top, area_h - split_h);
        // Two panes, each a white box: a header band, a path row, then the list.
        let pane_w = (w - 3 * m) / 2;
        let band = sc(32);
        let path_h = sc(22);
        for (k, p) in [&s.w.local, &s.w.remote].into_iter().enumerate() {
            let px = m + k as i32 * (pane_w + m);
            s.panes[k] = paint::rect(px, files_y, pane_w, files_h);
            let by = files_y + 1 + (band - btn) / 2;
            ui::place(p.places, (px + sc(5), by, sc(64), btn));
            let other_w = if k == 0 { sc(70) } else { sc(46) };
            let right = px + pane_w - sc(5);
            ui::place(p.other, (right - other_w, by, other_w, btn));
            ui::place(p.home, (right - other_w - sc(4) - sc(56), by, sc(56), btn));
            let tx = px + sc(5) + sc(64) + sc(8);
            ui::place(
                p.title,
                (tx, files_y + 1, right - other_w - sc(64) - tx, band - 2),
            );
            ui::place(
                p.path,
                (px + 2, files_y + band + sc(4), pane_w - 4, path_h - sc(6)),
            );
            let body_y = files_y + band + path_h + 1;
            let body_h = files_h - band - path_h - 2;
            ui::place(p.list, (px + 1, body_y, pane_w - 2, body_h));
        }
        ui::place(s.w.term_tabs, (m, term_y, w - 2 * m, sc(26)));
        let body = (m, term_y + sc(26), w - 2 * m, term_h - sc(26));
        ui::place(s.w.term.hwnd, body);
        let on_console = t.term_tab() == TermTab::Console;
        let row = if on_console { sc(30) } else { 0 };
        ui::place(s.w.term_text, (body.0, body.1, body.2, body.3 - row));
        if on_console {
            let y = body.1 + body.3 - row + sc(4);
            ui::place(s.w.console, (body.0, y, body.2 - sc(86), sc(24)));
            ui::place(
                s.w.console_send,
                (body.0 + body.2 - sc(80), y, sc(80), sc(24)),
            );
        }
        // Transfers: a row of buttons, the queue list under it.
        let mut x = body.0;
        for (id, h) in &s.w.q_buttons {
            let bw = QUEUE_BUTTONS
                .iter()
                .find(|(i, _)| i == id)
                .map_or(80, |(_, l)| 24 + 7 * l.len() as i32);
            ui::place(*h, (x, body.1 + sc(4), sc(bw), sc(24)));
            x += sc(bw) + sc(4);
        }
        ui::place(
            s.w.q_list,
            (body.0, body.1 + sc(32), body.2, body.3 - sc(32)),
        );
        true
    });
    if moved != Some(true) {
        return;
    }
    if let Some(hwnd) = with(|s| s.window) {
        // SAFETY: a plain repaint request for our own window.
        let _ = unsafe {
            RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
            )
        };
    }
}

fn paint_main(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: the standard paint cycle on our own window.
    unsafe {
        let dc = BeginPaint(hwnd, &mut ps);
        let r = ui::client(hwnd);
        paint::fill(dc, &r, paint::FACE);
        let dpi = ui::dpi(hwnd);
        let parts = with(|s| {
            (
                s.panes,
                s.splitter,
                matches!(s.app.active(), Tab::Connection(_)),
            )
        });
        if let Some((panes, split, true)) = parts {
            let band = ui::scale(32, dpi);
            let path_h = ui::scale(22, dpi);
            for p in panes {
                paint::fill(dc, &p, paint::WHITE);
                paint::fill(
                    dc,
                    &RECT {
                        bottom: p.top + band,
                        ..p
                    },
                    paint::BAND,
                );
                let line = |y: i32| RECT {
                    top: y,
                    bottom: y + 1,
                    ..p
                };
                paint::fill(dc, &line(p.top + band), paint::LINE);
                paint::fill(dc, &line(p.top + band + path_h), paint::LINE);
                paint::frame(dc, &p, paint::LINE2);
            }
            let gx = (split.left + split.right) / 2;
            let gy = (split.top + split.bottom) / 2;
            let grip = paint::rect(gx - ui::scale(20, dpi), gy - 1, ui::scale(40, dpi), 2);
            paint::fill(dc, &grip, paint::LINE2);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

fn draw_tab(d: &DRAWITEMSTRUCT) {
    let Some(look) = with(|s| s.drawn.tabs.get(d.itemID as usize).cloned()).flatten() else {
        return;
    };
    let dc: HDC = d.hDC;
    let r = d.rcItem;
    let dpi = ui::dpi(d.hwndItem);
    let s = |v| ui::scale(v, dpi);
    let selected = d.itemState.0 & ODS_SELECTED.0 != 0;
    // A connection tab wears its machine's colour: a tinted face and a bar along the top.
    let face = match look.color {
        Some(c) if selected => paint::tint(c, 0.22),
        Some(c) => paint::tint(c, 0.12),
        None if selected => paint::FACE,
        None => paint::TAB_OFF,
    };
    paint::fill(dc, &r, face);
    if let Some(c) = look.color {
        paint::fill(
            dc,
            &RECT {
                top: r.top + 1,
                bottom: r.top + s(5),
                left: r.left + 1,
                right: r.right - 1,
            },
            c,
        );
    }
    let font = ui::font_for(d.hwndItem);
    let mut x = r.left + s(10);
    if let Some(st) = look.state {
        let dd = s(9);
        paint::dot(
            dc,
            x,
            (r.top + r.bottom - dd) / 2 + s(1),
            dd,
            header::state_rgb(st),
        );
        x += dd + s(7);
    }
    let tr = RECT {
        left: x,
        top: r.top + s(2),
        right: r.right - s(18),
        bottom: r.bottom,
    };
    let f = if selected {
        paint::title_font(dpi)
    } else {
        font
    };
    paint::text(dc, &look.title, &tr, paint::TEXT, f);
    if look.state.is_some() {
        let xr = RECT {
            left: r.right - s(18),
            top: r.top + s(2),
            right: r.right - s(4),
            bottom: r.bottom,
        };
        paint::text(dc, "x", &xr, paint::MUTED, font);
    }
}

fn tab_menu(tab: usize) {
    let Some((hwnd, id, current, keys, rsh)) = with(|s| {
        let t = s.app.tab(tab.checked_sub(FIRST_CONN)?)?;
        Some((
            s.window,
            t.machine_id().to_string(),
            t.color(),
            t.can_set_up_key_login(),
            (t.can_set_up_rsh_trust(), t.can_check_rsh()),
        ))
    })
    .flatten() else {
        return;
    };
    let mut p = POINT::default();
    // SAFETY: a popup menu built, shown modally and destroyed here.
    let cmd = unsafe {
        let _ = GetCursorPos(&mut p);
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let labels: Vec<Vec<u16>> = TabColor::ALL.iter().map(|c| ui::wide(c.label())).collect();
        for (k, (c, label)) in TabColor::ALL.iter().zip(&labels).enumerate() {
            let flags = if *c == current {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(menu, flags, 100 + k, windows::core::PCWSTR(label.as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let key_label = ui::wide("Set up key login...");
        let flags = if keys {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };
        let _ = AppendMenuW(menu, flags, 98, windows::core::PCWSTR(key_label.as_ptr()));
        let grey = |on: bool| if on { MF_STRING } else { MF_STRING | MF_GRAYED };
        let rsh_label = ui::wide("Set up rsh trust...");
        let _ = AppendMenuW(
            menu,
            grey(rsh.0),
            96,
            windows::core::PCWSTR(rsh_label.as_ptr()),
        );
        let check_label = ui::wide("Check rsh from the machine's side");
        let _ = AppendMenuW(
            menu,
            grey(rsh.1),
            95,
            windows::core::PCWSTR(check_label.as_ptr()),
        );
        let settings = ui::wide("Settings...");
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            97,
            windows::core::PCWSTR(settings.as_ptr()),
        );
        let close = ui::wide("Close tab (logs off)");
        let _ = AppendMenuW(menu, MF_STRING, 99, windows::core::PCWSTR(close.as_ptr()));
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            p.x,
            p.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        cmd
    };
    match cmd {
        99 => dispatch(app::Action::CloseTab(tab - FIRST_CONN)),
        98 => dispatch(app::Action::Conn(
            tab - FIRST_CONN,
            conn::Action::SetUpKeyLogin,
        )),
        97 => dispatch(app::Action::EditMachine(id)),
        96 => dispatch(app::Action::Conn(
            tab - FIRST_CONN,
            conn::Action::SetUpRshTrust,
        )),
        95 => dispatch(app::Action::Conn(tab - FIRST_CONN, conn::Action::CheckRsh)),
        c if (100..100 + TabColor::ALL.len()).contains(&c) => {
            dispatch(app::Action::SetColor(id, TabColor::ALL[c - 100]))
        }
        _ => {}
    }
}

unsafe extern "system" fn tabs_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let (x, y) = ((lp.0 & 0xffff) as i16 as i32, (lp.0 >> 16) as i16 as i32);
    let main = with(|s| s.window);
    let post = |m: u32, i: usize| {
        if let Some(main) = main {
            // SAFETY: posting to our own main window; handled after this click.
            let _ = unsafe { PostMessageW(Some(main), m, WPARAM(i), LPARAM(0)) };
        }
    };
    match msg {
        WM_MBUTTONUP => {
            if let Some((i, _)) = controls::tab_at(hwnd, x, y).filter(|(i, _)| *i >= FIRST_CONN) {
                post(WM_APP_CLOSE_TAB, i);
            }
        }
        WM_RBUTTONUP => {
            if let Some((i, _)) = controls::tab_at(hwnd, x, y).filter(|(i, _)| *i >= FIRST_CONN) {
                post(WM_APP_TAB_MENU, i);
            }
        }
        WM_LBUTTONUP => {
            let close_w = ui::scale(20, ui::dpi(hwnd));
            if let Some((i, _)) = controls::tab_at(hwnd, x, y)
                .filter(|(i, r)| *i >= FIRST_CONN && x >= r.right - close_w)
            {
                post(WM_APP_CLOSE_TAB, i);
            }
        }
        _ => {}
    }
    // SAFETY: the tab control's own handling continues.
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

fn tick() {
    let effects = with(|s| {
        s.wake.ticked();
        s.app.tick()
    })
    .unwrap_or_default();
    run(effects);
    refresh();
}

/// The chord for a key press, from the virtual-key code and the modifier state.
fn chord(vk: u16) -> Option<Chord> {
    // SAFETY: plain keyboard-state reads on the UI thread.
    let down = |k: u16| unsafe { GetKeyState(k as i32) } < 0;
    let key = match vk {
        0x30..=0x39 => Key::Digit((vk - 0x30) as u8),
        0x41..=0x5A => Key::Char(vk as u8 as char),
        0x70..=0x87 => Key::F((vk - 0x6F) as u8),
        0x09 => Key::Tab,
        0x0D => Key::Enter,
        0x08 => Key::Backspace,
        0x2E => Key::Delete,
        0x26 => Key::Up,
        0x28 => Key::Down,
        _ => return None,
    };
    Some(Chord {
        key,
        mods: Mods {
            primary: down(VK_CONTROL.0),
            shift: down(VK_SHIFT.0),
            alt: false,
        },
    })
}

/// A key press from the message loop: the key map's command, if any. True when it was used.
/// The names selected in one side's list.
fn selected_names(side: Side) -> Vec<String> {
    with(|s| {
        let (list, rows) = match side {
            Side::Local => (s.w.local.list, &s.drawn.local),
            Side::Remote => (s.w.remote.list, &s.drawn.remote),
        };
        controls::selected_all(list)
            .into_iter()
            .filter_map(|i| rows.get(i).map(|r| r.name.clone()))
            .collect()
    })
    .unwrap_or_default()
}

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

/// The queue entries selected in the Transfers list.
fn selected_in(list: HWND, ids: &[u64]) -> Vec<u64> {
    controls::selected_all(list)
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
        let picked = selected_in(s.w.q_list, &s.drawn.queue_ids);
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
    let (Some((buttons, picked)), Some(hwnd)) = (queue_buttons(), with(|s| s.window)) else {
        return;
    };
    let mut p = POINT::default();
    // SAFETY: a popup menu built, shown modally and destroyed here.
    let cmd = unsafe {
        let _ = GetCursorPos(&mut p);
        let Ok(menu) = CreatePopupMenu() else { return };
        let labels: Vec<Vec<u16>> = buttons.iter().map(|b| ui::wide(b.label)).collect();
        for (i, (b, label)) in buttons.iter().zip(&labels).enumerate() {
            let flags = if b.enabled {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            if b.op == conn::QueueOp::ClearFinished {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            }
            let _ = AppendMenuW(menu, flags, i + 1, windows::core::PCWSTR(label.as_ptr()));
        }
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            p.x,
            p.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        cmd
    };
    if let Some(b) = cmd.checked_sub(1).and_then(|i| buttons.get(i)) {
        conn_action(conn::Action::Queue(b.op, picked));
    }
}

fn send_console() {
    let Some(edit) = with(|s| s.w.console) else {
        return;
    };
    let line = ui::text(edit);
    if !line.trim().is_empty() {
        ui::set_text(edit, "");
        conn_action(conn::Action::ConsoleSend(line));
    }
}

/// The right-click menu of a file list: what can be done with its selection.
fn file_menu(side: Side) {
    let Some((hwnd, other)) = with(|s| {
        let other = match s.app.active() {
            Tab::Connection(i) => s.app.tab(i).map(|t| match side {
                Side::Local => format!("Copy to {}", t.title()),
                Side::Remote => "Copy to this computer".to_string(),
            }),
            Tab::Machines | Tab::Keys => None,
        };
        (s.window, other)
    }) else {
        return;
    };
    let names = selected_names(side);
    let mut p = POINT::default();
    let items: Vec<(usize, String, bool)> = vec![
        (
            1,
            format!("{}\tF6", other.unwrap_or_else(|| "Copy".into())),
            !names.is_empty(),
        ),
        (6, "Add to queue".into(), !names.is_empty()),
        (2, "New folder...\tCtrl+N".into(), true),
        (3, "Rename...\tF2".into(), names.len() == 1),
        (4, "Delete...\tDel".into(), !names.is_empty()),
        (5, "Refresh\tF5".into(), true),
    ];
    // SAFETY: a popup menu built, shown modally and destroyed here.
    let cmd = unsafe {
        let _ = GetCursorPos(&mut p);
        let Ok(menu) = CreatePopupMenu() else { return };
        let labels: Vec<Vec<u16>> = items.iter().map(|(_, l, _)| ui::wide(l)).collect();
        for ((id, _, on), label) in items.iter().zip(&labels) {
            let flags = if *on {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            let _ = AppendMenuW(menu, flags, *id, windows::core::PCWSTR(label.as_ptr()));
            if *id == 6 || *id == 4 {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            }
        }
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            p.x,
            p.y,
            None,
            hwnd,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        cmd
    };
    match cmd {
        1 => conn_action(conn::Action::Copy(side, names)),
        6 => conn_action(conn::Action::AddToQueue(side, names)),
        2 => conn_action(conn::Action::NewFolder(side)),
        3 => conn_action(conn::Action::Rename(side, names[0].clone())),
        4 => conn_action(conn::Action::Delete(side, names)),
        5 => conn_action(conn::Action::Refresh(side)),
        _ => {}
    }
}

pub fn key(vk: u16, focus: HWND) -> bool {
    if vk == 0x0D && with(|s| s.w.console == focus).unwrap_or(false) {
        send_console();
        return true;
    }
    let Some(c) = chord(vk) else {
        return false;
    };
    let Some(cmd) = keys::command_for(c) else {
        return false;
    };
    let (in_list, in_term) = with(|s| {
        let lists = [s.w.local.list, s.w.remote.list, s.w.m_list];
        (lists.contains(&focus), focus == s.w.term.hwnd)
    })
    .unwrap_or((false, false));
    // The terminal keeps every key a 1990s program might want; only Ctrl+Shift chords escape it.
    if in_term && !(c.mods.primary && c.mods.shift) {
        return false;
    }
    if !c.mods.primary && !in_list {
        return false;
    }
    let side = with(|s| {
        if focus == s.w.remote.list {
            Side::Remote
        } else {
            Side::Local
        }
    })
    .unwrap_or(Side::Local);
    let tabs = with(|s| s.app.tabs().len()).unwrap_or(0);
    let current = active_conn();
    match cmd {
        Command::Swap => conn_action(conn::Action::Swap),
        Command::LocalPlaces => conn_action(conn::Action::Places(Side::Local)),
        Command::RemotePlaces => conn_action(conn::Action::Places(Side::Remote)),
        Command::LocalHome => conn_action(conn::Action::LocalHome),
        Command::GoTo => conn_action(conn::Action::GoTo(side)),
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
    true
}

fn selected_key() -> Option<String> {
    with(|s| {
        let names: Vec<String> = s.app.keys().into_iter().map(|k| k.name).collect();
        controls::selected(s.w.k_list).and_then(|i| names.get(i).cloned())
    })
    .flatten()
}

fn selected_machine() -> Option<String> {
    with(|s| {
        let ids: Vec<String> = s.app.machines().into_iter().map(|m| m.id).collect();
        controls::selected(s.w.m_list).and_then(|i| ids.get(i).cloned())
    })
    .flatten()
}

fn command(from: HWND, code: u32) {
    if code != BN_CLICKED {
        return;
    }
    if let Some(k) = with(|s| {
        [
            s.w.k_new,
            s.w.k_import,
            s.w.k_export,
            s.w.k_copy,
            s.w.k_delete,
        ]
    }) {
        let [new, import, export, copy, delete] = k;
        let named = |f: fn(String) -> sshkeys::Action| {
            if let Some(name) = selected_key() {
                dispatch(app::Action::Keys(f(name)));
            }
        };
        match from {
            h if h == new => return dispatch(app::Action::Keys(sshkeys::Action::New)),
            h if h == import => return dispatch(app::Action::Keys(sshkeys::Action::Import)),
            h if h == export => return named(sshkeys::Action::Export),
            h if h == copy => return named(sshkeys::Action::CopyPublic),
            h if h == delete => return named(sshkeys::Action::Delete),
            _ => {}
        }
    }
    if let Some((send, queue)) = with(|s| (s.w.console_send, s.w.q_buttons.clone())) {
        if from == send {
            return send_console();
        }
        if let Some((id, _)) = queue.iter().find(|(_, h)| *h == from) {
            return queue_click(id);
        }
    }
    let Some(b) = with(|s| {
        [
            s.w.m_add,
            s.w.m_open,
            s.w.m_remove,
            s.w.m_settings,
            s.w.m_options,
            s.w.m_disconnect_all,
            s.w.m_setup,
            s.w.local.places,
            s.w.local.home,
            s.w.local.other,
            s.w.remote.places,
            s.w.remote.home,
            s.w.remote.other,
        ]
    }) else {
        return;
    };
    let [add, open, remove, settings, options, all, setup, l_places, l_home, l_go_to, r_places, r_home, r_up] =
        b;
    match from {
        h if h == add => dispatch(app::Action::AddMachine),
        h if h == open => {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::OpenMachine(id));
            }
        }
        h if h == remove => {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::RemoveMachine(id));
            }
        }
        h if h == settings => {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::EditMachine(id));
            }
        }
        h if h == options => dispatch(app::Action::EditOptions),
        h if h == all => dispatch(app::Action::DisconnectAll),
        h if h == setup => dispatch(app::Action::Setup),
        h if h == l_places => conn_action(conn::Action::Places(Side::Local)),
        h if h == l_home => conn_action(conn::Action::LocalHome),
        h if h == l_go_to => conn_action(conn::Action::GoTo(Side::Local)),
        h if h == r_places => conn_action(conn::Action::Places(Side::Remote)),
        h if h == r_home => conn_action(conn::Action::RemoteHome),
        h if h == r_up => conn_action(conn::Action::RemoteUp),
        _ => {}
    }
}

fn notify(hdr: &NMHDR) {
    let Some((w, local, remote)) = with(|s| {
        (
            (
                s.w.tabs,
                s.w.m_list,
                s.w.local.list,
                s.w.remote.list,
                s.w.term_tabs,
            ),
            s.drawn.local.clone(),
            s.drawn.remote.clone(),
        )
    }) else {
        return;
    };
    let (tabs, m_list, l_list, r_list, term_tabs) = w;
    let from = hdr.hwndFrom;
    if from == tabs && hdr.code == TCN_SELCHANGE {
        match controls::tab_selected(tabs) {
            Some(0) | None => dispatch(app::Action::ShowMachines),
            Some(1) => dispatch(app::Action::ShowKeys),
            Some(n) => dispatch(app::Action::SelectTab(n - FIRST_CONN)),
        }
    } else if from == term_tabs && hdr.code == TCN_SELCHANGE {
        if let Some(t) = controls::tab_selected(term_tabs).and_then(|n| TermTab::ALL.get(n)) {
            conn_action(conn::Action::SelectTerm(*t));
        }
    } else if hdr.code == LVN_BEGINDRAG && (from == l_list || from == r_list) {
        let side = if from == l_list {
            Side::Local
        } else {
            Side::Remote
        };
        let names = selected_names(side);
        if !names.is_empty() {
            let main = with(|s| {
                s.file_drag = Some((side, names));
                s.window
            });
            if let Some(main) = main {
                // SAFETY: capture the mouse for the drag; the button-up ends it.
                unsafe { SetCapture(main) };
            }
        }
    } else if hdr.code == NM_RCLICK && with(|s| s.w.q_list == from).unwrap_or(false) {
        queue_menu();
    } else if hdr.code == NM_RCLICK && (from == l_list || from == r_list) {
        file_menu(if from == l_list {
            Side::Local
        } else {
            Side::Remote
        });
    } else if hdr.code == LVN_ITEMACTIVATE {
        if from == m_list {
            if let Some(id) = selected_machine() {
                dispatch(app::Action::OpenMachine(id));
            }
        } else if from == l_list {
            let row = controls::selected(l_list).and_then(|i| local.get(i));
            if let Some(r) = row.filter(|r| r.is_dir) {
                conn_action(conn::Action::LocalOpen(r.name.clone()));
            }
        } else if from == r_list {
            let row = controls::selected(r_list).and_then(|i| remote.get(i));
            if let Some(r) = row.filter(|r| r.is_dir) {
                conn_action(conn::Action::RemoteOpen(r.name.clone()));
            }
        }
    }
}

/// Which pane a point in the main window's client area is over.
pub fn pane_at(x: i32, y: i32) -> Option<Side> {
    with(|s| {
        if !matches!(s.app.active(), Tab::Connection(_)) {
            return None;
        }
        let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(&s.panes[0]) {
            Some(Side::Local)
        } else if inside(&s.panes[1]) {
            Some(Side::Remote)
        } else {
            None
        }
    })
    .flatten()
}

fn over_splitter(y: i32) -> bool {
    with(|s| {
        matches!(s.app.active(), Tab::Connection(_)) && y >= s.splitter.top && y < s.splitter.bottom
    })
    .unwrap_or(false)
}

fn cursor_y(hwnd: HWND) -> i32 {
    let mut p = POINT::default();
    // SAFETY: plain cursor queries for our own window.
    unsafe {
        let _ = GetCursorPos(&mut p);
        let _ = ScreenToClient(hwnd, &mut p);
    }
    p.y
}

/// Background colours for the labels and read-only fields in the pane bands.
fn ctl_color(wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let ctl = HWND(lp.0 as _);
    let rgb = with(|s| {
        if [s.w.local.title, s.w.remote.title].contains(&ctl) {
            Some(paint::BAND)
        } else if [s.w.local.path, s.w.remote.path].contains(&ctl) {
            Some(paint::WHITE)
        } else {
            None
        }
    })
    .flatten()?;
    // SAFETY: setting the text background on the DC the control gave us.
    unsafe { SetBkColor(HDC(wp.0 as _), paint::colorref(rgb)) };
    Some(LRESULT(brush(rgb).0 as isize))
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_JOB => {
            tick();
            LRESULT(0)
        }
        WM_APP_CLOSE_TAB => {
            if wp.0 >= FIRST_CONN {
                dispatch(app::Action::CloseTab(wp.0 - FIRST_CONN));
            }
            LRESULT(0)
        }
        WM_APP_TAB_MENU => {
            tab_menu(wp.0);
            LRESULT(0)
        }
        WM_SIZE => {
            layout();
            LRESULT(0)
        }
        WM_PAINT => {
            paint_main(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DRAWITEM => {
            // SAFETY: WM_DRAWITEM's lParam is a DRAWITEMSTRUCT.
            draw_tab(unsafe { &*(lp.0 as *const DRAWITEMSTRUCT) });
            LRESULT(1)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => match ctl_color(wp, lp) {
            Some(r) => r,
            // SAFETY: default handling for our own window.
            None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        },
        WM_COMMAND => {
            command(HWND(lp.0 as _), ((wp.0 >> 16) & 0xffff) as u32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            // SAFETY: WM_NOTIFY's lParam is an NMHDR.
            let hdr = unsafe { &*(lp.0 as *const NMHDR) };
            notify(hdr);
            LRESULT(0)
        }
        WM_SETCURSOR if with(|s| s.file_drag.is_some()).unwrap_or(false) => {
            // SAFETY: a system cursor while rows are dragged between panes.
            unsafe { SetCursor(LoadCursorW(None, IDC_HAND).ok()) };
            LRESULT(1)
        }
        WM_SETCURSOR if over_splitter(cursor_y(hwnd)) => {
            // SAFETY: a system cursor.
            unsafe { SetCursor(LoadCursorW(None, IDC_SIZENS).ok()) };
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let y = (lp.0 >> 16) as i16 as i32;
            if over_splitter(y) {
                with(|s| {
                    let share = match s.app.active() {
                        Tab::Connection(i) => s.app.tab(i).map_or(0.75, |t| t.layout().files_share),
                        Tab::Machines | Tab::Keys => 0.75,
                    };
                    s.drag = Some(share);
                });
                // SAFETY: capture the mouse for the drag.
                unsafe { SetCapture(hwnd) };
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if with(|s| s.file_drag.is_some()).unwrap_or(false) => {
            // SAFETY: a system cursor; captured mouse moves do not send WM_SETCURSOR.
            unsafe { SetCursor(LoadCursorW(None, IDC_HAND).ok()) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lp.0 >> 16) as i16 as i32;
            let moved = with(|s| {
                s.drag?;
                let (top, h) = s.split_area;
                let swapped = match s.app.active() {
                    Tab::Connection(i) => s.app.tab(i).is_some_and(|t| t.layout().swapped),
                    Tab::Machines | Tab::Keys => false,
                };
                let f = ((y - top) as f32 / h.max(1) as f32)
                    .clamp(conn::MIN_SHARE, 1.0 - conn::MIN_SHARE);
                s.drag = Some(if swapped { 1.0 - f } else { f });
                Some(())
            })
            .flatten();
            if moved.is_some() {
                layout();
            }
            LRESULT(0)
        }
        crate::ole_drop::WM_APP_DROPPED => {
            if let Some((side, paths)) = crate::ole_drop::take() {
                conn_action(conn::Action::Drop(side, paths));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if with(|s| s.file_drag.is_some()).unwrap_or(false) => {
            let (x, y) = ((lp.0 & 0xffff) as i16 as i32, (lp.0 >> 16) as i16 as i32);
            let drag = with(|s| s.file_drag.take()).flatten();
            // SAFETY: release the capture taken when the drag began.
            let _ = unsafe { ReleaseCapture() };
            if let Some((from, names)) = drag {
                match pane_at(x, y) {
                    Some(to) if to != from => conn_action(conn::Action::Copy(from, names)),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(share) = with(|s| s.drag.take()).flatten() {
                // SAFETY: release the capture taken on button down.
                let _ = unsafe { ReleaseCapture() };
                conn_action(conn::Action::SetFilesShare(share));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            dispatch(app::Action::Quit);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends the message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
