//! Thin wrappers over the common controls the window uses: report list views, tree views, tabs.

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    SetWindowTheme, HIMAGELIST, HTREEITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_IMAGE, LVIF_TEXT,
    LVITEMW, LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETIMAGELIST, LVM_SETITEMTEXTW, LVNI_SELECTED, LVSIL_SMALL,
    LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_REPORT, LVS_SHAREIMAGELISTS, LVS_SHOWSELALWAYS,
    LVS_SINGLESEL, TCIF_TEXT, TCITEMW, TCM_DELETEALLITEMS, TCM_GETCURSEL, TCM_GETITEMCOUNT,
    TCM_GETITEMRECT, TCM_INSERTITEMW, TCM_SETCURSEL, TCM_SETITEMSIZE, TCS_FIXEDWIDTH,
    TCS_OWNERDRAWFIXED, TVE_EXPAND, TVGN_CARET, TVIF_IMAGE, TVIF_PARAM, TVIF_SELECTEDIMAGE,
    TVIF_TEXT, TVINSERTSTRUCTW, TVINSERTSTRUCTW_0, TVITEMW, TVI_LAST, TVI_ROOT, TVM_DELETEITEM,
    TVM_EXPAND, TVM_INSERTITEMW, TVM_SELECTITEM, TVM_SETIMAGELIST, TVSIL_NORMAL, TVS_HASBUTTONS,
    TVS_HASLINES, TVS_LINESATROOT, TVS_SHOWSELALWAYS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_TABSTOP,
};

use crate::ui;

fn send(h: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    // SAFETY: plain control messages; every pointer passed in `lp` outlives the call.
    unsafe { SendMessageW(h, msg, Some(WPARAM(wp)), Some(LPARAM(lp))).0 }
}

/// Explorer's look for a list or tree (hover, modern selection, chevrons in trees).
pub fn explorer_theme(h: HWND) {
    // SAFETY: a plain theme call on our own control.
    let _ = unsafe { SetWindowTheme(h, w!("Explorer"), PCWSTR::null()) };
}

/// A report list view with `columns` as (title, width in DIPs), its icons from a shared list.
pub fn list(parent: HWND, id: &str, columns: &[(&str, i32)], images: HIMAGELIST) -> HWND {
    let style = WINDOW_STYLE(
        LVS_REPORT | LVS_SHOWSELALWAYS | LVS_SINGLESEL | LVS_SHAREIMAGELISTS | WS_TABSTOP.0,
    );
    let h = ui::child(
        parent,
        w!("SysListView32"),
        "",
        style,
        WINDOW_EX_STYLE(0),
        id,
    );
    explorer_theme(h);
    send(h, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images.0);
    let ex = (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as usize;
    send(h, LVM_SETEXTENDEDLISTVIEWSTYLE, ex, ex as isize);
    let dpi = ui::dpi(parent);
    for (i, (title, width)) in columns.iter().enumerate() {
        let mut t = ui::wide(title);
        let col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH,
            cx: ui::scale(*width, dpi),
            pszText: PWSTR(t.as_mut_ptr()),
            ..Default::default()
        };
        send(h, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
    }
    h
}

/// Replace a list view's rows; each row is its icon and its cells in column order.
pub fn fill(list: HWND, rows: &[(i32, Vec<String>)]) {
    send(list, LVM_DELETEALLITEMS, 0, 0);
    for (n, (icon, row)) in rows.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            let mut t = ui::wide(&ui::plain(cell));
            let item = LVITEMW {
                mask: if col == 0 {
                    LVIF_TEXT | LVIF_IMAGE
                } else {
                    LVIF_TEXT
                },
                iItem: n as i32,
                iSubItem: col as i32,
                iImage: *icon,
                pszText: PWSTR(t.as_mut_ptr()),
                ..Default::default()
            };
            let msg = if col == 0 {
                LVM_INSERTITEMW
            } else {
                LVM_SETITEMTEXTW
            };
            send(list, msg, n, &item as *const _ as isize);
        }
    }
}

/// The selected row of a single-selection list view.
pub fn selected(list: HWND) -> Option<usize> {
    let i = send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    usize::try_from(i).ok()
}

