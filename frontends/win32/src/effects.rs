//! Dialogs that fulfil the controllers' effects: the folder picker, task dialogs, the clipboard,
//! and one small modal prompt that serves text, password and path questions.

use std::cell::RefCell;
use std::path::PathBuf;

use legcon::controller::picks;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{FillRect, GetSysColorBrush, COLOR_BTNFACE, HBRUSH, HDC};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, EM_SETSEL, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDCBF_CANCEL_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW, TD_INFORMATION_ICON,
    TD_WARNING_ICON,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
    FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowRect,
    IsDialogMessageW, IsWindow, PostMessageW, RegisterClassW, SendMessageW, SetWindowPos,
    ShowWindow, TranslateMessage, BN_CLICKED, CW_USEDEFAULT, EN_CHANGE, ES_AUTOHSCROLL,
    ES_PASSWORD, HMENU, IDCANCEL, IDOK, LBN_DBLCLK, LBN_SELCHANGE, LBS_NOTIFY, LB_ADDSTRING,
    LB_GETCURSEL, MSG, SWP_NOZORDER, SW_SHOWNORMAL, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_ERASEBKGND, WNDCLASSW, WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_POPUP,
    WS_SYSMENU, WS_TABSTOP, WS_VSCROLL,
};

use crate::ui;

/// The system folder picker, or the flow suite's scripted answer.
pub fn pick_folder(
    owner: HWND,
    purpose: &str,
    title: &str,
    start: Option<&PathBuf>,
) -> Option<PathBuf> {
    if let Some(answer) = picks::scripted(purpose) {
        return answer;
    }
    // SAFETY: COM is initialised on this thread; every string outlives the call that takes it.
    unsafe {
        let d: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let dialog: IFileDialog = d.cast().ok()?;
        let opts = dialog.GetOptions().ok()? | FOS_FORCEFILESYSTEM | FOS_PICKFOLDERS;
        let _ = dialog.SetOptions(opts);
        let t = ui::wide(title);
        let _ = dialog.SetTitle(PCWSTR(t.as_ptr()));
        if let Some(start) = start {
            let s = ui::wide(&start.display().to_string());
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(s.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// The system file picker for an existing file, or the flow suite's scripted answer.
pub fn pick_file(
    owner: HWND,
    purpose: &str,
    title: &str,
    start: Option<&PathBuf>,
) -> Option<PathBuf> {
    if let Some(answer) = picks::scripted(purpose) {
        return answer;
    }
    // SAFETY: COM is initialised on this thread; every string outlives the call that takes it.
    unsafe {
        let d: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let dialog: IFileDialog = d.cast().ok()?;
        let opts = dialog.GetOptions().ok()? | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;
        let _ = dialog.SetOptions(opts);
        let t = ui::wide(title);
        let _ = dialog.SetTitle(PCWSTR(t.as_ptr()));
        if let Some(start) = start {
            let s = ui::wide(&start.display().to_string());
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(s.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(PathBuf::from)
    }
}

const FIRST_BUTTON: i32 = 100;

/// A Task Dialog with `buttons` and optionally Cancel; the button pressed, if any.
fn task_dialog(
    owner: HWND,
    title: &str,
    message: &str,
    buttons: &[String],
    warn: bool,
    cancel: bool,
) -> Option<usize> {
    let title_w = ui::wide(title);
    let message_w = ui::wide(message);
    let labels: Vec<Vec<u16>> = buttons.iter().map(|b| ui::wide(b)).collect();
    let specs: Vec<TASKDIALOG_BUTTON> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| TASKDIALOG_BUTTON {
            nButtonID: FIRST_BUTTON + i as i32,
            pszButtonText: PCWSTR(l.as_ptr()),
        })
        .collect();
    let mut cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        pszWindowTitle: PCWSTR(title_w.as_ptr()),
        pszContent: PCWSTR(message_w.as_ptr()),
        cButtons: specs.len() as u32,
        pButtons: specs.as_ptr(),
        nDefaultButton: if cancel { IDCANCEL.0 } else { FIRST_BUTTON },
        ..Default::default()
    };
    if cancel {
        cfg.dwCommonButtons = TDCBF_CANCEL_BUTTON;
    }
    cfg.Anonymous1.pszMainIcon = if warn {
        TD_WARNING_ICON
    } else {
        TD_INFORMATION_ICON
    };
    let mut pressed = 0i32;
    // SAFETY: every pointer in `cfg` points into locals that outlive the call.
    let ok = unsafe { TaskDialogIndirect(&cfg, Some(&mut pressed), None, None) };
    if ok.is_err() {
        return None;
    }
    let i = pressed - FIRST_BUTTON;
    (0..buttons.len() as i32).contains(&i).then_some(i as usize)
}

pub fn confirm(owner: HWND, title: &str, message: &str, label: &str, destructive: bool) -> bool {
    task_dialog(
        owner,
        title,
        message,
        &[label.to_string()],
        destructive,
        true,
    )
    .is_some()
}

pub fn choose(
    owner: HWND,
    title: &str,
    message: &str,
    choices: &[String],
    dismiss: usize,
) -> usize {
    task_dialog(owner, title, message, choices, false, false).unwrap_or(dismiss)
}

pub fn note(owner: HWND, title: &str, message: &str) {
    task_dialog(owner, title, message, &["OK".to_string()], false, false);
}

pub fn copy_text(owner: HWND, text: &str) {
    let w = ui::wide(&ui::plain(text));
    let bytes = w.len() * 2;
    // SAFETY: the clipboard owns the global block once SetClipboardData succeeds.
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                let _ = GlobalUnlock(mem);
                let _ = SetClipboardData(
                    CF_UNICODETEXT.0 as u32,
                    Some(windows::Win32::Foundation::HANDLE(mem.0)),
                );
            }
        }
        let _ = CloseClipboard();
    }
}

/// A controller's rule for a prompt's text.
type Validator = fn(&str) -> Result<(), String>;

/// What a prompt asks for.
pub struct Ask<'a> {
    pub title: &'a str,
    pub prompt: &'a str,
    pub initial: &'a str,
    pub ok: &'a str,
    pub secret: bool,
    /// Recent places to pick from, and a Browse... button, for a path prompt.
    pub recent: Option<&'a [PathBuf]>,
    pub hint: &'a str,
    pub validator: Option<Validator>,
    pub purpose: &'a str,
}

