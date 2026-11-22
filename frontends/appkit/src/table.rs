//! A list: a view-based `NSTableView` over rows of text the shell hands it (Win32's ListView).
//! It keeps the rows only for drawing; the selection is read back when an action needs it.
//! Rows drag out (local rows as files Finder can take) and files or rows drop in.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBorderType, NSControlTextEditingDelegate, NSDragOperation,
    NSDraggingInfo, NSEvent, NSFilePromiseProvider, NSFilePromiseProviderDelegate,
    NSFilePromiseReceiver, NSImage, NSImageView, NSPasteboardItem, NSPasteboardTypeFileURL,
    NSPasteboardWriting, NSScrollView, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTableViewDropOperation, NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSIndexSet, NSNotification, NSString, NSURL};

use crate::ui::{self, ns};

/// The pasteboard type of rows dragged between the panes; its value is the side they left.
const ROWS: &str = "org.legcon.rows";

/// What a row shows before its first cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Icon {
    None,
    /// A tab colour's swatch (Machines).
    Color(u32),
    /// The system's icon for a file of this name, or a folder.
    File {
        name: String,
        dir: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub icon: Icon,
    pub cells: Vec<String>,
}

/// What landed on a list, and the folder row it landed on (`None`: the list's own folder).
pub enum Dropped {
    /// Rows from the list whose drag side is this.
    Rows(&'static str, Option<String>),
    Files(Vec<PathBuf>, Option<String>),
}

type Handler = Option<Rc<dyn Fn()>>;

/// Opens the folder row a drag was held over, by name.
type Spring = Rc<dyn Fn(String)>;

#[derive(Default)]
pub struct State {
    rows: Vec<Line>,
    /// "local" or "remote": rows drag out with this side.
    side: Option<&'static str>,
    /// The folder local rows live in, so they drag out as files.
    dir: Option<PathBuf>,
    on_drop: Option<Rc<dyn Fn(Dropped)>>,
    /// Remote rows drag out as file promises redeemed through this.
    on_promise: Option<Promised>,
    on_select: Handler,
    /// A click on a column header, with the column's index.
    on_sort: Option<Rc<dyn Fn(usize)>>,
    /// The column and direction the header arrow shows.
    sorted: Option<(usize, bool)>,
    /// A drag held over a folder row opens it after this long, through `on_spring`.
    spring: Option<(std::time::Duration, Spring)>,
    /// The folder row a drag is over, since when, and whether it has opened already.
    hover: Option<(isize, std::time::Instant, bool)>,
    /// The shell is setting the selection itself, so the change is not reported back.
    quiet: bool,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCTableSource"]
    #[ivars = RefCell<State>]
    pub struct Source;

    unsafe impl NSObjectProtocol for Source {}
    unsafe impl NSControlTextEditingDelegate for Source {}

    unsafe impl NSTableViewDataSource for Source {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn count(&self, _table: &NSTableView) -> isize {
            self.ivars().borrow().rows.len() as isize
        }

        #[unsafe(method_id(tableView:pasteboardWriterForRow:))]
        fn writer(
            &self,
            _table: &NSTableView,
            row: isize,
        ) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
            writer(self.mtm(), &self.ivars().borrow(), row)
        }

