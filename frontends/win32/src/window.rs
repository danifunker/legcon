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
use legcon::controller::{ids, ConnState, Controller, Effect, TabColor};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, RedrawWindow, ScreenToClient, SetBkColor, HBRUSH, HDC,
    PAINTSTRUCT, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, DRAWITEMSTRUCT, NMHDR, NMTREEVIEWW, NM_DBLCLK, ODS_SELECTED,
    SB_SETTEXTW, TCN_SELCHANGE, TVN_SELCHANGEDW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetCursor, ShowWindow, TrackPopupMenu, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_PUSHLIKE,
    CW_USEDEFAULT, ES_AUTOHSCROLL, ES_MULTILINE, ES_READONLY, HMENU, IDC_ARROW, IDC_SIZENS,
    MF_CHECKED, MF_SEPARATOR, MF_STRING, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONUP, WM_MOUSEMOVE, WM_NOTIFY, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE,
    WNDCLASSW, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VSCROLL,
};

use crate::controls;
use crate::effects::{self, Ask};
use crate::header::{self, Header};
use crate::icons;
use crate::paint;
use crate::term_view::{self, TermView};
use crate::ui;
use crate::wake::{Win32Wake, WM_APP_JOB};

/// Posted by the tab strip: close tab `wParam` (0 is Machines).
const WM_APP_CLOSE_TAB: u32 = WM_APP + 3;
/// Posted by the tab strip: the colour menu for tab `wParam`.
const WM_APP_TAB_MENU: u32 = WM_APP + 4;

pub struct Pane {
    toggle: HWND,
    title: HWND,
    home: HWND,
    other: HWND,
    path: HWND,
    tree: HWND,
    list: HWND,
}

pub struct Widgets {
    tabs: HWND,
    status: HWND,
    m_add: HWND,
    m_open: HWND,
    m_remove: HWND,
    m_disconnect_all: HWND,
    m_list: HWND,
    header: Header,
    local: Pane,
    remote: Pane,
    term_tabs: HWND,
    term: TermView,
    term_text: HWND,
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
    local: Vec<Row>,
    remote: Vec<Row>,
    local_trail: Vec<String>,
    remote_trail: Vec<String>,
    header: Option<HeaderInfo>,
    screen: Option<TermScreen>,
    machine: String,
}

/// Everything a layout depends on.
type LayoutKey = (i32, i32, i64, u32, bool, bool, bool, bool);

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
        })
    });
    // SAFETY: showing the window we created.
    let _ = unsafe { ShowWindow(window, SW_SHOWNORMAL) };
    refresh();
    dispatch(app::Action::Start);
}

fn pane(window: HWND, local: bool) -> Pane {
    let (toggle_id, home_id, path_id, tree_id, list_id) = if local {
        (
            ids::LOCAL_TREE_TOGGLE,
            ids::LOCAL_HOME,
            ids::LOCAL_PATH,
            ids::LOCAL_TREE,
            ids::LOCAL_LIST,
        )
    } else {
        (
            ids::REMOTE_TREE_TOGGLE,
            ids::REMOTE_HOME,
            ids::REMOTE_PATH,
            ids::REMOTE_TREE,
            ids::REMOTE_LIST,
        )
    };
    let mut cols: Vec<(&str, i32)> = vec![("Name", 220), ("Size", 80), ("Modified", 90)];
    if !local {
        cols.push(("Permissions", 90));
    }
    let toggle = ui::child(
        window,
        w!("BUTTON"),
        "Folders",
        WINDOW_STYLE((BS_AUTOCHECKBOX | BS_PUSHLIKE) as u32 | WS_TABSTOP.0),
        WINDOW_EX_STYLE(0),
        toggle_id,
    );
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
        toggle,
        title: ui::label(window, if local { "This computer" } else { "" }),
        home: ui::button(window, "Home", home_id),
        other: if local {
            ui::button(window, "Leave home...", ids::LOCAL_LEAVE_HOME)
        } else {
            ui::button(window, "Up", "remote.up")
        },
        path,
        tree: controls::tree(window, tree_id),
        list: controls::list(window, list_id, &cols, icons::system_list()),
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
        m_disconnect_all: ui::button(window, "Disconnect all", ids::MACHINES_DISCONNECT_ALL),
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
        ),
        header: header::create(window),
        local: pane(window, true),
        remote: pane(window, false),
        term_tabs,
        term: term_view::create(window),
        term_text,
    }
}

