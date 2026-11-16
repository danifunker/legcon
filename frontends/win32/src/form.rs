//! The modal dialog that draws any controller form: a tab per section, label left, control right.

use std::cell::RefCell;
use std::path::PathBuf;

use legcon::controller::{FieldKind, FieldValue, FormSpec, FormValues};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetDC, GetPixel, GetSysColorBrush, InvalidateRect,
    ReleaseDC, SetBkColor, UpdateWindow, CLR_INVALID, COLOR_BTNFACE, HBRUSH, HDC, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    NMHDR, TCIF_TEXT, TCITEMW, TCM_GETCURSEL, TCM_INSERTITEMW, TCN_SELCHANGE, WC_TABCONTROLW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowRect,
    IsDialogMessageW, IsWindow, PostMessageW, RegisterClassW, SendMessageW, SetWindowPos,
    ShowWindow, TranslateMessage, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX,
    CBN_SELCHANGE, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CW_USEDEFAULT,
    EN_CHANGE, ES_AUTOHSCROLL, ES_MULTILINE, ES_PASSWORD, ES_READONLY, HMENU, IDCANCEL, IDOK, MSG,
    SWP_NOZORDER, SW_SHOWNORMAL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_ERASEBKGND, WM_NOTIFY, WNDCLASSW, WS_CAPTION,
    WS_CLIPSIBLINGS, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU, WS_TABSTOP,
    WS_VSCROLL,
};

use crate::effects::pick_folder;
use crate::ui;

/// One field's controls; `page` is the section (tab) it sits on.
struct Row {
    id: &'static str,
    kind: Kind,
    page: usize,
    label: Option<HWND>,
    control: HWND,
    browse: Option<HWND>,
    hint: Option<HWND>,
    /// Rows of height it takes: a long read-only text wraps.
    lines: i32,
}

