# legcon: legacy connections, plan

Status: **draft 10, 2026-11-13.** Nothing is built yet. Decisions marked *open* need an answer
before the phase that depends on them starts.

legcon is a **client**. It connects from a modern computer to an existing machine from the 1990s
that already runs its own servers (FTP, Telnet, SMB, NFS, rsh, SSH), so the user can move files
to and from it and run commands on it. It does not serve anything to the old machine.

The main focus is **usability**: a user with a Windows 95 PC, a SPARCstation or an old Mac on the
LAN types its address, sees what it offers, and drags files across, without knowing which SMB
dialect or SSH cipher it needs.

It follows the programming model of `../rusty-backup` (CONTRIBUTING.md § Architecture policy,
docs/native_frontends_plan.md): model, controllers, views that only render, slow work as jobs.

---

## 1. How it fits together

### 1.1 What the user sees

The user thinks in **machines**, not protocols. A machine is one old computer; it offers one or
more **services** (the SPARCstation has Telnet, FTP and NFS; the Win95 PC has SMB and maybe FTP).
legcon picks the best service for each job and says which it picked.

**There are no modes.** Each connection has one **connection view** with files on top and a
terminal area below, and single clicks rearrange it. Mockup: [connection-view.html](mockups/connection-view.html).

#### Connections: tabs in one window (D11)

- **One tab per connected machine** in one main window. The tab shows the machine's name and its
  state light (§1.4). Closing a tab is disconnecting that machine (a clean log-off, and it asks
  first if a transfer, an unsaved edit or a shell is still running).
- **The first tab is Machines**: the saved machines, Add machine (address, then the probe), and
  Disconnect all. Opening a machine opens its tab.
- **A tab can become its own window**: drag it out, or "Open in new window" on a machine. The
  window is the same connection view; dragging it back docks it. So "one window with tabs" and
  "a window per machine" are both available, and the user picks per machine. New connections open as
  tabs (D11; mockup: [windows-or-tabs.html](mockups/windows-or-tabs.html)).
- **Remote to remote:** drag files onto another machine's tab; the tab opens and the drop lands
  in that machine's current folder.

#### The connection view

```
+------------------------------------------------------------------+
| [Machines] [* sparc  x] [* win95  x] [o indy  x]  [+]            |  tabs = connections
+------------------------------------------------------------------+
| sparc  FTP wu-ftpd 2.6.1  [plaintext]  [Swap]  [Connect/Disc.]  |  machine header
+--------------------------------+---------------------------------+
| This computer  [Home] [Leave home...] [Media v]                  |
| +- Retro        | name   size  | +- /export/home  | name  size   |  files area:
| |  +- sparc     | ...          | |  +- dani        | ...          |  two panes, each a
| |  - src        |              | |  - src          |              |  folder tree (+/-)
+--------------------------------+---------------------------------+  and a list
| ==== splitter (drag) ====   Shell | Console | Log | Transfers     |  terminal tabs
| $ ls -l                                                          |  terminal area,
| ...                                                              |  1/4 of the height
+------------------------------------------------------------------+
```

- **Files area (top)**: two panes, this computer on the left and the machine on the right. Each
  pane is a **folder tree with +/- expanders** next to a file list, like Explorer. Drag between
  them, or copy with the keys (rusty-backup's browser pane and Commander key map underneath).
  - **Each pane's tree collapses with one click** (a button at the left of the pane header, and
    Ctrl+Shift+1 / Ctrl+Shift+2), leaving the file list the full width of the pane; the same
    button brings it back. A View menu item collapses or shows both. The state is remembered per
    pane and per machine.
- **Terminal area (bottom)**, a quarter of the height by default (configurable in Settings; a
  dragged splitter is remembered per machine), with tabs:
  - **Shell**: a terminal session on the machine (Telnet, rlogin, SSH). It shows a **Connect**
    button until the user presses it: the file service and the command service connect
    separately, and nothing opens on its own. Several shell sessions per machine are allowed
    (sub-tabs).
  - **Console**: the protocol's own commands on the file connection, like a classic `ftp>` prompt
    (`SITE CHMOD`, `QUOTE STAT`); a command line for one-shot commands (rsh, rexec, SSH exec)
    with their output and exit status.
  - **Log**: the protocol trace (commands and replies, passwords masked).
  - **Transfers**: progress per file, pause, resume, retry, verify, conflicts. A copy starting
    flashes the tab; it does not steal focus from the shell.
- **Just two areas with a splitter** between them: drag it to change their heights. There is no
  maximize, restore or tiling.
- **Swap** (one button in the top area, in the machine header, and Ctrl+Shift+S) puts the
  terminal area on top and the files below, and back. The split height and the order are
  remembered per machine, so a machine used mostly as a shell opens that way.
- **Machine header**: name, the services in use and their servers, the security badge, and
  Connect / Disconnect for the file service and the command service.

#### The local pane: home, leaving home, media

- **Home folder.** On first run legcon asks for one folder to work from (for example
  `D:\Retro`). Every connection's local pane opens there, with the tree rooted at it. A machine can
  have its own home instead (for example `D:\Retro\sparc`), so its files land in the same place
  every time.
- **Leave home...** is the only way out of the home tree, on purpose: it asks for a path, with a
  text box to type or paste one, a **Browse...** button (the Windows folder picker), and the recent
  places. The pane then shows that place, with **Home** one click back.
- **Media**: a drop-down of removable media attached right now, kept current as media arrive and
  leave. One click goes there (a leave-home).
  - Windows: drives that report removable or CD-ROM, plus fixed drives whose bus is USB, SD or MMC
    (USB hard disks report as fixed), from the volume list and device-change notifications.
  - macOS: the entries in `/Volumes` other than the startup volume.
  - Linux: `/media/$USER`, `/run/media/$USER` and `/mnt`.
- **The remote pane** works the same way: its tree is rooted at the login folder, **Go to
  folder...** asks for a remote path, and Home goes back to the login folder.

#### Preview

- **Right-click a file -> Preview** (also Space, and a double-click setting) opens a preview
  window beside the main one, local or remote. It stays open and follows the selection, so arrow
  keys step through files.
- **What it reads:** only the start of a remote file (a size limit in Settings, 256 KB by
  default), read at an offset on protocols that allow it (SMB, NFS, SFTP) and with `REST` / `ABOR`
  or a capped download on FTP. "Load all" fetches the rest. A preview never writes anything.
- **Viewers:** Text (with the machine's character set and line endings, switchable in the
  window), Hex, Image (GIF, BMP, PCX, TGA, XBM / XPM, MacPaint, PICT where we can decode it),
  Archive listing (tar, ZIP, `.Z`, `.gz`, StuffIt / BinHex / MacBinary through rusty-backup's
  `arc-mac`), and **Open with** a configured external program (§1.3).
- **Edit** in the preview window turns a text preview into the editor (below).
- **Configured by extension:** Settings -> Preview holds a table of extension -> viewer (and its
  options, such as the character set), with defaults for the common 90s types. A machine can
  override rows (CP437 text on a DOS machine, MacRoman on a Mac). Files without an extension, or
  with one not in the table, are sniffed (text or binary) and fall back to Text or Hex. The
  preview window has "Always use this viewer for .ext", which adds the row.
- **Mac files:** a file with a resource fork shows a fork switch (data / resource), as
  rusty-backup's File Info preview does.

#### Editing and new files

- **A text editor inside legcon** (a window like the preview window, from Preview -> Edit, a
  double-click setting, or F4). It keeps the file's character set and line endings, shows them in
  its status line, and saves back where the file came from: on a remote file, Save uploads it
  (FTP `STOR`, SMB / NFS write, SFTP write), with a check that the remote file did not change
  since it was opened (size and time where the protocol gives them). Undo, find / replace, go to
  line. Built on rusty-backup's `model::text_edit`. "Edit with" can instead use a configured
  external editor (§1.3).
- **New file** (right-click in a list, or Ctrl+Shift+N): asks for a name, creates an empty file
  on that side (an empty upload on FTP, a create on SMB / NFS / SFTP), and opens it in the editor.
  The name is checked against the machine's rules first (8.3 on DOS, allowed characters, case).
- **New folder** as before (Ctrl+N).

#### Remote actions (right-click commands)

