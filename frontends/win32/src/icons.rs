//! Icons for the lists and trees: the system's file-type icons, and a square per tab colour.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Controls::{ImageList_Add, ImageList_Create, HIMAGELIST, ILC_COLOR32};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
};

use crate::paint;
use crate::ui;

thread_local! {
    static SYS: RefCell<(Option<isize>, HashMap<String, i32>)> = RefCell::new((None, HashMap::new()));
    static COLORS: RefCell<Option<isize>> = const { RefCell::new(None) };
}

fn lookup(name: &str, dir: bool) -> (isize, i32) {
    let attrs = if dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let w = ui::wide(name);
    let mut info = SHFILEINFOW::default();
    // SAFETY: `info` is sized for the call; USEFILEATTRIBUTES never touches the disk.
    let list = unsafe {
        SHGetFileInfoW(
            windows::core::PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    (list as isize, info.iIcon)
}

/// The system's small icon list (shared: a control must not destroy it).
pub fn system_list() -> HIMAGELIST {
    SYS.with(|s| {
        let mut s = s.borrow_mut();
        if s.0.is_none() {
            s.0 = Some(lookup("folder", true).0);
        }
        HIMAGELIST(s.0.unwrap_or(0))
    })
}

/// The icon index for a file or folder by its name's extension, as Explorer shows it.
pub fn file_icon(name: &str, dir: bool) -> i32 {
    let key = if dir {
        "/".to_string()
    } else {
        match name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => format!(".{}", ext.to_ascii_lowercase()),
            _ => String::new(),
        }
    };
    SYS.with(|s| {
        if let Some(i) = s.borrow().1.get(&key) {
            return *i;
        }
        let probe = if dir {
            "folder".to_string()
        } else {
            format!("file{key}")
        };
        let i = lookup(&probe, dir).1;
        s.borrow_mut().1.insert(key, i);
        i
    })
}

/// A list of filled squares, one per colour in `rgbs`, `size` pixels wide.
pub fn color_list(rgbs: &[u32], size: i32) -> HIMAGELIST {
    COLORS.with(|c| {
        if let Some(h) = *c.borrow() {
            return HIMAGELIST(h);
        }
        // SAFETY: bitmaps drawn on a memory DC, added to the list (which copies them), then freed.
        let list = unsafe {
            let list = ImageList_Create(size, size, ILC_COLOR32, rgbs.len() as i32, 0);
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            for &rgb in rgbs {
                let bmp = CreateCompatibleBitmap(screen, size, size);
                let old = SelectObject(dc, HGDIOBJ(bmp.0));
                paint::fill(dc, &paint::rect(0, 0, size, size), paint::WHITE);
                paint::fill(dc, &paint::rect(2, 2, size - 4, size - 4), rgb);
                SelectObject(dc, old);
                ImageList_Add(list, bmp, None);
                let _ = DeleteObject(HGDIOBJ(bmp.0));
            }
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            list
        };
        *c.borrow_mut() = Some(list.0);
        list
    })
}
