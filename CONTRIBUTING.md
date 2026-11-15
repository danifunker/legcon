# Contributing to legcon

legcon is a client for talking to 1990s machines over the protocols they speak (FTP, Telnet, SMB,
NFS, SSH, rexec, later rsh and serial). The plan is [docs/PLAN.md](docs/PLAN.md); read its
decisions table (§2) before changing behaviour.

## Build / run / test

```bash
cargo build                         # the lc CLI and the library
cargo test --workspace              # every test, crates included
cargo fmt --check
cargo clippy --workspace --all-targets
cargo run --bin lc -- machines list
cd frontends/win32 && cargo run     # the Win32 shell (its own workspace and lockfile)
cd frontends/appkit && cargo run    # the AppKit shell on macOS (likewise); bundle/build-app.sh makes legcon.app
```

## Architecture policy (MVC) - read before adding any code

This section is **binding**. It is rusty-backup's policy (`../rusty-backup/CONTRIBUTING.md`),
adapted. If a change seems to need an exception, stop and raise it instead of working around it.

### The three layers

```
VIEW        src/cli/ (lc verbs)   frontends/win32/   frontends/appkit/ (later frontends/gtk4)
            renders state, maps input to Actions, fulfils Effects
CONTROLLER  src/controller/  one controller per task, screen or dialog
            state, Actions in / Effects out, validation, key map, widget ids, jobs
MODEL       crates/lc-*  (one crate per protocol, plus lc-core)
            src/model/   (machines, sessions, transfers, jobs, local files)
```

Dependencies point **down only**: view -> controller -> model. A model module never imports a
controller or a view; a controller never imports a view or a toolkit.

| Layer | Owns | Must not |
|---|---|---|
| **Model** | Protocols, sessions, the machine store and settings, local files, transfers. Runs slow work as `model::job` jobs. | Touch a UI toolkit, hold UI state (selection, which dialog is open), or know a controller exists. |
| **Controller** (`src/controller/`) | What the user is doing: open tabs, selection, layout, prompts, validation rules, the `Action` and `Effect` enums, widget ids, starting and cancelling jobs. | Import a toolkit, spawn raw threads, block on I/O, or assume a particular view. |
| **View** | Layout, painting, focus, scrolling. Turns input into controller `Action`s through the key map and fulfils `Effect`s. | Contain rules, import `model` or protocol crates, spawn threads, decide what a key does. |

**The litmus test**: could a second view need it? If yes, it is controller or model code.

### Crates

- `crates/` holds **protocol crates only** (`lc-ftp`, `lc-telnet`, `lc-smb`, ...) and `lc-core`,
  the traits, entry types and character sets they share. The MVC code, the machine store, the
  terminal screen model and the like are app code in `src/`.
- A protocol crate depends on `lc-core` and its own protocol dependencies, nothing in `src/`.
- Protocol crates are MIT; the app and the shells are AGPL-3.0.

### Controllers: the contract

- A controller implements `controller::Controller`: `apply(Action) -> Vec<Effect>` and `tick()`.
- **`Action` and `Effect` are exhaustive.** Views match them with no `_` arm, so a new variant is
  a compile error in every view that ignores it.
- **Every actionable element has a stable widget id** (the controller's `ids` module); a view
  sets it as the accessibility id (UIA `AutomationId` on Win32).
- **Shortcuts are data** in `src/controller/keys.rs`; `Primary` is Ctrl on Windows and Linux and
  Cmd on macOS.
- **Prompts are effects** (`PickFolder`, `Confirm`, `AskText`, `AskPassword`, `Choose`); a text
  prompt carries a controller-defined validator the view runs as the user types and the
  controller runs again on the answer.

### Async work

- **Every slow operation is a `model::job::Job`**: anything that touches the network, a remote
  machine or a folder listing. Nothing slow runs in `apply` / `tick` or on a view's thread.
- **No `thread::spawn` outside `model::job` and `model::worker`.** `scripts/check-boundaries.sh`
  enforces it.
- Views wake through `Notify` (Win32: a posted `WM_APP` message) rather than polling.

### Connections

- Every protocol implements `lc_core::Connection`, and `disconnect()` is a **clean log-off** on
  the server (PLAN.md §1.4), never just a closed socket.
- **No inbound connections** to the user's computer, ever (D26): FTP is passive only, no protocol
  is used in a mode where the server connects back.
- Nothing connects behind the user's back: connecting is always an action the user took.

## Specific rules

- **Comments are one line, two at most.** Longer explanations go in the module's `//!` header or
  in `docs/`. Say why, not what.
- **No Unicode glyphs in UI text or log lines**: plain ASCII (`OK`, `->`, `Warning`). Text read
  from a remote machine is shown as it is.
- Errors: `Result` and `?` throughout; `thiserror` in crates, `anyhow` in the app.
- No AI at run time (D25): every feature works from code and data.

## Commits

Small commits, each building and passing its tests. Message: a short `area: what` subject, a body
saying why.