/// A tree view with lines and +/- buttons.
pub fn tree(parent: HWND, id: &str) -> HWND {
    let style = WINDOW_STYLE(
        TVS_HASLINES | TVS_HASBUTTONS | TVS_LINESATROOT | TVS_SHOWSELALWAYS | WS_TABSTOP.0,
    );
    let h = ui::child(
        parent,
        w!("SysTreeView32"),
        "",
        style,
        WINDOW_EX_STYLE(0),
        id,
    );
    explorer_theme(h);
    send(
        h,
        TVM_SETIMAGELIST,
        TVSIL_NORMAL as usize,
        crate::icons::system_list().0,
    );
    h
}

/// Show `trail` as a chain of nested folders, all expanded, the last one selected.
///
/// Each item's lParam is its depth, so a click can say how far up to go.
pub fn fill_trail(tree: HWND, trail: &[String]) {
    send(tree, TVM_DELETEITEM, 0, TVI_ROOT.0);
    let mut parent = TVI_ROOT;
    let mut last = HTREEITEM::default();
    for (depth, name) in trail.iter().enumerate() {
        let mut t = ui::wide(&ui::plain(name));
        let ins = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: TVI_LAST,
            Anonymous: TVINSERTSTRUCTW_0 {
                item: TVITEMW {
                    mask: TVIF_TEXT | TVIF_PARAM | TVIF_IMAGE | TVIF_SELECTEDIMAGE,
                    iImage: crate::icons::file_icon("", true),
                    iSelectedImage: crate::icons::file_icon("", true),
                    pszText: PWSTR(t.as_mut_ptr()),
                    lParam: LPARAM(depth as isize),
                    ..Default::default()
                },
            },
        };
        let item = HTREEITEM(send(tree, TVM_INSERTITEMW, 0, &ins as *const _ as isize));
        if depth > 0 {
            send(tree, TVM_EXPAND, TVE_EXPAND.0 as usize, parent.0);
        }
        parent = item;
        last = item;
    }
    if last.0 != 0 {
        send(tree, TVM_SELECTITEM, TVGN_CARET as usize, last.0);
    }
}

/// A tab strip (its pages are drawn by the window beneath it); `owner_draw` lets the parent paint it.
pub fn tabs(parent: HWND, id: &str, owner_draw: bool) -> HWND {
    let style = if owner_draw {
        TCS_OWNERDRAWFIXED | TCS_FIXEDWIDTH
    } else {
        0
    };
    ui::child(
        parent,
        w!("SysTabControl32"),
        "",
        WINDOW_STYLE(WS_TABSTOP.0 | style),
        WINDOW_EX_STYLE(0),
        id,
    )
}

/// The tab under a point in the strip's client coordinates.
pub fn tab_at(tabs: HWND, x: i32, y: i32) -> Option<(usize, windows::Win32::Foundation::RECT)> {
    let n = send(tabs, TCM_GETITEMCOUNT, 0, 0).max(0) as usize;
    (0..n).find_map(|i| {
        let mut r = windows::Win32::Foundation::RECT::default();
        send(tabs, TCM_GETITEMRECT, i, &mut r as *mut _ as isize);
        (x >= r.left && x < r.right && y >= r.top && y < r.bottom).then_some((i, r))
    })
}

pub fn set_tabs(tabs: HWND, labels: &[String]) {
    send(tabs, TCM_DELETEALLITEMS, 0, 0);
    for (i, label) in labels.iter().enumerate() {
        let mut t = ui::wide(&ui::plain(label));
        let item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: PWSTR(t.as_mut_ptr()),
            ..Default::default()
        };
        send(tabs, TCM_INSERTITEMW, i, &item as *const _ as isize);
    }
}

pub fn tab_selected(tabs: HWND) -> Option<usize> {
    usize::try_from(send(tabs, TCM_GETCURSEL, 0, 0)).ok()
}

pub fn select_tab(tabs: HWND, i: usize) {
    if tab_selected(tabs) != Some(i) {
        send(tabs, TCM_SETCURSEL, i, 0);
    }
}

/// Every tab of a fixed-width strip is `w` by `h` pixels.
pub fn set_tab_size(tabs: HWND, w: i32, h: i32) {
    send(
        tabs,
        TCM_SETITEMSIZE,
        0,
        ((h as isize) << 16) | (w as isize & 0xffff),
    );
}
