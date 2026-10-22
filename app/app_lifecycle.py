"""Per-user lifecycle records and an independent Windows recovery task.

Only component names, PIDs, exit codes and times are logged; never Jira data.
The task is demand-started by the launcher/Windows Run entry, not a logon
trigger: disabling Windows startup therefore remains effective.
"""
from __future__ import annotations

import faulthandler
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from app_release import VERSION
import app_updates as updates

ROLES = {"overlay": "jira_overlay.pyw", "learner": "triage_learner.pyw"}
GUARD_MUTEX = "JiraOverlayGuardian"
_active = None
_reason = "normal-exit"


def folder(root, *, create=True):
    root = Path(root).resolve()
    path = root / ".app-lifecycle"
    if path.is_symlink() or path.resolve().parent != root or (path.exists() and not path.is_dir()):
        raise updates.UpdateError("Unsafe lifecycle directory")
    if create:
        path.mkdir(exist_ok=True)
    return path


def read(root, role):
    if role not in (*ROLES, "guardian", "recovery"):
        raise ValueError("Unknown application component")
    try:
        path = updates._inside_file(folder(root, create=False), role + ".json")
        if path.stat().st_size > 16384:
            return {}
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError, updates.UpdateError):
        return {}


def write(root, role, **fields):
    value = read(root, role)
    value.update(fields)
    value.update(version=VERSION, updatedAt=time.time())
    updates._atomic(updates._inside_file(folder(root), role + ".json"), json.dumps(value).encode())


def event(root, role, action, **fields):
    # Separate files avoid cross-process append races. One bounded backup per
    # file prevents a restart loop growing diagnostic logs indefinitely.
    path = updates._inside_file(folder(root), role + ".jsonl")
    if path.exists() and path.stat().st_size > 1024 * 1024:
        backup = updates._inside_file(folder(root), role + ".jsonl.1")
        os.replace(path, backup)
    value = dict(time=time.time(), version=VERSION, component=role, event=action, pid=os.getpid(), **fields)
    with path.open("ab") as stream:
        stream.write(json.dumps(value).encode() + b"\n")
        stream.flush()


def wanted(root, role):
    return read(root, role).get("wanted", True) is True


def set_wanted(root, role, enabled):
    write(root, role, wanted=bool(enabled))


def reason(value, *, quit=False):
    global _reason
    if _active is None:
        return  # Imported test/UI modules must never write the live profile.
    _reason = value
    root, role = _active
    write(root, role, reason=value, **({"wanted": False} if quit else {}))
    event(root, role, value)


def ready():
    if _active:
        write(*_active, phase="running", reason="running", exitCode=None)


def supervised(root):
    return os.environ.get("JIRA_OVERLAY_SUPERVISED_ROOT", "").casefold() == str(Path(root).resolve()).casefold()


def run(root, role, main):
    global _active, _reason
    root = Path(root).resolve()
    _active, _reason = (root, role), "normal-exit"
    fault = None
    try:
        write(root, role, wanted=True, phase="starting", pid=os.getpid(), startedAt=time.time(), reason="starting", exitCode=None)
        event(root, role, "started")
        path = updates._inside_file(folder(root), role + ".fault.log")
        if path.exists() and path.stat().st_size > 1024 * 1024:
            os.replace(path, updates._inside_file(folder(root), role + ".fault.log.1"))
        fault = path.open("ab")
        faulthandler.enable(file=fault, all_threads=True)
        result = main()
        write(root, role, phase="exited", reason=_reason, exitCode=0)
        event(root, role, "exited", reason=_reason, exitCode=0)
        return result
    except BaseException as exc:
        code = (0 if exc.code is None else exc.code if isinstance(exc.code, int) else 1) if isinstance(exc, SystemExit) else 1
        write(root, role, phase="exited", reason="exception", exitCode=code)
        # Exception strings may contain request data; log the type only.
        event(root, role, "exited", reason="exception", exitCode=code, exceptionType=type(exc).__name__)
        raise
    finally:
        if fault:
            faulthandler.disable()
            fault.close()
        _active = None


def task_name(root):
    digest = hashlib.sha256(str(Path(root).resolve()).casefold().encode()).hexdigest()[:12]
    return "JiraOverlay-" + digest + "-Recovery"


