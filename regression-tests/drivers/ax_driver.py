"""The AppKit shell's flow driver: macOS Accessibility over pyobjc, one JSON op per line (FLOWS.md).

Run by run_flows.py as `python ax_driver.py <legcon> <config dir> <secrets file>`. It starts the
shell, answers {"ready": true}, then answers each op with {"ok": true} or
{"ok": false, "error": ...}. Controls are found by AXIdentifier, which the shell sets to the
controller's widget id, in the main window, its dialogs and any open popup menu.

The process running this needs the Accessibility permission (System Settings > Privacy &
Security > Accessibility, for the terminal or agent that starts it). Keys go to the shell as
CGEvents; Primary is Cmd, so a flow's `ctrl+n` is Cmd+N here.
"""

import json
import os
import subprocess
import sys
import time

import ApplicationServices as AX
import Quartz

PROC = None
APP = None


class Fail(Exception):
    pass


def attr(e, name):
    err, v = AX.AXUIElementCopyAttributeValue(e, name, None)
    return v if err == 0 else None


def text(e, name):
    v = attr(e, name)
    if v is None:
        return ""
    if isinstance(v, bool):
        return "1" if v else "0"
    return str(v)


def children(e):
    return list(attr(e, "AXChildren") or [])


def windows():
    return list(attr(APP, "AXWindows") or [])


def ident(e):
    return text(e, "AXIdentifier")


def is_main(w):
    return ident(w) == "main"


def dialogs():
    return [w for w in windows() if not is_main(w)]


def walk(e, depth=0, out=None):
    out = [] if out is None else out
    if depth > 40:
        return out
    out.append(e)
    for c in children(e):
        walk(c, depth + 1, out)
    return out


def menus():
    """Open popup menus: under the control they drop from, or the app for one at the mouse."""
    out = [c for c in children(APP) if text(c, "AXRole") == "AXMenu"]
    for w in windows():
        out += [e for e in walk(w) if text(e, "AXRole") == "AXMenu"]
    return out


def elements():
    """Every element of the shell's windows (open menus included), dialogs first."""
    out = []
    for w in dialogs() + [w for w in windows() if is_main(w)]:
        walk(w, 0, out)
    for m in children(APP):
        if text(m, "AXRole") == "AXMenu":
            walk(m, 0, out)
    return out


def find(aid, timeout=5.0):
    end = time.time() + timeout
    while True:
        for e in elements():
            if ident(e) == aid:
                return e
        if time.time() > end:
            raise Fail(f"no control {aid}")
        time.sleep(0.2)


def names(e):
    return [n for n in (text(e, k) for k in ("AXTitle", "AXValue", "AXDescription")) if n]


# The press reached the app, which went into a modal dialog before answering.
CANNOT_COMPLETE = -25204


def press(e, what):
    err = AX.AXUIElementPerformAction(e, "AXPress")
    if err not in (0, CANNOT_COMPLETE):
        raise Fail(f"{what} did not take a press (AX error {err})")


def op_activate(o):
    e = find(o["id"])
    if attr(e, "AXEnabled") is False:
        raise Fail(f"{o['id']} is disabled")
    press(e, o["id"])
    time.sleep(o.get("pause", 0.5))


def row_name(row):
    """A table row's name: the text of its first cell."""
    for e in walk(row):
        if text(e, "AXRole") == "AXStaticText":
            v = text(e, "AXValue")
            if v:
                return v
    return ""


def rows_of(aid):
    t = find(aid)
    return [(row_name(r), r) for r in (attr(t, "AXRows") or [])], t


def op_select_row(o):
    rows, t = rows_of(o["id"])
    for name, r in rows:
        if name == o["name"]:
            err = AX.AXUIElementSetAttributeValue(t, "AXSelectedRows", [r])
            if err != 0:
                AX.AXUIElementSetAttributeValue(r, "AXSelected", True)
            time.sleep(0.3)
            return
    raise Fail(f"no row {o['name']} in {o['id']}: {[n for n, _ in rows]}")


