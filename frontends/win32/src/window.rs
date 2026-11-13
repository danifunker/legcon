//! The main window: connection tabs over the Machines page or a connection view.
//!
//! The view only renders `AppController` and turns input into its actions (CONTRIBUTING.md).
//! Effects are run with the controller released, since their dialogs pump messages.

use std::cell::RefCell;
use std::sync::Arc;

use legcon::controller::app::{self, default_dir, AppController, Tab};
use legcon::controller::connection::{self as conn, Row, Side, TermTab};
use legcon::controller::keys::{self, Chord, Command, Key, Mods};
use legcon::controller::{ids, ConnState, Controller, Effect};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    FillRect, GetSysColorBrush, ScreenToClient, COLOR_BTNFACE, HBRUSH, HDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    NMHDR, NMTREEVIEWW, NM_DBLCLK, SB_SETTEXTW, TCN_SELCHANGE, TVN_SELCHANGEDW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, LoadCursorW, PostQuitMessage,
    RegisterClassW, SendMessageW, SetCursor, ShowWindow, BN_CLICKED, CW_USEDEFAULT, ES_AUTOHSCROLL,
    ES_MULTILINE, ES_READONLY, HMENU, IDC_ARROW, IDC_SIZENS, SW_SHOWNORMAL, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NOTIFY, WM_SETCURSOR, WM_SIZE, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_VSCROLL,
};

use crate::controls;
use crate::effects::{self, Ask};
use crate::ui;
use crate::wake::{Win32Wake, WM_APP_JOB};

/// The connection view's controls; one set, filled from whichever tab is active.
struct ConnWidgets {
    header: HWND,
    badge: HWND,
    connect: HWND,
    disconnect: HWND,
    swap: HWND,
    close: HWND,
    l_toggle: HWND,
    l_title: HWND,
    l_home: HWND,
    l_leave: HWND,
    l_path: HWND,
    l_tree: HWND,
    l_list: HWND,
    r_toggle: HWND,
    r_title: HWND,
    r_home: HWND,
    r_up: HWND,
    r_path: HWND,
    r_tree: HWND,
    r_list: HWND,
    term_tabs: HWND,
    term_body: HWND,
}

struct Widgets {
    tabs: HWND,
    status: HWND,
    m_add: HWND,
    m_open: HWND,
    m_remove: HWND,
    m_disconnect_all: HWND,
    m_list: HWND,
    c: ConnWidgets,
}

/// What was last drawn, so a refresh only touches what changed.
#[derive(Default, PartialEq)]
struct Drawn {
    tabs: Vec<String>,
    machines: Vec<Vec<String>>,
    local: Vec<Row>,
    remote: Vec<Row>,
    local_trail: Vec<String>,
    remote_trail: Vec<String>,
    term: Option<usize>,
    machine: String,
}

struct Shell {
    window: HWND,
    app: AppController,
    wake: Arc<Win32Wake>,
    w: Widgets,
    drawn: Drawn,
    /// Set while the splitter is dragged: the live share.
    drag: Option<f32>,
    /// The splitter's rectangle (y, height) in client pixels, from the last layout.
    splitter: (i32, i32),
    /// The split area's top and height, for turning a drag into a share.
    split_area: (i32, i32),
    /// What the last layout was for; a layout with nothing changed is skipped (no flicker).
    laid: Option<LayoutKey>,
}

/// Everything a layout depends on.
type LayoutKey = (i32, i32, i64, u32, bool, bool, bool, bool);

thread_local! {
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
}

/// Borrow the shell; `None` while it is already borrowed (a nested message during a call).
fn with<R>(f: impl FnOnce(&mut Shell) -> R) -> Option<R> {
    SHELL.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
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
            hbrBackground: HBRUSH(GetSysColorBrush(COLOR_BTNFACE).0),
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
            splitter: (0, 0),
            split_area: (0, 0),
            laid: None,
        })
    });
    // SAFETY: showing the window we created.
    let _ = unsafe { ShowWindow(window, SW_SHOWNORMAL) };
    refresh();
    dispatch(app::Action::Start);
}

