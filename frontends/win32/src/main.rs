//! The Win32 shell: a pure view over `legcon::controller` (docs/PLAN.md D13).
//!
//! Layers and rules: CONTRIBUTING.md § Architecture policy. Nothing here names the model or a
//! protocol crate; `scripts/check-boundaries.sh` enforces that.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod controls;
#[cfg(windows)]
mod effects;
#[cfg(windows)]
mod form;
#[cfg(windows)]
mod header;
#[cfg(windows)]
mod icons;
#[cfg(windows)]
mod ole_drop;
#[cfg(windows)]
mod paint;
#[cfg(windows)]
mod term_view;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod wake;
#[cfg(windows)]
mod window;

#[cfg(windows)]
fn main() {
    use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
    use windows::Win32::UI::Controls::{
        InitCommonControlsEx, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES,
        ICC_TAB_CLASSES, ICC_TREEVIEW_CLASSES, INITCOMMONCONTROLSEX,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG, WM_KEYDOWN,
    };

    // SAFETY: COM and the common controls are set up once on the UI thread before any window.
    unsafe {
        // OLE, not just COM: Explorer's drag and drop needs it.
        let _ = OleInitialize(None);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES
                | ICC_BAR_CLASSES
                | ICC_LISTVIEW_CLASSES
                | ICC_TREEVIEW_CLASSES
                | ICC_TAB_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }
    window::start();
    let mut msg = MSG::default();
    // SAFETY: the standard message loop; the key map sees key presses before the controls do.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // The key's own target, not GetFocus(): the two differ for keys posted to a control.
            if msg.message == WM_KEYDOWN && window::key(msg.wParam.0 as u16, msg.hwnd) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        ui::free_fonts();
        OleUninitialize();
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("legcon's Win32 shell runs on Windows only; use the lc CLI.");
}
