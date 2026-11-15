# CLAUDE.md

Guidance for Claude Code in this repository.

## Project

legcon is a Rust client for 1990s machines: file transfer and remote commands over FTP, Telnet,
SMB1/2, NFS v2/v3, SSH (pre-2005 servers), rexec, and later rsh, serial, TFTP and AFP. The main
GUI is Win32; there is also the `lc` CLI. The plan and every decision so far: `docs/PLAN.md`
(read §2, the decisions table). Mockups: `docs/mockups/`.

It follows the programming model of `../rusty-backup` (model / controller / view, jobs, effects).

## Architecture policy (mandatory)

**Before adding or changing any code, read [CONTRIBUTING.md § Architecture policy (MVC)](CONTRIBUTING.md)
in full, every session.** It is binding. In short:

- **Model**: `crates/lc-*` (protocol crates and `lc-core` only) and `src/model/`. No UI.
- **Controller**: `src/controller/`, one per task, `Action`s in, `Effect`s out, exhaustive enums,
  widget ids, key map as data. No toolkit, no raw threads.
- **View**: `src/cli/`, `frontends/win32/` and `frontends/appkit/`. Renders and fulfils effects only; never imports
  `model` or a protocol crate.
- Slow work is a `model::job::Job`.
- Disconnect is a clean server-side log-off; no inbound connections to the client.

## Build

```bash
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets
cd frontends/win32 && cargo build   # separate workspace
cd frontends/appkit && cargo build  # separate workspace (macOS); bundle/build-app.sh makes legcon.app
```

Never run two cargo builds at once.

## Rules worth repeating

- Comments in `.rs` files: one line, two at most.
- No Unicode glyphs in UI text or logs.
- When a decision in `docs/PLAN.md` changes, update the decisions table in the same commit.