        #[unsafe(method(tableView:validateDrop:proposedRow:proposedDropOperation:))]
        fn validate(
            &self,
            table: &NSTableView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            row: isize,
            op: NSTableViewDropOperation,
        ) -> NSDragOperation {
            let mut state = self.ivars().borrow_mut();
            if state.on_drop.is_none() {
                return NSDragOperation::None;
            }
            let ok = match from_side(info) {
                Some(from) => Some(from) != state.side,
                None => !dropped_paths(info).is_empty(),
            };
            if !ok {
                return NSDragOperation::None;
            }
            // Over a folder row: that folder is the target, and held there it opens.
            let folder = (op == NSTableViewDropOperation::On)
                .then(|| folder_at(&state, row))
                .flatten();
            let Some(name) = folder else {
                state.hover = None;
                // The whole list is the target, not a row.
                table.setDropRow_dropOperation(-1, NSTableViewDropOperation::On);
                return NSDragOperation::Copy;
            };
            table.setDropRow_dropOperation(row, NSTableViewDropOperation::On);
            let now = std::time::Instant::now();
            let since = match state.hover {
                Some((r, t, opened)) if r == row => (t, opened),
                _ => (now, false),
            };
            let due = state
                .spring
                .as_ref()
                .filter(|(wait, _)| !since.1 && now.duration_since(since.0) >= *wait)
                .map(|(_, f)| Rc::clone(f));
            state.hover = Some((row, since.0, since.1 || due.is_some()));
            if let Some(f) = due {
                ui::later(Box::new(move || f(name)));
            }
            NSDragOperation::Copy
        }

        #[unsafe(method(tableView:acceptDrop:row:dropOperation:))]
        fn accept(
            &self,
            _table: &NSTableView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            row: isize,
            op: NSTableViewDropOperation,
        ) -> bool {
            let mut state = self.ivars().borrow_mut();
            state.hover = None;
            let folder = (op == NSTableViewDropOperation::On)
                .then(|| folder_at(&state, row))
                .flatten();
            accept(&state, info, folder)
        }
    }

    unsafe impl NSTableViewDelegate for Source {
        #[unsafe(method(tableView:didClickTableColumn:))]
        fn header_clicked(&self, _table: &NSTableView, column: &NSTableColumn) {
            let i = column.identifier().to_string().parse::<usize>().ok();
            let f = self.ivars().borrow().on_sort.clone();
            if let (Some(f), Some(i)) = (f, i) {
                ui::later(Box::new(move || f(i)));
            }
        }

        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn cell(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<NSView>> {
            let col = column
                .and_then(|c| c.identifier().to_string().parse::<usize>().ok())
                .unwrap_or(0);
            let width = column.map_or(100.0, |c| c.width());
            let state = self.ivars().borrow();
            state
                .rows
                .get(row as usize)
                .map(|line| cell_view(self.mtm(), line, col, width))
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _note: &NSNotification) {
            let f = {
                let s = self.ivars().borrow();
                if s.quiet {
                    return;
                }
                s.on_select.clone()
            };
            if let Some(f) = f {
                ui::later(Box::new(move || f()));
            }
        }
    }
);

/// A row as it leaves by drag: the side it left, and a file URL for local rows.
/// A row as it leaves by drag: a local row is its file's URL, a remote row a file promise the
/// Finder redeems by naming a folder; our own panes know each other by the drag's source.
fn writer(
    mtm: MainThreadMarker,
    state: &State,
    row: isize,
) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
    let side = state.side?;
    let line = state.rows.get(row as usize)?;
    let name = line.cells.first()?.clone();
    if let Some(dir) = &state.dir {
        let url = NSURL::fileURLWithPath(&ns(&dir.join(&name).to_string_lossy()));
        return Some(ProtocolObject::from_retained(url));
    }
    if let Some(promised) = &state.on_promise {
        let dir = matches!(line.icon, Icon::File { dir: true, .. });
        let kind = if dir { "public.folder" } else { "public.data" };
        let delegate = promise_delegate(mtm, promised.clone());
        let p = NSFilePromiseProvider::initWithFileType_delegate(
            mtm.alloc(),
            &ns(kind),
            ProtocolObject::from_ref(&*delegate),
        );
        let name: Retained<AnyObject> = Retained::into_super(Retained::into_super(ns(&name)));
        // SAFETY: an NSString, read back by the delegate; the provider keeps it.
        unsafe { p.setUserInfo(Some(&name)) };
        // The provider holds its delegate weakly: keep it for as long as the provider lives.
        PROMISES.with(|k| k.borrow_mut().push(delegate));
        return Some(ProtocolObject::from_retained(p));
    }
    let item = NSPasteboardItem::new();
    item.setString_forType(&ns(side), &ns(ROWS));
    Some(ProtocolObject::from_retained(item))
}