Some jobs are easiest done on the old machine itself, such as packing a folder before copying it
over a slow link. legcon offers them as **remote actions**: right-click menu items that run a
command on the machine through its command service (rsh, rexec, SSH exec, or the Shell tab).

- **Defined as data, not code**: each action is a template with a name, what it applies to (a
  folder, a file, extensions), the tools it needs, and the command per system family. Examples
  shipped with legcon:
  - **Archive as .tar.gz**: `tar cf - 'src' | gzip -9 > 'src.tar.gz'` (the pipe form, because
    SunOS / IRIX / HP-UX `tar` have no `z` flag);
  - **Archive as .tar.Z** where `gzip` is missing: `tar cf - 'src' | compress > 'src.tar.Z'`;
  - **Extract here** for `.tar`, `.tar.gz`, `.tar.Z`, `.zip` (with `unzip`);
  - **Folder size** (`du -sk`), **Make executable** (`chmod +x`).
- **Only offered when it can work**: the machine must have a command service, and the tools are
  checked once per connection (`which tar gzip compress unzip`, or the probe's results); a missing
  tool greys the item out and says which tool is missing.
- **Show command...**: every action shows the exact command it will run, with the paths filled
  in and quoted for that shell, before it runs. The user can edit it, copy it, run it once
  (output and exit status in Console), or paste it into the Shell tab. The pane refreshes when it
  finishes.
- **The user can add their own** actions in Settings -> Remote actions, with the same fields.
- **When the machine has no command service** (Win95 over SMB, many FTP-only boxes), the
  archive actions run **on this computer instead**: legcon reads the remote folder over the file
  connection and writes the `.tar.gz` locally itself (Rust `tar` + `flate2`), streaming, without
  any tool installed anywhere. The menu says where it runs ("on sparc" or "here").
- **No AI at run time.** Commands come only from these templates. Commands are never suggested by an AI model
  (O9 dropped, 2026-11-14).

#### Remote management

From the remote pane: rename, delete, new file, new folder, permissions / attributes, Get Info
(with the remote's own metadata: DOS attributes, Unix mode, Mac type / creator), edit (the
editor above), remote actions, and "Open shell here" (the Shell tab, `cd` to this folder after
login where the protocol allows).

#### Settings

The **Settings** button in the machine header opens that machine's settings: name and address,
which services to use for files and for commands, the home folder, the layout (split, which area
is on top, trees shown), character set and name translation, legacy crypto, host-key checking,
credentials and keys, the logout command, idle disconnect, and preview / remote-action
overrides. App-wide settings (default home folder, split, preview rules, remote actions, key
manager) are under Tools -> Settings.

The CLI (`lc`) drives the same controllers as the GUI: `lc ls sparc:/usr/local`, `lc get`,
`lc put`, `lc shell sparc`, `lc run sparc "uname -a"`, `lc action sparc tgz src`,
`lc disconnect sparc`.

### 1.2 What happens underneath

```
                       legcon window, lc CLI
                                 |
          controllers (machines, connection view, editor, terminal, transfers, actions)
                                 |
                    session manager (model)            one connection per machine and service,
                          |                            shared by the panes, transfers and shell
             FileSession / ShellSession traits (lc-core)
                          |
   lc-ftp  lc-telnet  lc-rcmd  lc-ssh  lc-smb  lc-nfs   (one crate per protocol)
                          |
                   TCP (later: serial)  ->  the old machine's own servers
```

- **One session manager** owns every connection. The connection view, a transfer and an edit on the
  same machine share one connection where the protocol allows it. Old servers often allow very
  few sessions (Win95 shares, single-user FTP daemons), so this matters.
- **Every protocol crate implements the same two traits** (§3), so the controllers
  never know which protocol they are talking to. Adding a protocol adds a crate and one line in
  the service registry; no screen changes.

### 1.3 No mounts; external programs only when configured

legcon does not plug into the operating system's file system or shell (D22): it does not mount
machines as drives or register itself as a handler. The "shim" of the first drafts is legcon
itself: the window and the `lc` CLI are how a modern computer reaches the old machines.

**External programs are allowed, but only ones legcon knows about**, on every platform:

- **Settings -> Programs** lists them: a name, what it is for (text editor, hex editor, image
  viewer, archiver, terminal, other), the path, and its arguments (`{file}`, `{line}`).
- **Detection**: on first run and on "Detect again", legcon looks for a catalog of known programs
  (shipped as data: Notepad++, VS Code, Sublime Text, BBEdit, gedit, Kate, HxD, Hex Fiend,
  IrfanView, 7-Zip, and so on) and for anything the user names:
  - Windows: the `App Paths` registry keys, `Program Files` folders and `PATH`;
  - macOS: `/Applications` and `~/Applications` bundles, and `PATH`;
  - Linux: `PATH` and the `.desktop` files in the XDG data folders.

  Each found program is shown for the user to accept; nothing is added silently.
- **Where they are used**: a preview rule can be **Open with** a program (§1.1 Preview), the editor
  can be **Edit with** one, and a right-click **Open with** lists the programs that match the
  file's kind.
- **Remote files**: legcon downloads the file to a per-session temporary folder, starts the
  program on it, and watches the file; when the program saves it, legcon offers to upload it (with
  the same "changed on the server since?" check as the built-in editor). The temporary copy is
  deleted when the tab closes.
- **Never required**: every feature has a built-in path (viewers, editor, the tar.gz writer). A
  program that is configured but missing is shown as missing, not run.

What else legcon uses from the OS: the local file system (the home folder and wherever the user
leaves home to), the standard folder picker behind Browse..., the list of removable drives for
Media, and the OS credential store only for machines where the user opts in (§5).

### 1.4 Connecting and disconnecting

The user connects to and disconnects from machines **on demand**, and every protocol supports it
the same way. Disconnecting is a clean log-off on the server side, not just a closed socket: a
Windows 9x / NT machine that still thinks a client is attached to its share asks its own user
about it at shutdown, and many old servers hold a session slot until they time out.

- **One state machine per machine connection**, in the session manager, shown everywhere by the
  same state light: `Disconnected -> Connecting -> Connected <-> Busy -> Disconnecting ->
  Disconnected`, plus `Lost` (the link dropped) with a Reconnect action.
- **Every protocol crate implements `disconnect()` as its proper log-off**, with a timeout, then
  closes the socket:

  | Protocol | Clean disconnect |
  |---|---|
  | SMB1 | close open files, `TREE_DISCONNECT` each share, `LOGOFF_ANDX`, then close the NetBIOS session |
  | SMB2 | `CLOSE` open handles, `TREE_DISCONNECT`, `LOGOFF` |
  | NFS | `UMNT` (MOUNT protocol) for each export, so the server's mount table is cleared |
  | FTP | `ABOR` any transfer, `QUIT` |
  | Telnet / rlogin | send the machine's logout command if one is set (`exit`, `logout`), then close |
  | rsh / rexec / rcp | close (each command is its own connection) |
  | SSH / SCP / SFTP | close channels, send `SSH_MSG_DISCONNECT` |
  | TFTP, Kermit, ZMODEM | finish or cancel the current transfer cleanly |

- **What a disconnect does to work in progress:** transfers in the queue for that machine ask
  Finish first / Pause / Cancel; an unsaved edit asks to save; an open terminal asks to close
  it. The CLI takes these as flags.
- **Disconnect all** from the menu, and automatically when the app quits, when the computer goes to
  sleep or shuts down (the app asks the OS for time to log off), and optionally after an idle time
  per machine.
- **Connections are never opened behind the user's back.** A disconnected machine's tab shows a
  Connect button, not a silent reconnect.

### 1.5 Drag and drop

- **In (built):** files and folders dragged from Explorer (any OLE source offering `CF_HDROP`) onto
  either pane: onto the machine's pane they are uploaded, onto the local pane copied into its
  folder, with the same Overwrite / Skip existing / Cancel question as a copy. Rows dragged from
  one pane to the other copy the same way.
- **Out (next):** rows dragged from the machine's pane to Explorer, as **virtual files**
  (`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`): Explorer pulls each file's bytes from legcon
  as it copies, so nothing is downloaded before the drop and no temporary copy is left behind.
  Local rows drag out as plain files. Folders are listed in the descriptor with their contents.

### 1.6 Mac resource forks off macOS (D31)

Windows and Linux have no resource forks, so a classic Mac file's fork and Finder info (type,
creator, flags) must be kept some other way when it lands on this computer:

