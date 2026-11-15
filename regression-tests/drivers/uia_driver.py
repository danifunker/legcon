"""The Win32 shell's flow driver: UI Automation over comtypes, one JSON op per line (FLOWS.md).

Run by run_flows.py as `python uia_driver.py <legcon.exe> <config dir> <secrets file>`. It starts
the shell, answers {"ready": true}, then answers each op with {"ok": true} or
{"ok": false, "error": ...}. Controls are found by AutomationId, which the shell sets to the
controller's widget id, in the main window and in every dialog the shell has open.
"""

import ctypes
import ctypes.wintypes
import json
import os
import subprocess
import sys
import time

import comtypes
import comtypes.client

comtypes.client.GetModule("UIAutomationCore.dll")
from comtypes.gen import UIAutomationClient as U  # noqa: E402

user32 = ctypes.windll.user32
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
user32.GetParent.restype = ctypes.wintypes.HWND
user32.SendMessageW.restype = ctypes.c_ssize_t
user32.SendMessageW.argtypes = [ctypes.wintypes.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]
user32.PostMessageW.argtypes = [ctypes.wintypes.HWND, ctypes.c_uint, ctypes.c_size_t, ctypes.c_ssize_t]

BM_CLICK = 0x00F5
BM_GETCHECK = 0x00F0
WM_CLOSE = 0x0010
WM_COMMAND = 0x0111
CB_SETCURSEL = 0x014E
CB_FINDSTRINGEXACT = 0x0158
CBN_SELCHANGE = 1

UIA = comtypes.client.CreateObject(U.CUIAutomation, interface=U.IUIAutomation)
PROC = None
MAIN = 0


class Fail(Exception):
    pass


def windows():
    """The shell's visible top-level windows, dialogs first (the newest on top)."""
    found = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.wintypes.HWND, ctypes.wintypes.LPARAM)
    def cb(h, _):
        pid = ctypes.wintypes.DWORD()
        user32.GetWindowThreadProcessId(h, ctypes.byref(pid))
        if pid.value == PROC.pid and user32.IsWindowVisible(h):
            found.append(h)
        return True

    user32.EnumWindows(cb, 0)
    return found


def class_of(h):
    buf = ctypes.create_unicode_buffer(64)
    user32.GetClassNameW(h, buf, 64)
    return buf.value


def dialogs():
    return [h for h in windows() if h != MAIN]


def find(aid, timeout=5.0):
    """The element with AutomationId `aid` in any of the shell's windows."""
    # A property condition misses ids set through IAccPropServices until the tree is walked, so walk it.
    end = time.time() + timeout
    every = UIA.CreateTrueCondition()
    while True:
        for h in windows():
            arr = UIA.ElementFromHandle(h).FindAll(U.TreeScope_Descendants, every)
            for i in range(arr.Length):
                e = arr.GetElement(i)
                if e.CurrentAutomationId == aid:
                    return e
        if time.time() > end:
            raise Fail(f"no control {aid}")
        time.sleep(0.2)


def hwnd_of(e):
    return e.CurrentNativeWindowHandle


def pattern(e, pid, iface):
    p = e.GetCurrentPattern(pid)
    if not p:
        raise Fail(f"{e.CurrentAutomationId} does not support {iface.__name__}")
    return p.QueryInterface(iface)


def op_activate(o):
    """Press a button without waiting: a button may open a modal dialog."""
    e = find(o["id"])
    h = hwnd_of(e)
    if not e.CurrentIsEnabled:
        raise Fail(f"{o['id']} is disabled")
    if h:
        user32.PostMessageW(h, BM_CLICK, 0, 0)
    else:
        pattern(e, U.UIA_InvokePatternId, U.IUIAutomationInvokePattern).Invoke()
    time.sleep(o.get("pause", 0.5))


def op_select_row(o):
    lst = find(o["id"])
    items = lst.FindAll(U.TreeScope_Descendants, UIA.CreatePropertyCondition(U.UIA_ControlTypePropertyId, U.UIA_ListItemControlTypeId))
    for i in range(items.Length):
        it = items.GetElement(i)
        if it.CurrentName == o["name"]:
            pattern(it, U.UIA_SelectionItemPatternId, U.IUIAutomationSelectionItemPattern).Select()
            time.sleep(0.3)
            return
    raise Fail(f"no row {o['name']} in {o['id']}")


def op_fill(o):
    e = find(o["id"])
    pattern(e, U.UIA_ValuePatternId, U.IUIAutomationValuePattern).SetValue(o["text"])
    time.sleep(0.3)


def op_check(o):
    e = find(o["id"])
    h = hwnd_of(e)
    on = user32.SendMessageW(h, BM_GETCHECK, 0, 0) == 1
    if on != bool(o["on"]):
        user32.PostMessageW(h, BM_CLICK, 0, 0)
        time.sleep(0.3)


