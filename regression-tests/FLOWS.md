# UI flows

A flow drives a real shell the way a user would, then checks what the shell saved (the machines
file, the OS-store stand-in, files in the home folder, `lc` output), never the screen alone. Flows
prove a shell is wired to the shared controllers; the logic itself is tested by `cargo test`.

```bash
cargo build && cargo build -p lc-ssh --example fake_sshd1 && (cd frontends/win32 && cargo build)
python regression-tests/run_flows.py                    # every flow on win32
python regression-tests/run_flows.py connection.toml    # one file
python regression-tests/run_flows.py flow.settings.os-store
```

Needs Python 3.9+ with `comtypes`, `tomli` and (for failure screenshots) `Pillow`.

## Writing one

Flows live in `flows/*.toml`. Each `[[case]]` gives the machines to start with, the UI steps, and
what must be true afterwards:

```toml
[[case]]
id = "flow.settings.edit-and-save"
shells = ["win32"]                       # default: win32
machines = [{ id = "sparc", name = "sparc", address = "demo", services = [{ kind = "demo", port = 0 }] }]
secrets = { sparc = "pw" }               # the OS store's contents (a file stands in for it)
ui = [
  { op = "select_row", id = "machines.list", name = "sparc" },
  { op = "activate", id = "machines.settings" },
  { op = "wait_dialog" },
  { op = "fill", id = "machine.uid", text = "1001" },
  { op = "activate", id = "machines.settings.ok" },
]
servers = [{ example = "fake_sshd1", args = ["2231"] }]   # pretend servers, from target/debug/examples
setup_lc = [{ args = ["keys", "new", "lab", "--kind", "rsa1024"] }]   # before the shell starts
picks = { "keys.import" = "{work}/src" } # answers to file and folder pickers, by purpose
expect_files = ["incoming"]              # relative to the home folder
expect_secrets = { sparc = "pw" }        # "__absent__" for gone
lc = [{ args = ["machines", "show", "sparc"], stdout_has = ["1001"] }]

[case.expect_machines]                   # dotted paths into machines.json's list
"0.uid" = 1001
```

`{work}` anywhere in a case is the flow's scratch folder (the home folder; the config is in
`{work}/config`).

Controls are named by their **controller widget id** (`src/controller/ids.rs`), never by what one
shell calls them, so a flow runs unchanged on every shell that has a driver. Form fields use their
field id (`machine.uid`); a form's sections, OK and Cancel are `<purpose>.sections`,
`<purpose>.ok` and `<purpose>.cancel`; the one-line prompt is `prompt.text` / `prompt.ok`.

## Ops

| Op | Fields | Does |
|---|---|---|
| `activate` | `id` | Press a button (without waiting: it may open a dialog) |
| `select_row` | `id`, `name` | Select a list row by its name |
| `expect_row` | `id`, `name`, `present` | Wait until a list has (or lacks) a row |
| `fill` | `id`, `text` | Set a text field |
| `check` | `id`, `on` | Set a check box |
| `choose` | `id`, `option` | Pick a drop-down entry by its text |
| `section` | `id`, `name` | Show a form section (tab) |
| `key` | `id`, `key` | Focus a control and type a chord (`ctrl+n`, `f6`, `enter`) |
| `press` | `name` | Press a message-box button by its label (they have no ids) |
| `wait_dialog`, `expect_no_dialog` | `timeout` | A dialog opened / every dialog closed |
| `expect_enabled` | `id`, `enabled` | A control is (not) enabled |
| `expect_value` | `id`, `value` | A field holds this text |
| `expect_text` | `text` | Some control shows this text |

## Drivers

A driver starts the shell with the flow's config folder (`LEGCON_CONFIG_DIR`) and OS-store file
(`LEGCON_TEST_SECRETS`), prints `{"ready": true}`, then reads one JSON op per line and answers
`{"ok": true}` or `{"ok": false, "error": ...}`; `{"op": "quit"}` closes the shell, which must exit
cleanly. `drivers/uia_driver.py` is the Win32 one (UI Automation by AutomationId). The GTK and
AppKit shells get drivers speaking the same protocol (AT-SPI, the macOS accessibility API), and
their entry in `run_flows.py`'s `SHELLS`.