/// What a promise is redeemed with: the row's name and the folder it lands in.
pub type Promised = Rc<dyn Fn(String, PathBuf)>;

define_class!(
    /// Answers the Finder for a remote row dragged out: its name, then where it goes.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCPromiseDelegate"]
    #[ivars = Promised]
    pub struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, p: &NSFilePromiseProvider, _kind: &NSString) -> Retained<NSString> {
            promised_name(p).map_or_else(|| ns("file"), |n| ns(&n))
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(
            &self,
            p: &NSFilePromiseProvider,
            url: &NSURL,
            done: &block2::DynBlock<dyn Fn(*mut objc2_foundation::NSError)>,
        ) {
            let dir = url
                .path()
                .map(|s| PathBuf::from(s.to_string()))
                .and_then(|p| p.parent().map(PathBuf::from));
            if let (Some(name), Some(dir)) = (promised_name(p), dir) {
                (self.ivars())(name, dir);
            }
            // The download runs in the transfer queue, which shows its progress and errors.
            done.call((std::ptr::null_mut(),));
        }
    }
);

fn promised_name(p: &NSFilePromiseProvider) -> Option<String> {
    let info = p.userInfo()?;
    info.downcast_ref::<NSString>().map(|s| s.to_string())
}

fn promise_delegate(mtm: MainThreadMarker, f: Promised) -> Retained<PromiseDelegate> {
    let this = mtm.alloc::<PromiseDelegate>().set_ivars(f);
    // SAFETY: NSObject's plain init.
    unsafe { msg_send![super(this), init] }
}

/// The side of the legcon pane a drag comes from, if it does.
fn from_side(info: &ProtocolObject<dyn NSDraggingInfo>) -> Option<&'static str> {
    let source = info.draggingSource()?;
    let list = source.downcast_ref::<ListView>()?;
    let side = list.ivars().borrow().side;
    side.or_else(|| match rows_from(info)?.as_str() {
        "local" => Some("local"),
        "remote" => Some("remote"),
        _ => None,
    })
}

/// The name of row `row` when it is a folder.
fn folder_at(state: &State, row: isize) -> Option<String> {
    let line = state.rows.get(usize::try_from(row).ok()?)?;
    match &line.icon {
        Icon::File { name, dir: true } => Some(name.clone()),
        _ => None,
    }
}

fn accept(
    state: &State,
    info: &ProtocolObject<dyn NSDraggingInfo>,
    folder: Option<String>,
) -> bool {
    let Some(f) = state.on_drop.clone() else {
        return false;
    };
    let what = match from_side(info) {
        Some(side) => Dropped::Rows(side, folder),
        None => Dropped::Files(dropped_paths(info), folder),
    };
    // The drop may ask a question: let the drag finish first.
    ui::later(Box::new(move || f(what)));
    true
}

/// The side the dragged rows came from, when the drag is legcon's own rows.
fn rows_from(info: &ProtocolObject<dyn NSDraggingInfo>) -> Option<String> {
    let board = info.draggingPasteboard();
    board.stringForType(&ns(ROWS)).map(|s| s.to_string())
}

/// The file paths a drag from Finder carries.
fn dropped_paths(info: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    let board = info.draggingPasteboard();
    let Some(items) = board.pasteboardItems() else {
        return Vec::new();
    };
    // SAFETY: AppKit's own pasteboard type constant.
    let kind = unsafe { NSPasteboardTypeFileURL };
    items
        .iter()
        .filter_map(|item| item.stringForType(kind))
        .filter_map(|url| NSURL::URLWithString(&url))
        .filter_map(|url| url.path())
        .map(|p| PathBuf::from(p.to_string()))
        .collect()
}

thread_local! {
    static ICONS: RefCell<HashMap<String, Retained<NSImage>>> = RefCell::default();
    /// Promise delegates of the drags so far (small objects; a drag's lifetime is not told).
    static PROMISES: RefCell<Vec<Retained<PromiseDelegate>>> = const { RefCell::new(Vec::new()) };
}