- **AppleDouble** (the default proposed): the data fork as the file, the resource fork and Finder
  info in a `._name` sidecar. macOS, netatalk and rusty-backup's `arc-mac` all read it, and
  copying the pair back to a Mac server restores the file.
- **MacBinary** (per transfer or per machine): one `.bin` file carrying both forks, safest for
  moving through tools that would lose a sidecar.
- **NTFS alternate data streams** (optional, Windows only): the fork in `name:AFP_Resource` and
  Finder info in `name:AFP_AfpInfo`, the layout Windows NT's Services for Macintosh used. Lost on
  FAT, ZIP and most copies, so never the default.

Where the forks come from on the remote side: AFP has them natively (phase 8); NFS and SMB
servers running netatalk or A/UX keep them as AppleDouble files (`._name` or `.AppleDouble/name`);
NT 4 SMB shares with Services for Macintosh keep them in the `AFP_Resource` stream; Mac FTP
servers (NetPresenz, AppleShare IP) can send MacBinary on request. legcon recognises each layout,
converts to the chosen local form, and converts back on upload. The fork code is rusty-backup's
`arc-mac` crate.

## 2. Decisions so far

| # | Decision |
|---|---|
| D1 | **legcon is a client only.** It connects to existing legacy servers. No reverse bridge (serving legacy protocols to old machines) is planned. |
| D2 | **Same MVC as rusty-backup, binding from day one.** Model (`src/model/`, the protocol crates), controllers (`src/controller/`, `Action`s in, `Effect`s out, exhaustive enums, widget ids, key map as data), views that only render. |
| D3 | **Every slow operation is a `Job`** (probe, connect, list, transfer, save, remote action). No raw `thread::spawn` outside the job module. |
| D4 | **Crates are for protocols only**: one crate per protocol, plus `lc-core` (the traits, `Entry`, character sets) they all share. Each depends only on `lc-core`, usable on its own like rusty-backup's `fs-*` crates on `disk-core`. Where a good crate exists, the protocol crate wraps it; where not, it implements the protocol. |
| D5 | **Machines, not protocols, in the UI.** The user picks a machine; legcon picks the service and says which. |
| D6 | **Legacy crypto and plaintext auth are allowed per machine, with a visible warning.** Never globally, never silently. |
| D7 | **CLI and GUI only, no TUI.** The Win32 GUI is built from the first protocol phase on, next to the CLI; later GUI shells (AppKit, GTK4) follow rusty-backup's shell rules (every shell shows the same UI). |
| D8 | **Modern stable Rust** (decided 2026-11-13). No Rust 1.73 floor, so legcon can reuse suppaftp, ssh2, `smb` and vt100. |
| D9 | **One connection view per machine, no modes** (§1.1, replaces the Files / Shell modes of draft 4): files on top in two panes (Transmit-like, each a folder tree with +/- and a list), the terminal area below at 1/4 height by default, a draggable splitter, and one **Swap** button in the top area that exchanges the two areas (no maximize, restore or tiling). |
| D10 | **Connect and disconnect on demand, for every protocol, with a clean server-side log-off** (§1.4). |
| D11 | **A tab per connection** in one window; closing a tab disconnects; a tab can be dragged out into its own window and back (decided 2026-11-13, option A of the mockup). |
| D12 | **The MVC code is app code, not a crate**, as in rusty-backup: jobs in `src/model/job.rs`, the controller trait, effects, log and key map in `src/controller/`, copied from rusty-backup and adapted. New crates go under `crates/` in this repo and are protocol crates only (2026-11-13). |
| D13 | **Win32 is the first and main GUI shell** (2026-11-13). |
| D16 | **A local home folder**, picked on first run and overridable per machine; **Leave home...** (type a path or Browse...) is the way out, Home the way back, and a **Media** list goes to removable media (§1.1). |
| D17 | **The terminal area has tabs**: Shell (with a Connect button), Console (the protocol's own commands and one-shot commands), Log and Transfers (§1.1). |
| D18 | **Credentials are part of the product**: SSH key generation and import, throw-away keys, a guided set-up that installs a key on an old machine, an easy host-key bypass for machines that are reinstalled often, and passwords saved in legcon's own machines file by default (§5, D28). |
| D20 | **Each pane's folder tree collapses with one click**, remembered per pane and machine (§1.1). |
| D21 | **Right-click Preview**, reading only the start of a remote file, with viewers chosen by extension in Settings and overridable per machine (§1.1). |
| D22 | **No mounts or shell integration**; legcon itself is the way in (replaces the shim layer of earlier drafts). **External programs only when configured or detected**, the same way on every platform, and never required (§1.3). |
| D23 | **In-app text editor and New file**, saving back to wherever the file lives (§1.1). |
| D24 | **Remote actions as data**: right-click commands from templates (Archive as .tar.gz and others), offered only when the machine has the tools, always showing the exact command first, run locally in Rust when the machine has no command service (§1.1). |
| D25 | **No AI at run time.** Everything works from code and templates; model-suggested commands were considered and dropped (O9, 2026-11-14). |
| D26 | **No inbound connections to the client, ever.** legcon only makes outgoing connections, so nothing has to be open or forwarded on the user's computer: FTP uses passive mode only (`PASV`, `EPSV` where offered; no `PORT`), rsh is asked for no stderr channel, NFS works without the lock manager's callbacks (no NLM), and no protocol is used in a mode where the server connects back. |
| D27 | **rsh, rlogin and rcp come later** (phase 8). rexec, Telnet, FTP and SSH cover commands and files until then (§4.2). |
| D28 | **Passwords are saved in legcon's own machines file by default** (encoded, not encrypted, as FileZilla does), with "Ask every time" and the OS credential store as per-machine choices (§5). Revised 2026-11-14: **no master password** (O10 dropped); these machines mostly have throw-away passwords, so the file keeps them readable. The OS store is a per-machine checkbox, "Keep the password in Windows Credential Manager" (Keychain / Secret Service on later shells). |
| D29 | **Telnet and FTP are our own code**, not wrappers (decided in phase 1, 2026-11-13). Telnet's option engine is small and needs exactly legcon's policy; suppaftp decodes listings with `from_utf8_lossy` and sends paths as UTF-8, which mangles the Latin-1 / MacRoman names 1990s servers use. Both crates depend only on `lc-core`. |
| D30 | **Every machine has a tab colour** (a palette of nine, assigned in turn, changed from the tab's right-click menu): its tab is tinted with it and its header carries a strip of it. |
| D31 | **Mac resource forks: AppleDouble by default, MacBinary as an option** (per transfer or per machine; decided 2026-11-13). NTFS streams are not offered for now. The code is reused from rusty-backup: `fs-mac` (MIT: `build_appledouble`, `parse_appledouble`, `build_macbinary`) and `arc-mac` (reading MacBinary, BinHex and StuffIt), taken from crates.io as rusty-backup publishes them. Revised with AFP (2026-11-13): AppleDouble is legcon's own small code (`src/model/appledouble.rs`), because `fs-mac` builds on `fs-mac-unicode`, which is GPL-2.0-only and cannot be combined with legcon's AGPL-3.0; MacBinary will be the same. |
| D32 | **NFS is our own code** (`lc-nfs`, decided in phase 5, 2026-11-13), not the `nfs` crate: ONC RPC with AUTH_UNIX over UDP (retransmits) and TCP (record marking), portmapper, MOUNT v1 / v3 and NFS v2 / v3 are small, NFSv2 exists nowhere else, and names must stay bytes in the machine's character set. Connecting tries v3 then v2, UDP then TCP, from a reserved source port when it can bind one. The remote pane's root is a folder tree made from the export paths; an export is mounted when entered and unmounted (`UMNT`) on disconnect. The uid / gid sent are per-machine settings (nobody when unset). |
| D33 | **TCP/IP protocols only for now** (2026-11-13). Next: rsh / rlogin / rcp (moved up from phase 8, revising D27), HTTP/1.0 and Gopher (read-only browsing), and AFP over TCP (DSI). Serial links with Kermit / ZMODEM come later, as a direct-connection group; TFTP is a maybe. **Remote screens (VNC / RFB) belong to vintrdp**, not legcon. AFP over TCP needs Open Transport on the Mac: AppleShare IP 5 and later, Mac OS 9 File Sharing, netatalk; MacTCP-only Macs are reached over FTP or HTTP instead, and AFP over AppleTalk stays out of scope. |
| D34 | **Names Windows cannot hold become look-alikes, and come back** (2026-11-13). Downloads map `\ / : * ? " < > |` to their fullwidth forms (`?` -> `？`), control characters to control pictures (the Mac's `Icon
` -> `Icon␍`), a trailing dot or space to fullwidth / ideographic forms, and a device name's last letter to fullwidth (`CON` -> `COＮ`). Uploads map them back where the machine allows the plain character (not `/` on Unix, `:` on a Mac, none of them on DOS), except on Shift-JIS machines, where fullwidth letters are real. A name the machine cannot hold (characters outside its character set, longer than its limit: 31 for AFP 2.x) fails that file with the reason; nothing is sent as mangled bytes. (`src/model/names.rs`) |
| D35 | **Next: Win32 polish (phase 6), then the editor (phase 7); serial, Kermit, ZMODEM and TFTP are deferred** (2026-11-14). The editor is built in (D23); an external editor stays an option, never needed. Mac OS 8.x File Sharing (AppleTalk only) would need our own EtherTalk handler sending raw Ethernet frames; skipped for now (D33). |
| D36 | **SSH keys live in legcon** (2026-11-14): a **Keys** tab (and `lc keys`) makes, imports, exports, copies and deletes login keys kept in the config folder's `keys`. Kinds for old servers: RSA 2048 and RSA 1024 (SSH-2), DSA 1024 (OpenSSH before 2.5, SSH.com before 2.4), and SSH-1 RSA 1024 / 2048 identity files (SSH 1.2.x, OpenSSH `Protocol 1`) for other clients and old servers, since legcon itself logs in over SSH-2 only. Import reads OpenSSH, PEM, PKCS#8, PuTTY `.ppk` and SSH-1 files, converted to traditional PEM; passphrase-protected, ed25519 and ECDSA keys are refused with the reason. **legcon never writes to `~/.ssh`**: Import opens the picker there, Export writes the private key, `.pub` and an SSH.com `.secsh.pub` into a folder the user picks. A machine's settings choose its key; "Set up key login" uses that key, or makes one for the machine. |
| D37 | **SSH-1 is our own code** (`lc_ssh::ssh1`, 2026-11-14): protocol 1.5 for SSH 1.2.x, F-Secure 1.x and OpenSSH with `Protocol 1` only; libssh2 has no SSH-1. Connecting reads the server's version line first: `SSH-1.x` (not `1.99`) gets SSH-1, everything else SSH-2. 3DES in SSH-1's three-CBC mode (the cipher every SSH-1 server has), password and RSA key login (SSH-1 identity files), host keys checked like SSH-2's (`rsa1` keys). SSH-1 runs one command per connection, so files go over SCP and `/bin/sh -c` commands with a connection each, and the terminal is its own connection. "Set up key login" on an SSH-1 machine makes an SSH-1 RSA 1024 key and adds it to `~/.ssh/authorized_keys`. Not supported: Blowfish, DES, IDEA, compression, TIS / Kerberos logins, rhosts-RSA. |
| D38 | **The lab is containers of FOSS servers** (2026-11-14): `lab/containers/unix90s`, one Debian 12 image with every protocol legcon speaks (vsftpd, ProFTPD, inetutils telnetd, netkit rsh / rlogin / rexec, Samba with SMB1, OpenSSH, Dropbear, OpenSSH 7.3 as an SSH-1-only server, unfs3 for NFSv3 and the old nfs-user-server for NFSv2, Netatalk 4, Apache, Gophernicus), driven by `lab/lab.sh` with plain `docker` (no compose), built and tested by `.github/workflows/lab.yml` and published to the GitHub Container Registry from main. `tests/lab_unix90s.rs` (`--ignored`) and `regression-tests/flows/lab.toml` (`run_flows.py --lab`) test legcon against it, the second through the GUI. Containers replace the earlier disk-image idea: they are thrown away after each run and pulled from a registry. Windows FTP listings stay with recorded fixtures and real NT machines (no FOSS server writes them). |
| D19 | **SSH is libssh2** through the `ssh2` crate, because the target is SSH servers from 2005 and earlier (§4.1). Revised in phase 4 (2026-11-13): on Windows libssh2 uses its WinCNG backend (no OpenSSL to build); its pre-2005 algorithms (`diffie-hellman-group1-sha1`, `group14-sha1`, `gex-sha1`, `ssh-dss`, `ssh-rsa`, `3des-cbc`, `arcfour`, `hmac-md5`, `hmac-sha1`) are compile-time opt-ins, switched on in `.cargo/config.toml`; Blowfish and CAST are not available with WinCNG. libssh2 prefers modern algorithms, so the old ones are used only when the server offers nothing newer. **SFTP v3 is our own code** over an ssh2 channel: ssh2's SFTP turns names into `Path`s, which must be UTF-8 on Windows. Host keys are kept per machine (`host_key`), learned on first connect; a changed key asks Accept / Accept once / Cancel before any password is sent. |
| D15 | **Licence: MIT for the protocol crates and `lc-core`, AGPL-3.0 for the app** (2026-11-13). |

## 3. The core traits (`lc-core`)

```rust
pub trait Connection: Send {                     // every protocol, file or shell
    fn state(&self) -> ConnState;                // drives the state light (§1.4)
    fn disconnect(&mut self, timeout: Duration) -> Result<()>;   // clean server-side log-off
}
pub trait FileSession: Connection {
    fn caps(&self) -> &FileCaps;                 // random read / write, rename, set times, ...
    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>>;
    fn stat(&mut self, p: &RemotePath) -> Result<Entry>;
    fn open_read(&mut self, p: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>>;
    fn open_write(&mut self, p: &RemotePath, mode: WriteMode) -> Result<Box<dyn Write + Send + '_>>;
    // mkdir, remove, rename, set_times, set_mode: gated by caps
}
pub trait ShellSession: Connection {
    fn caps(&self) -> &ShellCaps;                // pty, one-shot exec, exit status, window size
    fn exec(&mut self, cmd: &str) -> Result<ExecResult>;
    fn open_pty(&mut self, term: &str, size: (u16, u16)) -> Result<PtyStream>;
}
```

- `Entry` keeps what old systems have: the raw name bytes and the decoded name, DOS attributes,
  Unix mode and owner, Mac type / creator and a fork flag, VMS versions, and how far each
  timestamp can be trusted (an FTP `LIST` line may have no year and no zone).
- `RemotePath` keeps the remote's separator and case rules, so nothing above it guesses.
- **Translation** (in `lc-core`, shared by all): character sets (CP437 / 850 /
  1252, MacRoman, Shift-JIS, ISO-8859-x, per machine), names illegal on one side, 8.3 limits, line
  endings (opt-in, per extension), Mac forks as AppleDouble / MacBinary (rusty-backup's `arc-mac`
  crate), DOS 2-second times and zone-less times.
- **Blocking interface.** rusty-backup runs each connection on a job thread with blocking I/O and
  no async runtime. The traits are blocking too; a crate that wraps an async library runs its own
  small runtime inside (see SSH below), so callers never see async.

## 4. Protocol crates

One crate per protocol. "Reuse" is what exists on crates.io today (checked 2026-11-13) and what
we would do with it; every reuse is confirmed by a spike against the lab before it is adopted.

### 4.1 The initial set

| Crate | Protocol | Existing crates | Plan | What we add on top |
|---|---|---|---|---|
| `lc-ftp` | FTP (own code, D29) | **suppaftp** (mature; sync and async; active and passive mode; built-in `LIST` parser for Unix and DOS only). **ftp-cmd-list-parse** (Unix and DOS only). | **Wrap suppaftp** in sync mode for the control and data connections, **passive mode only** (D26). | The `LIST` parsers the 90s need: VMS, NetWare, OS/2, MVS, AS/400, MacOS (NetPresenz, AppleShare IP), Windows NT/IIS variants, each with fixtures; quirks (no `SIZE` / `MDTM` / `MLSD`, `TYPE L 8`, `REST` support, `SITE` commands). Fixes found go upstream. |
| `lc-telnet` | Telnet (own code, D29) | **libtelnet-rs** and its fork **libmudtelnet** (sans-I/O option parser: WILL/WONT/DO/DONT, subnegotiation, NAWS, TTYPE). The **telnet** crate refuses every option, so it is not usable. | **Wrap libtelnet-rs** (or libmudtelnet) as the option parser, with our own socket loop. | Option policy (ECHO, SGA, BINARY, TTYPE, NAWS), 7-bit hosts, login scripting (wait for `login:`, send the user). |
| `src/model/term` (app code, not a crate) | terminal screen model (used by Telnet, rlogin, SSH) | **vt100** (a terminal parser plus in-memory screen, on top of **vte**). | **Reuse vt100** for VT100 / xterm. | VT52, VT220 extras, ANSI-BBS with CP437, and scrollback, if vt100 lacks them (extend or contribute). |
| `lc-rcmd` | rsh, rlogin, rexec, rcp | None found. | **Write it**: each protocol is a few hundred lines. | The reserved source port rsh / rlogin / rcp use (§4.2); rexec needs none. Shares the rcp code with SCP. |
| `lc-ssh` | SSH-2, SCP, SFTP | **ssh2** (libssh2; blocking API, which fits our job threads; SCP, SFTP v3, exec, pty, public-key and agent auth). libssh2 has long carried the pre-2005 set: `diffie-hellman-group1-sha1`, `group14-sha1`, `group-exchange-sha1`, `ssh-dss`, `ssh-rsa`, `3des-cbc`, `blowfish-cbc`, `cast128-cbc`, `arcfour`, `aes*-cbc`, `hmac-md5`, `hmac-sha1` (and `-96` variants), depending on how it is built. **russh** lacks `ssh-dss`, `hmac-md5` and `blowfish-cbc`, so it is not chosen (D19). | **Wrap ssh2**, built with `vendored-openssl`, the OpenSSL legacy provider loaded and DSA enabled, so the algorithm set does not depend on the user's system. The phase 0 spike lists `libssh2_session_supported_algs()` and connects to every SSH server in the lab. | Target servers up to 2005: OpenSSH 2.x to 4.2, SSH.com / F-Secure 2.x-3.x, Sun SSH 1.0 (Solaris 9), Dropbear 0.4x. **SSH-1** (servers from before about 2000 that speak nothing else) is our own code, later. |
| `lc-smb` | SMB1, SMB2 (+ NetBIOS) | **smb** (pure Rust SMB2/3 client; NTLM and Kerberos through `sspi`) and **smb2** (pure Rust SMB2/3; SMB1 is an explicit non-goal). **No SMB1 client** exists in Rust. | **Write SMB1 and NetBIOS**; **wrap `smb`** for SMB 2.0.2 / 2.1 behind the same crate. | SMB1 dialects from CORE / LANMAN1.0 / LANMAN2.1 (WfW, Win95, OS/2) to NT LM 0.12 (NT4, Samba); share-level passwords (Win9x); LM and NTLMv1 auth; NetBIOS over TCP (port 139), name lookup and workgroup browsing. The biggest crate in the plan. |
| `lc-nfs` | NFS v2, v3 (+ ONC RPC, portmapper, MOUNT) | **nfs** (a client claiming v3 and v4, blocking by default, early development, not for production yet). NFS **server** crates (nfsserve, fernfs) are not useful here. **No NFSv2** found. | **Spike the `nfs` crate** for its RPC / XDR and v3 client; write v2 and fill gaps ourselves, upstream where possible. | UDP and TCP (SunOS 4, IRIX, HP-UX, Ultrix are often UDP-only), 8 KB UDP transfers and retransmits, AUTH_UNIX with a chosen uid / gid, a reserved source port when the server checks for one (§4.2). |

### 4.2 Reserved source ports (rsh, rlogin, rcp, some NFS servers)

**Why it exists.** rsh, rlogin and rcp have no password: the server trusts the client because of
`.rhosts` / `hosts.equiv` *and* because the connection comes from a source port below 1024. On a
1990s Unix only root could bind such a port, so a low port meant "this is the real, setuid-root
`rsh` program, not some user's script". It is how those protocols authenticate, so the servers
refuse anything else ("permission denied" or a silent close). Some NFS servers check the same
thing (`nfs_portmon` on SunOS / Solaris, the `secure` export option on Linux), but on most 1990s
servers it is off unless the admin turned it on.

**It is a problem only on Linux.** The restriction is in the client's OS, not the protocol:

| Client OS | Binding a source port below 1024 | What legcon does |
|---|---|---|
| **Windows** (the main target) | Allowed for any user; Windows has no privileged ports | Just binds one (1023 downwards, as `rresvport()` does) |
| **macOS 10.14+** | Allowed without root when binding the wildcard address, which is what a client does | Same; the phase 0 spike confirms it on current macOS |
| **Linux** | Needs root, `CAP_NET_BIND_SERVICE`, or a lowered `net.ipv4.ip_unprivileged_port_start` | The package grants the capability to the binary once at install (`setcap cap_net_bind_service=+ep`); if it is missing, the error explains it and offers the alternatives below |

**Ways around it that need no low port at all**, which legcon offers first when a machine has
them:
- **rexec** (port 512): runs a command with a user name and password, from any source port. Most
  1990s Unixes ship `rexecd`. legcon uses it for one-shot commands and remote actions.
- **Telnet** instead of rlogin for an interactive shell.
- **FTP** instead of rcp for files.
- rsh's **second connection** (the server connects back to the client for stderr, also from a
  low port, and it fails behind NAT) is skipped: legcon sends port `0`, which merges stderr into
  the output.

No elevated helper process is needed on any platform.

### 4.3 Other connection types worth adding

Ranked by how often they unlock a 90s machine against the effort. All are clients.

| Tier | Crate | Type | Why | Existing crates |
|---|---|---|---|---|
| **A** | `lc-serial` + `lc-kermit`, `lc-xmodem` | Serial (RS-232) link, Kermit, X/Y/ZMODEM | Many 90s machines have no working network but every one has a serial port and a terminal program. Gives a shell and file transfer over a null-modem cable, and also over Telnet. | **serialport** for the port. **rmodem** (XMODEM), **ymodem**, **txmodems** (ZMODEM still in progress). No Kermit. So: reuse the port crate, write ZMODEM and Kermit. |
| **A** | `lc-tftp` | TFTP | The only transfer some PROMs, routers and netbooting workstations speak. | **tftp_client** (sync, RFC 1350, retries). Wrap it; add block-size options (RFC 2347 / 2348) if missing. |
| **A** | `lc-afp` | AFP 2.x over TCP (DSI) | AppleShare IP 5+ (System 7.6.1 to Mac OS 9 with Open Transport), Mac OS 9 File Sharing, netatalk; forks native. Not MacTCP-only Macs (D33). AFP over AppleTalk (older Mac OS) needs raw link-layer frames and is much harder; later. | None found; write it. |
| **B** | `lc-http` | HTTP/1.0, WebDAV, Gopher | 90s web servers on old Unix and Macs; Gopher is tiny. | Any small HTTP client (`ureq`) for HTTP; write Gopher. |
| **B** | `lc-rsync` | rsync (old protocol versions) | Late-90s Unix often has rsync 2.x; resume and deltas. | To check. |
| **C** | - | NetWare NCP over IPX, LapLink, UUCP, DECnet / LAT | Niche, and IPX / LAT need raw frames and drivers. | - |

Recommendation: serial with Kermit / ZMODEM and TFTP right after the initial set, then AFP.

## 5. Usability features

Each lives in a controller, so every view and the CLI get it.

- **Probe on add.** Typing an address starts a probe job: the usual ports (21, 22, 23, 69/udp,
  111, 139, 445, 512-514, 548, 2049), banners, the NetBIOS name, the SMB dialect, the SSH version
  and algorithm lists. Result: "Looks like Windows 95 (LANMAN 2.1, share-level). Files: SMB.
  Commands: none." The user accepts or changes it.
- **Quirk database**, as data the user can extend: a banner or dialect maps to settings (passive or
  active FTP, the `LIST` parser, charset, SSH algorithms).
- **Errors that say what to do**: "This server only offers `ssh-dss` and
  `diffie-hellman-group1-sha1`, which are weak. Allow them for this machine only?"
- **Passwords are saved inside legcon by default** (D28), the way FileZilla does it: in legcon's
  own machines file (`machines.json` in the app's config folder), next to the machine they belong
  to. FileZilla keeps them in `sitemanager.xml`, Base64-encoded since 3.13, which hides them from
  a glance but is not encryption. legcon does the same and says so plainly in Settings. Per
  machine, the user can instead choose:
  - **Ask every time** (kept in memory until the tab closes, never written);
  - **OS credential store** (Windows Credential Manager, macOS Keychain, Secret Service), a
    checkbox per machine; the machines file then holds only a reference.

  No master password (D28, 2026-11-14). SSH keys (§5.1) remain the way to avoid passwords at all.
  Key handling and the guided set-up are in §5.1.
- **Transfer queue** with pause, resume (`REST`, offsets), retry, verify (size, and a checksum where
  the remote can compute one: `SITE CRC`, `XCRC`, `md5sum` over a shell), and a conflict question
  per file or for all.
- **Protocol trace** per session (commands and replies, passwords masked), one click to copy for a
  bug report.
- **A security badge** on every plaintext or weak-crypto session that stays visible without
  nagging.

### 5.1 Credentials and keys

Old machines are insecure, so legcon helps the user make them less so where the machine allows
it, and walks them through it rather than leaving it to manuals.

- **Key manager** (a window off the machine settings and the app menu): generate keys in a form
  the target accepts (RSA 2048 with `ssh-rsa`, DSA 1024 for servers that only take `ssh-dss`),
  import existing keys (OpenSSH, PuTTY `.ppk`, SSH.com / SECSH format), export the public half in
  each of those formats, passphrase-protect them, and use the Windows OpenSSH agent or Pageant.
- **"Set up key login" walkthrough** for a machine that has SSH: legcon logs in once with the
  password, finds the server type from its banner, and installs the public key the way that server
  wants it: `~/.ssh/authorized_keys` or `authorized_keys2` (OpenSSH 2.x-3.x), `~/.ssh2/authorization`
  plus a key file (SSH.com / F-Secure), with the folder and file permissions those servers insist
  on. It then tests a key login, and offers to stop saving the password.
- **Weak-crypto opt-in per machine**, shown in the walkthrough and the settings: the algorithms the
  server offered, which ones legcon will accept, and why.
- **Throw-away keys.** Vintage machines are wiped and reinstalled often, so keys are cheap: one
  click makes a key for one machine (named after it), the walkthrough installs it, and after a
  reinstall "Set up key login" again replaces it. Keys can also be shared across a group of
  machines.
- **Host keys, with an easy bypass.** A reinstalled machine has a new host key, which on a modern
  client is a scary stop. legcon shows the fingerprint (MD5 hex as well as SHA-256, as an old
  server's admin would see it) and offers, in one dialog:
  - **Accept the new key** (replace the stored one; one click, the default for a machine marked
    "reinstalled often"),
  - **Accept once** (this connection only),
  - **Cancel**.

  A machine setting, **Don't check host keys**, skips the check entirely for lab machines; the
  machine header shows it as part of the security badge. Nothing here is global.
- **Plaintext protocols** (FTP, Telnet, rsh, SMB with LM auth): when the machine also offers SSH,
  legcon says so and offers to switch; for rsh, an optional walkthrough sets up `.rhosts` for this
  computer only, with what that trusts spelled out.

## 6. Reuse from rusty-backup

| What | From | How |
|---|---|---|
| `Job`, `JobCtx`, `Notify`, `Resource`, `JobRegistry`, `spawn_guarded` | `src/model/job.rs`, `worker.rs` | Copy into `src/model/job.rs` (D12). |
| `Controller` trait, the generic `Effect`s (pickers, `Confirm`, `AskText` with validator, `AskPassword`, `Message`, clipboard), log store, key-map types, scripted picks, labels | `src/controller/{mod,keys,labels,log,picks,task}.rs` | Same shared crate; each app wraps it in its own exhaustive `Effect`. |
| Browser pane controller with profiles, Commander two-pane layout and keys, staged operations with review | `src/controller/browser`, `commander` | Port the design: a pane's source becomes "this computer or a machine". |
| Win32 shell skeleton (Common Controls v6, UIA ids, effect host), later GTK4 and AppKit | `frontends/*` | Copy the skeletons. |
| `check-boundaries.sh`, `check-platform-matrix.py`, `platform_matrix.md`, `capabilities.md`, `tests/ui_scenarios/`, tier 8 flows and drivers | `scripts/`, `docs/`, `regression-tests/` | Copy and adapt. |
| AppleDouble, MacBinary, BinHex | `crates/arc-mac` | Depend on it for Mac files over FTP, SMB and NFS. |
| Filesystem crates | `crates/disk-core`, `fs-*`, `dsk-*` | Optional: browse inside a disk image on the old machine through a random-read `FileSession`. |

## 7. Repository layout

```
Cargo.toml          workspace; bin `lc` (CLI)
crates/
  lc-core           traits, Entry, RemotePath, caps, errors, character sets
  lc-ftp  lc-telnet  lc-rcmd  lc-ssh  lc-smb  lc-nfs                initial protocols
  lc-serial  lc-kermit  lc-xmodem  lc-tftp  lc-afp                  later protocols
src/
  model/            job.rs (from rusty-backup), session manager, service registry, transfer
                    engine, machine and credential store, probe (service discovery), text
                    editor, remote-action templates, local tar.gz writer, terminal screen model
  controller/       the controller trait, effects, log, key map (from rusty-backup); machines,
                    connection view, browser panes, editor, terminal, transfers, actions, settings, app
  cli/              `lc` verbs
frontends/          win32, gtk4, appkit
lab/                the legacy-server test lab (§8)
docs/               this plan, platform_matrix.md, capabilities.md, protocol notes
tests/ui_scenarios/ controller scenarios
```

## 8. Testing: the legacy lab

- **Containers** for servers that still build: Samba with `server min protocol = LANMAN1` / `NT1`,
  vsftpd, ProFTPD and an old wu-ftpd, inetutils `telnetd` / `rshd` / `rlogind` / `rexecd`,
  Dropbear and old OpenSSH (2.x, 3.x) with legacy algorithms, unfs3 and a kernel NFS v2 / v3
  server, tftpd.
- **Emulated real machines**, reusing rusty-backup's `regression-tests/emulator/`: Windows 95 and
  NT 4 (86Box or QEMU), SunOS / Solaris (QEMU SPARC), Mac OS 8 / 9 (QEMU PPC), an early Linux;
  IRIX and NeXTSTEP where emulation allows.
- **Recorded fixtures**: `LIST` outputs, SMB negotiate replies and SSH KEXINIT lists per server
  family as unit-test inputs, so most tests need no network.
- **Controller scenarios** and **tier 8 flows** per shell, as in rusty-backup.

## 9. Phases

Each phase is small slices; each slice has its tests and every view handles its new Actions.

| Phase | Content | Exit |
|---|---|---|
| **0. Groundwork** | Workspace, the MVC code (D12), `lc-core`, machine store, session manager, the `lc` CLI and the Win32 shell skeleton (window, connection tabs, connection view with its splitter and Swap, home folder), boundary and matrix scripts, the container lab; spikes for suppaftp, libtelnet-rs, vt100, ssh2, `smb`, `nfs` | Spike verdicts written down; CI green; lab containers start |

**Phase 0 progress (2026-11-13):** done: `lc-core`, the job and controller base, the machine store
(passwords Base64, D28), settings with the home folder, the session manager with a built-in demo
machine, the app and connection-view controllers, the `lc` CLI, and the Win32 shell (tabs, Machines
page, connection view with both panes, folder trails, splitter, Swap, tree toggles, Home / Leave
home, connect / browse / disconnect, clean quit), driven by native UIA by AutomationId. Not yet:
the container lab (needs Docker), the protocol spikes, the CP437 / CP850 tables, the Media list,
full folder trees (the trees show the trail to the current folder), and the Add machine dialog
(prompts until mockup 4).
| **1. FTP + Telnet** | `lc-ftp` with the `LIST` parsers, `lc-telnet`, the terminal screen model; Machines, connection view, terminal and transfers controllers; `lc ls/get/put/shell` | Browse and transfer against every FTP server in the lab and a Telnet session to each Unix in it, from the CLI and the Win32 GUI |
| **2. Probe** | the probe (`src/model/probe`), the quirk database, error messages, protocol trace | Adding any lab machine by address suggests the right services and settings |
| **3. SMB1 / SMB2** | `lc-smb`: NetBIOS, SMB1 dialects, share-level security, LM / NTLMv1 / NTLMv2, SMB2 through `smb` | Win95, NT4 and Samba (LANMAN1, NT1, SMB2) shares browse, read and write |
| **4. SSH + rexec** | `lc-ssh` (shell, exec, SCP, SFTP), the per-machine legacy-algorithm policy; rexec from `lc-rcmd` (no low port needed) | Shells, commands and transfers to old OpenSSH / Dropbear and rexecd in the lab |
| **5. NFS v2 / v3** | `lc-nfs` over UDP and TCP | Mount, list, read and write on the lab servers and an emulated SunOS |
| **6. Win32 polish** | Whatever phases 1-5 left unfinished in the GUI; the UIA flows cover every screen | Tier 8 flows pass on Win32 |
| **7. Editor, preview, remote actions** | The editor and New file, the preview viewers and rules, remote-action templates with Show command, the local tar.gz writer | Edit and save a file on every protocol in the lab; Archive as .tar.gz on a Unix with and without gzip, and on Win95 (local) |
| **8. Tier A extras** | rsh, rlogin and rcp (§4.2, deferred from phase 4), serial, Kermit, ZMODEM, TFTP, then AFP over DSI. rsh / rlogin / rcp and AFP are done; **serial, Kermit, ZMODEM and TFTP are deferred** (D35) | A file to and from an emulated DOS machine over a virtual null-modem |
| **9. Other GUI shells** | AppKit, GTK4 | Tier 8 flows on every shell |

**Phase 1 progress (2026-11-13):** done: `lc-telnet` (RFC 854 options BINARY / ECHO / SGA / TTYPE /
NAWS, tested against a fake telnetd), the terminal screen model on `vt100` with a reader job, the
Shell tab in the Win32 shell (painted terminal, colours, cursor, VT220 keys, size follows its
area, logout command on disconnect), `lc-ftp` (own client, passive only to the control address,
Unix and DOS / IIS `LIST` parsers, names in the machine's charset, REST, STOR, rename, delete,
QUIT on disconnect, protocol trace) wired into the remote pane and the Log tab, the Win32
shell restyled to the mockup with per-machine tab colours, copying between panes both ways
(folders included, F6 or the right-click menu, an Overwrite / Skip existing / Cancel question,
the Transfers tab with Cancel), New folder / Rename / Delete on either side, and the Console tab
(the protocol's own commands, run in the pane's folder). Since done: drag and drop, the VMS /
NetWare / OS/2 `LIST` parsers (classic Mac servers read as Unix; 2026-11-14), the CP437 / CP850
tables (2026-11-14), and FTP against a real IIS. Not yet: MVS and AS/400 listings, Telnet against a
real 1990s Unix.

**Phase 3 progress (2026-11-13):** `lc-smb` (own SMB1 client: NetBIOS session with the
`*SMBSERVER` / real-name retry, LANMAN1.0 to NT LM 0.12, share- and user-level logins with LM /
NTLM and an NTLMv2 fallback, RAP share list, TRANS2 listings, read / write / folders / rename /
delete, TREE_DISCONNECT + LOGOFF) is in the remote pane, tested against pretend NT 4 and
Windows 95 servers and shown in the Win32 shell against the Windows 95 one. **Tried on a real
Windows NT machine** (NT LM 0.12, user-level, NTLM; 2026-11-13): shares, listings, reads, a 100 KB
write, mkdir, rename, delete and log-off all work, as does browsing it over anonymous FTP (IIS). Not yet: SMB2 (for
Vista-era and newer Samba setups that turned SMB1 off), workgroup browsing, and a real
Windows 95 / Samba machine. The CP437 / CP850 tables are in (2026-11-14) and the probe suggests
CP437 for SMB1, as non-Unicode SMB sends OEM names; not yet checked against a real Windows 9x
share with accented names.

**Also on 2026-11-14:** downloads keep the machine's modification date (and the AppleDouble
sidecar records it); New folder / Rename on the remote side follow that machine's naming rules
(a Mac takes `/` and `?` but not `:`, at most 31 characters).

**Phase 4 progress (2026-11-13):** `lc-rcmd` with rexec (stderr port 0, so nothing connects back), and a line-mode terminal in the Shell tab for exec-only services (local line editing, `cd` remembered, `exit`). `lc-ssh`: libssh2 transport with the legacy algorithms, password and keyboard-interactive login, own SFTP v3 (list, read, write, resume, mkdir, rename, delete, links followed), shell with pty and resize, exec with exit status, the Console tab running shell commands in the pane's folder, host-key check with the change dialog. Tested against a pretend OpenSSH 2.9 (paramiko offering only group1-sha1, ssh-dss, 3des-cbc, hmac-md5) from the CLI and the Win32 shell. Not yet: public-key login and the key manager (§5.1), SCP and `ls`-over-exec for servers without SFTP, SSH-1, the weak-algorithm opt-in per machine (old algorithms are always offered after the modern ones; the log says when one was used), and a real pre-2005 SSH server.

**Phase 4, keys and SCP (2026-11-13):** "Set up key login" (tab menu, and `lc key-login MACHINE`) logs in with the password, makes a key for that machine in the config folder's `keys` (RSA 2048, or DSA 1024 for OpenSSH before 2.4 and SSH.com before 2.4, as traditional unencrypted PEM, which libssh2's WinCNG backend reads), installs it the way the server family wants (`authorized_keys2` for OpenSSH before 3.0, `authorized_keys` after, `~/.ssh2/authorization` plus a SECSH key file for SSH.com), logs in again with the key alone to prove it, and offers to stop saving the password. A machine with a key connects without asking for a password. Servers without SFTP fall back on their own: SCP for file contents (uploads announce their size, so `FileSession::open_write_sized` was added), `ls -laL`, `mkdir`, `mv`, `rm` over exec for the rest. The `ls -l` parser moved from `lc-ftp` to `lc-core::listing` so both use it. Tested against pretend OpenSSH 2.3 (no SFTP), 2.9 and SSH.com 3.2 servers whose commands run in a real shell. Not yet: passphrase-protected keys (WinCNG cannot read encrypted PEM), importing OpenSSH / PuTTY keys, Pageant, the key manager window, names outside UTF-8 in SCP and exec commands (libssh2's exec takes UTF-8 text).

**Phase 5 progress (2026-11-13):** `lc-nfs` (D32): v2 and v3 browse (READDIRPLUS, or READDIR plus LOOKUP), read, write (v3 FILE_SYNC), overwrite (truncate), create, mkdir, rename, delete, and UMNT on disconnect; errors say what to change (read-only export, uid without access, a server that wants a reserved port). Tested against a pretend server (UDP; v3, and v2-only) and a TCP framing test, from the CLI (`lc machines add NAME ADDR -s nfs --uid N --gid N`) and the Win32 shell. Not yet: following symlinks (shown as links), NFS over TCP against a real server, a real SunOS / IRIX / Linux server, and a uid / gid field in the GUI (the machines file and the CLI only).

**rsh / rlogin / rcp (2026-11-13, D33):** `lc-rcmd::rsh`. Both connect from a reserved source port (1023 downwards; a clear message on Linux without the capability), with the machine's `local_user` (default: its user) as the name `.rhosts` trusts. rsh is a file service (rcp for contents, `ls -laL` / `mkdir` / `mv` / `rm` through `/bin/sh -c`, whatever the login shell, with the exit status echoed since rsh reports none) and a line-mode terminal; rlogin is a real terminal with the window size sent. The rcp protocol moved to `lc-core::rcp`, shared with SCP. Tested against pretend rshd / rlogind servers (Rust tests) and a pretend rshd running real commands. Not yet: a real 1990s rshd.

**HTTP/1.0 and Gopher (2026-11-13, D33):** `lc-http`, own code, read-only file services. HTTP: a folder is the links on its index page that point directly inside it (Apache / NCSA `Index of` pages give dates and sizes too), files are GETs with `Range` for resuming, one request per connection, Basic auth when the machine has a user, redirects followed, HTTP/0.9 answers accepted. Gopher: menus are folders, paths are chains of display names, items on other hosts and info / search / Telnet lines are left out, text files lose their closing `.` line. Tested against pretend servers, a local index page and the live gopher.floodgap.com. Not yet: WebDAV and PUT, following links to other hosts.

**AFP over TCP (2026-11-13, D33):** `lc-afp`, own code: DSI framing (tickles and attention handled), FPGetSrvrInfo, AFP 2.2 / 2.1 / 2.0, login by Randnum Exchange (DES; the password never crosses the wire), else Cleartext, or guest with no user; volumes at the root, Mac-style paths (`Volume:Folder:File`) as long names in MacRoman from the volume's root; enumerate, read, write (DSIWrite), overwrite (truncate), create, folders, delete, rename, move; FPCloseVol, FPLogout and DSICloseSession on disconnect. Resource forks and Finder info: `FileSession::mac_extras` / `set_mac_extras`; downloads write an AppleDouble `._name` next to any file with a fork or a type / creator, uploads apply a `._name` and do not copy it as a file (D31). Tested against a pretend AppleShare IP server (Rust), with the copy engine end to end, and **on a real Mac OS 9.1 File Sharing server** (the PPC 7300 core, 2026-11-13): AFP 2.2 with Randnum Exchange, the volume listed with types, creators and forks, a 100 KB write / rename / read-back / delete and a clean log-out; items with the Finder's invisible flag are left out of listings as the Finder does. Mac OS 8.x File Sharing serves AppleTalk only and cannot be reached (D33). Not yet: AppleShare IP and netatalk, paths deeper than 255 bytes (would need DirID walking), 2-Way Randnum and DHX logins, volume passwords, MacBinary as the per-transfer option. The Win32 shell browses it (used against the real Mac OS 9.1).

**Phase 2 progress (2026-11-14):** the probe (`src/model/probe.rs`): every usual port is tried at once (FTP, SSH, Telnet, Gopher, HTTP, SMB 139 / 445, the portmapper, rexec, rlogin, rsh, AFP), then each service that answered is asked about itself without a login: FTP and SSH banners, the web server's name, SMB dialect and security mode (`lc_smb::probe`), the AFP server's name and versions (`lc_afp::server_info`), NFS versions and exports. From that: the services to use (best file service first), a character set, and a guess at the system. A service that answers but cannot be used (a modern Windows refusing SMB1) is listed with the reason and not suggested. Add machine in the Win32 shell probes after the address and offers Add with these / Choose a service / Cancel; `lc probe ADDRESS`, and `lc machines add` without `-s`, do the same from the CLI. An unreachable machine costs about two seconds. Not yet: the quirk database, Telnet / rlogin banners (they need negotiation first), a probe of an existing machine from its tab.

**Key manager (2026-11-14, D36):** the Keys tab, `lc keys list / new / import / export / show / delete`, the key choice in machine settings, and four flows. Checked against the real `ssh-keygen`: it reads legcon's exported keys, and its fingerprints (SHA256 and MD5) match legcon's; its OpenSSH-format RSA keys import. SSH-1 identity files are checked structurally only (no SSH-1 tool here). Not yet: passphrases, Pageant / the Windows agent, `.ppk` export.

**SSH-1 (2026-11-14, D37):** `lc_ssh::ssh1`: key exchange, 3DES (checked against pycryptodome), the CRC-32 SSH-1 uses, password and RSA logins, exec, terminal, and files over SCP and shell commands; "Set up key login" with SSH-1 keys. Tested against a pretend SSH 1.2.27 (Rust tests, `cargo run -p lc-ssh --example fake_sshd1`, `lc ls` / `lc key-login`, and a UI flow). **Tried on a real SSH 1.2.27 server** (Solaris 8 on the MiSTer SPARCstation 20 core, 2026-11-14): login with an SSH-1 RSA key made on the Keys tab, `lc ls`, browsing, a download and an upload over SCP, and a terminal, all from the Win32 shell. That server is packaged as `LCssh1` in `lab/solaris8-sparc/` (not committed: SSH 1.2.27's own non-commercial licence).

**Phase 6 progress (2026-11-14):** controllers can now ask for a **form** (`Effect::Form`: fields of
text, secret, folder, check, choice and read-only info, grouped by section headings, one validator
over the whole form), so a settings screen is written once in the controller and every shell
draws it with one generic renderer (Win32: a dialog with a tab per section; GTK and AppKit will do
the same). The first form is **machine settings** (`controller::machine_form`, mockup 5): name,
address, file and command service with ports, user, password with the "Keep the password in Windows
Credential Manager" box (D28; `model::secrets` over the `keyring` crate, so Keychain and Secret Service
on the other shells), the login and host keys, host-key checking and forgetting, character set,
home folder, logout command, NFS uid / gid and the rsh local user. It opens from the Machines tab,
the machine header and the tab menu; the CLI has the same form as `lc machines show` / `lc machines
set NAME key=value`. **UI flows** (`regression-tests/`, FLOWS.md): TOML flows naming controls by
controller id, a runner, and a Win32 UI Automation driver speaking a line protocol later shells'
drivers will speak too; 11 flows cover the machine settings, adding and removing machines, and
connecting, browsing, copying, folder operations and the layout on the demo machine. Not yet: the
app-wide Settings, the Media list, full folder trees, probing an existing machine again, flows for
the Shell / Console / Log / Transfers tabs and the host-key and key-login dialogs.

## 10. Risks

| Risk | Mitigation |
|---|---|
| A wrapped crate does not handle a 90s quirk, or is abandoned | Wrap it behind our crate's API, contribute fixes upstream, and be ready to fork; the lab tells us early |
| SMB1 and FTP listings are a long tail of quirks | Fixtures per server family, the quirk database as data, the protocol trace for user reports |
| Legacy SSH algorithms are being dropped from libssh2 and OpenSSL | Build libssh2 and OpenSSL ourselves (vendored, pinned), with the legacy options on; the lab catches a removal on upgrade |
| rsh (and NFS servers that check) need a source port below 1024 | Free on Windows and macOS; on Linux the package grants `CAP_NET_BIND_SERVICE` once; rexec, Telnet and FTP need no low port (§4.2) |
| Plaintext protocols expose passwords, and the machines file holds them encoded, not encrypted | Per-machine opt-in for weak protocols, a visible badge, "Ask every time" and the OS store as per-machine choices; no master password (D28) |

## 11. Open decisions

| # | Question | Recommendation |
|---|---|---|
| O6 | Name | `legcon` / `lc` as a working name |

## 12. Next: GUI mockups and layouts

The plan is detailed enough to design the UI from: the screens, the connection states, the
actions on a machine and a file, and what each protocol can and cannot do (`FileCaps`,
`ShellCaps`) are all named above. The mockups come **before** the controllers are written,
because in this architecture the controllers' state, `Action`s, widget ids and labels are what the
mockups show; the mockups become the controller spec.

Mockups to make, in this order (done ones link to their file):
1. [connection-view.html](mockups/connection-view.html): the connection view with its tabs, the
   files area (trees with +/-), the terminal area at 1/4 with a splitter, and Swap; Leave
   home... and the Media list.
2. [windows-or-tabs.html](mockups/windows-or-tabs.html): a connection as a tab or as its own
   window (decided: tabs, drag out, D11).
3. The Machines tab (cards and rows), with every connection state.
4. Add machine: address, probe progress, the result and its suggested settings.
5. Machine settings: services, home folder, layout, charset and translation, legacy-crypto opt-in,
   host-key checking, logout command, idle disconnect, credentials (built as a form, phase 6).
6. Key manager and the "Set up key login" walkthrough; the changed-host-key dialog.
7. Disconnect with work in progress (transfers, an unsaved edit, a shell), and Disconnect all.
8. Get Info for a remote file with legacy metadata, and the permissions / attributes editor.
9. Preview window and Settings -> Preview (in connection-view.html).
10. Transfers tab details: conflicts, verify, a failed transfer with its protocol trace.
11. Editor, New file, a remote action with Show command (in connection-view.html).
12. Errors that need a decision (weak SSH algorithms, plaintext password, low source port on Linux).
13. Settings -> Programs: detected programs, adding one, Open with on a remote file.

What stays for later, per phase, not needed for the mockups: each protocol crate's design notes
(written when its phase starts, after its spike), the machine store's file format, and the error
type hierarchy.
