"""Local lifecycle records and ordinary user-session recovery.

Only component names, PIDs, exit codes and times are logged; never Jira data.
No scheduled task, service or script host is created or executed.
"""
from __future__ import annotations

import faulthandler
import json
import os
from pathlib import Path
import subprocess
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
    if not running(GUARD_MUTEX):
        return {"healthy": False, "message": "User-session recovery is not running [JIRA-R101]. Start manually only after IT approval."}
    paused, waiting = [], []
    for role in ROLES:
        if not wanted(root, role):
            paused.append(role)
        elif role == "learner" and not configured(root):
            waiting.append("learner awaiting setup")
        elif not running(updates.MUTEXES[0 if role == "overlay" else 1]):
            waiting.append(role + " restarting with backoff")
    details = (["deliberately closed: " + ", ".join(paused)] if paused else []) + waiting
    return {"healthy": not waiting, "message": "User-session recovery active (no scheduled task)" + ("; " + "; ".join(details) if details else "")}


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