/// The system's icon for a file of `name` (by extension) or a folder, cached per kind.
#[allow(deprecated)] // iconForFileType: its replacement needs UniformTypeIdentifiers
fn file_icon(name: &str, dir: bool) -> Retained<NSImage> {
    let key = if dir {
        "/".to_string()
    } else {
        match name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
            _ => String::new(),
        }
    };
    ICONS.with(|c| {
        if let Some(i) = c.borrow().get(&key) {
            return i.clone();
        }
        let ws = NSWorkspace::sharedWorkspace();
        let kind = if dir {
            // The folder type code, as the Finder spells it.
            "'fldr'".to_string()
        } else {
            key.clone()
        };
        let image = ws.iconForFileType(&ns(&kind));
        c.borrow_mut().insert(key, image.clone());
        image
    })
}

const ROW_H: f64 = 20.0;

fn cell_view(mtm: MainThreadMarker, line: &Line, col: usize, width: f64) -> Retained<NSView> {
    let text = line.cells.get(col).cloned().unwrap_or_default();
    let label = ui::label(mtm, &text);
    if col > 0 || line.icon == Icon::None {
        label.setFrame(ui::rect(0.0, 1.0, width, ROW_H - 2.0));
        return Retained::into_super(Retained::into_super(label));
    }
    let holder = ui::flipped(mtm);
    holder.setFrame(ui::rect(0.0, 0.0, width, ROW_H));
    let icon: Retained<NSView> = match &line.icon {
        Icon::File { name, dir } => {
            let v = NSImageView::imageViewWithImage(&file_icon(name, *dir), mtm);
            Retained::into_super(Retained::into_super(v))
        }
        Icon::Color(c) => {
            let c = *c;
            let v = ui::canvas(
                mtm,
                Rc::new(move |b| {
                    let r = ui::rect(2.0, 2.0, b.size.width - 4.0, b.size.height - 4.0);
                    ui::pill(r, &ui::rgb(c), &ui::rgb(c), 3.0);
                }),
            );
            Retained::into_super(v)
        }
        Icon::None => unreachable!(),
    };
    icon.setFrame(ui::rect(2.0, 2.0, 16.0, 16.0));
    holder.addSubview(&icon);
    label.setFrame(ui::rect(22.0, 1.0, width - 22.0, ROW_H - 2.0));
    label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    holder.addSubview(&label);
    holder
}

#[derive(Default)]
pub struct Hooks {
    activate: Handler,
    menu: Handler,
    /// The side its rows drag out as, so a drop knows which pane they left.
    side: Option<&'static str>,
}

define_class!(
    /// A table that hands Return and right-clicks to the shell.
    #[unsafe(super(NSTableView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LCListView"]
    #[ivars = RefCell<Hooks>]
    pub struct ListView;

    unsafe impl NSObjectProtocol for ListView {}

    impl ListView {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let f = self.ivars().borrow().activate.clone();
            // Return and the keypad's Enter open the selected row, as a double-click does.
            match (event.keyCode(), f) {
                (36 | 76, Some(f)) => ui::later(Box::new(move || f())),
                // SAFETY: the table's own handling of every other key.
                _ => unsafe {
                    let _: () = msg_send![super(self), keyDown: event];
                },
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            let row = self.rowAtPoint(at);
            // As the Finder does: a click outside the selection selects that row first.
            if row >= 0 && !self.isRowSelected(row) {
                let set = NSIndexSet::indexSetWithIndex(row as usize);
                self.selectRowIndexes_byExtendingSelection(&set, false);
            }
            if let Some(w) = self.window() {
                w.makeFirstResponder(Some(self));
            }
            let f = self.ivars().borrow().menu.clone();
            if let Some(f) = f {
                ui::later(Box::new(move || f()));
            }
        }

        #[unsafe(method(fire:))]
        fn fire(&self, _sender: Option<&AnyObject>) {
            let f = self.ivars().borrow().activate.clone();
            if self.clickedRow() >= 0 {
                if let Some(f) = f {
                    ui::later(Box::new(move || f()));
                }
            }
        }
    }
);

