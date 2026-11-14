"""Run the UI flows (FLOWS.md) against a shell: `python regression-tests/run_flows.py [--shell win32] [name ...]`.

Each flow starts the shell with a scratch config, drives it by controller ids through the shell's
driver, then checks what it saved, through `lc` and the machines file, never the screen alone.
"""

import argparse
import glob
import json
import os
import subprocess
import sys
import tempfile
import uuid

import tomli

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

SHELLS = {
    "win32": {
        "driver": [sys.executable, os.path.join(HERE, "drivers", "uia_driver.py")],
        "exe": os.path.join(ROOT, "frontends", "win32", "target", "debug", "legcon.exe"),
    },
}
LC = os.path.join(ROOT, "target", "debug", "lc.exe" if os.name == "nt" else "lc")
# The lab container's machines (`lab/lab.sh machines`), for cases with machines = "lab".
LAB_MACHINES = os.path.join(HERE, "lab-machines.json")


def lookup(obj, dotted):
    for part in dotted.split("."):
        obj = obj[int(part)] if isinstance(obj, list) else obj.get(part)
        if obj is None:
            return None
    return obj


def fill(value, work, run):
    """Replace {work} (the flow's scratch folder) and {run} (unique per run) everywhere in a case."""
    if isinstance(value, str):
        return value.replace("{work}", work).replace("{run}", run)
    if isinstance(value, list):
        return [fill(v, work, run) for v in value]
    if isinstance(value, dict):
        return {fill(k, work, run): fill(v, work, run) for k, v in value.items()}
    return value


def run_lc(step, env):
    p = subprocess.run([LC] + step["args"], env=env, capture_output=True, text=True)
    if p.returncode != step.get("exit", 0):
        return f"lc {step['args']} exited {p.returncode}: {p.stderr.strip()}"
    for text in step.get("stdout_has", []):
        if text not in p.stdout:
            return f"lc {step['args']} did not print {text!r}:\n{p.stdout}"
    return None


def run_case(case, shell, work):
    case = fill(case, work, uuid.uuid4().hex[:8])
    cfg = os.path.join(work, "config")
    os.makedirs(cfg)
    secrets = os.path.join(work, "secrets.json")
    machines = case.get("machines", [])
    if machines == "lab":
        machines = json.load(open(LAB_MACHINES))["machines"]
    with open(os.path.join(cfg, "machines.json"), "w") as f:
        json.dump({"machines": machines}, f)
    for name, text in case.get("local_files", {}).items():
        with open(os.path.join(work, name), "w") as f:
            f.write(text)
    with open(os.path.join(cfg, "settings.json"), "w") as f:
        json.dump({"home_folder": work}, f)
    with open(secrets, "w") as f:
        json.dump(case.get("secrets", {}), f)
    picks = os.path.join(work, "picks.txt")
    with open(picks, "w") as f:
        f.writelines(f"{k}\t{v}\n" for k, v in case.get("picks", {}).items())
    env = dict(os.environ, LEGCON_CONFIG_DIR=cfg, LEGCON_TEST_SECRETS=secrets, LEGCON_TEST_PICKS=picks)
    for step in case.get("setup_lc", []):
        why = run_lc(step, env)
        if why:
            return f"setup: {why}"

    servers = []
    for srv in case.get("servers", []):
        exe = os.path.join(ROOT, "target", "debug", "examples", srv["example"] + (".exe" if os.name == "nt" else ""))
        servers.append(subprocess.Popen([exe] + srv.get("args", []), stdout=subprocess.PIPE, text=True))
        servers[-1].stdout.readline()
    try:
        return drive(case, shell, cfg, secrets, env, work)
    finally:
        for p in servers:
            p.kill()


def drive(case, shell, cfg, secrets, env, work):
    s = SHELLS[shell]
    d = subprocess.Popen(s["driver"] + [s["exe"], cfg, secrets], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
    hello = json.loads(d.stdout.readline() or '{"ready": false}')
    if not hello.get("ready"):
        return f"the shell did not start: {hello.get('error')}"
    failure = None
    for i, op in enumerate(case.get("ui", []) + [{"op": "quit"}]):
        d.stdin.write(json.dumps(op) + "\n")
        d.stdin.flush()
        r = json.loads(d.stdout.readline() or '{"ok": false, "error": "the driver stopped"}')
        if not r.get("ok"):
            failure = f"ui step {i + 1} {op}: {r.get('error')}"
            if op["op"] != "quit":
                d.stdin.write(json.dumps({"op": "quit"}) + "\n")
                d.stdin.flush()
                d.stdout.readline()
            break
    d.wait(30)
    if failure:
        return failure

    machines = json.load(open(os.path.join(cfg, "machines.json")))["machines"]
    for path, want in case.get("expect_machines", {}).items():
        got = lookup(machines, path)
        if want == "__any__" and got is not None:
            continue
        if got != want:
            return f"machines {path} is {got!r}, expected {want!r}"
    for rel in case.get("expect_files", []):
        if not os.path.exists(os.path.join(work, rel)):
            return f"no file {rel} in the home folder"
    stored = json.load(open(secrets))
    for key, want in case.get("expect_secrets", {}).items():
        if stored.get(key, "__absent__") != want:
            return f"secret {key} is {stored.get(key)!r}, expected {want!r}"
    for step in case.get("lc", []):
        why = run_lc(step, env)
        if why:
            return why
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shell", default="win32", choices=sorted(SHELLS))
    ap.add_argument("--lab", action="store_true", help="also run lab = true cases (needs lab/lab.sh up)")
    ap.add_argument("names", nargs="*", help="flow ids or files to run (all when none)")
    a = ap.parse_args()
    cases = []
    for path in sorted(glob.glob(os.path.join(HERE, "flows", "*.toml"))):
        for c in tomli.load(open(path, "rb"))["case"]:
            if c.get("lab") and not a.lab:
                continue
            if a.shell in c.get("shells", ["win32"]):
                if not a.names or c["id"] in a.names or os.path.basename(path) in a.names:
                    cases.append(c)
    failed = 0
    for c in cases:
        with tempfile.TemporaryDirectory(prefix="lcflow-") as work:
            why = run_case(c, a.shell, work)
        print(f"{'ok  ' if why is None else 'FAIL'} {c['id']}.{a.shell}" + ("" if why is None else f": {why}"))
        failed += why is not None
    print(f"{len(cases) - failed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