def op_expect_row(o):
    want = o.get("present", True)
    end = time.time() + o.get("timeout", 5)
    while True:
        names_ = [n for n, _ in rows_of(o["id"])[0]]
        if (o["name"] in names_) == want:
            return
        if time.time() > end:
            raise Fail(f"{o['id']} rows {names_}: {o['name']} {'missing' if want else 'still there'}")
        time.sleep(0.3)


def op_sort(o):
    """Click a list's column header by its title, as a user sorting by it does."""
    t = find(o["id"])
    header = None
    for col in attr(t, "AXColumns") or []:
        h = attr(col, "AXHeader")
        if h is not None and o["column"] in names(h):
            header = h
            break
    if header is None:
        # The header view sits beside the table in its scroll area, as buttons named by title.
        area = attr(t, "AXParent")
        for e in walk(area if area is not None else t):
            if text(e, "AXRole") == "AXButton" and o["column"] in names(e):
                header = e
                break
    if header is None:
        raise Fail(f"no column {o['column']} in {o['id']}")
    press(header, f"{o['id']} column {o['column']}")
    time.sleep(o.get("pause", 0.5))


def op_expect_order(o):
    """The list's first rows are `names`, in that order."""
    want = o["names"]
    end = time.time() + o.get("timeout", 5)
    while True:
        got = [n for n, _ in rows_of(o["id"])[0]]
        if got[: len(want)] == want:
            return
        if time.time() > end:
            raise Fail(f"{o['id']} rows {got}, expected them to start {want}")
        time.sleep(0.3)


def frontmost():
    AX.AXUIElementSetAttributeValue(APP, "AXFrontmost", True)
    time.sleep(0.1)


def type_text(s):
    for ch in s:
        if ch in "\r\n":
            post(KEYS["enter"], 0)
        else:
            post(0, 0, ch)
        time.sleep(0.01)


def op_fill(o):
    """Set a text field as typing would: the shell's live validation sees the change."""
    e = find(o["id"])
    want = o["text"]
    frontmost()
    AX.AXUIElementSetAttributeValue(e, "AXFocused", True)
    time.sleep(0.1)
    # Typed over a select-all, as a user would, so the shell's change handling runs.
    post(KEYS["a"], Quartz.kCGEventFlagMaskCommand)
    if want:
        type_text(want)
    else:
        post(KEYS["backspace"], 0)
    time.sleep(0.3)
    got = text(e, "AXValue")
    if got != want and attr(e, "AXSubrole") != "AXSecureTextField":
        raise Fail(f"{o['id']} holds {got!r} after filling {want!r}")


def op_check(o):
    e = find(o["id"])
    on = text(e, "AXValue") == "1"
    if on != bool(o["on"]):
        press(e, o["id"])
        time.sleep(0.3)


def op_choose(o):
    """Open a pop-up button's menu and pick an entry by its text."""
    e = find(o["id"])
    press(e, o["id"])
    end = time.time() + 5
    seen = []
    while time.time() < end:
        for m in walk(e):
            if text(m, "AXRole") == "AXMenuItem":
                seen.append(text(m, "AXTitle"))
                if text(m, "AXTitle") == o["option"]:
                    press(m, o["option"])
                    time.sleep(0.4)
                    return
        time.sleep(0.2)
    AX.AXUIElementPerformAction(e, "AXCancel")
    raise Fail(f"{o['id']} has no option {o['option']}: {seen}")


def op_section(o):
    """Show a tab by its title: a form's sections, or the main tab strip."""
    box = find(o["id"])
    for e in walk(box)[1:]:
        # A segment names itself in its description, a tab or button in its title.
        label = text(e, "AXTitle") or text(e, "AXDescription")
        if text(e, "AXRole") in ("AXRadioButton", "AXButton", "AXTab") and label == o["name"]:
            press(e, o["name"])
            time.sleep(0.5)
            return
    raise Fail(f"no section {o['name']} in {o['id']}")


def op_expect_enabled(o):
    e = find(o["id"])
    got = attr(e, "AXEnabled") is not False
    if got != bool(o["enabled"]):
        raise Fail(f"{o['id']} enabled is {got}, expected {o['enabled']}")