def op_choose(o):
    """Pick a drop-down entry by its text, telling the dialog as a user's pick would."""
    e = find(o["id"])
    h = hwnd_of(e)
    text = ctypes.create_unicode_buffer(o["option"])
    i = user32.SendMessageW(h, CB_FINDSTRINGEXACT, ctypes.c_size_t(-1).value, ctypes.addressof(text))
    if i < 0:
        raise Fail(f"{o['id']} has no option {o['option']}")
    user32.SendMessageW(h, CB_SETCURSEL, i, 0)
    user32.SendMessageW(user32.GetParent(h), WM_COMMAND, (CBN_SELCHANGE << 16), h)
    time.sleep(0.3)


def op_section(o):
    """Show a form's section (tab) by its title."""
    tabs = find(o["id"])
    items = tabs.FindAll(U.TreeScope_Children, UIA.CreatePropertyCondition(U.UIA_ControlTypePropertyId, U.UIA_TabItemControlTypeId))
    for i in range(items.Length):
        it = items.GetElement(i)
        if it.CurrentName == o["name"]:
            pattern(it, U.UIA_SelectionItemPatternId, U.IUIAutomationSelectionItemPattern).Select()
            time.sleep(0.3)
            return
    raise Fail(f"no section {o['name']}")


def op_expect_enabled(o):
    e = find(o["id"])
    if bool(e.CurrentIsEnabled) != bool(o["enabled"]):
        raise Fail(f"{o['id']} enabled is {bool(e.CurrentIsEnabled)}, expected {o['enabled']}")


def op_expect_value(o):
    e = find(o["id"])
    v = pattern(e, U.UIA_ValuePatternId, U.IUIAutomationValuePattern).CurrentValue or ""
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
    raise Fail(f"a dialog is still open: {[class_of(h) for h in dialogs()]}")


def op_expect_text(o):
    """Some element in any window shows this text (name or value)."""
    end = time.time() + o.get("timeout", 5)
    while time.time() < end:
        for h in windows():
            arr = UIA.ElementFromHandle(h).FindAll(U.TreeScope_Descendants, UIA.CreateTrueCondition())
            for i in range(arr.Length):
                if o["text"] in (arr.GetElement(i).CurrentName or ""):
                    return
        time.sleep(0.3)
    raise Fail(f"no text {o['text']!r} on screen")


VK = {"enter": 0x0D, "escape": 0x1B, "tab": 0x09, "delete": 0x2E, "backspace": 0x08,
      "f2": 0x71, "f4": 0x73, "f5": 0x74, "f6": 0x75, "ctrl": 0x11, "shift": 0x10, "alt": 0x12}


def vk(name):
    name = name.lower()
    if name in VK:
        return VK[name]
    if len(name) == 1:
        return ord(name.upper())
    raise Fail(f"unknown key {name}")


def op_key(o):
    """Focus a control (when `id` is given) and type a key chord such as ctrl+n or f6."""
    if "id" in o:
        e = find(o["id"])
        top = e
        user32.SetForegroundWindow(MAIN if not dialogs() else dialogs()[0])
        e.SetFocus()
        time.sleep(0.2)
    keys = [vk(k) for k in o["key"].split("+")]
    for k in keys:
        user32.keybd_event(k, 0, 0, 0)
    for k in reversed(keys):
        user32.keybd_event(k, 0, 2, 0)
    time.sleep(o.get("pause", 0.6))


def rows_of(aid):
    lst = find(aid)
    items = lst.FindAll(U.TreeScope_Descendants, UIA.CreatePropertyCondition(U.UIA_ControlTypePropertyId, U.UIA_ListItemControlTypeId))
    return [items.GetElement(i).CurrentName for i in range(items.Length)]


def op_expect_row(o):
    """A list has (or lacks) a row named `name`, waiting for a listing to arrive."""
    want = o.get("present", True)
    end = time.time() + o.get("timeout", 5)
    while True:
        names = rows_of(o["id"])
        if (o["name"] in names) == want:
            return
        if time.time() > end:
            raise Fail(f"{o['id']} rows {names}: {o['name']} {'missing' if want else 'still there'}")
        time.sleep(0.3)


def op_press(o):
    """Press a dialog button by its label (task dialogs have no controller ids)."""
    end = time.time() + o.get("timeout", 10)
    c = UIA.CreatePropertyCondition(U.UIA_ControlTypePropertyId, U.UIA_ButtonControlTypeId)
    while time.time() < end:
        for h in dialogs():
            arr = UIA.ElementFromHandle(h).FindAll(U.TreeScope_Descendants, c)
            for i in range(arr.Length):
                b = arr.GetElement(i)
                if (b.CurrentName or "").replace("&", "") == o["name"]:
                    if b.CurrentNativeWindowHandle:
                        user32.PostMessageW(b.CurrentNativeWindowHandle, BM_CLICK, 0, 0)
                    else:
                        pattern(b, U.UIA_InvokePatternId, U.IUIAutomationInvokePattern).Invoke()
                    time.sleep(o.get("pause", 0.6))
                    return
        time.sleep(0.3)
    raise Fail(f"no dialog button {o['name']}")


