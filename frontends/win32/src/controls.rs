//! Thin wrappers over the common controls the window uses: report list views, tree views, tabs.

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Controls::{
    SetWindowTheme, HDF_SORTDOWN, HDF_SORTUP, HDITEMW, HDI_FORMAT, HDM_GETITEMCOUNT, HDM_GETITEMW,
    HDM_SETITEMW, HIMAGELIST, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVHITTESTINFO, LVIF_IMAGE,
    LVIF_STATE, LVIF_TEXT, LVIS_DROPHILITED, LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS,
    LVM_GETHEADER, LVM_GETNEXTITEM, LVM_HITTEST, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETIMAGELIST, LVM_SETITEMSTATE, LVM_SETITEMTEXTW,
    LVNI_SELECTED, LVSIL_SMALL, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_REPORT,
    LVS_SHAREIMAGELISTS, LVS_SHOWSELALWAYS, LVS_SINGLESEL, TCIF_TEXT, TCITEMW, TCM_DELETEALLITEMS,
    TCM_GETCURSEL, TCM_GETITEMCOUNT, TCM_GETITEMRECT, TCM_INSERTITEMW, TCM_SETCURSEL,
    TCM_SETITEMSIZE, TCS_FIXEDWIDTH, TCS_OWNERDRAWFIXED,
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
pub fn list(
    parent: HWND,
    id: &str,
    columns: &[(&str, i32)],
    images: HIMAGELIST,
    single: bool,
) -> HWND {
    let one = if single { LVS_SINGLESEL } else { 0 };
    let style =
        WINDOW_STYLE(LVS_REPORT | LVS_SHOWSELALWAYS | one | LVS_SHAREIMAGELISTS | WS_TABSTOP.0);
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

/// Show the sort arrow on column `col` of a list's header, up or down; the others show none.
pub fn set_sort(list: HWND, col: usize, descending: bool) {
    let header = HWND(send(list, LVM_GETHEADER, 0, 0) as *mut core::ffi::c_void);
    let n = send(header, HDM_GETITEMCOUNT, 0, 0).max(0) as usize;
    for i in 0..n {
        let mut item = HDITEMW {
            mask: HDI_FORMAT,
            ..Default::default()
        };
        send(header, HDM_GETITEMW, i, &mut item as *mut _ as isize);
        let mut fmt = item.fmt.0 & !(HDF_SORTUP.0 | HDF_SORTDOWN.0);
        if i == col {
            fmt |= if descending {
                HDF_SORTDOWN.0
            } else {
                HDF_SORTUP.0
            };
        }
        if fmt != item.fmt.0 {
            item.fmt.0 = fmt;
            send(header, HDM_SETITEMW, i, &item as *const _ as isize);
        }
    }
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

/// Bring a list from `old` rows to `new`: cell by cell when the count is unchanged, so the
/// selection and scroll stay put while a row's progress moves; refilled otherwise.
pub fn update(list: HWND, old: &[(i32, Vec<String>)], new: &[(i32, Vec<String>)]) {
    if old.len() != new.len() {
        fill(list, new);
        return;
    }
    for (n, (o, r)) in old.iter().zip(new).enumerate() {
        for (col, cell) in r.1.iter().enumerate() {
            if o.1.get(col) == Some(cell) && (col > 0 || o.0 == r.0) {
                continue;
            }
            let mut t = ui::wide(&ui::plain(cell));
            let item = LVITEMW {
                mask: if col == 0 {
                    LVIF_TEXT | LVIF_IMAGE
                } else {
                    LVIF_TEXT
                },
                iItem: n as i32,
                iSubItem: col as i32,
                iImage: r.0,
                pszText: PWSTR(t.as_mut_ptr()),
                ..Default::default()
            };
            send(list, LVM_SETITEMTEXTW, n, &item as *const _ as isize);
            if col == 0 && o.0 != r.0 {
                send(
                    list,
                    windows::Win32::UI::Controls::LVM_SETITEMW,
                    0,
                    &item as *const _ as isize,
                );
            }
        }
    }
}

/// The row under screen point `pt` in `list`, if any.
pub fn row_at(list: HWND, pt: POINT) -> Option<usize> {
    let mut p = pt;
    // SAFETY: converting a screen point into our own list's client area.
    let _ = unsafe { ScreenToClient(list, &mut p) };
    let mut hit = LVHITTESTINFO {
        pt: p,
        ..Default::default()
    };
    let i = send(list, LVM_HITTEST, 0, &mut hit as *mut _ as isize);
    usize::try_from(i).ok()
}

/// Mark `row` as where a drag would drop (Explorer's drop highlight), or no row.
pub fn drop_highlight(list: HWND, row: Option<usize>) {
    let state = |on: bool| LVITEMW {
        mask: LVIF_STATE,
        state: if on {
            LVIS_DROPHILITED
        } else {
            Default::default()
        },
        stateMask: LVIS_DROPHILITED,
        ..Default::default()
    };
    let off = state(false);
    send(
        list,
        LVM_SETITEMSTATE,
        usize::MAX,
        &off as *const _ as isize,
    );
    if let Some(r) = row {
        let on = state(true);
        send(list, LVM_SETITEMSTATE, r, &on as *const _ as isize);
    }
}

/// Select exactly these rows.
pub fn select_rows(list: HWND, rows: &[usize]) {
    let state = |on: bool| LVITEMW {
        mask: LVIF_STATE,
        state: if on {
            LVIS_SELECTED
        } else {
            Default::default()
        },
        stateMask: LVIS_SELECTED,
        ..Default::default()
    };
    let off = state(false);
    send(
        list,
        LVM_SETITEMSTATE,
        usize::MAX,
        &off as *const _ as isize,
    );
    let on = state(true);
    for &r in rows {
        send(list, LVM_SETITEMSTATE, r, &on as *const _ as isize);
    }
}

/// Every selected row, in order.
pub fn selected_all(list: HWND) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = usize::MAX;
    loop {
        let i = send(list, LVM_GETNEXTITEM, at, LVNI_SELECTED as isize);
        let Ok(i) = usize::try_from(i) else { break };
        // A destroyed list answers 0 forever; the next selected row always comes later.
        if at != usize::MAX && i <= at {
            break;
        }
        out.push(i);
        at = i;
    }
    out
}

pub fn selected(list: HWND) -> Option<usize> {
    let i = send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    usize::try_from(i).ok()
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