def op_expect_value(o):
    v = text(find(o["id"]), "AXValue")
    if v != o["value"]:
        raise Fail(f"{o['id']} is {v!r}, expected {o['value']!r}")


def op_wait_dialog(o):
    end = time.time() + o.get("timeout", 10)
    while time.time() < end:
        if dialogs():
            time.sleep(0.3)
            return
        time.sleep(0.2)
    raise Fail("no dialog opened")


def op_expect_no_dialog(o):
    end = time.time() + o.get("timeout", 3)
    while time.time() < end:
        if not dialogs():
            return
        time.sleep(0.2)
    raise Fail(f"a dialog is still open: {[text(w, 'AXTitle') for w in dialogs()]}")


def op_expect_text(o):
    end = time.time() + o.get("timeout", 5)
    while time.time() < end:
        for e in elements():
            if any(o["text"] in n for n in names(e)):
                return
        time.sleep(0.3)
    raise Fail(f"no text {o['text']!r} on screen")


def op_press(o):
    """Press a dialog button by its label (alert buttons have no controller ids)."""
    end = time.time() + o.get("timeout", 10)
    while time.time() < end:
        for w in dialogs():
            for b in walk(w):
                if text(b, "AXRole") == "AXButton" and text(b, "AXTitle") == o["name"]:
                    press(b, o["name"])
                    time.sleep(o.get("pause", 0.6))
                    return
        time.sleep(0.3)
    raise Fail(f"no dialog button {o['name']}")


def op_menu(o):
    """Choose an item of the open popup menu by its text."""
    end = time.time() + o.get("timeout", 5)
    seen = []
    while time.time() < end:
        for m in menus():
            items = [i for i in walk(m) if text(i, "AXRole") == "AXMenuItem"]
            seen = [text(i, "AXTitle") for i in items]
            for i in items:
                if text(i, "AXTitle") == o["name"]:
                    if attr(i, "AXEnabled") is False:
                        raise Fail(f"menu item {o['name']} is disabled")
                    press(i, o["name"])
                    gone = time.time() + 5
                    while time.time() < gone and menus():
                        time.sleep(0.1)
                    time.sleep(o.get("pause", 0.4))
                    return
        time.sleep(0.2)
    raise Fail(f"no menu item {o['name']} in {seen}")


KEYS = {"enter": 36, "return": 36, "tab": 48, "space": 49, "backspace": 51, "escape": 53,
        "delete": 117, "home": 115, "end": 119, "left": 123, "right": 124, "down": 125, "up": 126,
        "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97, "f7": 98, "f8": 100,
        "f9": 101, "f10": 109, "f11": 103, "f12": 111,
        "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9, "b": 11,
        "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "1": 18, "2": 19, "3": 20, "4": 21,
        "6": 22, "5": 23, "9": 25, "7": 26, "8": 28, "0": 29, "o": 31, "u": 32, "i": 34, "p": 35,
        "l": 37, "j": 38, "k": 40, "n": 45, "m": 46}
MODS = {"ctrl": Quartz.kCGEventFlagMaskCommand, "cmd": Quartz.kCGEventFlagMaskCommand,
        "shift": Quartz.kCGEventFlagMaskShift, "alt": Quartz.kCGEventFlagMaskAlternate,
        "control": Quartz.kCGEventFlagMaskControl}


def post(code, flags, unicode=None):
    for down in (True, False):
        ev = Quartz.CGEventCreateKeyboardEvent(None, code, down)
        Quartz.CGEventSetFlags(ev, flags)
        if unicode is not None:
            Quartz.CGEventKeyboardSetUnicodeString(ev, len(unicode), unicode)
        Quartz.CGEventPostToPid(PROC.pid, ev)
    time.sleep(0.05)