fn build(window: HWND) -> Widgets {
    let edit_ro = |id: &str| {
        ui::child(
            window,
            w!("EDIT"),
            "",
            WINDOW_STYLE(ES_AUTOHSCROLL as u32 | ES_READONLY as u32),
            WS_EX_CLIENTEDGE,
            id,
        )
    };
    let files_cols: &[(&str, i32)] = &[("Name", 220), ("Size", 80), ("Modified", 90)];
    let remote_cols: &[(&str, i32)] = &[
        ("Name", 200),
        ("Size", 70),
        ("Modified", 90),
        ("Permissions", 90),
    ];
    let term_body = ui::child(
        window,
        w!("EDIT"),
        "",
        WINDOW_STYLE(ES_MULTILINE as u32 | ES_READONLY as u32 | WS_VSCROLL.0),
        WS_EX_CLIENTEDGE,
        ids::TERM_BODY,
    );
    ui::set_font(term_body, ui::mono_font_for(window));
    let term_tabs = controls::tabs(window, ids::TERM_TABS);
    controls::set_tabs(
        term_tabs,
        &TermTab::ALL
            .iter()
            .map(|t| t.label().to_string())
            .collect::<Vec<_>>(),
    );
    let status = ui::child(
        window,
        w!("msctls_statusbar32"),
        "",
        WINDOW_STYLE(0),
        WINDOW_EX_STYLE(0),
        "status",
    );
    Widgets {
        tabs: controls::tabs(window, "tabs"),
        status,
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
        ),
        c: ConnWidgets {
            header: ui::label(window, ""),
            badge: ui::label(window, "[plaintext]"),
            connect: ui::button(window, "Connect", ids::CONN_CONNECT),
            disconnect: ui::button(window, "Disconnect", ids::CONN_DISCONNECT),
            swap: ui::button(window, "Swap", ids::CONN_SWAP),
            close: ui::button(window, "Close tab", "conn.close_tab"),
            l_toggle: ui::button(window, "Folders", ids::LOCAL_TREE_TOGGLE),
            l_title: ui::label(window, "This computer"),
            l_home: ui::button(window, "Home", ids::LOCAL_HOME),
            l_leave: ui::button(window, "Leave home...", ids::LOCAL_LEAVE_HOME),
            l_path: edit_ro(ids::LOCAL_PATH),
            l_tree: controls::tree(window, ids::LOCAL_TREE),
            l_list: controls::list(window, ids::LOCAL_LIST, files_cols),
            r_toggle: ui::button(window, "Folders", ids::REMOTE_TREE_TOGGLE),
            r_title: ui::label(window, ""),
            r_home: ui::button(window, "Home", ids::REMOTE_HOME),
            r_up: ui::button(window, "Up", "remote.up"),
            r_path: edit_ro(ids::REMOTE_PATH),
            r_tree: controls::tree(window, ids::REMOTE_TREE),
            r_list: controls::list(window, ids::REMOTE_LIST, remote_cols),
            term_tabs,
            term_body,
        },
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

fn conn_action(a: conn::Action) {
    if let Some(i) = active_conn() {
        dispatch(app::Action::Conn(i, a));
    }
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

fn state_mark(s: ConnState) -> &'static str {
    match s {
        ConnState::Connected | ConnState::Busy => "* ",
        ConnState::Connecting | ConnState::Disconnecting => "~ ",
        ConnState::Disconnected => "o ",
        ConnState::Lost => "! ",
    }
}

fn cells(rows: &[Row], perms: bool) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            let name = if r.is_dir {
                format!("[{}]", r.name)
            } else {
                r.name.clone()
            };
            let mut v = vec![name, r.size.clone(), r.modified.clone()];
            if perms {
                v.push(r.mode.clone());
            }
            v
        })
        .collect()
}