struct Prompt {
    window: HWND,
    edit: HWND,
    error: HWND,
    ok: HWND,
    cancel: HWND,
    browse: Option<HWND>,
    recent_list: Option<HWND>,
    recent: Vec<PathBuf>,
    validator: Option<Validator>,
    purpose: String,
    answer: Option<String>,
    done: bool,
}

thread_local! {
    static PROMPT: RefCell<Option<Prompt>> = const { RefCell::new(None) };
}

/// Show the prompt modally over `owner`; the text entered, or `None` for Cancel.
pub fn ask(owner: HWND, a: &Ask) -> Option<String> {
    // SAFETY: plain window creation and a modal loop over the owner, which is disabled meanwhile.
    unsafe {
        let inst = GetModuleHandleW(None).ok()?;
        let class = w!("LegconPrompt");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(prompt_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hbrBackground: HBRUSH(GetSysColorBrush(COLOR_BTNFACE).0),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let t = ui::wide(a.title);
        let window = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            PCWSTR(t.as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
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
        let label = ui::label(window, a.prompt);
        let style =
            ES_AUTOHSCROLL as u32 | WS_TABSTOP.0 | if a.secret { ES_PASSWORD as u32 } else { 0 };
        let edit = ui::child(
            window,
            w!("EDIT"),
            a.initial,
            WINDOW_STYLE(style),
            WS_EX_CLIENTEDGE,
            "prompt.text",
        );
        let browse = a
            .recent
            .map(|_| ui::button(window, "Browse...", "prompt.browse"));
        let recent_list = a.recent.map(|places| {
            let lb = ui::child(
                window,
                w!("LISTBOX"),
                "",
                WINDOW_STYLE(LBS_NOTIFY as u32 | WS_VSCROLL.0 | WS_TABSTOP.0),
                WS_EX_CLIENTEDGE,
                "prompt.recent",
            );
            for p in places {
                let t = ui::wide(&p.display().to_string());
                SendMessageW(lb, LB_ADDSTRING, None, Some(LPARAM(t.as_ptr() as isize)));
            }
            lb
        });
        let hint = ui::label(window, a.hint);
        let error = ui::label(window, "");
        let okb = ui::button(window, a.ok, "prompt.ok");
        let cancel = ui::button(window, "Cancel", "prompt.cancel");

        let width = s(460);
        let m = s(12);
        let row = s(24);
        let mut y = m;
        ui::place(label, (m, y, width - 2 * m, row));
        y += row + s(4);
        match browse {
            Some(b) => {
                ui::place(edit, (m, y, width - 3 * m - s(90), row));
                ui::place(b, (width - m - s(90), y, s(90), row));
            }
            None => ui::place(edit, (m, y, width - 2 * m, row)),
        }
        y += row + s(6);
        if let Some(lb) = recent_list {
            ui::place(lb, (m, y, width - 2 * m, s(96)));
            y += s(96) + s(6);
        }
        if !a.hint.is_empty() {
            ui::place(hint, (m, y, width - 2 * m, row));
            y += row;
        }
        ui::place(error, (m, y, width - 2 * m, row));
        y += row + s(6);
        ui::place(cancel, (width - m - s(85), y, s(85), s(28)));
        ui::place(okb, (width - 2 * m - s(170), y, s(85), s(28)));
        y += s(28) + m;

        let mut r = Default::default();
        let _ = GetWindowRect(owner, &mut r);
        let (cw, ch) = (width + s(16), y + s(40));
        let x = r.left + ((r.right - r.left) - cw) / 2;
        let top = r.top + ((r.bottom - r.top) - ch) / 3;
        let _ = SetWindowPos(window, None, x, top, cw, ch, SWP_NOZORDER);

        PROMPT.with(|p| {
            *p.borrow_mut() = Some(Prompt {
                window,
                edit,
                error,
                ok: okb,
                cancel,
                browse,
                recent_list,
                recent: a.recent.map(<[PathBuf]>::to_vec).unwrap_or_default(),
                validator: a.validator,
                purpose: a.purpose.to_string(),
                answer: None,
                done: false,
            });
        });
        check();
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(window, SW_SHOWNORMAL);
        let _ = SetFocus(Some(edit));
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        let mut msg = MSG::default();
        while PROMPT.with(|p| p.borrow().as_ref().is_some_and(|p| !p.done)) {
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
        let answer = PROMPT
            .with(|p| p.borrow_mut().take())
            .and_then(|p| p.answer);
        if IsWindow(Some(window)).as_bool() {
            let _ = DestroyWindow(window);
        }
        let _ = SetFocus(Some(owner));
        answer
    }
}

/// Run the controller's validator on the field: the error shows and OK follows it.
fn check() {
    PROMPT.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else { return };
        let verdict = p.validator.map_or(Ok(()), |v| v(&ui::text(p.edit)));
        ui::set_text(
            p.error,
            verdict.as_ref().err().map(String::as_str).unwrap_or(""),
        );
        ui::set_enabled(p.ok, verdict.is_ok());
    });
}

fn finish(ok: bool) {
    PROMPT.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            if ok {
                let text = ui::text(p.edit);
                if p.validator.is_some_and(|v| v(&text).is_err()) {
                    return;
                }
                p.answer = Some(text);
            }
            p.done = true;
        }
    });
}

