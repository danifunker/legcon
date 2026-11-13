//! Small helpers: wide strings, child controls with their AutomationId, the UI font (from rusty-backup).

use std::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT, HGDIOBJ};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{
    AutomationId_Property_GUID, CLSID_AccPropServices, IAccPropServices,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, SendMessageW,
    SetWindowPos, SetWindowTextW, ShowWindow, BS_PUSHBUTTON, CHILDID_SELF, GWL_STYLE, HMENU,
    NONCLIENTMETRICSW, OBJID_CLIENT, SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOCOPYBITS,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_SETFONT, WS_CHILD,
    WS_TABSTOP, WS_VISIBLE,
};

/// `s` as a NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Text from a remote machine can hold NUL, which would end a wide string early: drop it.
pub fn plain(s: &str) -> String {
    s.replace('\0', "")
}

/// Set the UIA AutomationId of `hwnd` to the controller id, so UI flows find it by id.
pub fn set_automation_id(hwnd: HWND, id: &str) {
    thread_local! {
        static SVC: RefCell<Option<IAccPropServices>> = const { RefCell::new(None) };
    }
    if id.is_empty() {
        return;
    }
    SVC.with(|svc| {
        let mut svc = svc.borrow_mut();
        if svc.is_none() {
            // SAFETY: COM is initialised on this (the UI) thread in main.
            *svc = unsafe { CoCreateInstance(&CLSID_AccPropServices, None, CLSCTX_INPROC_SERVER) }
                .ok();
        }
        if let Some(s) = svc.as_ref() {
            let text = wide(id);
            // SAFETY: `text` outlives the call; the window is ours.
            let _ = unsafe {
                s.SetHwndPropStr(
                    hwnd,
                    OBJID_CLIENT.0 as u32,
                    CHILDID_SELF,
                    AutomationId_Property_GUID,
                    PCWSTR(text.as_ptr()),
                )
            };
        }
    });
}

/// Create a child control of `class`; `id` is its controller id (empty for decoration).
pub fn child(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    ex: WINDOW_EX_STYLE,
    id: &str,
) -> HWND {
    let text = wide(&plain(text));
    // SAFETY: the class names are system classes; `text` outlives the call.
    let hwnd = unsafe {
        let inst = GetModuleHandleW(None).ok();
        CreateWindowExW(
            ex,
            class,
            PCWSTR(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(std::ptr::null_mut())),
            inst.map(|i| i.into()),
            None,
        )
    }
    .expect("a child control");
    set_automation_id(hwnd, id);
    set_font(hwnd, font_for(parent));
    hwnd
}

pub fn button(parent: HWND, text: &str, id: &str) -> HWND {
    child(
        parent,
        w!("BUTTON"),
        text,
        WINDOW_STYLE((BS_PUSHBUTTON as u32) | WS_TABSTOP.0),
        WINDOW_EX_STYLE(0),
        id,
    )
}

/// A one-line label, centred on its row; an overlong line loses its end.
pub fn label(parent: HWND, text: &str) -> HWND {
    // SS_CENTERIMAGE (0x200) centres one line vertically; SS_ENDELLIPSIS (0x4000) cuts what overflows.
    child(
        parent,
        w!("STATIC"),
        text,
        WINDOW_STYLE(0x4200),
        WINDOW_EX_STYLE(0),
        "",
    )
}

/// The window's text, or empty.
pub fn text(hwnd: HWND) -> String {
    // SAFETY: plain reads of a window we own.
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

/// Set the text only when it changed: a set resets the caret and fires `EN_CHANGE`.
pub fn set_text(hwnd: HWND, value: &str) {
    let value = plain(value);
    if text(hwnd) != value {
        let w = wide(&value);
        // SAFETY: `w` outlives the call.
        let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(w.as_ptr())) };
    }
}

/// The control's own visible flag: `IsWindowVisible` is false for every child of a hidden parent.
pub fn shown(hwnd: HWND) -> bool {
    // SAFETY: a plain read of the window's style.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    style & WS_VISIBLE.0 != 0
}

pub fn set_visible(hwnd: HWND, on: bool) {
    if shown(hwnd) != on {
        // SAFETY: plain window call.
        let _ = unsafe { ShowWindow(hwnd, if on { SW_SHOW } else { SW_HIDE }) };
    }
}

pub fn set_enabled(hwnd: HWND, on: bool) {
    // SAFETY: plain window calls.
    unsafe {
        if IsWindowEnabled(hwnd).as_bool() != on {
            let _ = EnableWindow(hwnd, on);
        }
    }
}

/// Place a control at `r` (x, y, w, h) in its parent's client area.
pub fn place(hwnd: HWND, r: (i32, i32, i32, i32)) {
    // SAFETY: moving our own control.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            r.0,
            r.1,
            r.2.max(0),
            r.3.max(0),
            // NOCOPYBITS: a moved control repaints rather than carrying its old pixels along.
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS,
        )
    };
}

/// A size in DIPs at `dpi`.
pub fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

pub fn dpi(hwnd: HWND) -> u32 {
    // SAFETY: a plain query of a window we own.
    unsafe { GetDpiForWindow(hwnd) }.max(96)
}

pub fn client(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: a plain query of a window we own.
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r) };
    r
}

thread_local! {
    /// The message font at each DPI it was asked for; fonts live as long as the process.
    static FONTS: RefCell<Vec<(u32, isize)>> = const { RefCell::new(Vec::new()) };
}

/// The system's message font at `hwnd`'s DPI (Segoe UI 9 pt at 100%).
pub fn font_for(hwnd: HWND) -> HFONT {
    cached_font(dpi(hwnd), false)
}

/// Consolas at the message font's size, for the terminal area.
pub fn mono_font_for(hwnd: HWND) -> HFONT {
    cached_font(dpi(hwnd), true)
}

fn cached_font(dpi: u32, mono: bool) -> HFONT {
    let key = dpi | (u32::from(mono) << 31);
    FONTS.with(|f| {
        if let Some((_, h)) = f.borrow().iter().find(|(d, _)| *d == key) {
            return HFONT(*h as _);
        }
        let mut m = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        // SAFETY: `m` is sized for the call.
        let font = unsafe {
            let _ = SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                m.cbSize,
                Some(&mut m as *mut _ as *mut _),
                0,
                dpi,
            );
            if mono {
                let face = wide("Consolas");
                m.lfMessageFont.lfFaceName = [0; 32];
                m.lfMessageFont.lfFaceName[..face.len()].copy_from_slice(&face);
            }
            CreateFontIndirectW(&m.lfMessageFont)
        };
        f.borrow_mut().push((key, font.0 as isize));
        font
    })
}

pub fn set_font(hwnd: HWND, font: HFONT) {
    // SAFETY: WM_SETFONT takes the font handle; the font outlives the control.
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

/// Free the fonts at exit.
pub fn free_fonts() {
    FONTS.with(|f| {
        for (_, h) in f.borrow_mut().drain(..) {
            // SAFETY: each font was created here and no control uses it once the window is gone.
            let _ = unsafe { DeleteObject(HGDIOBJ(h as _)) };
        }
    });
}