def op_menu(o):
    """Choose a popup menu item by its text (popup menus have no controller ids)."""
    end = time.time() + o.get("timeout", 5)
    c = UIA.CreatePropertyCondition(U.UIA_ControlTypePropertyId, U.UIA_MenuItemControlTypeId)
    seen = []
    while time.time() < end:
        for h in windows():
            if class_of(h) != "#32768":
                continue
            arr = UIA.ElementFromHandle(h).FindAll(U.TreeScope_Descendants, c)
            seen = [arr.GetElement(i).CurrentName for i in range(arr.Length)]
            for i in range(arr.Length):
                m = arr.GetElement(i)
                if (m.CurrentName or "").replace("&&", "&") == o["name"]:
                    if not m.CurrentIsEnabled:
                        raise Fail(f"menu item {o['name']} is disabled")
                    pattern(m, U.UIA_InvokePatternId, U.IUIAutomationInvokePattern).Invoke()
                    # Wait for the menu to close, so the next step cannot find it again.
                    gone = time.time() + 5
                    while time.time() < gone and any(class_of(w) == "#32768" for w in windows()):
                        time.sleep(0.1)
                    time.sleep(o.get("pause", 0.4))
                    return
        time.sleep(0.2)
    raise Fail(f"no menu item {o['name']} in {seen}")


def window_image(h, Image):
    """One window as the window itself draws it (PrintWindow), covered or not."""
    g = ctypes.windll.gdi32
    r = ctypes.wintypes.RECT()
    user32.GetWindowRect(h, ctypes.byref(r))
    w, ht = r.right - r.left, r.bottom - r.top
    if w <= 0 or ht <= 0:
        return None
    sdc = user32.GetDC(None)
    dc = g.CreateCompatibleDC(sdc)
    bmp = g.CreateCompatibleBitmap(sdc, w, ht)
    g.SelectObject(dc, bmp)
    user32.PrintWindow(h, dc, 2)
    bi = (ctypes.c_uint32 * 10)(40, w, ctypes.c_uint32(-ht).value, 1 | (32 << 16), 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * ht * 4)
    g.GetDIBits(dc, bmp, 0, ht, buf, bi, 0)
    g.DeleteObject(bmp)
    g.DeleteDC(dc)
    user32.ReleaseDC(None, sdc)
    return Image.frombuffer("RGB", (w, ht), buf.raw, "raw", "BGRX", 0, 1)


def op_screenshot(o):
    """The shell's own windows to `path`, for a failed step (never the rest of the screen)."""
    try:
        from PIL import Image
    except ImportError:
        raise Fail("no Pillow for screenshots")
    shots = [i for i in (window_image(h, Image) for h in reversed(windows() or [MAIN])) if i]
    if not shots:
        raise Fail("no window to capture")
    # Main window first, then any dialogs beside it.
    out = Image.new("RGB", (sum(i.width for i in shots), max(i.height for i in shots)), "white")
    x = 0
    for i in shots:
        out.paste(i, (x, 0))
        x += i.width
    out.save(o["path"])


OPS = {
    "screenshot": op_screenshot,
    "menu": op_menu,
    "key": op_key,
    "expect_row": op_expect_row,
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


def main():
    global PROC, MAIN
    exe, cfg, secrets = sys.argv[1:4]
    env = dict(os.environ, LEGCON_CONFIG_DIR=cfg, LEGCON_TEST_SECRETS=secrets)
    # LEGCON_TEST_PICKS, when the runner sets it, passes through to the shell.
    PROC = subprocess.Popen([exe], env=env)
    for _ in range(100):
        time.sleep(0.1)
        MAIN = user32.FindWindowW("LegconMain", None)
        if MAIN:
            break
    if not MAIN:
        reply({"ready": False, "error": "the window did not open"})
        return
    time.sleep(0.8)
    reply({"ready": True})
    for line in sys.stdin:
        o = json.loads(line)
        if o["op"] == "quit":
            user32.PostMessageW(MAIN, WM_CLOSE, 0, 0)
            try:
                code = PROC.wait(15)
                reply({"ok": code == 0, "error": f"exit code {code}"})
            except subprocess.TimeoutExpired:
                left = []
                for h in windows():
                    buf = ctypes.create_unicode_buffer(256)
                    user32.GetWindowTextW(h, buf, 256)
                    left.append(f"{class_of(h)} {buf.value!r}")
                PROC.kill()
                reply({"ok": False, "error": f"the shell did not quit; windows left: {left}"})
            return
        try:
            OPS[o["op"]](o)
            reply({"ok": True})
        except (Fail, KeyError, comtypes.COMError) as e:
            reply({"ok": False, "error": str(e)})
    PROC.kill()


if __name__ == "__main__":
    main()