def op_key(o):
    """Focus a control (when `id` is given) and type a chord such as ctrl+n (Cmd+N) or f6."""
    frontmost()
    if "id" in o:
        e = find(o["id"])
        AX.AXUIElementSetAttributeValue(e, "AXFocused", True)
        time.sleep(0.2)
    parts = o["key"].lower().split("+")
    flags = 0
    for m in parts[:-1]:
        if m not in MODS:
            raise Fail(f"unknown modifier {m}")
        flags |= MODS[m]
    if parts[-1] not in KEYS:
        raise Fail(f"unknown key {parts[-1]}")
    post(KEYS[parts[-1]], flags)
    time.sleep(o.get("pause", 0.6))


def op_type(o):
    """Type text into the focused control (the terminal, the console)."""
    frontmost()
    if "id" in o:
        AX.AXUIElementSetAttributeValue(find(o["id"]), "AXFocused", True)
        time.sleep(0.2)
    type_text(o["text"])
    time.sleep(o.get("pause", 0.4))


def op_screenshot(o):
    """The shell's own windows (screencapture -l), never the rest of the screen."""
    info = Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly, Quartz.kCGNullWindowID)
    ids = [w[Quartz.kCGWindowNumber] for w in info
           if w[Quartz.kCGWindowOwnerPID] == PROC.pid and w.get(Quartz.kCGWindowLayer, 99) < 25
           and w[Quartz.kCGWindowBounds]["Height"] > 60]
    if not ids:
        raise Fail("no window to capture")
    base, ext = os.path.splitext(o["path"])
    for i, wid in enumerate(ids):
        path = o["path"] if i == 0 else f"{base}-{i + 1}{ext}"
        subprocess.run(["screencapture", "-x", "-o", f"-l{wid}", path], check=False)


OPS = {
    "screenshot": op_screenshot,
    "menu": op_menu,
    "key": op_key,
    "type": op_type,
    "expect_row": op_expect_row,
    "sort": op_sort,
    "expect_order": op_expect_order,
    "press": op_press,
    "activate": op_activate,
    "select_row": op_select_row,
    "fill": op_fill,
    "check": op_check,
    "choose": op_choose,
    "section": op_section,
    "expect_enabled": op_expect_enabled,
    "expect_value": op_expect_value,
    "wait_dialog": op_wait_dialog,
    "expect_no_dialog": op_expect_no_dialog,
    "expect_text": op_expect_text,
}


def reply(d):
    sys.stdout.write(json.dumps(d) + "\n")
    sys.stdout.flush()


def quit_shell():
    main = [w for w in windows() if is_main(w)]
    close = attr(main[0], "AXCloseButton") if main else None
    if close is not None:
        AX.AXUIElementPerformAction(close, "AXPress")
    else:
        PROC.terminate()
    try:
        code = PROC.wait(15)
        reply({"ok": code == 0, "error": f"exit code {code}"})
    except subprocess.TimeoutExpired:
        left = [text(w, "AXTitle") for w in windows()]
        PROC.kill()
        reply({"ok": False, "error": f"the shell did not quit; windows left: {left}"})


def main():
    global PROC, APP
    exe, cfg, secrets = sys.argv[1:4]
    if not AX.AXIsProcessTrusted():
        reply({"ready": False, "error": "no Accessibility permission for this process"})
        return
    env = dict(os.environ, LEGCON_CONFIG_DIR=cfg, LEGCON_TEST_SECRETS=secrets)
    # LEGCON_TEST_PICKS, when the runner sets it, passes through to the shell.
    PROC = subprocess.Popen([exe], env=env)
    APP = AX.AXUIElementCreateApplication(PROC.pid)
    AX.AXUIElementSetMessagingTimeout(APP, 2.0)
    for _ in range(150):
        time.sleep(0.1)
        if any(is_main(w) for w in windows()):
            break
    else:
        PROC.kill()
        reply({"ready": False, "error": "the window did not open"})
        return
    time.sleep(0.5)
    reply({"ready": True})
    for line in sys.stdin:
        o = json.loads(line)
        if o["op"] == "quit":
            return quit_shell()
        try:
            OPS[o["op"]](o)
            reply({"ok": True})
        except (Fail, KeyError) as e:
            reply({"ok": False, "error": str(e)})
    PROC.kill()


if __name__ == "__main__":
    main()
