"""Installer privacy, verified legacy migration and owned-startup cleanup.

Runs against isolated files/registry values; never stops the user's live apps.
The actual EXE install/upgrade/uninstall is also exercised in Windows CI.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid
from unittest.mock import patch

from _harness import Checker, PROJECT as PROJECT_DIR
import app_updates as u
from app_release import VERSION, REPOSITORY

check = Checker("Windows installer")
PROJECT = Path(PROJECT_DIR)
support = PROJECT / "installer" / "support.ps1"


def ps(code):
    executable = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    return subprocess.run([str(executable), "-NoProfile", "-NonInteractive", "-Command",
        ". '" + str(support).replace("'", "''") + "'; " + code],
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=30)


with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    (root / "runtime").mkdir()
    (root / "runtime/pythonw.exe").write_bytes(b"test")
    check("a source ZIP cannot be mistaken for a native install", not u.native_install(root))
    (root / "unins000.exe").write_bytes(b"test")
    check("native installs select complete-runtime upgrades", u.native_install(root))
    body = b"installer-test-not-an-executable"
    asset_name = f"JiraOverlay-{VERSION}-Setup.exe"
    base = f"https://github.com/{REPOSITORY}/releases/download/v{VERSION}/"
    metadata = {"tag_name": "v" + VERSION, "assets": [
        {"name": f"JiraOverlay-{VERSION}.zip", "digest": "sha256:" + "1"*64,
         "browser_download_url": base + f"JiraOverlay-{VERSION}.zip"},
        {"name": asset_name, "digest": "sha256:" + hashlib.sha256(body).hexdigest(),
         "browser_download_url": base + asset_name}]}
    with patch.object(u, "_fetch", return_value=json.dumps(metadata).encode()):
        info = u.latest_release(current="2020.01.01")
        check("installer URL is bound to the advertised repository/version", info["installer"]["url"] == base + asset_name)
    with patch.object(u, "_fetch", return_value=body):
        staged = u.stage_installer(info)
        check("native setup is authenticated before execution", staged.read_bytes() == body)
        staged.unlink()
        staged.parent.rmdir()
        try:
            u.stage_installer(dict(info, installer=dict(info["installer"], sha256="0"*64)))
            rejected = False
        except u.UpdateError:
            rejected = True
        check("tampered installers are never staged for execution", rejected)

    if os.name == "nt":
        # Real Windows IPC, but with unique mutex names: NEVER signal/stop the
        # developer's running overlay or learner. Children exit cooperatively.
        for mode in ("legacy", "cooperative", "slow"):
            ipc_root = root / mode
            ipc_root.mkdir()
            mutex_name = "Local\\JiraInstallerTest-" + uuid.uuid4().hex
            event_name = u._event_name(ipc_root, "stop")
            child_code = r'''
import ctypes, sys
k = ctypes.windll.kernel32
k.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_bool, ctypes.c_wchar_p]
k.CreateMutexW.restype = ctypes.c_void_p
k.CreateEventW.argtypes = [ctypes.c_void_p, ctypes.c_bool, ctypes.c_bool, ctypes.c_wchar_p]
k.CreateEventW.restype = ctypes.c_void_p
k.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
k.CloseHandle.argtypes = [ctypes.c_void_p]
mutex = k.CreateMutexW(None, False, sys.argv[1])
assert mutex
event = k.CreateEventW(None, True, False, sys.argv[2]) if sys.argv[3] != 'legacy' else None
assert sys.argv[3] == 'legacy' or event
print('READY', flush=True)
try:
    if sys.argv[3] == 'cooperative':
        assert k.WaitForSingleObject(event, 20000) == 0
    else:
        sys.stdin.readline()
finally:
    if event: k.CloseHandle(event)
    k.CloseHandle(mutex)
'''
            child = subprocess.Popen([sys.executable, "-c", child_code, mutex_name, event_name, mode],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                assert child.stdout.readline().strip() == "READY"
                started = time.monotonic()
                result = ps(f"$TimeoutSeconds = 1; Stop-Companions @('{ipc_root}') @('{mutex_name}')")
                elapsed = time.monotonic() - started
                if mode == "cooperative":
                    check("real shutdown event closes a supported child", result.returncode == 0)
                    child.wait(timeout=5)
                else:
                    check(mode + " shutdown returns a clear, bounded retry message",
                        result.returncode != 0 and elapsed < 10 and "BOTH Jira tray icons" in result.stderr)
                    check(mode + " running process is never force-killed", child.poll() is None)
                check(mode + " preflight does not create personal data", not list(ipc_root.iterdir()))
            finally:
                child.communicate("\n", timeout=25)
        absent = "Local\\JiraInstallerAbsent-" + uuid.uuid4().hex
        result = ps(f"Stop-Companions @('{root}') @('{absent}')")
        check("fresh install without running companions needs no shutdown event", result.returncode == 0)
        check("new supported builds need no legacy Windows bridge",
            "Add-Type" not in support.read_text().split("function Stop-Companions", 1)[1])
        fixture = (PROJECT / "tests/fixtures/legacy_shutdown_app.py").read_bytes()
        for script in ("jira_overlay.pyw", "triage_learner.pyw"):
            old_root = root / ("legacy-" + script)
            old_root.mkdir()
            for name in ("jira_overlay.pyw", "triage_learner.pyw"):
                (old_root / name).write_bytes(fixture)
            (old_root / "jira_config.json").write_bytes(b'{"test":"not-a-real-credential"}')
            mutex = "Local\\JiraInstallerTest-" + uuid.uuid4().hex
            env = dict(os.environ, JIRA_TEST_MUTEX=mutex)
            child = subprocess.Popen([sys.executable, str(old_root / script)], env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                assert child.stdout.readline().strip() == "READY"
                result = ps(f"$TimeoutSeconds=5; Stop-Companions @('{old_root}') @('{mutex}')")
                if result.returncode:
                    print(result.stdout, result.stderr)
                check(script + " legacy windows quit automatically without killing Python", result.returncode == 0)
                child.wait(timeout=10)
                check(script + " reaches normal loop cleanup", (old_root / "closed.txt").exists())
                check(script + " retains existing config", (old_root / "jira_config.json").read_bytes() == b'{"test":"not-a-real-credential"}')
            finally:
                child.communicate("\n", timeout=20)
        old, new = root / "older build", root / "new build"
        old.mkdir()
        (old / "jira_config.json").write_bytes(b'{"tenant":"arbitrary","token":"fake"}')
        (old / "triage_state.json").write_bytes(b'{"training":"fake"}')
        (old / "triage_history.jsonl").write_bytes(b'{"ticket":"fake"}\n')
        (old / "do_not_import.py").write_text("raise Exception('old code')")
        (old / "unrelated.txt").write_text("not app data")
        p = ps(f"Migrate-Data '{old}' '{new}'")
        if p.returncode:
            print(p.stdout, p.stderr)
        check("legacy migration succeeds without Python/dependency setup", p.returncode == 0)
        check("credentials and learner data are copied byte-for-byte", all(
            (new / name).read_bytes() == (old / name).read_bytes()
            for name in ("jira_config.json", "triage_state.json", "triage_history.jsonl")))
        check("migration imports neither executable code nor arbitrary private files",
              not (new / "do_not_import.py").exists() and not (new / "unrelated.txt").exists())
        check("the old installation remains recoverable", (old / "jira_config.json").exists())
        before = (new / "jira_config.json").read_bytes()
        p = ps(f"Migrate-Data '{old}' '{new}'")
        check("two learners are never silently merged", p.returncode != 0 and (new / "jira_config.json").read_bytes() == before)
        p = ps(f"Safe-Tree '{root}'; Canonical '{old}'")
        check("ordinary paths containing spaces are supported", p.returncode == 0)
        # Mock runKey in a subprocess; no real Windows startup entries touched.
        p = ps(f"""
        $runKey = 'HKCU:\\Software\\JiraOverlay-Installer-Test-' + [Guid]::NewGuid().ToString('N')
        New-Item -Path $runKey -Force | Out-Null
        try {{
          New-ItemProperty -Path $runKey -Name JiraOverlay -Value '\"pythonw\" \"{old}\\jira_overlay.pyw\"' | Out-Null
          New-ItemProperty -Path $runKey -Name JiraTriageLearner -Value '\"pythonw\" \"C:\\Elsewhere\\triage_learner.pyw\"' | Out-Null
          Remove-OwnedStartup @('{old}')
          $v = Get-ItemProperty -Path $runKey
          if ($v.PSObject.Properties['JiraOverlay']) {{ throw 'Owned startup was not removed' }}
          if (-not $v.PSObject.Properties['JiraTriageLearner']) {{ throw 'Unrelated startup was removed' }}
        }} finally {{ Remove-Item -LiteralPath $runKey }}
        """)
        check("uninstall removes only startup entries belonging to its own folder", p.returncode == 0)
        invalid = ps("$Action='Prepare'; $Target=[Environment]::GetFolderPath('UserProfile'); Invoke-Lifecycle")
        check("validation failures include a stable reason code", invalid.returncode != 0 and "JIRA-I101" in invalid.stderr)

check("installer source has no wildcard data deletion",
      "[UninstallDelete]" not in (PROJECT / "installer/JiraOverlay.iss").read_text().split("[Code]")[0])
raise SystemExit(check.done())
