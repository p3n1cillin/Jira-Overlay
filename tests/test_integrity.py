"""End-to-end accounting, interrupted polling, recovery, and evaluation parity."""

import json
import os
import sys
from datetime import datetime, timedelta, timezone
from types import SimpleNamespace
from unittest.mock import patch

from _harness import Checker, load_learner, load_overlay
from triage_storage import daily_backup, verify_backup
from triage_support import status_view, wilson_lower, threshold_experiment

tl = load_learner()
ov = load_overlay()
check = Checker("integrity")


def watcher():
    w = object.__new__(tl.ShadowWatcher)
    w.state = {"stats": {"predictions": 10, "correct": 8}}
    w.plans = {}
    w.project = "SD"
    w.dests = ["With DEV", "With DATA"]
    w.resolved = {"Closed"}
    w.resolved_label = "IT"
    w.move_targets = ["IPS"]
    w.moved_label_overrides = {}
    w.moved_label_fmt = "Moved to {project}"
    w.learner = tl.Learner({})
    w._metrics = None
    w._issue_fields = []
    return w


def event(key="SD-1"):
    return {
        "event": "routed",
        "key": key,
        "predicted": "With DEV",
        "dest": "IT",
        "predictionCorrect": False,
        "review": True,
        "by": "tester",
        "when": "2026-09-07 10:00",
        "clones": [],
        "requestType": "Get IT help",
    }


w = watcher()
w._log(event())
w.write_report()
with open(tl.STATUS_FILE, encoding="utf-8") as f:
    status = json.load(f)
with open(tl.REPORT_FILE, encoding="utf-8") as f:
    report = f.read()
loaded = ov.JiraOverlay._load_learner_status(None, tl.STATUS_FILE)
check(
    "unused final route cannot overwrite report/status/overlay totals",
    status["accuracy"] == {"predictions": 10, "correct": 8, "pct": 80.0}
    and "80% (8/10)" in report
    and loaded["accuracy"] == status["accuracy"],
    str((status["accuracy"], "80% (8/10)" in report, loaded["accuracy"])),
)
check(
    "heartbeat expiry overrides a cached healthy flag",
    not status_view(
        {
            "healthy": True,
            "updatedAt": (
                datetime.now(timezone.utc) - timedelta(minutes=10)
            ).isoformat(),
        }
    )["healthy"],
)

j = object.__new__(tl.Jira)
j._refusals = set()


def response(code, path):
    return SimpleNamespace(
        status_code=code, request=SimpleNamespace(path_url=path, method="GET")
    )


j._note_refusal(response(404, "/rest/api/3/issue/SD-1"))
check("a missing individual issue is not a global outage", not j._refusals)
j._note_refusal(response(401, "/rest/api/3/search/jql"))
j._note_refusal(response(200, "/rest/api/3/search/jql"))
check("a successful project search clears an old refusal", not j._refusals)


class Page:
    ok = True

    def __init__(self, data):
        self.data = data

    def json(self):
        return self.data


pages = iter([Page({"issues": [{"key": "SD-1"}], "nextPageToken": "next"}), None])
j._send = lambda *a, **kw: next(pages)
j.incomplete_reads = 0
check(
    "partial search exposes incompleteness while retaining fetched work",
    len(j.search("query", [], cap=None)) == 1 and j.incomplete_reads == 1,
)
pages = iter(
    [
        Page(
            {
                "issues": [{"key": str(i)} for i in range(100)],
                "nextPageToken": str(n + 1),
            }
        )
        for n in range(6)
    ]
    + [Page({"issues": [{"key": "last"}], "isLast": True})]
)
j._send = lambda *a, **kw: next(pages)
j.incomplete_reads = 0
check(
    "complete mode reads beyond 500 tickets",
    len(j.search("query", [], cap=None)) == 601 and j.incomplete_reads == 0,
)


class Partial:
    incomplete_reads = 0

    def search(self, *args, **kwargs):
        self.incomplete_reads += 1
        return []