TASK_SCRIPT = r'''
param([string]$Root, [string]$Name, [ValidateSet('register','run','remove')][string]$Mode)
$ErrorActionPreference='Stop'
$service=New-Object -ComObject 'Schedule.Service'
$service.Connect()
$folder=$service.GetFolder('\')
$existing=$null
try { $existing=$folder.GetTask($Name) } catch {
    $errorCode=$_.Exception.HResult
    if ($_.Exception.InnerException) { $errorCode=$_.Exception.InnerException.HResult }
    if ($errorCode -ne -2147024894) { throw }
}
if ($existing) {
    $action=$existing.Definition.Actions.Item(1)
    if ($action.WorkingDirectory.TrimEnd('\') -ne $Root.TrimEnd('\') -or
        $action.Path -ne (Join-Path $Root 'runtime\pythonw.exe') -or
        ($action.Arguments.IndexOf(('"' + (Join-Path $Root 'app_guard.pyw') + '"'),[StringComparison]::OrdinalIgnoreCase) -lt 0 -and
         $action.Arguments -ne ('"' + (Join-Path $Root 'launch_installed.pyw') + '"'))) {
        throw 'Refusing to change an unrelated scheduled task.'
    }
}
if ($Mode -eq 'remove') {
    if ($existing) { $folder.DeleteTask($Name,0) }
    exit 0
}
if ($Mode -eq 'register' -or -not $existing) {
    $task=$service.NewTask(0)
    $task.RegistrationInfo.Description='Jira Overlay per-user recovery. No Jira credentials. Deliberate Quit and update shutdowns are respected.'
    $task.Principal.UserId=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $task.Principal.LogonType=3
    $task.Principal.RunLevel=0
    $task.Settings.Enabled=$true
    $task.Settings.AllowDemandStart=$true
    $task.Settings.StartWhenAvailable=$false
    $task.Settings.DisallowStartIfOnBatteries=$false
    $task.Settings.StopIfGoingOnBatteries=$false
    $task.Settings.ExecutionTimeLimit='PT0S'
    # An updater can still be a descendant of the previous scheduler action.
    # Queue its replacement rather than ignoring it until that job finishes.
    $task.Settings.MultipleInstances=1
    $task.Settings.RestartCount=3
    $task.Settings.RestartInterval='PT1M'
    $action=$task.Actions.Create(0)
    $action.Path=Join-Path $Root 'runtime\pythonw.exe'
    $action.Arguments='-B "' + (Join-Path $Root 'app_guard.pyw') + '"'
    $action.WorkingDirectory=$Root
    $existing=$folder.RegisterTaskDefinition($Name,$task,6,$task.Principal.UserId,$null,3)
}
if ($Mode -eq 'run') { $null=$existing.Run($null) }
'''


def configure_task(root, mode="register"):
    root = Path(root).resolve()
    executable = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    with tempfile.TemporaryDirectory(prefix="jira-recovery-task-") as directory:
        script = Path(directory) / "task.ps1"
        script.write_text(TASK_SCRIPT, encoding="utf-8")
        result = subprocess.run([str(executable), "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-File", str(script), "-Root", str(root), "-Name", task_name(root), "-Mode", mode],
            capture_output=True, text=True, timeout=30, creationflags=0x08000000)
    if result.returncode:
        write(root, "recovery", enabled=False, reason="task-registration-failed")
        event(root, "recovery", "task-registration-failed", exitCode=result.returncode)
        raise updates.UpdateError("[JIRA-R101] Windows could not configure independent recovery. "
            "Use the Start menu to launch the app; ask IT if Task Scheduler is restricted.")
    write(root, "recovery", enabled=mode != "remove", task=task_name(root))


def configured(root):
    try:
        value = json.loads((Path(root) / "jira_config.json").read_text(encoding="utf-8"))
        return isinstance(value, dict) and bool(value.get("domain") and value.get("email") and
            (value.get("tokenProtected") or value.get("token")))
    except (OSError, ValueError):
        return False


def recovery_status(root, *, running=None):
    """Read-only operational status, independent of a stale saved PID/phase."""
    running = running or updates.component_running
    if read(root, "recovery").get("enabled") is not True:
        return {"healthy": False, "message": "Automatic recovery unavailable [JIRA-R101]. Open Jira Overlay from Start; ask IT if tasks are restricted."}
    if not running(GUARD_MUTEX):
        return {"healthy": False, "message": "Automatic recovery is not running. Open Jira Overlay from Start to resume."}
    paused, waiting = [], []
    for role in ROLES:
        if not wanted(root, role):
            paused.append(role)
        elif role == "learner" and not configured(root):
            waiting.append("learner awaiting setup")
        elif not running(updates.MUTEXES[0 if role == "overlay" else 1]):
            waiting.append(role + " restarting with backoff")
    details = (["deliberately closed: " + ", ".join(paused)] if paused else []) + waiting
    return {"healthy": not waiting, "message": "Automatic recovery active" + ("; " + "; ".join(details) if details else "")}


