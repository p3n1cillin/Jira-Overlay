"""Portable route ranking, non-overlapping drift, and human review workflow."""
import json
import sys
from datetime import datetime, timedelta
from unittest.mock import patch

from _harness import Checker, load, load_learner
from triage_support import route_balance_factors, performance_drift

tl = load_learner()
review = load("triage_review.py", "adaptive_review")
check = Checker("adaptive routes")
balance = {"observations": 100, "actual": {"Équipe α": 20, "Queue Ω": 80},
           "predicted": {"Équipe α": 80, "Queue Ω": 20}}
factors = route_balance_factors(balance, 1)
check("overpredicted arbitrary routes are damped and underpredicted routes boosted",
      factors == {"Équipe α": 0.5, "Queue Ω": 2.0})
check("new desks and neutral configuration leave ranking unchanged",
      not route_balance_factors(dict(balance, observations=10), 1)
      and not route_balance_factors(balance, 0))
check("tiny route buckets are not corrected on scant evidence",
      not route_balance_factors({"observations": 100, "actual": {"rare": 2},
                                "predicted": {"rare": 8}}, 1))
for malformed in (None, {"observations": None}, {"actual": None, "predicted": "bad"},
                  {"actual": {"route": "bad", "other": float("nan")}, "observations": -1}):
    learner = tl.Learner({"model": {"routeBalance": malformed}})
    learner.learn("Only observed route", ["word"], "Portal type", [])
    check("old or malformed balance state loads with neutral scoring",
          route_balance_factors(learner.m["routeBalance"], 1) == {})

records = [{"summary": "query " + str(i % 4), "desc": "support", "labels": [],
            "components": [], "structured": [], "reporter": "person",
            "requestType": "Besoin général", "dest": "Queue Ω" if i % 3 else "Équipe α",
            "cloneProjects": []} for i in range(120)]
w = object.__new__(tl.ShadowWatcher)
w.dests = ["Queue Ω"]
w.state, w.plans, w.route_safety = {}, {}, {"Queue Ω": {"forceReview": True}}
cfg = dict(tl.TUNING, routeBalanceWeight=1.0, mistakeWeight=4)
saved = dict(tl.TUNING)
try:
    tl.TUNING.update(cfg)
    replay = w._replay_history(records, cfg, score_start=0, include_model=True)
    w._rebuild_model(records)
    check("adaptive ranking retains exact rebuild/replay equivalence",
          replay["model"] == w.learner.m and w.learner.m["routeBalance"]["observations"] > 50)
    rename = {"Queue Ω": "Custom department 7", "Équipe α": "Front office"}
    w.dests = [rename["Queue Ω"]]
    renamed = w._replay_history([dict(r, dest=rename[r["dest"]]) for r in records], cfg,
                                score_start=0)
    check("renaming departments does not change model decisions or accuracy",
          [rename[p["predicted"]] for p in replay["samples"]] == [p["predicted"] for p in renamed["samples"]]
          and replay["acc"] == renamed["acc"])
    before = json.dumps(w.learner.m["routeBalance"], sort_keys=True)
    w.learner.predict_ex(["query"], "Besoin général")
    check("making a prediction never observes a future label or mutates balance counts",
          before == json.dumps(w.learner.m["routeBalance"], sort_keys=True))
finally:
    tl.TUNING.clear()
    tl.TUNING.update(saved)

def metrics(rate=0.9, macro=0.9, firm=0.98):
    return dict(acc=rate, macroF1=macro, balanced=macro, worst=macro,
                firmBalanced=firm, firmWorst=firm, firmAcc=firm, firmCov=0.7)

baseline = metrics()
check("better macro score cannot hide a material overall or confident regression",
      not w._accept_challenger(baseline, metrics(rate=0.88, macro=0.92))
      and not w._accept_challenger(baseline, metrics(macro=0.92, firm=0.95))
      and w._accept_challenger(baseline, metrics(rate=0.91, macro=0.92)))

calls = []
def replay_probe(recs, cfg, score_start=None, score_end=None):
    calls.append((len(recs), score_start, score_end))
    return dict(metrics(), n=200)

probe = object.__new__(tl.ShadowWatcher)
probe._replay_history = replay_probe
picked = probe._self_tune([{} for _ in range(5000)])
check("candidate selection scores the recent prefix tail without gate/final outcomes",
      (3800, 2600, None) in calls
      and all(start == 2600 for size, start, _ in calls if size == 3800)
      and (5000, 3800, 4600) in calls and (5000, 4600, None) in calls
      and picked["challengerRejected"])