/// A list in its scroll view; the shell keeps both.
pub struct Table {
    pub scroll: Retained<NSScrollView>,
    pub view: Retained<ListView>,
    source: Retained<Source>,
}

impl Table {
    /// `columns` are (title, width in points); `id` is the controller id.
    pub fn new(mtm: MainThreadMarker, id: &str, columns: &[(&str, i32)], multi: bool) -> Table {
        let this = mtm
            .alloc::<ListView>()
            .set_ivars(RefCell::new(Hooks::default()));
        // SAFETY: NSTableView's designated initialiser.
        let view: Retained<ListView> =
            unsafe { msg_send![super(this), initWithFrame: ui::rect(0.0, 0.0, 10.0, 10.0)] };
        for (i, (title, width)) in columns.iter().enumerate() {
            let col = NSTableColumn::initWithIdentifier(mtm.alloc(), &ns(&i.to_string()));
            col.setTitle(&ns(title));
            col.setWidth(f64::from(*width));
            view.addTableColumn(&col);
        }
        view.setRowHeight(ROW_H);
        view.setUsesAlternatingRowBackgroundColors(true);
        view.setAllowsMultipleSelection(multi);
        view.setAllowsEmptySelection(true);
        ui::set_id(&view, id);
        let source: Retained<Source> = {
            let this = mtm
                .alloc::<Source>()
                .set_ivars(RefCell::new(State::default()));
            // SAFETY: NSObject's plain init.
            unsafe { msg_send![super(this), init] }
        };
        // SAFETY: the table keeps these weakly; `Table` keeps the source alive with it.
        unsafe {
            view.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            view.setDelegate(Some(ProtocolObject::from_ref(&*source)));
            view.setTarget(Some(&view));
            view.setDoubleAction(Some(objc2::sel!(fire:)));
        }
        let scroll = NSScrollView::initWithFrame(mtm.alloc(), ui::rect(0.0, 0.0, 10.0, 10.0));
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setBorderType(NSBorderType::BezelBorder);
        scroll.setDocumentView(Some(&view));
        Table {
            scroll,
            view,
            source,
        }
    }

    /// Return or a double-click on a row runs `f`.
    pub fn on_activate(&self, f: Rc<dyn Fn()>) {
        self.view.ivars().borrow_mut().activate = Some(f);
    }

    /// A right-click runs `f` once the row under it is selected.
    pub fn on_menu(&self, f: Rc<dyn Fn()>) {
        self.view.ivars().borrow_mut().menu = Some(f);
    }

    /// The user changed the selection.
    pub fn on_select(&self, f: Rc<dyn Fn()>) {
        self.source.ivars().borrow_mut().on_select = Some(f);
    }

    /// A drag held over a folder row for `wait` runs `f` with the folder's name (to open it).
    pub fn spring_open(&self, wait: std::time::Duration, f: Rc<dyn Fn(String)>) {
        self.source.ivars().borrow_mut().spring = Some((wait, f));
    }

    /// A click on a column header runs `f` with that column's index.
    pub fn on_sort(&self, f: Rc<dyn Fn(usize)>) {
        self.source.ivars().borrow_mut().on_sort = Some(f);
    }

    /// Show the sort arrow on column `col`, pointing up or down; the others show none.
    pub fn set_sort(&self, col: usize, descending: bool) {
        if self.source.ivars().borrow().sorted == Some((col, descending)) {
            return;
        }
        self.source.ivars().borrow_mut().sorted = Some((col, descending));
        let name = if descending {
            "NSDescendingSortIndicator"
        } else {
            "NSAscendingSortIndicator"
        };
        let arrow = NSImage::imageNamed(&ns(name));
        let columns = self.view.tableColumns();
        for (i, c) in columns.iter().enumerate() {
            let image = (i == col).then(|| arrow.clone()).flatten();
            self.view
                .setIndicatorImage_inTableColumn(image.as_deref(), &c);
            if i == col {
                self.view.setHighlightedTableColumn(Some(&c));
            }
        }
    }

