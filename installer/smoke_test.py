"""Actual EXE fresh/repair/upgrade/uninstall/reinstall test on a CLEAN CI user.

Never run against a developer's desktop: the companions must not be running,
and no native installation may already exist. No Jira requests are made.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import winreg


APP_ID = "{70CD5DF5-2577-42CC-AD73-24419C907568}_is1"


@contextlib.contextmanager
def running_legacy():
    """CI-only old app simulator: owns the real mutex, has NO stop listener."""
    code = r'''
import ctypes, sys
k = ctypes.windll.kernel32
k.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_bool, ctypes.c_wchar_p]
k.CreateMutexW.restype = ctypes.c_void_p
k.CloseHandle.argtypes = [ctypes.c_void_p]
handle = k.CreateMutexW(None, False, 'JiraOverlaySingleInstance')
assert handle
print('READY', flush=True)
try: sys.stdin.readline()
finally: k.CloseHandle(handle)
'''
    child = subprocess.Popen([sys.executable, "-c", code], stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().strip() == "READY"
        yield child
    finally:
        child.communicate("\n", timeout=10)


def check_blocked(command, log, child):
    result = subprocess.run([str(x) for x in command] + ["/LOG=" + str(log)], timeout=45)
    text = log.read_text(encoding="utf-8-sig", errors="replace")
    assert result.returncode != 0, "A running legacy writer must block code replacement"
    assert "BOTH Jira tray icons" in text, text[-10000:]
    assert "JIRA-I111" in text, text[-10000:]
    assert child.poll() is None, "Installer force-killed a running legacy writer"


def execute(command):
    result = subprocess.run([str(x) for x in command], timeout=600)
    if result.returncode:
        for argument in command:
            if str(argument).startswith("/LOG="):
                log = Path(str(argument)[5:])
                if log.is_file():
                    print(log.read_text(encoding="utf-8-sig", errors="replace")[-14000:], flush=True)
        raise RuntimeError(f"Installer command failed ({result.returncode}): {command}")


def check_install(root):
    execute([root / "runtime/python.exe", "-B", "-c",
        "import app_release, requests, PIL, pystray, tzdata, tkinter; "
        "from zoneinfo import ZoneInfo; ZoneInfo('Europe/London'); "
        "r=tkinter.Tk(); r.withdraw(); r.update(); r.destroy(); print(app_release.VERSION)"])
    # Import the real entry points under the PRIVATE interpreter. Do not call
    # main(), construct a watcher or contact Jira, even with migration fixtures.
    execute([root / "runtime/python.exe", "-B", "-c",
        "import importlib.util,sys,pathlib;root=pathlib.Path(sys.argv[1]);"
        "\nfor name in ('jira_overlay','triage_learner'):"
        "\n spec=importlib.util.spec_from_file_location(name,root/(name+'.pyw'));"
        "module=importlib.util.module_from_spec(spec);sys.modules[name]=module;spec.loader.exec_module(module)", root])
    assert (root / "unins000.exe").is_file(), "Missing uninstaller"
    assert (root / "installer_support.ps1").is_file()


def main(installer):
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\" + APP_ID):
            raise RuntimeError("Refusing smoke test: this user already has a native installation")
    except FileNotFoundError:
        pass
    import ctypes
    # No installer test may stop a user's live learner or edit their startup.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from source_layout import activate
    activate(Path(__file__).resolve().parents[1])
    import app_updates
    import app_startup
    if app_updates.apps_running():
        raise RuntimeError("Refusing smoke test: real companions are running")
    assert ctypes.sizeof(ctypes.c_void_p) == 8
    with tempfile.TemporaryDirectory(prefix="jira-exe-smoke-") as directory:
        base = Path(directory)
        root = base / "Application with spaces"
        old = base / "Old ZIP"
        old.mkdir()
        for script in ("jira_overlay.pyw", "triage_learner.pyw"):
            (old / script).write_text("# old code must NOT be imported")
        personal = {"jira_config.json": b'{"serviceDeskId":"arbitrary-test","apiToken":"FAKE-ONLY"}',
                    "triage_state.json": b'{"example":"private-model"}',
                    "triage_history.jsonl": b'{"decision":"example"}\n'}
        for name, data in personal.items():
            (old / name).write_bytes(data)
        args = [installer, "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART",
                "/NOSTARTUP", "/NOSHORTCUTS", "/NOLAUNCH", "/DIR=" + str(root)]
        with running_legacy() as child:
            check_blocked(args + ["/NOMIGRATE"], base / "blocked-fresh.log", child)
            check_blocked(args + ["/LEGACYDIR=" + str(old)], base / "blocked-migration.log", child)
            assert not (root / "jira_overlay.pyw").exists()
            assert not (root / "jira_config.json").exists()
            for name, data in personal.items():
                assert (old / name).read_bytes() == data
        execute(args + ["/NOMIGRATE", "/LOG=" + str(base / "fresh.log")])
        check_install(root)
        assert not (root / "jira_config.json").exists(), "Installer must not ship credentials"
        # Actual EXE asks an old tray-based learner to quit before replacing
        # its script/private interpreter. This is the user's reported scenario.
        fixture = (Path(__file__).resolve().parents[1] / "tests/fixtures/legacy_shutdown_app.py").read_bytes()
        (root / "triage_learner.pyw").write_bytes(fixture)
        child = subprocess.Popen([str(root / "runtime/python.exe"), str(root / "triage_learner.pyw")],
            env=dict(os.environ, JIRA_TEST_MUTEX="JiraTriageLearner"), stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            assert child.stdout.readline().strip() == "READY"
            execute(args + ["/NOMIGRATE", "/LOG=" + str(base / "automatic-quit.log")])
            child.wait(timeout=10)
            assert (root / "closed.txt").read_text() == "normal loop exit"
            assert (root / "triage_learner.pyw").read_bytes() != fixture
        finally:
            child.communicate("\n", timeout=20)
        execute([root / "unins000.exe", "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        # Legacy migration, with deterministic explicit selection in silent mode.
        execute(args + ["/LEGACYDIR=" + str(old), "/LOG=" + str(base / "install.log")])
        check_install(root)
        for name, data in personal.items():
            assert (root / name).read_bytes() == data
            assert (old / name).read_bytes() == data
        # Simulate older code and a missing managed file, but keep personal data.
        (root / "app_release.py").write_text('VERSION = "2020.01.01"\n')
        (root / "triage_review.py").unlink()
        with running_legacy() as child:
            check_blocked(args + ["/NOMIGRATE"], base / "blocked-upgrade.log", child)
            assert (root / "app_release.py").read_text() == 'VERSION = "2020.01.01"\n'
            assert not (root / "triage_review.py").exists()
            for name, data in personal.items():
                assert (root / name).read_bytes() == data
        # Upgrade without the override: an explicitly disabled startup choice
        # must remain disabled even if the previous installer task was checked.
        execute([arg for arg in args if arg != "/NOSTARTUP"] + ["/NOMIGRATE", "/LOG=" + str(base / "upgrade.log")])
        check_install(root)
        assert (root / "triage_review.py").is_file()
        with winreg.CreateKey(winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Run") as key:
            for name in ("JiraOverlay", "JiraTriageLearner"):
                try:
                    value = winreg.QueryValueEx(key, name)[0]
                except FileNotFoundError:
                    continue
                paths = app_startup.command_paths(value)
                assert paths is None or paths[1].parent != root.resolve(), "Upgrade re-enabled disabled Windows startup"
        for name, data in personal.items():
            assert hashlib.sha256((root / name).read_bytes()).digest() == hashlib.sha256(data).digest()
        # One registered installation, not duplicate entries per version.
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\" + APP_ID) as key:
            assert Path(winreg.QueryValueEx(key, "InstallLocation")[0]).resolve() == root.resolve()
        execute([root / "unins000.exe", "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        assert not (root / "jira_overlay.pyw").exists()
        assert not (root / "runtime/python.exe").exists()
        for name, data in personal.items():
            assert (root / name).read_bytes() == data, "Uninstall removed personal data"
        # Retained data is picked up when reinstalling the very same EXE.
        execute([arg for arg in args if arg != "/NOSTARTUP"] + ["/NOMIGRATE", "/LOG=" + str(base / "reinstall.log")])
        check_install(root)
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Run") as key:
            command = winreg.QueryValueEx(key, "JiraOverlay")[0]
            assert app_startup.command_paths(command) == (
                (root / "runtime/pythonw.exe").resolve(), (root / "jira_overlay.pyw").resolve()), command
            try:
                winreg.QueryValueEx(key, "JiraTriageLearner")
                raise AssertionError("An independent learner startup would race first-run setup")
            except FileNotFoundError:
                pass
        for name, data in personal.items():
            assert (root / name).read_bytes() == data
        execute([root / "unins000.exe", "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Run") as key:
            try:
                paths = app_startup.command_paths(winreg.QueryValueEx(key, "JiraOverlay")[0])
                assert paths is None or paths[1].parent != root.resolve()
            except FileNotFoundError:
                pass
    print("PASS: native fresh/migration, upgrade/repair, uninstall/data retention and reinstall")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("installer", type=Path)
    main(parser.parse_args().installer.resolve())
