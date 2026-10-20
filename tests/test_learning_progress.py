"""Refit/replay equivalence, recent coverage, and honest review provenance."""
import json
import os
import sys
from datetime import datetime, timedelta

from _harness import Checker, load, load_learner
from triage_support import rolling_metrics

tl = load_learner()
review = load("triage_review.py", "review_progress")
check = Checker("learning progress")

records = []
for i in range(90):
    # Overlapping language and contradictory outcomes force mistakes, so
    # this catches the actual weighting defect rather than just clean fits.
    records.append({"key": str(i), "summary": "case query " + str(i % 3),
                    "desc": "request support", "labels": [], "components": [],
                    "reporter": "r", "requestType": "General enquiry", "structured": [],
                    "cloneProjects": [], "dest": "Tier Z" if i % 3 else "Front desk"})
w = object.__new__(tl.ShadowWatcher)
w.dests = ["Tier Z"]
w.plans = {}
w.state = {}
w.route_safety = {"Tier Z": {"forceReview": True}}
saved = dict(tl.TUNING)
try:
    tl.TUNING["mistakeWeight"] = 4
    replay = w._replay_history(records, dict(tl.TUNING), score_start=0, include_model=True)
    w._rebuild_model(records)
    check("nightly rebuild reproduces the weighted replay model exactly",
          w.learner.m == replay["model"])
    check("equivalence test includes mistakes that require extra weighting",
          any(not row["correct"] for row in replay["samples"]))
    check("rebuild preserves live route policy",
          w.learner._route_safety is w.route_safety)
finally:
    tl.TUNING.clear()
    tl.TUNING.update(saved)

now = datetime(2030, 5, 10, 12)
def event(days, predicted="Tier Z", correct=False, review_flag=True, reason="predicted"):
    return {"ts": (now - timedelta(days=days)).isoformat(sep=" "),
            "predicted": predicted, "dest": "IT", "predictionCorrect": correct,
            "review": review_flag, "requestType": "General enquiry", "coverageReason": reason}
rows = [event(1), event(8, "IT", True, False), event(31),
        event(2, None, reason="no_intake_observation"), event(3, "IT", True, None)]
week = rolling_metrics(rows, now=now, days=7)
month = rolling_metrics(rows, now=now, days=30)
check("recent windows exclude old events and preserve denominator for missing predictions",
      week["routed"] == 3 and week["predicted"] == 2
      and month["routed"] == 4 and month["accuracy"]["pct"] == 66.7)
check("unknown historical review flags are not counted as confident",
      week["bands"].get("confident") is None and week["bands"]["unknown"] == [1, 1])
check("daily coverage distinguishes missing observations from errors",
      week["coverageReasons"]["no_intake_observation"] == 1
      and sum(n - scored for n, scored in week["dailyCoverage"].values()) == 1)
check("aging without new log writes expires the rolling window",
      rolling_metrics(rows, now=now+timedelta(days=40), days=30)["routed"] == 0)

# Live log folding records evidence for missing events across process restart.
w = object.__new__(tl.ShadowWatcher)
w.canonical_dest = lambda name: name
w._metrics = None
for row in [
    {"event": "would_have_done", "key": "A", "prediction": None, "review": True},
    {"event": "routed", "key": "A", "dest": "IT", "predicted": None},
    {"event": "routed", "key": "A", "dest": "IT", "predicted": None},
    {"event": "routed", "key": "B", "dest": "IT", "predicted": None},
    {"event": "would_have_done", "key": "C", "prediction": "IT", "review": False},
    {"event": "routed", "key": "C", "dest": "IT", "predicted": None},
]:
    w._log(row)
metrics = w._live_metrics()
check("coverage reasons follow observable intake and routing evidence",
      metrics["coverageReasons"] == {"cold_start": 1,
          "repeat_transition_without_prediction": 1, "no_intake_observation": 1,
          "prediction_context_not_retained": 1})
reason_records = metrics["recentEvents"]
check("rolling totals use the same events as cumulative metrics",
      rolling_metrics(reason_records)["routed"] == metrics["routed"] == 4)

case = {"actual": "Équipe α", "predicted": "Tier Z",
        "summary": "printer access", "confidence": 55}
evidence = review.analysis_evidence(
    [{"requestType": "Besoin général", "dest": "Équipe α"},
     {"requestType": "Besoin général", "dest": "Tier Z"}], {"case": case})
analysis = review.analyse_case(case, {"requestType": "Besoin général", "featureOrigin": "intake"}, evidence)
check("automated review is explicitly pending human confirmation",
      analysis["source"] == "automated_log_analysis" and analysis["requiresHumanReview"]
      and analysis["observedFacts"]["actual"] == case["actual"])
check("analysis derives arbitrary desk labels and request-type outcomes from observations",
      analysis["observedFacts"]["requestTypeOutcomes"] == {"Équipe α": 1, "Tier Z": 1}
      and analysis["observedFacts"]["matchingMistakes"] == 1)
check("report segments use arbitrary portal names without specialist buckets",
      week["segments"] == {"General enquiry": [2, 1]})

# Model/report changes must not silently overwrite totals or stale periods.
w.state = {"stats": {"predictions": 4, "correct": 3}}
w.project = "SD"
w.dests = ["Tier Z"]
w.resolved_label = "IT"
w.move_targets = []
w.learner = tl.Learner({})
w.plans = {}
w.write_report()
with open(tl.STATUS_FILE, encoding="utf-8") as f:
    status = json.load(f)
with open(tl.REPORT_FILE, encoding="utf-8") as f:
    report = f.read()
check("report and machine status expose recent counts and coverage consistently",
      status["recentPerformance"]["30"]["routed"] == 4
      and status["accuracy"]["predictions"] == 4
      and "repeat transition without prediction: 1" in report)
rotation_limit = tl.LOG_ROTATE_BYTES
try:
    tl.LOG_ROTATE_BYTES = 1
    w._log({"event": "routed", "key": "D", "dest": "IT", "predicted": None})
    restarted = object.__new__(tl.ShadowWatcher)
    restarted.canonical_dest = lambda name: name
    after = restarted._live_metrics()
    check("recent events and coverage provenance survive log rotation and restart",
          rolling_metrics(after["recentEvents"])["routed"] == after["routed"] == 5
          and after["coverageReasons"]["no_intake_observation"] == 2)
finally:
    tl.LOG_ROTATE_BYTES = rotation_limit
check("historical log files remain isolated from the real install",
      os.path.basename(tl.LOG_FILE) == "log_file.dat")

ev = load("triage_eval.py", "portable_evaluation")
ev.tl.CONFIG_FILE = tl.CONFIG_FILE
with open(tl.CONFIG_FILE, "w", encoding="utf-8") as f:
    json.dump({"triageStatuses": ["Équipe α", "Tier Z"]}, f)
check("offline workflow roles use configuration rather than English status prefixes",
      ev.handoff_dests(records) == {"Équipe α", "Tier Z"})
with open(tl.CONFIG_FILE, "w", encoding="utf-8") as f:
    json.dump({"triageStatuses": []}, f)
check("a desk without handoffs does not invent roles from outcome names",
      ev.handoff_dests([{"dest": "With Anything"}]) == set())
sys.exit(check.done())
