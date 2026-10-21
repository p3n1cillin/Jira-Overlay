"""Recovery policy, deliberate Quit, safe update waits and private diagnostics."""
import json
import os
from pathlib import Path
import tempfile
import threading
from types import SimpleNamespace

from _harness import Checker
import app_lifecycle as life

check = Checker("application lifecycle and recovery")

with tempfile.TemporaryDirectory(prefix="jira-lifecycle-test-") as directory:
    base = Path(directory)
    root = base / "recording"
    root.mkdir()
    check("reading a missing status never creates files", life.read(root, "overlay") == {} and not list(root.iterdir()))
    life.run(root, "overlay", lambda: life.reason("user-quit", quit=True))
    check("intentional Quit survives process exit", not life.wanted(root, "overlay") and life.read(root, "overlay")["reason"] == "user-quit")
    life.run(root, "overlay", lambda: life.reason("update"))
    check("update shutdown retains the desired running state", life.wanted(root, "overlay") and life.read(root, "overlay")["reason"] == "update")

    def fail():
        raise RuntimeError("FAKE_SECRET_SHOULD_NOT_BE_LOGGED")

    try:
        life.run(root, "learner", fail)
    except RuntimeError:
        pass
    log = (life.folder(root) / "learner.jsonl").read_text()
    check("unhandled exceptions leave an exit record without request/secret text",
        '"exceptionType": "RuntimeError"' in log and "FAKE_SECRET" not in log and life.read(root, "learner")["exitCode"] == 1)
    check("native-fault recording files are available", (life.folder(root) / "learner.fault.log").exists())
    budget = life.RestartBudget()
    check("crash loops use bounded backoff", [budget.exited(i, 0) for i in range(4)] == [5, 15, 60, 300])
    check("a stable five-minute run resets the crash budget", budget.exited(1000, 301) == 5)

    for scenario in ("crash", "update", "quit-learner"):
        root = base / scenario
        root.mkdir()
        (root / "jira_config.json").write_text(json.dumps({"domain": "example.invalid", "email": "fake", "token": "fake"}))
        stop = threading.Event()
        children, launches, tick = {}, [], [0]
        if scenario == "quit-learner":
            life.set_wanted(root, "learner", False)

        def running(role):
            return role in children and children[role].code is None

        def launch(role):
            process = SimpleNamespace(pid=100 + len(launches), code=None)
            process.poll = lambda: process.code
            children[role] = process
            launches.append((role, tick[0]))
            life.write(root, role, phase="running")
            return process

        def wait():
            tick[0] += 1
            assert tick[0] < 30, "Guardian policy did not terminate in the isolated test"
            if scenario == "crash":
                if tick[0] == 1:
                    children["overlay"].code = 7
                if tick[0] == 8:
                    for role in life.ROLES:
                        life.set_wanted(root, role, False)
                        children[role].code = 0
            elif scenario == "update":
                stop.set()
                if tick[0] == 3:
                    for child in children.values():
                        child.code = 0
            else:
                life.set_wanted(root, "overlay", False)
                children["overlay"].code = 0

        life.watch(root, stop, running=running, launch=launch, clock=lambda: tick[0], wait=wait)
        events = [json.loads(line) for line in (life.folder(root) / "guardian.jsonl").read_text().splitlines()]
        if scenario == "crash":
            check("an unexpected exit recovers only the missing component after backoff",
                [r for r, _ in launches] == ["overlay", "learner", "overlay"] and launches[-1][1] >= 6)
            check("external termination records its exact child exit code",
                any(e["event"] == "child-exited" and e.get("exitCode") == 7 and not e["expected"] for e in events))
        elif scenario == "update":
            check("update shutdown waits for writers and never relaunches them", tick[0] == 3 and len(launches) == 2 and events[-1]["event"] == "stopped-for-update")
        else:
            check("an intentionally closed learner is never resurrected", [r for r, _ in launches] == ["overlay"])

    # Real registration of a demand-only task in an isolated fake directory.
    # It has NO trigger and is never run; no user's actual apps are touched.
    if os.name == "nt":
        root = base / "isolated task"
        (root / "runtime").mkdir(parents=True)
        (root / "runtime/pythonw.exe").write_bytes(b"not executed")
        (root / "app_guard.pyw").write_text("# not executed")
        try:
            life.configure_task(root)
            check("independent per-user recovery registers without a password or logon trigger", life.read(root, "recovery").get("enabled") is True)
        finally:
            life.configure_task(root, "remove")
        check("uninstall removes only its own demand task", life.read(root, "recovery").get("enabled") is False)

check("recovery has no tenant configuration", "serviceDeskId" not in life.TASK_SCRIPT and "apiToken" not in life.TASK_SCRIPT)
raise SystemExit(check.done())
