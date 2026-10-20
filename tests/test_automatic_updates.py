"""Durable retry policy and external installer supervisor, fully isolated."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

from _harness import Checker
import app_updates as updates
import native_update

check = Checker("automatic updates")

with tempfile.TemporaryDirectory(prefix="jira-auto-tests-") as directory:
    base = Path(directory)
    root = base / "App with spaces"
    root.mkdir()
    stage = base / "Staged installer"
    stage.mkdir()
    installer = stage / "Setup.exe"
    installer.write_bytes(b"Never execute: isolated test data")
    digest = hashlib.sha256(installer.read_bytes()).hexdigest()
    release = {"version": "2099.01.01", "installer": {"sha256": digest}}
    check("new installations automatically accept a newer published release", updates.automatic_update_due(root, release["version"]))
    state = updates.record_update(root, release["version"], "downloading")
    check("attempts survive application restarts", updates.update_state(root)["phase"] == "downloading")
    check("failed or interrupted attempts do not loop every launch", not updates.automatic_update_due(root, release["version"]))
    check("automatic retries resume after six hours", updates.automatic_update_due(root, release["version"], now=state["attemptedAt"] + updates.AUTO_RETRY_SECONDS + 1))
    check("a new release is not blocked by an older failed attempt", updates.automatic_update_due(root, "2099.01.02"))
    check("clock rollback does not indefinitely disable updates", updates.automatic_update_due(root, release["version"], now=state["attemptedAt"] - 1))
    (root / ".git").write_text("gitdir: test")
    check("Git worktrees never update themselves", not updates.automatic_update_due(root, release["version"]))
    (root / ".git").unlink()
    (root / updates.UPDATE_STATE).write_text("not json")
    check("a damaged diagnostic file is non-fatal", updates.update_state(root) == {})
    (root / "runtime").mkdir()
    (root / "runtime/pythonw.exe").write_bytes(b"not executable")
    (root / "unins000.exe").write_bytes(b"not executable")
    launched = []
    with patch.object(native_update.subprocess, "Popen", side_effect=lambda args, **kw: launched.append(args) or SimpleNamespace(pid=1)):
        native_update.launch(root, installer, release)
    check("the native supervisor does not run from the runtime being replaced",
        len(launched) == 1 and "WindowsPowerShell" in launched[0][0] and str(root / "runtime") not in launched[0][0])
    check("monitor script lives outside the installed application", (stage / "monitor.ps1").is_file())
    check("unattended setup suppresses prompts, preserves startup and never reboots",
        all(flag in native_update.MONITOR_SCRIPT for flag in ("/SUPPRESSMSGBOXES", "/NORESTART", "/NOMIGRATE", "/NOLAUNCH"))
        and "/NOSTARTUP" not in native_update.MONITOR_SCRIPT)
    installer.write_bytes(b"changed")
    try:
        native_update.launch(root, installer, release)
        rejected = False
    except updates.UpdateError:
        rejected = True
    check("installer tampering is rejected before launching PowerShell", rejected)

    if os.name == "nt":
        # Exercise the actual PowerShell supervisor with process launch mocked
        # inside its isolated interpreter. No EXE or app is executed here.
        executable = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        monitor = stage / "monitor.ps1"
        installer.write_bytes(b"Never execute: isolated test data")
        for mode in ("success", "failure", "wrong-version", "tampered"):
            scenario = base / mode
            scenario.mkdir()
            (scenario / "app_release.py").write_text('VERSION = "' + ("2020.01.01" if mode == "wrong-version" else release["version"]) + '"\n')
            (scenario / "jira_config.json").write_bytes(b'{"token":"fake-private"}')
            # Mock only Start-Process. All state, hashing, mutexes, error-code
            # propagation and relaunch decisions execute their production code.
            status = 7 if mode == "failure" else 0
            mock = f"""
function Start-Process {{
 param($FilePath,$ArgumentList,[switch]$PassThru,[switch]$Wait,$WindowStyle,$WorkingDirectory)
 if ($FilePath -like '*Setup.exe') {{ return [PSCustomObject]@{{ExitCode={status}}} }}
 [IO.File]::WriteAllText((Join-Path $WorkingDirectory 'relaunched.txt'), 'yes')
}}
& '{monitor}' -Installer '{installer}' -Target '{scenario}' -Version '{release['version']}' -Digest '{'0'*64 if mode == 'tampered' else digest}'
"""
            result = subprocess.run([str(executable), "-NoProfile", "-NonInteractive", "-Command", mock],
                capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=30)
            state = updates.update_state(scenario)
            if state.get("phase") != ("completed" if mode == "success" else "failed") or (mode != "tampered" and not (scenario / "relaunched.txt").exists()):
                print("Supervisor diagnostics:", mode, result.returncode, state, result.stdout, result.stderr)
            check(mode + " supervisor result is durable", state.get("phase") == ("completed" if mode == "success" else "failed"))
            check(mode + " preserves private configuration", (scenario / "jira_config.json").read_bytes() == b'{"token":"fake-private"}')
            check(mode + " relaunch policy is safe", (scenario / "relaunched.txt").exists() == (mode != "tampered"))
            if mode != "success":
                check(mode + " returns a diagnostic code and failure status", result.returncode != 0 and "JIRA-U20" in state.get("detail", ""))

check("diagnostics contain no Jira account fields", "token" not in json.dumps(updates.update_state(root)))
raise SystemExit(check.done())
