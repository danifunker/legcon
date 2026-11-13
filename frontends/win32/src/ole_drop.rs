//! The window as an OLE drop target, so files dragged from Explorer land on a pane.
//!
//! OLE finds the target by walking up from the window under the cursor, so registering the main
//! window covers both panes. The drop only collects the paths and posts them; the copy (and any
//! question it asks) runs after Explorer's drag loop has ended.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{implement, Ref, Result};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, POINTL, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use legcon::controller::connection::Side;

/// Posted to the main window when files were dropped; [`take`] hands them over.
pub const WM_APP_DROPPED: u32 = WM_APP + 5;

thread_local! {
    static DROPPED: RefCell<Option<(Side, Vec<PathBuf>)>> = const { RefCell::new(None) };
}

/// The last drop, once.
pub fn take() -> Option<(Side, Vec<PathBuf>)> {
    DROPPED.with(|d| d.borrow_mut().take())
}

fn format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn paths(data: &IDataObject) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // SAFETY: the data object's HDROP is read while locked, then the medium is released.
    unsafe {
        let Ok(mut medium) = data.GetData(&format()) else {
            return out;
        };
        let mem = medium.u.hGlobal;
        let p = GlobalLock(mem);
        if !p.is_null() {
            let hdrop = HDROP(p);
            let n = DragQueryFileW(hdrop, u32::MAX, None);
            for i in 0..n {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(hdrop, i, Some(&mut buf));
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
            }
            let _ = GlobalUnlock(mem);
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

#[implement(IDropTarget)]
struct Target {
    window: HWND,
    /// Whether what is being dragged carries files.
    files: RefCell<bool>,
}

impl Target {
    fn side_at(&self, pt: &POINTL) -> Option<Side> {
        let mut p = POINT { x: pt.x, y: pt.y };
        // SAFETY: converting a screen point into our own window's client area.
        let _ = unsafe { ScreenToClient(self.window, &mut p) };
        crate::window::pane_at(p.x, p.y)
    }

    fn effect(&self, pt: &POINTL) -> DROPEFFECT {
        if *self.files.borrow() && self.side_at(pt).is_some() {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        }
    }
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        // SAFETY: asking the source whether it offers an HDROP.
        let has = data
            .as_ref()
            .is_some_and(|d| unsafe { d.QueryGetData(&format()) }.is_ok());
        *self.files.borrow_mut() = has;
        // SAFETY: OLE passes a valid out pointer.
        unsafe { *effect = self.effect(pt) };
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        // SAFETY: OLE passes a valid out pointer.
        unsafe { *effect = self.effect(pt) };
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        *self.files.borrow_mut() = false;
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let side = self.side_at(pt);
        let list = data.as_ref().map(paths).unwrap_or_default();
        // SAFETY: OLE passes a valid out pointer.
        unsafe {
            *effect = if side.is_some() && !list.is_empty() {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            }
        };
        if let (Some(side), false) = (side, list.is_empty()) {
            DROPPED.with(|d| *d.borrow_mut() = Some((side, list)));
            // SAFETY: posting to our own window; handled after the drag loop.
            let _ =
                unsafe { PostMessageW(Some(self.window), WM_APP_DROPPED, WPARAM(0), LPARAM(0)) };
        }
        Ok(())
    }
}

/// Make `window` take files dragged from Explorer.
pub fn register(window: HWND) {
    let target: IDropTarget = Target {
        window,
        files: RefCell::new(false),
    }
    .into();
    // SAFETY: OLE is initialised on this thread; the registration keeps the target alive.
    if let Err(e) = unsafe { RegisterDragDrop(window, &target) } {
        log::warn!("Dropping files onto the window will not work: {e}");
    }
}
