//! The machine header (mockup: connection-view.html): a colour strip, the state light and name,
//! the address, a plaintext badge, and a chip per service with its own Connect / Disconnect,
//! then Swap and Close tab. Painted here; its buttons are real controls laid over the chips.

use legcon::controller::connection::HeaderInfo;
use legcon::controller::{ids, ConnState};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetDC, ReleaseDC, HDC, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, BN_CLICKED, HMENU, WINDOW_EX_STYLE,
    WM_COMMAND, WM_ERASEBKGND, WM_PAINT, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_VISIBLE,
};

use crate::paint;
use crate::ui;
use crate::window;

pub struct Header {
    pub hwnd: HWND,
    pub files: HWND,
    pub commands: HWND,
    pub swap: HWND,
    pub close: HWND,
}

pub fn create(parent: HWND) -> Header {
    // SAFETY: registering our class and creating its window on the UI thread.
    let hwnd = unsafe {
        let inst = GetModuleHandleW(None).expect("module handle");
        let class = w!("LegconHeader");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(header_proc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(std::ptr::null_mut())),
            Some(inst.into()),
            None,
        )
        .expect("the machine header")
    };
    ui::set_automation_id(hwnd, ids::CONN_HEADER);
    Header {
        hwnd,
        files: ui::button(hwnd, "Connect", ids::CONN_FILES),
        commands: ui::button(hwnd, "Connect", ids::CONN_COMMANDS),
        swap: ui::button(hwnd, "Swap", ids::CONN_SWAP),
        close: ui::button(hwnd, "Close tab", ids::CONN_CLOSE),
    }
}

pub fn state_rgb(s: ConnState) -> u32 {
    match s {
        ConnState::Connected => paint::OK,
        ConnState::Busy => paint::BUSY,
        ConnState::Connecting | ConnState::Disconnecting => paint::WARN,
        ConnState::Disconnected => paint::OFF,
        ConnState::Lost => paint::BAD,
    }
}

fn button_label(s: ConnState) -> &'static str {
    match s {
        ConnState::Connected | ConnState::Busy => "Disconnect",
        ConnState::Connecting => "Connecting...",
        ConnState::Disconnecting => "Logging off...",
        ConnState::Disconnected | ConnState::Lost => "Connect",
    }
}

/// Where each painted part goes, from the text widths; shared by painting and layout.
struct Geometry {
    name: RECT,
    address: RECT,
    badge: Option<RECT>,
    chips: [RECT; 2],
    buttons: [RECT; 4],
}

fn geometry(dc: HDC, hwnd: HWND, h: &HeaderInfo) -> Geometry {
    let r = ui::client(hwnd);
    let dpi = ui::dpi(hwnd);
    let s = |v| ui::scale(v, dpi);
    let font = ui::font_for(hwnd);
    let (top, height) = (s(4), r.bottom - s(8));
    // From the right: Close tab, Swap, then the two service chips.
    let mut x = r.right - s(8);
    let mut buttons = [RECT::default(); 4];
    for (i, w) in [(3, 80), (2, 70)] {
        x -= s(w);
        buttons[i] = paint::rect(x, top, s(w), height);
        x -= s(6);
    }
    let mut chips = [RECT::default(); 2];
    for (i, label) in [(1usize, &h.commands), (0usize, &h.files)] {
        let bw = s(92);
        let w = s(10) + s(8) + s(6) + paint::width(dc, label, font) + s(8) + bw + s(4);
        x -= w + s(6);
        chips[i] = paint::rect(x, top, w, height);
        buttons[i] = paint::rect(x + w - bw - s(4), top + s(3), bw, height - s(6));
    }
    let mut lx = s(14);
    let name_w = paint::width(dc, &h.name, paint::title_font(dpi)) + s(4);
    let name = paint::rect(lx + s(16), 0, name_w, r.bottom);
    lx = name.right + s(10);
    let addr_w = paint::width(dc, &h.address, font) + s(4);
    let address = paint::rect(lx, 0, addr_w.min((x - lx - s(100)).max(0)), r.bottom);
    let badge = h.plaintext.then(|| {
        let bw = paint::width(dc, "plaintext", font) + s(14);
        paint::rect(address.right + s(10), r.bottom / 2 - s(10), bw, s(20))
    });
    Geometry {
        name,
        address,
        badge,
        chips,
        buttons,
    }
}

