//! GDI drawing helpers for the painted parts (tabs, header, pane frames, the terminal).
//!
//! Colours are 0xRRGGBB, as the controllers give them; GDI wants 0x00BBGGRR.

use std::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, FillRect,
    GetTextExtentPoint32W, RoundRect, SelectObject, SetBkMode, SetTextColor, DRAW_TEXT_FORMAT,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ,
    PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::WindowsAndMessaging::{NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS};

use crate::ui;

/// The mockup's palette.
pub const FACE: u32 = 0xf0f0f0;
pub const WHITE: u32 = 0xffffff;
pub const LINE: u32 = 0xd9d9d9;
pub const LINE2: u32 = 0xc4c4c4;
pub const TEXT: u32 = 0x1b1b1b;
pub const MUTED: u32 = 0x6b6b6b;
pub const BAND: u32 = 0xfafafa;
pub const TAB_OFF: u32 = 0xd3d9df;
pub const OK: u32 = 0x2e8b3a;
pub const BUSY: u32 = 0x0067c0;
pub const WARN: u32 = 0xc98a00;
pub const OFF: u32 = 0x9aa0a6;
pub const BAD: u32 = 0xc42b1c;
pub const BADGE_BG: u32 = 0xfff4ce;
pub const BADGE_FG: u32 = 0x7a5a00;
pub const BADGE_LINE: u32 = 0xf0d78a;

pub fn colorref(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff))
}

/// `rgb` mixed with white: `amount` 0.0 is white, 1.0 the colour itself.
pub fn tint(rgb: u32, amount: f32) -> u32 {
    let mix = |shift: u32| {
        let c = ((rgb >> shift) & 0xff) as f32;
        ((255.0 - (255.0 - c) * amount).round() as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}

pub fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub fn fill(dc: HDC, r: &RECT, rgb: u32) {
    // SAFETY: a brush made, used and freed here, on a DC the caller owns.
    unsafe {
        let b = CreateSolidBrush(colorref(rgb));
        FillRect(dc, r, b);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

/// A rectangle outline of one pixel.
pub fn frame(dc: HDC, r: &RECT, rgb: u32) {
    fill(
        dc,
        &RECT {
            bottom: r.top + 1,
            ..*r
        },
        rgb,
    );
    fill(
        dc,
        &RECT {
            top: r.bottom - 1,
            ..*r
        },
        rgb,
    );
    fill(
        dc,
        &RECT {
            right: r.left + 1,
            ..*r
        },
        rgb,
    );
    fill(
        dc,
        &RECT {
            left: r.right - 1,
            ..*r
        },
        rgb,
    );
}

/// A rounded rectangle filled with `bg` and outlined with `line`.
pub fn pill(dc: HDC, r: &RECT, bg: u32, line: u32, radius: i32) {
    // SAFETY: GDI objects made, selected, restored and freed here.
    unsafe {
        let b = CreateSolidBrush(colorref(bg));
        let p = CreatePen(PS_SOLID, 1, colorref(line));
        let ob = SelectObject(dc, HGDIOBJ(b.0));
        let op = SelectObject(dc, HGDIOBJ(p.0));
        let _ = RoundRect(dc, r.left, r.top, r.right, r.bottom, radius, radius);
        SelectObject(dc, ob);
        SelectObject(dc, op);
        let _ = DeleteObject(HGDIOBJ(b.0));
        let _ = DeleteObject(HGDIOBJ(p.0));
    }
}

/// A filled circle of diameter `d` at (x, y).
pub fn dot(dc: HDC, x: i32, y: i32, d: i32, rgb: u32) {
    // SAFETY: GDI objects made, selected, restored and freed here.
    unsafe {
        let b = CreateSolidBrush(colorref(rgb));
        let p = CreatePen(PS_SOLID, 1, colorref(rgb));
        let ob = SelectObject(dc, HGDIOBJ(b.0));
        let op = SelectObject(dc, HGDIOBJ(p.0));
        let _ = Ellipse(dc, x, y, x + d, y + d);
        SelectObject(dc, ob);
        SelectObject(dc, op);
        let _ = DeleteObject(HGDIOBJ(b.0));
        let _ = DeleteObject(HGDIOBJ(p.0));
    }
}

/// One line of text in `r`, vertically centred, cut with an ellipsis when it does not fit.
pub fn text(dc: HDC, s: &str, r: &RECT, rgb: u32, font: HFONT) {
    let mut w: Vec<u16> = ui::plain(s).encode_utf16().collect();
    let mut r = *r;
    // SAFETY: drawing on a DC the caller owns; the font is restored.
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, colorref(rgb));
        let flags: DRAW_TEXT_FORMAT =
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
        DrawTextW(dc, &mut w, &mut r, flags);
        SelectObject(dc, old);
    }
}

/// How wide `s` is in `font`.
pub fn width(dc: HDC, s: &str, font: HFONT) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE::default();
    // SAFETY: measuring on a DC the caller owns; the font is restored.
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let _ = GetTextExtentPoint32W(dc, &w, &mut size);
        SelectObject(dc, old);
    }
    size.cx
}

thread_local! {
    static BOLD: RefCell<Vec<(u32, isize)>> = const { RefCell::new(Vec::new()) };
}

/// The message font in bold, a little larger: the machine's name in the header.
pub fn title_font(dpi: u32) -> HFONT {
    BOLD.with(|f| {
        if let Some((_, h)) = f.borrow().iter().find(|(d, _)| *d == dpi) {
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
            m.lfMessageFont.lfWeight = 700;
            m.lfMessageFont.lfHeight = m.lfMessageFont.lfHeight * 14 / 12;
            CreateFontIndirectW(&m.lfMessageFont)
        };
        f.borrow_mut().push((dpi, font.0 as isize));
        font
    })
}