/// Apply an action, run its effects, redraw.
pub fn dispatch(action: app::Action) {
    let effects = with(|s| s.app.apply(action)).unwrap_or_default();
    run(effects);
    refresh();
}

fn active_conn() -> Option<usize> {
    with(|s| match s.app.active() {
        Tab::Connection(i) => Some(i),
        Tab::Machines => None,
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
        Tab::Machines => String::new(),
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
            [h.files, h.commands, h.swap, h.close],
            info.as_ref().is_some_and(|i| live(i.files_state)),
            info.as_ref().is_some_and(|i| live(i.commands_state)),
        )
    }) else {
        return;
    };
    let [files, commands, swap, close] = buttons;
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
            Tab::Machines => (false, String::new()),
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
                if spec.purpose == "app.home" {
                    dispatch(app::Action::HomePicked(path));
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
                if spec.purpose == "local.leave_home" {
                    conn_action(conn::Action::LeaveHomeAnswered(answer.map(Into::into)));
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
                if spec.purpose == "machines.remove" {
                    dispatch(app::Action::RemoveConfirmed(yes));
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
                    "machines.add.address" => dispatch(app::Action::AddAddressAnswered(answer)),
                    "machines.add.name" => dispatch(app::Action::AddNameAnswered(answer)),
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
                if spec.purpose == "machines.add.service" {
                    dispatch(app::Action::AddServiceChosen(i));
                }
            }
            Effect::Message(text) => {
                set_status(&text);
                effects::note(owner, "legcon", &text);
            }
            Effect::CopyToClipboard(text) => effects::copy_text(owner, &text),
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

fn set_check(h: HWND, on: bool) {
    let v = if on { BST_CHECKED } else { BST_UNCHECKED };
    // SAFETY: a plain button message.
    unsafe { SendMessageW(h, BM_SETCHECK, Some(WPARAM(v.0 as usize)), None) };
}

/// Bring every control in line with the controller.
fn refresh() {
    let mut resize = None;
    let status = with(|s| {
        let looks: Vec<TabLook> = std::iter::once(TabLook {
            title: "Machines".into(),
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
            // Padding leaves room for the state light and the close box the strip paints.
            let labels: Vec<String> = looks
                .iter()
                .enumerate()
                .map(|(_, l)| l.title.clone())
                .collect();
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
                Tab::Connection(i) => i + 1,
            },
        );
        let on_machines = active == Tab::Machines;
        for h in [
            s.w.m_add,
            s.w.m_open,
            s.w.m_remove,
            s.w.m_disconnect_all,
            s.w.m_list,
        ] {
            ui::set_visible(h, on_machines);
        }
        for p in [&s.w.local, &s.w.remote] {
            for h in [p.toggle, p.title, p.home, p.other, p.path, p.list] {
                ui::set_visible(h, !on_machines);
            }
        }
        for h in [s.w.header.hwnd, s.w.term_tabs] {
            ui::set_visible(h, !on_machines);
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
                s.w.local.tree,
                s.w.remote.tree,
                s.w.term.hwnd,
                s.w.term_text,
            ] {
                ui::set_visible(h, false);
            }
            s.drawn.header = None;
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
            s.drawn.local_trail.clear();
            s.drawn.remote_trail.clear();
            s.drawn.machine = t.machine_id().to_string();
        }
        let info = t.header_info();
        if s.drawn.header.as_ref() != Some(&info) {
            header::update(&s.w.header, &info);
            s.drawn.header = Some(info);
        }
        ui::set_text(s.w.remote.title, &t.title());
        let (lt, rt) = (t.layout().local_tree, t.layout().remote_tree);
        ui::set_visible(s.w.local.tree, lt);
        ui::set_visible(s.w.remote.tree, rt);
        set_check(s.w.local.toggle, lt);
        set_check(s.w.remote.toggle, rt);
        let away = if t.away_from_home() {
            "   (outside home)"
        } else {
            ""
        };
        ui::set_text(s.w.local.path, &format!(" {}{away}", t.local_path()));
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
        let trail = t.local_trail();
        if trail != s.drawn.local_trail {
            controls::fill_trail(s.w.local.tree, &trail);
            s.drawn.local_trail = trail;
        }
        let trail = t.remote_trail();
        if trail != s.drawn.remote_trail {
            controls::fill_trail(s.w.remote.tree, &trail);
            s.drawn.remote_trail = trail;
        }
        let term = t.term_tab();
        if let Some(n) = TermTab::ALL.iter().position(|x| *x == term) {
            controls::select_tab(s.w.term_tabs, n);
        }
        let on_shell = term == TermTab::Shell;
        ui::set_visible(s.w.term.hwnd, on_shell);
        ui::set_visible(s.w.term_text, !on_shell);
        if !on_shell {
            ui::set_text(s.w.term_text, &t.term_text().replace('\n', "\r\n"));
        }
        if on_shell && ui::shown(s.w.term.hwnd) {
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
            Tab::Machines => (w, h, -1, 0, false, false, false, false),
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
                        l.local_tree,
                        l.remote_tree,
                        t.term_tab() == TermTab::Shell,
                    )
                }
                None => (w, h, -2, 0, false, false, false, false),
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
                (s.w.m_disconnect_all, 110),
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
        // Two panes, each a white box: a header band, a path row, then the tree and list.
        let pane_w = (w - 3 * m) / 2;
        let band = sc(32);
        let path_h = sc(22);
        for (k, (p, tree_on)) in [
            (&s.w.local, layout.local_tree),
            (&s.w.remote, layout.remote_tree),
        ]
        .into_iter()
        .enumerate()
        {
            let px = m + k as i32 * (pane_w + m);
            s.panes[k] = paint::rect(px, files_y, pane_w, files_h);
            let by = files_y + 1 + (band - btn) / 2;
            ui::place(p.toggle, (px + sc(5), by, sc(64), btn));
            let other_w = if k == 0 { sc(96) } else { sc(46) };
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
            let tree_w = if tree_on { pane_w * 35 / 100 } else { 0 };
            ui::place(p.tree, (px + 1, body_y, tree_w, body_h));
            let gap = i32::from(tree_on);
            ui::place(
                p.list,
                (
                    px + 1 + tree_w + gap,
                    body_y,
                    pane_w - 2 - tree_w - gap,
                    body_h,
                ),
            );
        }
        ui::place(s.w.term_tabs, (m, term_y, w - 2 * m, sc(26)));
        let body = (m, term_y + sc(26), w - 2 * m, term_h - sc(26));
        ui::place(s.w.term.hwnd, body);
        ui::place(s.w.term_text, body);
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
        let parts = with(|s| (s.panes, s.splitter, s.app.active() != Tab::Machines));
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
    let Some((hwnd, id, current)) = with(|s| {
        let t = s.app.tab(tab.checked_sub(1)?)?;
        Some((s.window, t.machine_id().to_string(), t.color()))
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
        99 => dispatch(app::Action::CloseTab(tab - 1)),
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
            if let Some((i, _)) = controls::tab_at(hwnd, x, y).filter(|(i, _)| *i > 0) {
                post(WM_APP_CLOSE_TAB, i);
            }
        }
        WM_RBUTTONUP => {
            if let Some((i, _)) = controls::tab_at(hwnd, x, y).filter(|(i, _)| *i > 0) {
                post(WM_APP_TAB_MENU, i);
            }
        }
        WM_LBUTTONUP => {
            let close_w = ui::scale(20, ui::dpi(hwnd));
            if let Some((i, _)) =
                controls::tab_at(hwnd, x, y).filter(|(i, r)| *i > 0 && x >= r.right - close_w)
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
pub fn key(vk: u16, focus: HWND) -> bool {
    let Some(c) = chord(vk) else {
        return false;
    };
    let Some(cmd) = keys::command_for(c) else {
        return false;
    };
    let (in_list, in_term) = with(|s| {
        let lists = [
            s.w.local.list,
            s.w.remote.list,
            s.w.local.tree,
            s.w.remote.tree,
            s.w.m_list,
        ];
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
        if focus == s.w.remote.list || focus == s.w.remote.tree {
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
        Command::ToggleLocalTree => conn_action(conn::Action::ToggleTree(Side::Local)),
        Command::ToggleRemoteTree => conn_action(conn::Action::ToggleTree(Side::Remote)),
        Command::LocalHome => conn_action(conn::Action::LocalHome),
        Command::LeaveHome => conn_action(conn::Action::LeaveHome),
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
        Command::NextTab | Command::PrevTab if tabs > 0 => {
            let n = tabs + 1;
            let at = current.map_or(0, |i| i + 1);
            let next = if cmd == Command::NextTab {
                (at + 1) % n
            } else {
                (at + n - 1) % n
            };
            dispatch(if next == 0 {
                app::Action::ShowMachines
            } else {
                app::Action::SelectTab(next - 1)
            });
        }
        Command::NextTab | Command::PrevTab => {}
        Command::ShowMachines => dispatch(app::Action::ShowMachines),
    }
    true
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
    let Some(b) = with(|s| {
        [
            s.w.m_add,
            s.w.m_open,
            s.w.m_remove,
            s.w.m_disconnect_all,
            s.w.local.toggle,
            s.w.local.home,
            s.w.local.other,
            s.w.remote.toggle,
            s.w.remote.home,
            s.w.remote.other,
        ]
    }) else {
        return;
    };
    let [add, open, remove, all, l_toggle, l_home, l_leave, r_toggle, r_home, r_up] = b;
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
        h if h == all => dispatch(app::Action::DisconnectAll),
        h if h == l_toggle => conn_action(conn::Action::ToggleTree(Side::Local)),
        h if h == l_home => conn_action(conn::Action::LocalHome),
        h if h == l_leave => conn_action(conn::Action::LeaveHome),
        h if h == r_toggle => conn_action(conn::Action::ToggleTree(Side::Remote)),
        h if h == r_home => conn_action(conn::Action::RemoteHome),
        h if h == r_up => conn_action(conn::Action::RemoteUp),
        _ => {}
    }
}

fn notify(hdr: &NMHDR, lp: LPARAM) {
    let Some((w, local, remote, trails)) = with(|s| {
        (
            (
                s.w.tabs,
                s.w.m_list,
                s.w.local.list,
                s.w.remote.list,
                s.w.local.tree,
                s.w.remote.tree,
                s.w.term_tabs,
            ),
            s.drawn.local.clone(),
            s.drawn.remote.clone(),
            (s.drawn.local_trail.len(), s.drawn.remote_trail.len()),
        )
    }) else {
        return;
    };
    let (tabs, m_list, l_list, r_list, l_tree, r_tree, term_tabs) = w;
    let from = hdr.hwndFrom;
    if from == tabs && hdr.code == TCN_SELCHANGE {
        match controls::tab_selected(tabs) {
            Some(0) | None => dispatch(app::Action::ShowMachines),
            Some(n) => dispatch(app::Action::SelectTab(n - 1)),
        }
    } else if from == term_tabs && hdr.code == TCN_SELCHANGE {
        if let Some(t) = controls::tab_selected(term_tabs).and_then(|n| TermTab::ALL.get(n)) {
            conn_action(conn::Action::SelectTerm(*t));
        }
    } else if hdr.code == NM_DBLCLK {
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
    } else if (from == l_tree || from == r_tree) && hdr.code == TVN_SELCHANGEDW {
        // SAFETY: TVN_SELCHANGEDW's lParam is an NMTREEVIEWW.
        let nm = unsafe { &*(lp.0 as *const NMTREEVIEWW) };
        let depth = nm.itemNew.lParam.0 as usize;
        let len = if from == l_tree { trails.0 } else { trails.1 };
        // Clicking an ancestor in the trail goes up that many levels.
        for _ in depth + 1..len {
            conn_action(if from == l_tree {
                conn::Action::LocalUp
            } else {
                conn::Action::RemoteUp
            });
        }
    }
}

fn over_splitter(y: i32) -> bool {
    with(|s| s.app.active() != Tab::Machines && y >= s.splitter.top && y < s.splitter.bottom)
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
            if wp.0 > 0 {
                dispatch(app::Action::CloseTab(wp.0 - 1));
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
            notify(hdr, lp);
            LRESULT(0)
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
                        Tab::Machines => 0.75,
                    };
                    s.drag = Some(share);
                });
                // SAFETY: capture the mouse for the drag.
                unsafe { SetCapture(hwnd) };
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lp.0 >> 16) as i16 as i32;
            let moved = with(|s| {
                s.drag?;
                let (top, h) = s.split_area;
                let swapped = match s.app.active() {
                    Tab::Connection(i) => s.app.tab(i).is_some_and(|t| t.layout().swapped),
                    Tab::Machines => false,
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
