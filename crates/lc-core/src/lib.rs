//! What every legcon protocol crate shares (docs/PLAN.md §3).
//!
//! A protocol crate implements [`Connection`] and one or both of [`FileSession`] and
//! [`ShellSession`], and declares what it can do through [`FileCaps`] / [`ShellCaps`]. The app's
//! controllers only ever see these traits, so adding a protocol changes no screen.
//!
//! Everything here is blocking: each session runs on its own job thread in the app, with
//! timeouts on its sockets. [`Connection::disconnect`] is a clean server-side log-off, never just
//! a closed socket (PLAN.md §1.4).

pub mod charset;
pub mod checksum;
pub mod dos;
pub mod entry;
pub mod error;
pub mod listing;
pub mod memory;
pub mod path;
pub mod rcp;
pub mod session;
pub mod shellfile;
pub mod trace;

pub use charset::Charset;
pub use checksum::Checksum;
pub use entry::{Entry, EntryKind, TimePrecision, Timestamp};
pub use error::{Error, Result};
pub use path::{PathStyle, RemotePath};
pub use session::{
    ConnState, Connection, ExecResult, FileCaps, FileSession, MacExtras, Pty, PtyWriter, ShellCaps,
    ShellSession, WriteMode,
};
pub use trace::{Trace, TraceLog};