/// Lay out and label the buttons for `h`; the window repaints its painted parts.
pub fn update(hd: &Header, h: &HeaderInfo) {
    // SAFETY: a DC borrowed for measuring and released.
    let g = unsafe {
        let dc = GetDC(Some(hd.hwnd));
        let g = geometry(dc, hd.hwnd, h);
        ReleaseDC(Some(hd.hwnd), dc);
        g
    };
    ui::set_text(hd.files, button_label(h.files_state));
    ui::set_text(hd.commands, button_label(h.commands_state));
    ui::set_enabled(
        hd.files,
        !h.files.starts_with("No ")
            && matches!(
                h.files_state,
                ConnState::Connected | ConnState::Disconnected | ConnState::Lost
            ),
    );
    ui::set_enabled(
        hd.commands,
        !h.commands.starts_with("No ")
            && matches!(
                h.commands_state,
                ConnState::Connected | ConnState::Disconnected | ConnState::Lost
            ),
    );
    for (b, r) in [hd.files, hd.commands, hd.swap, hd.close]
        .into_iter()
        .zip(g.buttons)
    {
        ui::place(b, (r.left, r.top, r.right - r.left, r.bottom - r.top));
    }
    crate::term_view::invalidate(hd.hwnd);
}

fn paint_now(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: the standard paint cycle on our own window.
    unsafe {
        let dc = BeginPaint(hwnd, &mut ps);
        let r = ui::client(hwnd);
        paint::fill(dc, &r, paint::FACE);
        if let Some(h) = window::header_info() {
            let dpi = ui::dpi(hwnd);
            let s = |v| ui::scale(v, dpi);
            let font = ui::font_for(hwnd);
            let g = geometry(dc, hwnd, &h);
            paint::fill(
                dc,
                &paint::rect(0, s(4), s(5), r.bottom - s(8)),
                h.color.rgb(),
            );
            let d = s(10);
            let tab_state = if matches!(h.files_state, ConnState::Connected)
                || matches!(h.commands_state, ConnState::Connected)
            {
                ConnState::Connected
            } else {
                h.files_state
            };
            paint::dot(dc, s(14), (r.bottom - d) / 2, d, state_rgb(tab_state));
            paint::text(dc, &h.name, &g.name, paint::TEXT, paint::title_font(dpi));
            paint::text(dc, &h.address, &g.address, paint::MUTED, font);
            if let Some(b) = g.badge {
                paint::pill(dc, &b, paint::BADGE_BG, paint::BADGE_LINE, s(16));
                let t = RECT {
                    left: b.left + s(7),
                    ..b
                };
                paint::text(dc, "plaintext", &t, paint::BADGE_FG, font);
            }
            for (chip, (label, state)) in g
                .chips
                .iter()
                .zip([(&h.files, h.files_state), (&h.commands, h.commands_state)])
            {
                paint::pill(dc, chip, paint::WHITE, paint::LINE2, chip.bottom - chip.top);
                let cd = s(8);
                paint::dot(
                    dc,
                    chip.left + s(10),
                    (chip.top + chip.bottom - cd) / 2,
                    cd,
                    state_rgb(state),
                );
                let t = RECT {
                    left: chip.left + s(24),
                    right: chip.right - s(100),
                    ..*chip
                };
                paint::text(dc, label, &t, paint::TEXT, font);
            }
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn header_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_now(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_COMMAND => {
            if ((wp.0 >> 16) & 0xffff) as u32 == BN_CLICKED {
                window::header_button(HWND(lp.0 as _));
            }
            LRESULT(0)
        }
        // SAFETY: default handling for our own window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