fn browse() {
    let (window, edit, purpose) = match PROMPT.with(|p| {
        p.borrow()
            .as_ref()
            .map(|p| (p.window, p.edit, p.purpose.clone()))
    }) {
        Some(x) => x,
        None => return,
    };
    let start = PathBuf::from(ui::text(edit));
    let start = start.is_dir().then_some(start);
    if let Some(path) = pick_folder(window, &purpose, "Choose a folder", start.as_ref()) {
        ui::set_text(edit, &path.display().to_string());
        check();
    }
}

fn pick_recent(and_finish: bool) {
    PROMPT.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else { return };
        let Some(lb) = p.recent_list else { return };
        // SAFETY: a plain list box query.
        let i = unsafe { SendMessageW(lb, LB_GETCURSEL, None, None).0 };
        if let Some(path) = usize::try_from(i).ok().and_then(|i| p.recent.get(i)) {
            ui::set_text(p.edit, &path.display().to_string());
        }
    });
    check();
    if and_finish {
        finish(true);
    }
}

unsafe extern "system" fn prompt_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let code = ((wp.0 >> 16) & 0xffff) as u32;
            let from = HWND(lp.0 as _);
            let id = (wp.0 & 0xffff) as i32;
            let (ok, cancel, edit, b, lb) = PROMPT.with(|p| {
                p.borrow().as_ref().map_or(Default::default(), |p| {
                    (
                        p.ok,
                        p.cancel,
                        p.edit,
                        p.browse.unwrap_or_default(),
                        p.recent_list.unwrap_or_default(),
                    )
                })
            });
            if code == EN_CHANGE && from == edit {
                check();
            } else if !lb.is_invalid() && from == lb && code == LBN_SELCHANGE {
                pick_recent(false);
            } else if !lb.is_invalid() && from == lb && code == LBN_DBLCLK {
                pick_recent(true);
            } else if !b.is_invalid() && from == b && code == BN_CLICKED {
                browse();
            } else if (code == BN_CLICKED && from == ok) || (id == IDOK.0 && from.is_invalid()) {
                finish(true);
            } else if (code == BN_CLICKED && from == cancel) || id == IDCANCEL.0 {
                finish(false);
            }
            LRESULT(0)
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

