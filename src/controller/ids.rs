//! Stable widget ids: every view sets them as the accessibility id (UIA `AutomationId`).

pub const TAB_MACHINES: &str = "tabs.machines";
/// A connection tab is `tabs.conn.<machine id>`.
pub const TAB_CONN_PREFIX: &str = "tabs.conn.";

pub const MACHINES_LIST: &str = "machines.list";
pub const MACHINES_ADD: &str = "machines.add";
pub const MACHINES_OPEN: &str = "machines.open";
pub const MACHINES_REMOVE: &str = "machines.remove";
pub const MACHINES_DISCONNECT_ALL: &str = "machines.disconnect_all";

pub const CONN_HEADER: &str = "conn.header";
pub const CONN_CONNECT: &str = "conn.connect";
pub const CONN_DISCONNECT: &str = "conn.disconnect";
pub const CONN_SWAP: &str = "conn.swap";
pub const CONN_SPLITTER: &str = "conn.splitter";

pub const LOCAL_TREE_TOGGLE: &str = "local.tree_toggle";
pub const LOCAL_HOME: &str = "local.home";
pub const LOCAL_LEAVE_HOME: &str = "local.leave_home";
pub const LOCAL_PATH: &str = "local.path";
pub const LOCAL_TREE: &str = "local.tree";
pub const LOCAL_LIST: &str = "local.list";

pub const REMOTE_TREE_TOGGLE: &str = "remote.tree_toggle";
pub const REMOTE_HOME: &str = "remote.home";
pub const REMOTE_PATH: &str = "remote.path";
pub const REMOTE_TREE: &str = "remote.tree";
pub const REMOTE_LIST: &str = "remote.list";

pub const TERM_TABS: &str = "term.tabs";
pub const TERM_BODY: &str = "term.body";
