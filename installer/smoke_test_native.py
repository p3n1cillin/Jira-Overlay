"""End-to-end smoke test of the native (Rust) installer. CI runners only.

    python installer/smoke_test_native.py <native Setup.exe> <published Python Setup.exe>

1. Installs the published Python build with Windows startup enabled and
   seeds fake personal data.
2. Upgrades it in place with the native installer: personal data must be
   byte-identical, the private Python runtime and scripts retired, the
   version marker present, startup preserved and pointed at JiraOverlay.exe,
   and still exactly one uninstall entry.
3. Launches the native build: the security hold keeps it closed; without the
   hold the launcher starts the guardian, which starts the overlay and the
   learner; setup's stop event then closes everything without force.
4. Uninstalls (program files go, personal data and no Run value remain), then
   checks a fresh native install leaves startup off.

Refuses to run where Jira Overlay is already installed: it would take over
that installation's registration.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import winreg

PROJECT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT))
from source_layout import activate  # noqa: E402

activate(PROJECT)
import app_updates as updates  # noqa: E402

APP_ID = "{70CD5DF5-2577-42CC-AD73-24419C907568}_is1"
UNINSTALL = r"Software\Microsoft\Windows\CurrentVersion\Uninstall"
RUN = r"Software\Microsoft\Windows\CurrentVersion\Run"
ROOT = Path(os.environ["LOCALAPPDATA"]) / "Programs" / "JiraOverlay"
PERSONAL = {
    "jira_config.json": json.dumps({"domain": "example.invalid", "email": "smoke@example.invalid",
                                    "token": "FAKE-smoke-token", "triageStatuses": ["With DEV"]}).encode(),
    "triage_state.json": b'{"model": {}, "seenTransitions": []}',
    "triage_history.jsonl": b'{"key": "SD-1", "dest": "With DEV", "summary": "fake"}\n',
    "TRIAGE_REPORT.md": b"# fake report\n",
}


def check(name, condition, detail=""):
    print(("PASS  " if condition else "FAIL  ") + name + (f"   {detail}" if detail else ""), flush=True)
    if not condition:
        raise SystemExit(1)


def run_setup(installer, *extra):
    log = Path(os.environ.get("RUNNER_TEMP", os.environ["TEMP"])) / f"setup-{time.time_ns()}.log"
    args = [str(installer), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/NOLAUNCH", f"/LOG={log}", *extra]
    result = subprocess.run(args, timeout=600)
    if result.returncode != 0:
        print(log.read_text(encoding="utf-8", errors="replace")[-6000:])
    return result.returncode


def run_value():
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN) as key:
            return winreg.QueryValueEx(key, "JiraOverlay")[0]
    except FileNotFoundError:
        return None


def uninstall_entries():
    found = []
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, UNINSTALL) as key:
        for i in range(winreg.QueryInfoKey(key)[0]):
            name = winreg.EnumKey(key, i)
            if name == APP_ID:
                with winreg.OpenKey(key, name) as entry:
                    found.append(winreg.QueryValueEx(entry, "InstallLocation")[0])
    return found


def digests():
    return {n: hashlib.sha256((ROOT / n).read_bytes()).hexdigest() for n in PERSONAL}


def wait_for(predicate, timeout, what):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.5)
    raise SystemExit(f"FAIL  timed out waiting for {what}")


def record(role):
    try:
        return json.loads((ROOT / ".app-lifecycle" / f"{role}.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def uninstall():
    subprocess.run([str(ROOT / "unins000.exe"), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"], timeout=600)
    wait_for(lambda: not (ROOT / "JiraOverlay.exe").exists() and not (ROOT / "unins000.exe").exists(), 180,
             "uninstall to remove program files")


def main(native, python):
    check("no existing installation would be disturbed", not uninstall_entries() and not ROOT.exists())

    # 1. Published Python build with startup enabled, plus personal data.
    check("published Python build installs", run_setup(python, "/TASKS=startup") == 0)
    check("Python build registered its startup entry", "launch_installed.pyw" in (run_value() or ""), run_value())
    for name, data in PERSONAL.items():
        (ROOT / name).write_bytes(data)
    before = digests()

    # 2. In-place upgrade to the native build.
    check("native installer upgrades in place", run_setup(native) == 0)
    check("personal data is byte-identical after the upgrade", digests() == before)
    check("native executable installed", (ROOT / "JiraOverlay.exe").is_file())
    check("private Python runtime retired", not (ROOT / "runtime").exists())
    check("Python scripts retired", not list(ROOT.glob("*.pyw")))
    marker = (ROOT / "app_release.py").read_text(encoding="utf-8")
    check("version marker kept for older installers' downgrade check", 'VERSION = "' in marker)
    check("startup preference preserved and pointed at the exe",
          run_value() == f'"{ROOT / "JiraOverlay.exe"}"', run_value())
    entries = uninstall_entries()
    check("exactly one uninstall entry at the same location",
          len(entries) == 1 and Path(entries[0]).resolve() == ROOT.resolve(), str(entries))

    # 3. Launch behaviour.
    exe = ROOT / "JiraOverlay.exe"
    hold = ROOT / ".jira-overlay-security-hold"
    hold.write_text("smoke test hold")
    held = subprocess.run([str(exe)], timeout=60)
    check("security hold keeps the launcher closed", held.returncode == 1 and not updates.apps_running())
    hold.unlink()
    subprocess.run([str(exe)], timeout=60)
    wait_for(lambda: record("overlay").get("phase") == "running" and record("learner").get("phase") == "running",
             90, "the guardian to start the overlay and learner")
    check("launcher started guardian, overlay and learner",
          all(updates.component_running(m) for m in updates.MUTEXES))
    with updates.stop_apps(ROOT, timeout=120):
        pass
    check("setup's stop event closes every component without force",
          not updates.apps_running() and record("overlay").get("reason") == "update"
          and record("learner").get("reason") == "update", json.dumps([record("overlay"), record("learner")]))
    check("an update stop keeps the components wanted", record("overlay").get("wanted", True) is True)

    # 4. Uninstall, then a fresh install.
    uninstall()
    check("uninstall keeps personal data", (ROOT / "triage_state.json").is_file())
    check("uninstall removes the startup entry", run_value() is None, run_value())
    for name in [*PERSONAL, "jira_config.json.bak"]:
        (ROOT / name).unlink(missing_ok=True)
    check("fresh native install succeeds", run_setup(native) == 0)
    check("fresh install leaves Windows startup off", run_value() is None, run_value())
    uninstall()
    print("RESULT: ALL PASS")


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