/// Bring every control in line with the controller.
fn refresh() {
    let status = with(|s| {
        let tabs: Vec<String> = std::iter::once("Machines".to_string())
            .chain(
                s.app
                    .tabs()
                    .iter()
                    .map(|t| format!("{}{}", state_mark(t.state()), t.title())),
            )
            .collect();
        if tabs != s.drawn.tabs {
            controls::set_tabs(s.w.tabs, &tabs);
            s.drawn.tabs = tabs;
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
        let c = &s.w.c;
        for h in [
            c.header,
            c.connect,
            c.disconnect,
            c.swap,
            c.close,
            c.l_toggle,
            c.l_title,
            c.l_home,
            c.l_leave,
            c.l_path,
            c.r_toggle,
            c.r_title,
            c.r_home,
            c.r_up,
            c.r_path,
            c.term_tabs,
            c.term_body,
            c.l_list,
            c.r_list,
        ] {
            ui::set_visible(h, !on_machines);
        }
        let machines: Vec<Vec<String>> = s
            .app
            .machines()
            .into_iter()
            .map(|m| {
                vec![
                    m.name,
                    m.address,
                    m.services,
                    state_text(m.state).to_string(),
                    if m.password_saved {
                        "Saved (encoded)".into()
                    } else {
                        "Asks".into()
                    },
                ]
            })
            .collect();
        if machines != s.drawn.machines {
            controls::fill(s.w.m_list, &machines);
            s.drawn.machines = machines;
        }
        let Tab::Connection(i) = active else {
            ui::set_visible(c.badge, false);
            ui::set_visible(c.l_tree, false);
            ui::set_visible(c.r_tree, false);
            return format!(
                "{} machines saved. Double-click one to open it.",
                s.drawn.machines.len()
            );
        };
        let Some(t) = s.app.tab(i) else {
            return String::new();
        };
        if s.drawn.machine != t.machine_id() {
            s.drawn = Drawn {
                tabs: std::mem::take(&mut s.drawn.tabs),
                machines: std::mem::take(&mut s.drawn.machines),
                ..Drawn::default()
            };
            s.drawn.machine = t.machine_id().to_string();
        }
        ui::set_text(
            c.header,
            &format!("{}    [{}]", t.header(), state_text(t.state())),
        );
        ui::set_visible(c.badge, t.plaintext());
        ui::set_enabled(c.connect, t.can_connect());
        ui::set_enabled(c.disconnect, t.can_disconnect());
        ui::set_text(c.r_title, &t.title());
        let lt = t.layout().local_tree;
        let rt = t.layout().remote_tree;
        ui::set_visible(c.l_tree, lt);
        ui::set_visible(c.r_tree, rt);
        ui::set_text(c.l_toggle, if lt { "Hide folders" } else { "Folders" });
        ui::set_text(c.r_toggle, if rt { "Hide folders" } else { "Folders" });
        let away = if t.away_from_home() {
            "   (outside home)"
        } else {
            ""
        };
        ui::set_text(c.l_path, &format!("{}{away}", t.local_path()));
        ui::set_text(c.r_path, &t.remote_path());
        ui::set_enabled(c.r_home, t.can_disconnect());
        ui::set_enabled(c.r_up, t.can_disconnect());
        let local = t.local_rows();
        if local != s.drawn.local {
            controls::fill(c.l_list, &cells(&local, false));
            s.drawn.local = local;
        }
        let remote = t.remote_rows();
        if remote != s.drawn.remote {
            controls::fill(c.r_list, &cells(&remote, true));
            s.drawn.remote = remote;
        }
        let lt = t.local_trail();
        if lt != s.drawn.local_trail {
            controls::fill_trail(c.l_tree, &lt);
            s.drawn.local_trail = lt;
        }
        let rt = t.remote_trail();
        if rt != s.drawn.remote_trail {
            controls::fill_trail(c.r_tree, &rt);
            s.drawn.remote_trail = rt;
        }
        let term = TermTab::ALL.iter().position(|x| *x == t.term_tab());
        if let Some(n) = term {
            controls::select_tab(c.term_tabs, n);
        }
        ui::set_text(c.term_body, &t.term_text().replace('\n', "\r\n"));
        s.drawn.term = term;
        t.status_line()
    });
    if let Some(text) = status {
        set_status(&text);
    }
    layout();
}

/// Place every control for the current client size, tab and layout; false when nothing moved.
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
                        ui::shown(s.w.c.badge),
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
            let mut sr = Default::default();
            // SAFETY: a plain query of our own control.
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowRect(s.w.status, &mut sr)
            };
            sr.bottom - sr.top
        };
        let m = sc(6);
        let tab_h = sc(26);
        ui::place(s.w.tabs, (0, 0, w, tab_h));
        let top = tab_h + m;
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
                ui::place(b, (x, top, sc(bw), btn));
                x += sc(bw) + m;
            }
            ui::place(
                s.w.m_list,
                (m, top + btn + m, w - 2 * m, bottom - top - btn - m),
            );
            return true;
        }
        let Tab::Connection(i) = s.app.active() else {
            return true;
        };
        let Some(t) = s.app.tab(i) else { return true };
        let layout = t.layout().clone();
        let c = &s.w.c;
        // Header row: the machine line, then its buttons at the right.
        let mut x = w - m;
        for (b, bw) in [
            (c.close, 80),
            (c.swap, 70),
            (c.disconnect, 90),
            (c.connect, 80),
        ] {
            x -= sc(bw);
            ui::place(b, (x, top, sc(bw), btn));
            x -= m;
        }
        let badge_w = if ui::shown(c.badge) { sc(80) } else { 0 };
        x -= badge_w;
        ui::place(c.badge, (x, top, badge_w, btn));
        ui::place(c.header, (m, top, x - 2 * m, btn));
        // The split area: files and terminal with the splitter between.
        let area_top = top + btn + m;
        let area_h = bottom - area_top;
        let split_h = sc(6);
        let share = s.drag.unwrap_or(layout.files_share);
        let files_h = ((area_h - split_h) as f32 * share) as i32;
        let term_h = area_h - split_h - files_h;
        let (files_y, split_y, term_y) = if layout.swapped {
            (area_top + term_h + split_h, area_top + term_h, area_top)
        } else {
            (area_top, area_top + files_h, area_top + files_h + split_h)
        };
        s.splitter = (split_y, split_h);
        s.split_area = (area_top, area_h - split_h);
        // Files area: two panes.
        let pane_w = (w - 3 * m) / 2;
        let row = sc(26);
        let pane = |px: i32,
                    toggle: HWND,
                    title: HWND,
                    b1: (HWND, i32),
                    b2: (HWND, i32),
                    path: HWND,
                    tree: HWND,
                    list: HWND,
                    tree_on: bool| {
            let mut bx = px;
            ui::place(toggle, (bx, files_y, sc(90), row));
            bx += sc(90) + m;
            let right = px + pane_w;
            let b2x = right - sc(b2.1);
            let b1x = b2x - m - sc(b1.1);
            ui::place(b2.0, (b2x, files_y, sc(b2.1), row));
            ui::place(b1.0, (b1x, files_y, sc(b1.1), row));
            ui::place(title, (bx, files_y, b1x - bx - m, row));
            ui::place(path, (px, files_y + row + sc(4), pane_w, sc(22)));
            let body_y = files_y + row + sc(30);
            let body_h = files_h - row - sc(30);
            let tree_w = if tree_on { pane_w * 35 / 100 } else { 0 };
            ui::place(tree, (px, body_y, tree_w, body_h));
            let gap = if tree_on { sc(4) } else { 0 };
            ui::place(
                list,
                (px + tree_w + gap, body_y, pane_w - tree_w - gap, body_h),
            );
        };
        pane(
            m,
            c.l_toggle,
            c.l_title,
            (c.l_home, 60),
            (c.l_leave, 100),
            c.l_path,
            c.l_tree,
            c.l_list,
            layout.local_tree,
        );
        pane(
            2 * m + pane_w,
            c.r_toggle,
            c.r_title,
            (c.r_home, 60),
            (c.r_up, 50),
            c.r_path,
            c.r_tree,
            c.r_list,
            layout.remote_tree,
        );
        // Terminal area: its tab strip, then the body.
        ui::place(c.term_tabs, (m, term_y, w - 2 * m, sc(24)));
        ui::place(
            c.term_body,
            (m, term_y + sc(24), w - 2 * m, term_h - sc(24)),
        );
        true
    });
    if moved != Some(true) {
        return;
    }
    // Repaint the window and every control, so nothing keeps pixels from where a control was.
    if let Some(hwnd) = with(|s| s.window) {
        use windows::Win32::Graphics::Gdi::{
            RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE,
        };
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
    let Some(c) = chord(vk) else { return false };
    let Some(cmd) = keys::command_for(c) else {
        return false;
    };
    // Plain keys only count in the lists and trees, not while typing in a field.
    let in_list = with(|s| {
        [
            s.w.c.l_list,
            s.w.c.r_list,
            s.w.c.l_tree,
            s.w.c.r_tree,
            s.w.m_list,
        ]
        .contains(&focus)
    })
    .unwrap_or(false);
    if !c.mods.primary && !in_list {
        return false;
    }
    let side = with(|s| {
        if focus == s.w.c.r_list || focus == s.w.c.r_tree {
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

fn list_row(list: HWND, rows: &[Row]) -> Option<Row> {
    controls::selected(list).and_then(|i| rows.get(i).cloned())
}

fn command(from: HWND, code: u32) {
    if code != BN_CLICKED {
        return;
    }
    let Some((w, selected_machine)) = with(|s| {
        let ids: Vec<String> = s.app.machines().into_iter().map(|m| m.id).collect();
        let sel = controls::selected(s.w.m_list).and_then(|i| ids.get(i).cloned());
        (
            [
                s.w.m_add,
                s.w.m_open,
                s.w.m_remove,
                s.w.m_disconnect_all,
                s.w.c.connect,
                s.w.c.disconnect,
                s.w.c.swap,
                s.w.c.close,
                s.w.c.l_toggle,
                s.w.c.l_home,
                s.w.c.l_leave,
                s.w.c.r_toggle,
                s.w.c.r_home,
                s.w.c.r_up,
            ],
            sel,
        )
    }) else {
        return;
    };
    let [add, open, remove, all, connect, disconnect, swap, close, l_toggle, l_home, l_leave, r_toggle, r_home, r_up] =
        w;
    match from {
        h if h == add => dispatch(app::Action::AddMachine),
        h if h == open => {
            if let Some(id) = selected_machine {
                dispatch(app::Action::OpenMachine(id));
            }
        }
        h if h == remove => {
            if let Some(id) = selected_machine {
                dispatch(app::Action::RemoveMachine(id));
            }
        }
        h if h == all => dispatch(app::Action::DisconnectAll),
        h if h == connect => conn_action(conn::Action::Connect),
        h if h == disconnect => conn_action(conn::Action::Disconnect),
        h if h == swap => conn_action(conn::Action::Swap),
        h if h == close => {
            if let Some(i) = active_conn() {
                dispatch(app::Action::CloseTab(i));
            }
        }
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
                s.w.c.l_list,
                s.w.c.r_list,
                s.w.c.l_tree,
                s.w.c.r_tree,
                s.w.c.term_tabs,
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
            let id = with(|s| {
                let ids: Vec<String> = s.app.machines().into_iter().map(|m| m.id).collect();
                controls::selected(m_list).and_then(|i| ids.get(i).cloned())
            })
            .flatten();
            if let Some(id) = id {
                dispatch(app::Action::OpenMachine(id));
            }
        } else if from == l_list {
            if let Some(r) = list_row(l_list, &local).filter(|r| r.is_dir) {
                conn_action(conn::Action::LocalOpen(r.name));
            }
        } else if from == r_list {
            if let Some(r) = list_row(r_list, &remote).filter(|r| r.is_dir) {
                conn_action(conn::Action::RemoteOpen(r.name));
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
    with(|s| {
        s.app.active() != Tab::Machines
            && y >= s.splitter.0 - 2
            && y < s.splitter.0 + s.splitter.1 + 2
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

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_JOB => {
            tick();
            LRESULT(0)
        }
        WM_SIZE => {
            layout();
            LRESULT(0)
        }
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
                    s.drag = Some(
                        s.app
                            .tab(match s.app.active() {
                                Tab::Connection(i) => i,
                                Tab::Machines => 0,
                            })
                            .map_or(0.75, |t| t.layout().files_share),
                    )
                });
                // SAFETY: capture the mouse for the drag.
                unsafe { SetCapture(hwnd) };
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = (lp.0 >> 16) as i16 as i32;
            let moved = with(|s| {
                let _ = s.drag?;
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
        WM_ERASEBKGND => {
            let r = ui::client(hwnd);
            // SAFETY: painting our own background.
            unsafe { FillRect(HDC(wp.0 as _), &r, GetSysColorBrush(COLOR_BTNFACE)) };
            LRESULT(1)
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