/// Lines a read-only text wraps to, at about 60 characters a line.
fn info_lines(kind: &FieldKind) -> i32 {
    match kind {
        FieldKind::Info(t) => t.chars().count().div_ceil(60).max(1) as i32,
        _ => 1,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Check,
    Choice,
    Info,
}

struct Form {
    window: HWND,
    tabs: Option<HWND>,
    /// The tab page's own colour and a brush of it, so labels on it match.
    page: Option<(COLORREF, HBRUSH)>,
    rows: Vec<Row>,
    error: HWND,
    ok: HWND,
    cancel: HWND,
    validator: fn(&FormValues) -> Result<(), String>,
    purpose: &'static str,
    answer: Option<FormValues>,
    done: bool,
}

thread_local! {
    static FORM: RefCell<Option<Form>> = const { RefCell::new(None) };
}

fn edit(parent: HWND, text: &str, style: u32, id: &str) -> HWND {
    ui::child(
        parent,
        w!("EDIT"),
        text,
        WINDOW_STYLE(ES_AUTOHSCROLL as u32 | style),
        WS_EX_CLIENTEDGE,
        id,
    )
}

/// Show `spec` modally over `owner`; the values on Save, `None` on Cancel.
pub fn show(owner: HWND, spec: &FormSpec) -> Option<FormValues> {
    // SAFETY: plain window creation and a modal loop over the owner, which is disabled meanwhile.
    unsafe {
        let inst = GetModuleHandleW(None).ok()?;
        let class = w!("LegconForm");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(form_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hbrBackground: HBRUSH(GetSysColorBrush(COLOR_BTNFACE).0),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let t = ui::wide(&spec.title);
        let window = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            PCWSTR(t.as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPSIBLINGS,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            Some(owner),
            Some(HMENU(std::ptr::null_mut())),
            Some(inst.into()),
            None,
        )
        .ok()?;
        let dpi = ui::dpi(window);
        let s = |v| ui::scale(v, dpi);

        // Sections become tabs; a form without headings is one page.
        let headings: Vec<&str> = spec
            .fields
            .iter()
            .filter(|f| f.kind == FieldKind::Heading)
            .map(|f| f.label.as_str())
            .collect();
        let tabs = (!headings.is_empty()).then(|| {
            let h = ui::child(
                window,
                WC_TABCONTROLW,
                "",
                WINDOW_STYLE(WS_TABSTOP.0 | WS_CLIPSIBLINGS.0),
                WINDOW_EX_STYLE(0),
                &format!("{}.sections", spec.purpose),
            );
            for (i, name) in headings.iter().enumerate() {
                let mut text = ui::wide(name);
                let item = TCITEMW {
                    mask: TCIF_TEXT,
                    pszText: windows::core::PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(
                    h,
                    TCM_INSERTITEMW,
                    Some(WPARAM(i)),
                    Some(LPARAM(&item as *const _ as isize)),
                );
            }
            h
        });

        let mut rows = Vec::new();
        let mut page = 0usize;
        let mut seen_heading = false;
        for f in &spec.fields {
            let (kind, control, label_text) = match &f.kind {
                FieldKind::Heading => {
                    if seen_heading {
                        page += 1;
                    }
                    seen_heading = true;
                    continue;
                }
                FieldKind::Text(t) | FieldKind::Folder(t) => (
                    Kind::Text,
                    edit(window, t, WS_TABSTOP.0, f.id),
                    Some(&f.label),
                ),
                FieldKind::Secret(t) => (
                    Kind::Text,
                    edit(window, t, WS_TABSTOP.0 | ES_PASSWORD as u32, f.id),
                    Some(&f.label),
                ),
                FieldKind::Info(t) if info_lines(&f.kind) > 1 => (
                    Kind::Info,
                    // Multi-line without auto-scroll, so it wraps at words.
                    ui::child(
                        window,
                        w!("EDIT"),
                        t,
                        WINDOW_STYLE(ES_MULTILINE as u32 | ES_READONLY as u32 | WS_TABSTOP.0),
                        WS_EX_CLIENTEDGE,
                        f.id,
                    ),
                    Some(&f.label),
                ),
                FieldKind::Info(t) => (
                    Kind::Info,
                    edit(window, t, WS_TABSTOP.0 | ES_READONLY as u32, f.id),
                    Some(&f.label),
                ),
                FieldKind::Check(on) => {
                    let b = ui::child(
                        window,
                        w!("BUTTON"),
                        &f.label,
                        WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0),
                        WINDOW_EX_STYLE(0),
                        f.id,
                    );
                    let v = if *on { 1 } else { 0 };
                    SendMessageW(b, BM_SETCHECK, Some(WPARAM(v)), None);
                    (Kind::Check, b, None)
                }
                FieldKind::Choice { options, selected } => {
                    let c = ui::child(
                        window,
                        w!("COMBOBOX"),
                        "",
                        WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_TABSTOP.0 | WS_VSCROLL.0),
                        WINDOW_EX_STYLE(0),
                        f.id,
                    );
                    for o in options {
                        let t = ui::wide(o);
                        SendMessageW(c, CB_ADDSTRING, None, Some(LPARAM(t.as_ptr() as isize)));
                    }
                    SendMessageW(c, CB_SETCURSEL, Some(WPARAM(*selected)), None);
                    (Kind::Choice, c, Some(&f.label))
                }
            };
            let browse = matches!(f.kind, FieldKind::Folder(_))
                .then(|| ui::button(window, "Browse...", &format!("{}.browse", f.id)));
            rows.push(Row {
                id: f.id,
                kind,
                page,
                label: label_text.map(|l| ui::label(window, l)),
                control,
                browse,
                hint: (!f.hint.is_empty()).then(|| ui::label(window, &f.hint)),
                lines: info_lines(&f.kind),
            });
        }
        let error = ui::label(window, "");
        let okb = ui::button(window, &spec.ok_label, &format!("{}.ok", spec.purpose));
        let cancel = ui::button(window, "Cancel", &format!("{}.cancel", spec.purpose));

        // Layout: every page is laid out from the same top; the tallest sets the height.
        let width = s(560);
        let m = s(12);
        let label_w = s(170);
        let row_h = s(24);
        let hint_h = s(18);
        let top = if tabs.is_some() { m + s(30) } else { m };
        let control_x = m + s(8) + label_w;
        let control_w = width - control_x - m - s(8);
        let mut bottom = top;
        let pages = page + 1;
        for p in 0..pages {
            let mut y = top + s(6);
            for r in rows.iter().filter(|r| r.page == p) {
                if let Some(l) = r.label {
                    ui::place(l, (m + s(8), y, label_w - s(6), row_h));
                }
                let h = if r.kind == Kind::Choice {
                    s(220)
                } else {
                    row_h * r.lines
                };
                match (r.kind, r.browse) {
                    (_, Some(b)) => {
                        ui::place(r.control, (control_x, y, control_w - s(96), h));
                        ui::place(b, (control_x + control_w - s(90), y, s(90), row_h));
                    }
                    (Kind::Check, None) => {
                        ui::place(r.control, (m + s(8), y, width - 2 * m - s(16), h))
                    }
                    _ => ui::place(r.control, (control_x, y, control_w, h)),
                }
                y += row_h * r.lines + s(2);
                if let Some(hint) = r.hint {
                    let x = if r.kind == Kind::Check {
                        m + s(26)
                    } else {
                        control_x
                    };
                    ui::place(hint, (x, y, width - x - m - s(8), hint_h));
                    y += hint_h;
                }
                y += s(6);
            }
            bottom = bottom.max(y);
        }
        if let Some(t) = tabs {
            ui::place(t, (m, m, width - 2 * m, bottom - m + s(4)));
            // The tab control is a sibling drawn under the fields, not their parent.
            let _ = SetWindowPos(
                t,
                Some(windows::Win32::UI::WindowsAndMessaging::HWND_BOTTOM),
                0,
                0,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
            );
        }
        let mut y = bottom + s(10);
        ui::place(error, (m, y, width - 2 * m, row_h));
        y += row_h + s(6);
        ui::place(cancel, (width - m - s(85), y, s(85), s(28)));
        ui::place(okb, (width - 2 * m - s(170), y, s(85), s(28)));
        y += s(28) + m;

        let mut r = Default::default();
        let _ = GetWindowRect(owner, &mut r);
        let (cw, ch) = (width + s(16), y + s(40));
        let x = r.left + ((r.right - r.left) - cw) / 2;
        let wy = r.top + ((r.bottom - r.top) - ch) / 3;
        let _ = SetWindowPos(window, None, x, wy.max(r.top), cw, ch, SWP_NOZORDER);

        let first = rows
            .iter()
            .find(|r| r.kind != Kind::Info)
            .map(|r| r.control);
        FORM.with(|f| {
            *f.borrow_mut() = Some(Form {
                window,
                tabs,
                rows,
                error,
                ok: okb,
                cancel,
                page: None,
                validator: spec.validator,
                purpose: spec.purpose,
                answer: None,
                done: false,
            });
        });
        show_page(0);
        check();
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(window, SW_SHOWNORMAL);
        if let Some(page) = tabs.and_then(page_color) {
            FORM.with(|f| {
                if let Some(f) = f.borrow_mut().as_mut() {
                    f.page = Some(page);
                }
            });
            let _ = InvalidateRect(Some(window), None, true);
        }
        if let Some(c) = first {
            let _ = SetFocus(Some(c));
        }
        let mut msg = MSG::default();
        while FORM.with(|f| f.borrow().as_ref().is_some_and(|f| !f.done)) {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // WM_QUIT belongs to the main loop: put it back.
                let _ = PostMessageW(None, msg.message, msg.wParam, msg.lParam);
                break;
            }
            if !IsDialogMessageW(window, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = EnableWindow(owner, true);
        let form = FORM.with(|f| f.borrow_mut().take());
        if let Some((_, brush)) = form.as_ref().and_then(|f| f.page) {
            let _ = DeleteObject(HGDIOBJ(brush.0));
        }
        let answer = form.and_then(|f| f.answer);
        if IsWindow(Some(window)).as_bool() {
            let _ = DestroyWindow(window);
        }
        let _ = SetFocus(Some(owner));
        answer
    }
}

/// The tab page's fill as drawn, which differs from the dialog's (sampled: theme hints are off).
fn page_color(tabs: HWND) -> Option<(COLORREF, HBRUSH)> {
    // SAFETY: a DC of our own control, released again.
    unsafe {
        let _ = UpdateWindow(tabs);
        let r = ui::client(tabs);
        let dc = GetDC(Some(tabs));
        let c = GetPixel(dc, r.left + 6, r.bottom - 6);
        ReleaseDC(Some(tabs), dc);
        (c.0 != CLR_INVALID).then(|| (c, CreateSolidBrush(c)))
    }
}

/// Show the controls of section `page` only.
fn show_page(page: usize) {
    FORM.with(|f| {
        let f = f.borrow();
        let Some(f) = f.as_ref() else { return };
        for r in &f.rows {
            let on = r.page == page;
            for h in [r.label, Some(r.control), r.browse, r.hint]
                .into_iter()
                .flatten()
            {
                ui::set_visible(h, on);
            }
        }
    });
}

fn values(f: &Form) -> FormValues {
    FormValues(
        f.rows
            .iter()
            .map(|r| {
                // SAFETY: plain control queries.
                let v = unsafe {
                    match r.kind {
                        Kind::Text => FieldValue::Text(ui::text(r.control)),
                        Kind::Check => FieldValue::Check(
                            SendMessageW(r.control, BM_GETCHECK, None, None).0 == 1,
                        ),
                        Kind::Choice => FieldValue::Choice(
                            SendMessageW(r.control, CB_GETCURSEL, None, None).0.max(0) as usize,
                        ),
                        Kind::Info => FieldValue::None,
                    }
                };
                (r.id, v)
            })
            .collect(),
    )
}

/// Run the controller's rule over the whole form: the error shows and Save follows it.
fn check() {
    FORM.with(|f| {
        let f = f.borrow();
        let Some(f) = f.as_ref() else { return };
        let verdict = (f.validator)(&values(f));
        ui::set_text(
            f.error,
            verdict.as_ref().err().map(String::as_str).unwrap_or(""),
        );
        ui::set_enabled(f.ok, verdict.is_ok());
    });
}

fn finish(ok: bool) {
    FORM.with(|f| {
        if let Some(f) = f.borrow_mut().as_mut() {
            if ok {
                let v = values(f);
                if (f.validator)(&v).is_err() {
                    return;
                }
                f.answer = Some(v);
            }
            f.done = true;
        }
    });
}

fn browse(button: HWND) {
    let Some((window, edit, purpose)) = FORM.with(|f| {
        let f = f.borrow();
        let f = f.as_ref()?;
        let r = f.rows.iter().find(|r| r.browse == Some(button))?;
        Some((f.window, r.control, f.purpose))
    }) else {
        return;
    };
    let start = PathBuf::from(ui::text(edit));
    let start = start.is_dir().then_some(start);
    if let Some(path) = pick_folder(window, purpose, "Choose a folder", start.as_ref()) {
        ui::set_text(edit, &path.display().to_string());
        check();
    }
}

unsafe extern "system" fn form_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let code = ((wp.0 >> 16) & 0xffff) as u32;
            let from = HWND(lp.0 as _);
            let id = (wp.0 & 0xffff) as i32;
            let (ok, cancel, is_browse) = FORM.with(|f| {
                f.borrow().as_ref().map_or(Default::default(), |f| {
                    (
                        f.ok,
                        f.cancel,
                        f.rows.iter().any(|r| r.browse == Some(from)),
                    )
                })
            });
            if code == BN_CLICKED && is_browse {
                browse(from);
            } else if (code == BN_CLICKED && from == ok) || (id == IDOK.0 && from.is_invalid()) {
                finish(true);
            } else if (code == BN_CLICKED && from == cancel) || id == IDCANCEL.0 {
                finish(false);
            } else if code == EN_CHANGE || code == BN_CLICKED || code == CBN_SELCHANGE {
                check();
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            // SAFETY: WM_NOTIFY's lParam is an NMHDR from a child control.
            let hdr = unsafe { &*(lp.0 as *const NMHDR) };
            let tabs = FORM.with(|f| f.borrow().as_ref().and_then(|f| f.tabs));
            if Some(hdr.hwndFrom) == tabs && hdr.code == TCN_SELCHANGE {
                // SAFETY: a plain tab control query.
                let i = unsafe { SendMessageW(hdr.hwndFrom, TCM_GETCURSEL, None, None).0 };
                show_page(i.max(0) as usize);
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            // Controls on the tab page take its colour; the error line keeps the dialog's.
            let from = HWND(lp.0 as _);
            let page = FORM.with(|f| {
                let f = f.borrow();
                let f = f.as_ref()?;
                let on_page = f
                    .rows
                    .iter()
                    .any(|r| [r.label, Some(r.control), r.hint].contains(&Some(from)));
                f.page.filter(|_| on_page)
            });
            match page {
                Some((color, brush)) => {
                    // SAFETY: the DC is the one the control passed for this paint.
                    unsafe { SetBkColor(HDC(wp.0 as _), color) };
                    LRESULT(brush.0 as isize)
                }
                // SAFETY: the default colours.
                None => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
            }
        }
        WM_CLOSE => {
            finish(false);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            let r = ui::client(hwnd);
            // SAFETY: painting our own window's background.
            unsafe { FillRect(HDC(wp.0 as _), &r, GetSysColorBrush(COLOR_BTNFACE)) };
            LRESULT(1)
        }
        // SAFETY: the default handling of a window we registered.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