# Large, disjoint baseline/recent samples with a gradual overall deterioration.
now = datetime(2030, 5, 30, 12)
events = []
for days, n, correct in [(15, 1000, 970), (2, 1000, 880)]:
    for i in range(n):
        events.append({"ts": (now-timedelta(days=days)).isoformat(sep=" "),
                       "predicted": "Queue Ω", "dest": "Queue Ω" if i < correct else "Équipe α",
                       "predictionCorrect": i < correct, "review": False,
                       "requestType": "Besoin général"})
drift = performance_drift(events, now=now)
check("gradual drift alerts cover overall, confidence, routes and observed portal types",
      {w["kind"] for w in drift["warnings"]} == {"overall", "band", "route_precision", "request_type"}
      and drift["baseline"]["predictions"] == drift["recent"]["predictions"] == 1000)
tiny = [dict(r) for r in events[:5]] + [dict(r) for r in events[-5:]]
check("small samples do not produce spurious drift warnings",
      performance_drift(tiny, now=now)["warnings"] == [])
stable = [dict(r, predictionCorrect=True) for r in events]
check("stable results and empty desks do not warn",
      not performance_drift(stable, now=now)["warnings"]
      and not performance_drift([], now=now)["warnings"])
boundary = {"ts": (now-timedelta(days=7)).isoformat(sep=" "), "predicted": "Queue Ω",
            "dest": "Queue Ω", "predictionCorrect": True}
edge = performance_drift([boundary], now=now)
check("the exact seven-day boundary is not counted in both comparison periods",
      edge["recent"]["predictions"] == 1 and edge["baseline"]["predictions"] == 0)
outside = [dict(boundary, ts=(now-timedelta(days=29)).isoformat(sep=" ")),
           dict(boundary, ts=(now+timedelta(days=1)).isoformat(sep=" "))]
edge = performance_drift(outside, now=now)
check("drift ignores expired and future-dated events",
      edge["recent"]["predictions"] == edge["baseline"]["predictions"] == 0)

def case(confidence, summary, predicted="Queue Ω", actual="Équipe α"):
    return dict(predicted=predicted, actual=actual, confidence=confidence,
                summary=summary, ts="2030-05-20 12:00", requestType="Besoin général")

cases = {"A": case(99, "Repeated title"), "B": case(98, "repeated title"),
         "C": case(85, "Different wording"), "D": case(100, "Rare confusion", actual="Other")}
selected = review.prioritised_cases(cases)
check("review prioritises recurring confusions and samples diverse examples",
      [key for key, _, _ in selected] == ["A", "C", "D"])
check("queue output preserves arbitrary labels without pretending human confirmation",
      "Besoin général" in review.review_markdown(selected, 4)
      and "never change training labels" in review.review_markdown(selected, 4))
check("untrusted ticket text cannot embed images or links in the worksheet",
      "![" not in review.review_markdown([("X", case(0, "![image](https://example.invalid/)"), 1)], 1)
      and "Confidence: 0;" in review.review_markdown([("X", case(0, "zero"), 1)], 1))

# Exercise actual annotation/export entry points against isolated fixture files.
from pathlib import Path
tmp = Path(tl.CONFIG_FILE).parent
(tmp / "triage_log.jsonl").write_text(json.dumps(dict(cases["A"], event="routed", key="Q-1",
    when="2030-05-20 12:00", dest="Équipe α", predictionCorrect=False))+"\n", encoding="utf-8")
reviews = {"Q-1@2030-05-20 12:00": {"analysis": {"requiresHumanReview": True}}}
(tmp / "triage_reviews.json").write_text(json.dumps(reviews), encoding="utf-8")
with patch.object(review, "__file__", str(tmp / "triage_review.py")), \
     patch.dict(sys.modules, {"triage_eval": type("Evaluation", (), {"tl": tl})()}), \
     patch.object(sys, "argv", ["review", "--case", "Q-1@2030-05-20 12:00", "--note", "Ambiguous initial wording",
                                "--verdict", "ambiguous", "--export", str(tmp / "queue.md")]):
    review.main()
annotated = json.loads((tmp / "triage_reviews.json").read_text(encoding="utf-8"))
note = annotated["Q-1@2030-05-20 12:00"]
check("human verdict preserves automated provenance and remains a local annotation",
      note["verdict"] == "ambiguous" and note["source"] == "human"
      and note["analysis"]["requiresHumanReview"] and note["note"])
check("export excludes already reviewed cases",
      "0 pending mistakes" in (tmp / "queue.md").read_text(encoding="utf-8"))
sys.exit(check.done())
