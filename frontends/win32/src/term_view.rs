//! The Shell tab's terminal: a painted grid of the controller's screen, keys back as actions.
//!
//! While no session runs it shows the controller's message and a Connect button instead.

use legcon::controller::connection::{self as conn, TermKey, TermScreen, DEFAULT_BG};
use legcon::controller::ids;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EndPaint, ExtTextOutW, GetTextMetricsW, InvalidateRect, SelectObject, SetBkColor, SetTextColor,
    ETO_OPAQUE, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
    VK_F1, VK_F12, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, PostMessageW, RegisterClassW, BN_CLICKED, DLGC_WANTALLKEYS,
    DLGC_WANTCHARS, HMENU, WINDOW_EX_STYLE, WM_APP, WM_CHAR, WM_COMMAND, WM_ERASEBKGND,
    WM_GETDLGCODE, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_PAINT, WM_SETFOCUS, WM_SIZE,
    WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP, WS_VISIBLE,
};

use crate::paint;
use crate::ui;
use crate::window;

/// Posted to itself after a resize, so the new size reaches the controller outside layout.
const WM_APP_RESIZED: u32 = WM_APP + 10;

pub struct TermView {
    pub hwnd: HWND,
    pub connect: HWND,
}

pub fn create(parent: HWND) -> TermView {
    // SAFETY: registering our class and creating its window on the UI thread.
    let hwnd = unsafe {
        let inst = GetModuleHandleW(None).expect("module handle");
        let class = w!("LegconTerm");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(term_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_CLIPCHILDREN,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(std::ptr::null_mut())),
            Some(inst.into()),
            None,
        )
        .expect("the terminal view")
    };
    ui::set_automation_id(hwnd, ids::TERM_BODY);
    let connect = ui::button(hwnd, "Connect", "term.connect");
    TermView { hwnd, connect }
}

/// One character cell's size in the terminal font.
fn cell(dc: HDC, hwnd: HWND) -> (i32, i32) {
    let font = ui::mono_font_for(hwnd);
    let mut tm = TEXTMETRICW::default();
    // SAFETY: measuring on a DC we own; the font is restored.
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let _ = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
    }
    (paint::width(dc, "M", font).max(1), tm.tmHeight.max(1))
}

/// The grid size that fits the view's client area.
pub fn grid(hwnd: HWND) -> (u16, u16) {
    let r = ui::client(hwnd);
    // SAFETY: a screen-compatible DC borrowed for one measurement.
    let (cw, ch) = unsafe {
        let dc = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
        let c = cell(dc, hwnd);
        windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), dc);
        c
    };
    let pad = ui::scale(6, ui::dpi(hwnd));
    (
        ((r.right - 2 * pad) / cw).max(10) as u16,
        ((r.bottom - 2 * pad) / ch).max(2) as u16,
    )
}

fn draw(dc: HDC, hwnd: HWND, screen: &TermScreen, focused: bool) {
    let r = ui::client(hwnd);
    paint::fill(dc, &r, DEFAULT_BG);
    let (cw, ch) = cell(dc, hwnd);
    let pad = ui::scale(6, ui::dpi(hwnd));
    let font = ui::mono_font_for(hwnd);
    // SAFETY: text drawn on our memory DC; the font is restored.
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        for (row, runs) in screen.lines.iter().enumerate() {
            let y = pad + row as i32 * ch;
            let mut col = 0i32;
            for run in runs {
                let w: Vec<u16> = run.text.encode_utf16().collect();
                let n = run.text.chars().count() as i32;
                let x = pad + col * cw;
                SetTextColor(dc, paint::colorref(run.fg));
                SetBkColor(dc, paint::colorref(run.bg));
                let cellr = RECT {
                    left: x,
                    top: y,
                    right: x + n * cw,
                    bottom: y + ch,
                };
                let dx: Vec<i32> = vec![cw; w.len()];
                let _ = ExtTextOutW(
                    dc,
                    x,
                    y,
                    ETO_OPAQUE,
                    Some(&cellr),
                    windows::core::PCWSTR(w.as_ptr()),
                    w.len() as u32,
                    Some(dx.as_ptr()),
                );
                col += n;
            }
        }
        SelectObject(dc, old);
    }
    if let Some((row, col)) = screen.cursor {
        let x = pad + i32::from(col) * cw;
        let y = pad + i32::from(row) * ch;
        let cur = paint::rect(x, y, cw, ch);
        if focused {
            paint::fill(dc, &paint::rect(x, y + ch - ch / 4, cw, ch / 4), 0xcccccc);
        } else {
            paint::frame(dc, &cur, 0x808080);
        }
    }
}