    /// Rows drag out as `side`; the list takes drops through `f`.
    pub fn drag_and_drop(&self, side: &'static str, f: Rc<dyn Fn(Dropped)>) {
        {
            let mut s = self.source.ivars().borrow_mut();
            s.side = Some(side);
            s.on_drop = Some(f);
        }
        self.view.ivars().borrow_mut().side = Some(side);
        // SAFETY: AppKit's own pasteboard type constant.
        let mut kinds = vec![unsafe { NSPasteboardTypeFileURL }.to_owned(), ns(ROWS)];
        // File promises too: remote rows dragged onto the local pane carry nothing else.
        kinds.extend(NSFilePromiseReceiver::readableDraggedTypes().iter());
        let kinds: Vec<&NSString> = kinds.iter().map(|k| &**k).collect();
        self.view
            .registerForDraggedTypes(&NSArray::from_slice(&kinds));
        self.view
            .setDraggingSourceOperationMask_forLocal(NSDragOperation::Copy, true);
        self.view
            .setDraggingSourceOperationMask_forLocal(NSDragOperation::Copy, false);
    }

    /// Rows drag out to the Finder as file promises; `f` gets each row's name and folder.
    pub fn promise_files(&self, f: Promised) {
        self.source.ivars().borrow_mut().on_promise = Some(f);
    }

    /// The folder the rows are in, so local rows drag out to Finder as files.
    pub fn set_dir(&self, dir: Option<PathBuf>) {
        self.source.ivars().borrow_mut().dir = dir;
    }

    /// Show `rows`; only a change reloads, keeping the selected indexes that still exist.
    pub fn set_rows(&self, rows: Vec<Line>) {
        if self.source.ivars().borrow().rows == rows {
            return;
        }
        let keep = self.selected_all();
        let n = rows.len();
        self.source.ivars().borrow_mut().rows = rows;
        self.quietly(|| self.view.reloadData());
        let keep: Vec<usize> = keep.into_iter().filter(|i| *i < n).collect();
        self.select_rows(&keep);
    }

    /// Another folder's rows: nothing selected, scrolled to the first row.
    pub fn to_top(&self) {
        self.select_rows(&[]);
        if self.len() > 0 {
            self.view.scrollRowToVisible(0);
        }
    }

    fn len(&self) -> usize {
        self.source.ivars().borrow().rows.len()
    }

    pub fn selected_all(&self) -> Vec<usize> {
        let set = self.view.selectedRowIndexes();
        let mut out = Vec::new();
        let mut i = set.firstIndex();
        while i != objc2_foundation::NSNotFound as usize {
            out.push(i);
            i = set.indexGreaterThanIndex(i);
        }
        out
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected_all().first().copied()
    }

    /// Select these rows as the shell's own change: it is not reported back.
    pub fn select_rows(&self, rows: &[usize]) {
        if self.selected_all() == rows {
            return;
        }
        let set = objc2_foundation::NSMutableIndexSet::new();
        for r in rows {
            set.addIndex(*r);
        }
        self.quietly(|| self.view.selectRowIndexes_byExtendingSelection(&set, false));
    }

    /// The list has the keyboard.
    pub fn focused(&self) -> bool {
        let Some(w) = self.view.window() else {
            return false;
        };
        w.firstResponder().is_some_and(|r| {
            std::ptr::eq(
                &*r as *const _ as *const u8,
                &*self.view as *const _ as *const u8,
            )
        })
    }

    fn quietly(&self, f: impl FnOnce()) {
        self.source.ivars().borrow_mut().quiet = true;
        f();
        self.source.ivars().borrow_mut().quiet = false;
    }
}