/// A popup menu under `anchor` (or at the mouse); the chosen item's index, `None` when dismissed.
pub fn menu(
    owner: HWND,
    anchor: Option<HWND>,
    items: &[legcon::controller::MenuItem],
) -> Option<usize> {
    use legcon::controller::MenuItem;
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, TrackPopupMenu, MF_GRAYED,
        MF_SEPARATOR, MF_STRING, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN,
    };
    // SAFETY: a menu we create, show modally and destroy here; the label buffers outlive each call.
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, item) in items.iter().enumerate() {
            let (flags, label) = match item {
                MenuItem::Item { label, enabled } => (
                    if *enabled {
                        MF_STRING
                    } else {
                        MF_STRING | MF_GRAYED
                    },
                    label.as_str(),
                ),
                MenuItem::Heading(label) => (MF_STRING | MF_GRAYED, label.as_str()),
                MenuItem::Separator => (MF_SEPARATOR, ""),
            };
            // Menus treat '&' as a mnemonic marker; paths may contain one.
            let text = ui::wide(&label.replace('&', "&&"));
            let _ = AppendMenuW(menu, flags, i + 1, PCWSTR(text.as_ptr()));
        }
        let mut at = POINT::default();
        let mut r = RECT::default();
        match anchor {
            Some(a) if GetWindowRect(a, &mut r).is_ok() => {
                at = POINT {
                    x: r.left,
                    y: r.bottom,
                }
            }
            _ => {
                let _ = GetCursorPos(&mut at);
            }
        }
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
            at.x,
            at.y,
            None,
            owner,
            None,
        )
        .0 as usize;
        let _ = DestroyMenu(menu);
        cmd.checked_sub(1)
    }
}