w = watcher()
w.jira = Partial()
w.state["lastPollEpoch"] = 100
w._window_jql_time = lambda: '"2026/09/01 00:00"'
w._learn_moved_out = lambda *args: 0
w._save_state = lambda **kwargs: None
w._write_suggestions = lambda *args: None
w.write_report = lambda: None
w.resolve_and_learn()
check(
    "failed pages do not advance the durable checkpoint",
    w.state["lastPollEpoch"] == 100 and w.state["pollIncomplete"],
)
w.jira.search = lambda *args, **kwargs: []
w.resolve_and_learn()
check(
    "a completed retry advances the checkpoint and restores health",
    w.state["lastPollEpoch"] > 100 and not w.state["pollIncomplete"],
)

# Several rotations and a fresh process must still see every event once.
tl.LOG_ROTATE_BYTES = 200
w = watcher()
for n in range(6):
    w._log(event(str(n)))
m = w._live_metrics()
fresh = watcher()._live_metrics()
check(
    "rotating/restarting preserves cumulative metrics exactly once",
    m["predicted"] == fresh["predicted"] == 7,
    str((m["predicted"], fresh["predicted"])),
)
replace = tl.os.replace


def locked_rotation(source, target):
    if source == tl.LOG_FILE:
        raise PermissionError("reader lock")
    return replace(source, target)


with patch.object(tl.os, "replace", side_effect=locked_rotation):
    w._log(event("locked"))
check(
    "failed replacement plus restart does not double-count archived prefix",
    watcher()._live_metrics()["predicted"] == 8,
)

snapshot = {
    "summary": "original",
    "desc": "before routing",
    "labels": [],
    "components": [],
    "structured": [],
    "reporter": "intake reporter",
    "requestType": "Get IT help",
}
w.state["predicted"] = {"SD-99": {"intakeFeatures": snapshot}}
det = dict(snapshot, key="SD-99", summary="edited after routing", clones=[])
w._append_history(det, "IT", "2026-09-07 12:00")
with open(tl.HISTORY_FILE, encoding="utf-8") as f:
    captured = json.loads(f.readlines()[-1])
check(
    "training and replay use the intake snapshot without changing outcome",
    w._learning_details(det)["summary"] == captured["summary"] == "original"
    and captured["dest"] == "IT"
    and captured["featureOrigin"] == "intake",
)
check(
    "ten perfect examples do not prove reliable precision",
    wilson_lower(10, 10) < 0.90 < wilson_lower(100, 100),
)
gate = [{"predicted": "D", "conf": 99, "margin": 30, "correct": 1}] * 100
final = [dict(gate[0], correct=0)] * 5
experiment = threshold_experiment(gate, final)
check(
    "threshold selection does not look at final-window outcomes",
    experiment["D"]["reviewConf"] == 80 and experiment["D"]["final"]["pct"] == 0,
)

with open(tl.STATE_FILE, "w", encoding="utf-8") as f:
    json.dump({"model": {}}, f)
backup = daily_backup(tl.STATE_FILE, tl.HISTORY_FILE, tl.LOG_FILE)
check(
    "recovery archive verifies and excludes credentials",
    os.path.basename(tl.STATE_FILE) in verify_backup(backup)
    and os.path.basename(tl.CONFIG_FILE) not in verify_backup(backup),
)

from _harness import load

ev = load("triage_eval.py", "evaluation_test")
records = [
    {
        "key": str(n),
        "summary": "data query" if n % 2 else "server restart",
        "desc": "",
        "labels": [],
        "components": [],
        "structured": [],
        "reporter": "r",
        "requestType": "",
        "cloneProjects": [],
        "dest": "With DATA" if n % 2 else "IT",
    }
    for n in range(50)
]
core = object.__new__(ev.tl.ShadowWatcher)
core.dests = ["With DATA"]
direct = core._replay_history(records, dict(ev.tl.TUNING), score_start=10)
offline = ev.replay(records, dict(ev.tl.TUNING), warmup=10)
check(
    "offline and live replay have identical predictions and metrics",
    direct["samples"] == offline["samples"] and direct["macroF1"] == offline["macroF1"],
)
sys.exit(check.done())