class RestartBudget:
    """Bounded recovery, including exit-code zero without an explicit Quit."""
    def __init__(self):
        self.failures = []
        self.next_attempt = 0

    def exited(self, now, lifetime):
        self.failures = [] if lifetime >= 300 else [t for t in self.failures if now - t < 600]
        self.failures.append(now)
        delay = (5, 15, 60)[min(len(self.failures) - 1, 2)] if len(self.failures) <= 3 else 300
        self.next_attempt = now + delay
        return delay


def watch(root, stop, *, running, launch, clock=time.monotonic, wait=None):
    """Policy loop, injectable for offline tests. Never force-kills a writer."""
    children, budgets = {}, {role: RestartBudget() for role in ROLES}
    wait = wait or (lambda: time.sleep(1))
    ready()
    while True:
        now = clock()
        for role, (process, started) in list(children.items()):
            code = process.poll()
            if code is not None:
                del children[role]
                stream = getattr(process, "_lifecycle_stream", None)
                if stream:
                    stream.close()
                event(root, "guardian", "child-exited", child=role, childPid=process.pid, exitCode=code,
                    expected=stop.is_set() or not wanted(root, role))
                if wanted(root, role) and not stop.is_set():
                    delay = budgets[role].exited(now, now - started)
                    event(root, "guardian", "recovery-delayed", child=role, seconds=delay)
        if stop.is_set():
            # Keep the scheduler action alive until the actual writers finish;
            # exiting its job early must not interrupt a learner save.
            if not children and not any(running(role) for role in ROLES):
                event(root, "guardian", "stopped-for-update")
                reason("update")
                return
        else:
            desired = [role for role in ROLES if wanted(root, role)]
            if not desired and not children and not any(running(role) for role in ROLES):
                event(root, "guardian", "stopped-by-user")
                reason("user-quit", quit=True)
                return
            for role in desired:
                if role in children or running(role) or now < budgets[role].next_attempt:
                    continue
                if role == "learner" and not configured(root):
                    continue
                try:
                    process = launch(role)
                    children[role] = process, now
                    event(root, "guardian", "child-started", child=role, childPid=process.pid)
                except OSError:
                    delay = budgets[role].exited(now, 0)
                    event(root, "guardian", "launch-failed", child=role, seconds=delay)
        wait()


def guardian(root):
    import threading
    root = Path(root).resolve()
    kernel = updates._kernel()
    handle = kernel.CreateMutexW(None, False, GUARD_MUTEX)
    if not handle:
        raise updates.UpdateError("Cannot create recovery mutex")
    if kernel.GetLastError() == 183:
        kernel.CloseHandle(handle)
        return
    stop = threading.Event()
    updates.listen_for_update(root, stop.set)
    streams = []

    def launch(role):
        path = updates._inside_file(folder(root), role + ".stderr.log")
        if path.exists() and path.stat().st_size > 1024 * 1024:
            os.replace(path, updates._inside_file(folder(root), role + ".stderr.log.1"))
        stream = path.open("ab")
        environment = dict(os.environ, JIRA_OVERLAY_SUPERVISED_ROOT=str(root))
        try:
            process = subprocess.Popen([str(root / "runtime" / "pythonw.exe"), "-X", "faulthandler", str(root / ROLES[role])],
                cwd=root, env=environment, stdout=stream, stderr=subprocess.STDOUT, creationflags=0x08000000)
        except BaseException:
            stream.close()
            raise
        streams[:] = [previous for previous in streams if not previous.closed]
        streams.append(stream)
        process._lifecycle_stream = stream
        return process

    try:
        # A racing duplicate must not overwrite the live guardian's records.
        run(root, "guardian", lambda: watch(root, stop,
            running=lambda role: updates.component_running(updates.MUTEXES[0 if role == "overlay" else 1]), launch=launch))
    finally:
        for stream in streams:
            stream.close()
        # Keep the raw mutex handle until Windows ends this process. Releasing
        # it here would let setup copy Python DLLs while interpreter shutdown
        # (including lifecycle/fault-log cleanup) is still using the runtime.
