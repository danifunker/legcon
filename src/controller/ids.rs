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
/// The files chip's Connect / Disconnect.
pub const CONN_FILES: &str = "conn.files";
/// The commands chip's Connect / Disconnect.
pub const CONN_COMMANDS: &str = "conn.commands";
pub const CONN_CLOSE: &str = "conn.close_tab";
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
pub const TERM_CONNECT: &str = "term.connect";

// Machine settings form (controller::machine_form).
pub const MACHINES_SETTINGS: &str = "machines.settings";

// The Keys tab (controller::ssh_keys).
pub const TAB_KEYS: &str = "tabs.keys";
pub const KEYS_LIST: &str = "keys.list";
pub const KEYS_NEW: &str = "keys.new";
pub const KEYS_IMPORT: &str = "keys.import";
pub const KEYS_EXPORT: &str = "keys.export";
pub const KEYS_COPY: &str = "keys.copy";
pub const KEYS_DELETE: &str = "keys.delete";
pub const KEY_NAME: &str = "key.name";
pub const KEY_KIND: &str = "key.kind";
pub const KEY_COMMENT: &str = "key.comment";
pub const CONN_SETTINGS: &str = "conn.settings";
pub const MS_GENERAL: &str = "machine.section.general";
pub const MS_NAME: &str = "machine.name";
pub const MS_ADDRESS: &str = "machine.address";
pub const MS_FILES: &str = "machine.files";
pub const MS_FILES_PORT: &str = "machine.files_port";
pub const MS_COMMANDS: &str = "machine.commands";
pub const MS_COMMANDS_PORT: &str = "machine.commands_port";
pub const MS_LOGIN: &str = "machine.section.login";
pub const MS_USER: &str = "machine.user";
pub const MS_PASSWORD: &str = "machine.password";
pub const MS_OS_STORE: &str = "machine.os_store";
pub const MS_KEY_FILE: &str = "machine.key_file";
pub const MS_HOST_KEY: &str = "machine.host_key";
pub const MS_CHECK_HOST_KEY: &str = "machine.check_host_key";
pub const MS_FORGET_HOST_KEY: &str = "machine.forget_host_key";
pub const MS_SYSTEM: &str = "machine.section.system";
pub const MS_CHARSET: &str = "machine.charset";
pub const MS_HOME: &str = "machine.home";
pub const MS_LOGOUT: &str = "machine.logout";
pub const MS_UID: &str = "machine.uid";
pub const MS_GID: &str = "machine.gid";
pub const MS_NFS_VERSION: &str = "machine.nfs_version";
pub const MS_LOCAL_USER: &str = "machine.local_user";
