"""Recovery policy, deliberate Quit, safe update waits and private diagnostics."""
import json
from pathlib import Path
import tempfile
import threading
from types import SimpleNamespace
from unittest.mock import patch

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
    observed = []
    def became_ready():
        life.ready()
        observed.append(life.read(root, "learner"))
    life.run(root, "learner", became_ready)
    check("a recovered process clears old failure diagnostics when ready",
        observed[0]["phase"] == "running" and observed[0]["reason"] == "running" and observed[0]["exitCode"] is None)

    root = base / "health"
    root.mkdir()
    no_process = lambda name: False
    check("unavailable recovery gives a stable actionable code without creating files",
        "JIRA-R101" in life.recovery_status(root, running=no_process)["message"] and not list(root.iterdir()))
    life.write(root, "recovery", enabled=True)
    life.write(root, "guardian", phase="running", pid=123)
    check("a saved running phase is not mistaken for a live guardian",
        "not running" in life.recovery_status(root, running=no_process)["message"])
    (root / "jira_config.json").write_text('{"domain":"example.invalid","email":"fake","token":"fake"}')
    all_running = lambda name: True
    check("live recovery reports active independently of saved process IDs",
        life.recovery_status(root, running=all_running) == {"healthy": True, "message": "User-session recovery active (no scheduled task)"})
    life.set_wanted(root, "learner", False)
    check("deliberate Quit is distinguished from an unhealthy crash",
        life.recovery_status(root, running=all_running)["healthy"] and "deliberately closed: learner" in life.recovery_status(root, running=all_running)["message"])
    life.set_wanted(root, "learner", True)
    status = life.recovery_status(root, running=lambda name: name != life.updates.MUTEXES[1])
    check("backoff is visible without killing a slow writer", not status["healthy"] and "restarting with backoff" in status["message"])
    (root / "jira_config.json").unlink()
    check("a fresh account explains why its learner has not started",
        "awaiting setup" in life.recovery_status(root, running=all_running)["message"])

    before = (life.folder(root) / "guardian.json").read_bytes()
    closed = []
    kernel = SimpleNamespace(CreateMutexW=lambda *a: 1, GetLastError=lambda: 183,
        CloseHandle=lambda handle: closed.append(handle))
    with patch.object(life.updates, "_kernel", return_value=kernel):
        life.guardian(root)
    check("a duplicate guardian leaves the live guardian's record untouched",
        before == (life.folder(root) / "guardian.json").read_bytes() and closed == [1])

    streams = []
    kernel.GetLastError = lambda: 0
    def blocked_launch(*args, **kwargs):
        streams.append(kwargs["stdout"])
        raise OSError("isolated launch refusal")
    def exercise_launch(root, stop, *, running, launch):
        life.ready()
        observed.append(life.read(root, "guardian"))
        for _ in range(3):
            try:
                launch("overlay")
            except OSError:
                pass
    with patch.object(life.updates, "_kernel", return_value=kernel), \
         patch.object(life.updates, "listen_for_update"), \
         patch.object(life.subprocess, "Popen", side_effect=blocked_launch), \
         patch.object(life, "watch", side_effect=exercise_launch):
        life.guardian(root)
    check("failed companion launches close their diagnostic streams", len(streams) == 3 and all(s.closed for s in streams))
    events = [json.loads(line) for line in (life.folder(root) / "guardian.jsonl").read_text().splitlines()]
    check("guardian readiness and a single start are recorded accurately",
        observed[-1]["phase"] == "running" and sum(e["event"] == "started" for e in events) == 1)

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

check("recovery never registers scheduled tasks", not hasattr(life, "TASK_SCRIPT") and not hasattr(life, "configure_task"))
raise SystemExit(check.done())
