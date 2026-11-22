//! Ordering a pane's list by a column (D50): a header click sorts by it, a second reverses it.
//! Folders stay above files, as Explorer and the Finder keep them; ties go by name.

use std::cmp::Ordering;

use super::{ConnectionController, Side};
use crate::model::local::LocalEntry;
use crate::model::machines::{Sort, SortBy};
use lc_core::Entry;

/// The columns in the order the panes show them (the local pane has no Permissions).
pub const SORT_COLUMNS: [SortBy; 4] = [
    SortBy::Name,
    SortBy::Size,
    SortBy::Modified,
    SortBy::Permissions,
];

/// Which way a column sorts on its first click: names A to Z, sizes and dates largest first.
fn first_descending(by: SortBy) -> bool {
    matches!(by, SortBy::Size | SortBy::Modified)
}

/// The sort after a click on `by`: the other way round if it was already this column.
pub(super) fn clicked(now: Sort, by: SortBy) -> Sort {
    if now.by == by {
        Sort {
            by,
            descending: !now.descending,
        }
    } else {
        Sort {
            by,
            descending: first_descending(by),
        }
    }
}

fn by_name(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// `key` in the sort's direction, a missing value last either way, then the name A to Z.
fn order<K: Ord>(sort: Sort, a: (Option<K>, &str), b: (Option<K>, &str)) -> Ordering {
    let keyed = match (a.0, b.0) {
        (Some(x), Some(y)) if sort.descending => y.cmp(&x),
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    keyed.then_with(|| by_name(a.1, b.1))
}

fn by_name_in(sort: Sort, a: &str, b: &str) -> Ordering {
    let o = by_name(a, b);
    if sort.descending {
        o.reverse()
    } else {
        o
    }
}

pub(super) fn sort_local(rows: &mut [LocalEntry], sort: Sort) {
    rows.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| match sort.by {
            SortBy::Name | SortBy::Permissions => by_name_in(sort, &a.name, &b.name),
            SortBy::Size => order(
                sort,
                ((!a.is_dir).then_some(a.size), &a.name),
                ((!b.is_dir).then_some(b.size), &b.name),
            ),
            SortBy::Modified => order(sort, (a.modified, &a.name), (b.modified, &b.name)),
        })
    });
}

pub(super) fn sort_remote(rows: &mut [Entry], sort: Sort) {
    rows.sort_by(|a, b| {
        let folders = b.is_folder().cmp(&a.is_folder());
        folders.then_with(|| match sort.by {
            SortBy::Name => by_name_in(sort, &a.name, &b.name),
            SortBy::Size => order(sort, (a.size, &a.name), (b.size, &b.name)),
            SortBy::Modified => order(
                sort,
                (a.modified.map(|t| t.unix), &a.name),
                (b.modified.map(|t| t.unix), &b.name),
            ),
            SortBy::Permissions => order(sort, (a.mode, &a.name), (b.mode, &b.name)),
        })
    });
}

impl ConnectionController {
    /// How a pane's list is ordered, for the view's header arrow.
    pub fn sort(&self, side: Side) -> Sort {
        match side {
            Side::Local => self.layout.sort_local,
            Side::Remote => self.layout.sort_remote,
        }
    }

    /// A click on a pane's column header.
    pub(super) fn sort_by(&mut self, side: Side, by: SortBy) {
        let next = clicked(self.sort(side), by);
        match side {
            Side::Local => self.layout.sort_local = next,
            Side::Remote => self.layout.sort_remote = next,
        }
        self.layout_changed = true;
        self.resort(side);
    }

    /// Put a pane's rows in its order again (after a listing or a header click).
    pub(super) fn resort(&mut self, side: Side) {
        match side {
            Side::Local => sort_local(&mut self.local_rows, self.layout.sort_local),
            Side::Remote => sort_remote(&mut self.remote_rows, self.layout.sort_remote),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lc_core::{EntryKind, TimePrecision, Timestamp};

    fn local(name: &str, dir: bool, size: u64, modified: Option<i64>) -> LocalEntry {
        LocalEntry {
            name: name.into(),
            is_dir: dir,
            size,
            modified,
        }
    }

    #[test]
    fn a_second_click_reverses_and_a_new_column_starts_its_own_way() {
        let name = Sort::default();
        assert!(clicked(name, SortBy::Name).descending);
        let date = clicked(name, SortBy::Modified);
        assert_eq!(
            (date.by, date.descending),
            (SortBy::Modified, true),
            "newest first"
        );
        assert!(!clicked(date, SortBy::Modified).descending);
    }

    #[test]
    fn folders_stay_first_and_files_follow_the_column() {
        let mut rows = vec![
            local("b.txt", false, 10, Some(300)),
            local("Zeta", true, 0, Some(100)),
            local("a.txt", false, 2000, Some(100)),
            local("c.txt", false, 10, None),
            local("alpha", true, 0, Some(900)),
        ];
        let names = |r: &[LocalEntry]| r.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        sort_local(&mut rows, Sort::default());
        assert_eq!(names(&rows), ["alpha", "Zeta", "a.txt", "b.txt", "c.txt"]);
        let newest = Sort {
            by: SortBy::Modified,
            descending: true,
        };
        sort_local(&mut rows, newest);
        assert_eq!(
            names(&rows),
            ["alpha", "Zeta", "b.txt", "a.txt", "c.txt"],
            "no date last"
        );
        let biggest = Sort {
            by: SortBy::Size,
            descending: true,
        };
        sort_local(&mut rows, biggest);
        assert_eq!(
            names(&rows)[2..],
            ["a.txt", "b.txt", "c.txt"],
            "same size by name"
        );
        let z_to_a = Sort {
            by: SortBy::Name,
            descending: true,
        };
        sort_local(&mut rows, z_to_a);
        assert_eq!(names(&rows), ["Zeta", "alpha", "c.txt", "b.txt", "a.txt"]);
    }

    #[test]
    fn remote_rows_sort_by_size_date_and_permissions() {
        let e = |name: &str, size: Option<u64>, unix: i64, mode: u32| Entry {
            size,
            modified: Some(Timestamp {
                unix,
                precision: TimePrecision::LocalUnknownZone,
            }),
            mode: Some(mode),
            ..Entry::new(name, EntryKind::File)
        };
        let mut rows = vec![
            e("x", Some(5), 50, 0o644),
            e("y", None, 70, 0o600),
            e("z", Some(9), 60, 0o755),
        ];
        let names = |r: &[Entry]| r.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        let s = |by, descending| Sort { by, descending };
        sort_remote(&mut rows, s(SortBy::Size, true));
        assert_eq!(names(&rows), ["z", "x", "y"], "unknown size last");
        sort_remote(&mut rows, s(SortBy::Modified, false));
        assert_eq!(names(&rows), ["x", "z", "y"]);
        sort_remote(&mut rows, s(SortBy::Permissions, false));
        assert_eq!(names(&rows), ["y", "x", "z"]);
    }
}
