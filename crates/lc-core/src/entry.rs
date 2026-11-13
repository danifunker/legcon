//! One file or folder as a remote machine reports it, keeping what old systems have.

/// How much of a timestamp the server actually told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimePrecision {
    /// Date and time to the second, in UTC (SFTP, NFS, SMB).
    #[default]
    Exact,
    /// Local time of unknown zone (FTP `LIST`, DOS), to the minute or 2 seconds.
    LocalUnknownZone,
    /// `Nov 13 10:04`: the year was guessed as this year or last.
    YearGuessed,
    /// `Nov 13  1997`: no time of day.
    DateOnly,
}

/// A point in time as seconds since 1970 (UTC where the precision says so), with its precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timestamp {
    pub unix: i64,
    pub precision: TimePrecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Folder,
    /// A symlink; its target, when known, is in [`Entry::link_target`].
    Link,
    /// A device, pipe or anything else the listing shows.
    Other,
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name decoded with the machine's character set.
    pub name: String,
    /// The name exactly as the server sent it, for addressing it again.
    pub raw_name: Vec<u8>,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<Timestamp>,
    /// Unix mode bits, when the listing has them.
    pub mode: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// DOS attributes (read-only 0x01, hidden 0x02, system 0x04, archive 0x20).
    pub dos_attributes: Option<u8>,
    /// Classic Mac type and creator codes.
    pub mac_type_creator: Option<([u8; 4], [u8; 4])>,
    /// A Mac file with a resource fork.
    pub has_resource_fork: bool,
    pub link_target: Option<String>,
}

impl Entry {
    /// A plain entry with only a name and kind; listing parsers fill in the rest.
    pub fn new(name: impl Into<String>, kind: EntryKind) -> Self {
        let name = name.into();
        Entry {
            raw_name: name.as_bytes().to_vec(),
            name,
            kind,
            size: None,
            modified: None,
            mode: None,
            owner: None,
            group: None,
            dos_attributes: None,
            mac_type_creator: None,
            has_resource_fork: false,
            link_target: None,
        }
    }

    pub fn is_folder(&self) -> bool {
        self.kind == EntryKind::Folder
    }
}