fn paint_now(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: the standard paint cycle, drawn through a memory DC to avoid flicker.
    unsafe {
        let dc = BeginPaint(hwnd, &mut ps);
        let r = ui::client(hwnd);
        let mem = CreateCompatibleDC(Some(dc));
        let bmp = CreateCompatibleBitmap(dc, r.right.max(1), r.bottom.max(1));
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        match window::shell_screen() {
            Some(screen) => draw(mem, hwnd, &screen, GetFocus() == hwnd),
            None => {
                paint::fill(mem, &r, paint::BAND);
                let msg = window::shell_message();
                let dpi = ui::dpi(hwnd);
                let tr = RECT {
                    left: ui::scale(16, dpi),
                    top: r.bottom / 2 - ui::scale(40, dpi),
                    right: r.right - ui::scale(16, dpi),
                    bottom: r.bottom / 2 - ui::scale(14, dpi),
                };
                let w = paint::width(mem, &msg, ui::font_for(hwnd));
                let tr = RECT {
                    left: ((r.right - w) / 2).max(tr.left),
                    ..tr
                };
                paint::text(mem, &msg, &tr, paint::MUTED, ui::font_for(hwnd));
            }
        }
        let _ = BitBlt(dc, 0, 0, r.right, r.bottom, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Place the Connect button in the middle; called after a resize or a state change.
pub fn place_connect(v: &TermView, shown: bool, label: &str) {
    ui::set_visible(v.connect, shown);
    if shown {
        ui::set_text(v.connect, label);
        let r = ui::client(v.hwnd);
        let dpi = ui::dpi(v.hwnd);
        let (w, h) = (ui::scale(180, dpi), ui::scale(28, dpi));
        ui::place(v.connect, ((r.right - w) / 2, r.bottom / 2, w, h));
    }
}

pub fn invalidate(hwnd: HWND) {
    // SAFETY: a plain repaint request.
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}

fn special(vk: u16) -> Option<TermKey> {
    let k = match vk {
        v if v == VK_RETURN.0 => TermKey::Enter,
        v if v == VK_BACK.0 => TermKey::Backspace,
        v if v == VK_TAB.0 => TermKey::Tab,
        v if v == VK_ESCAPE.0 => TermKey::Escape,
        v if v == VK_UP.0 => TermKey::Up,
        v if v == VK_DOWN.0 => TermKey::Down,
        v if v == VK_LEFT.0 => TermKey::Left,
        v if v == VK_RIGHT.0 => TermKey::Right,
        v if v == VK_HOME.0 => TermKey::Home,
        v if v == VK_END.0 => TermKey::End,
        v if v == VK_PRIOR.0 => TermKey::PageUp,
        v if v == VK_NEXT.0 => TermKey::PageDown,
        v if v == VK_INSERT.0 => TermKey::Insert,
        v if v == VK_DELETE.0 => TermKey::Delete,
        v if (VK_F1.0..=VK_F12.0).contains(&v) => TermKey::F((v - VK_F1.0 + 1) as u8),
        _ => return None,
    };
    Some(k)
}

unsafe extern "system" fn term_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_now(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            // SAFETY: posting to ourselves; handled outside whatever layout moved us.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_RESIZED, WPARAM(0), LPARAM(0)) };
            LRESULT(0)
        }
        WM_APP_RESIZED => {
            let (cols, rows) = grid(hwnd);
            window::conn_action(conn::Action::ShellResize(cols, rows));
            window::place_term_connect();
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_GETDLGCODE => LRESULT((DLGC_WANTALLKEYS | DLGC_WANTCHARS) as isize),
        WM_LBUTTONDOWN => {
            // SAFETY: taking focus for typing.
            let _ = unsafe { SetFocus(Some(hwnd)) };
            LRESULT(0)
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // SAFETY: plain keyboard-state reads.
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
            let vk = wp.0 as u16;
            if let Some(k) = special(vk) {
                window::conn_action(conn::Action::ShellKey(k));
            } else if ctrl && !shift && (0x41..=0x5a).contains(&vk) {
                window::conn_action(conn::Action::ShellKey(TermKey::Ctrl(vk as u8 as char)));
            }
            LRESULT(0)
        }
        WM_CHAR => {
            let c = wp.0 as u32;
            // Control characters came through WM_KEYDOWN as keys; only printable text is typed here.
            if c >= 0x20 && c != 0x7f {
                if let Some(ch) = char::from_u32(c) {
                    window::conn_action(conn::Action::ShellText(ch.to_string()));
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if ((wp.0 >> 16) & 0xffff) as u32 == BN_CLICKED {
                window::conn_action(conn::Action::ShellConnect);
            }
            LRESULT(0)
        }
        // SAFETY: default handling for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
