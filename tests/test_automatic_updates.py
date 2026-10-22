"""Security regression: a legacy call can never execute downloaded code."""
from unittest.mock import patch
import subprocess
from _harness import Checker
import native_update
import app_updates as updates

check = Checker("notice-only updates")
with patch.object(subprocess, "Popen", side_effect=AssertionError("No process may be launched")):
    try:
        native_update.launch("unused", "unused", {"version": "2099.01.01"})
        refused = False
    except updates.UpdateError as exc:
        refused = "JIRA-U106" in str(exc)
check("legacy automatic-installer callers fail closed without spawning a process", refused)
check("no temporary PowerShell supervisor is generated", not hasattr(native_update,"MONITOR_SCRIPT"))
raise SystemExit(check.done())
