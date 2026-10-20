"""Pure, shared reporting and confidence helpers; no Jira or GUI access."""

from __future__ import annotations

import math
from datetime import datetime, timezone, timedelta


def accuracy_counts(total, correct):
    return {
        "predictions": total,
        "correct": correct,
        "pct": round(100 * correct / total, 1) if total else None,
    }


def wilson_lower(correct, total, z=1.645):
    """One-sided 95% lower bound; ten lucky predictions are not certainty."""
    if total <= 0:
        return 0.0
    p = correct / total
    return (
        p
        + z * z / (2 * total)
        - z * math.sqrt(p * (1 - p) / total + z * z / (4 * total * total))
    ) / (1 + z * z / total)


def status_view(data, now=None, max_age=300):
    """Derive display health from both heartbeat age and poll completion."""
    result = dict(data)
    now = now or datetime.now(timezone.utc)
    try:
        stamp = datetime.fromisoformat(str(data.get("updatedAt", "")))
        if stamp.tzinfo is None:
            stamp = stamp.astimezone()
        age = (now - stamp).total_seconds()
        stale = age > max_age or age < -60
    except (ValueError, TypeError):
        stale = True
    if stale:
        result.update(
            healthy=False,
            healthMessage="Learner status is stale; check that it is running",
        )
    elif not data.get("healthy", True):
        result.setdefault("healthMessage", "Learner polling needs attention")
    return result


def segment_name(request_type):
    """Use each desk's observed request types, without site-specific buckets."""
    return str(request_type or "Unknown request type")


def coverage_reason(record, intake=None, prior_routing=False):
    """Describe observed evidence, never assume why an intake ticket was missed."""
    if record.get("predicted") is not None:
        return "predicted"
    if prior_routing:
        return "repeat_transition_without_prediction"
    if intake is not None:
        return ("cold_start" if intake.get("prediction") is None
                else "prediction_context_not_retained")
    return "no_intake_observation"


def route_balance_factors(balance, weight):
    """Bounded, smoothed correction for routes historically over/under-predicted.

    Counts must come only from predictions made before their outcomes were
    learned. This is a ranking adjustment, not calibrated probability evidence.
    Small/new desks and unseen routes retain neutral scores.
    """
    if not weight or balance.get("observations", 0) < 50:
        return {}
    actual, predicted = balance.get("actual", {}), balance.get("predicted", {})
    factors = {}
    for dest in set(actual) | set(predicted):
        observed, suggested = actual.get(dest, 0), predicted.get(dest, 0)
        if max(observed, suggested) < 10:
            continue
        ratio = max(0.5, min(2.0, (observed + 20) / (suggested + 20)))
        factors[dest] = ratio ** max(0.0, min(2.0, weight))
    return factors


def performance_drift(records, now=None):
    """Compare a recent week with the preceding, disjoint 21-day baseline.

    Alerts require both a material drop and conservative sample evidence.
    They are investigation prompts, not proof of a model or workflow change.
    """
    now = now or datetime.now()
    split = (now - timedelta(days=7)).isoformat(sep=" ", timespec="seconds")
    recent_rows = [r for r in records if r.get("ts", "") >= split]
    baseline_rows = [r for r in records if r.get("ts", "") < split]
    current = rolling_metrics(recent_rows, now=now, days=7)
    baseline = rolling_metrics(baseline_rows, now=now-timedelta(days=7), days=21)
    comparisons = []

    def compare(kind, label, previous, latest, minimum):
        old_n, old_ok = previous
        new_n, new_ok = latest
        if min(old_n, new_n) < minimum:
            return
        old_rate, new_rate = old_ok / old_n, new_ok / new_n
        pooled = (old_ok + new_ok) / (old_n + new_n)
        error = math.sqrt(pooled * (1-pooled) * (1/old_n + 1/new_n))
        drop = old_rate-new_rate
        z = drop / error if error else 0.0
        comparisons.append({"kind": kind, "label": label,
                            "baseline": accuracy_counts(old_n, old_ok),
                            "recent": accuracy_counts(new_n, new_ok),
                            "dropPoints": round(100*drop, 2),
                            "warning": drop >= 0.03 and z >= 3.0})

    compare("overall", "Overall accuracy", [baseline["predicted"], baseline["correct"]],
            [current["predicted"], current["correct"]], 50)
    compare("band", "Confident suggestions", baseline["bands"].get("confident", [0, 0]),
            current["bands"].get("confident", [0, 0]), 50)
    for kind, table in (("route_precision", "byPredicted"), ("request_type", "segments")):
        for label in sorted(set(baseline[table]) & set(current[table])):
            compare(kind, label, baseline[table][label], current[table][label], 30)
    return {"recentDays": 7, "baselineDays": 21,
            "recent": current["accuracy"], "baseline": baseline["accuracy"],
            "comparisons": comparisons,
            "warnings": [c for c in comparisons if c["warning"]]}


def rolling_metrics(records, now=None, days=30):
    """Aggregate routing events by observation time, with known confidence only."""
    now = now or datetime.now()
    cutoff = (now - timedelta(days=days)).isoformat(sep=" ", timespec="seconds")
    end = now.isoformat(sep=" ", timespec="seconds")
    rows = [r for r in records if cutoff <= r.get("ts", "") <= end]
    totals = {"routed": len(rows), "predicted": 0, "correct": 0,
              "byActual": {}, "byPredicted": {}, "segments": {},
              "bands": {}, "coverageReasons": {}, "dailyCoverage": {}}
    for row in rows:
        reason = row.get("coverageReason", "unclassified")
        coverage = totals["coverageReasons"]
        coverage[reason] = coverage.get(reason, 0) + 1
        day = totals["dailyCoverage"].setdefault(row["ts"][:10], [0, 0])
        day[0] += 1
        if row.get("predicted") is None:
            continue
        day[1] += 1
        hit = int(bool(row.get("predictionCorrect")))
        totals["predicted"] += 1
        totals["correct"] += hit
        band = "unknown" if row.get("review") is None else "review" if row["review"] else "confident"
        for table, key in (("byActual", row.get("dest", "unknown")),
                           ("byPredicted", row["predicted"]),
                           ("segments", segment_name(row.get("requestType"))),
                           ("bands", band)):
            counts = totals[table].setdefault(key, [0, 0])
            counts[0] += 1
            counts[1] += hit
    totals["accuracy"] = accuracy_counts(totals["predicted"], totals["correct"])
    totals["coveragePct"] = round(100 * totals["predicted"] / len(rows), 1) if rows else None
    totals["windowDays"] = days
    return totals


def threshold_experiment(gate, final):
    """Select per-route review thresholds on gate only; evaluate on final."""
    result = {}
    for dest in sorted({p["predicted"] for p in gate}):
        options = []
        for conf in (80, 85, 90, 95, 99):
            for margin in (20, 25, 30):
                kept = [
                    p
                    for p in gate
                    if p["predicted"] == dest
                    and p["conf"] >= conf
                    and p["margin"] >= margin
                    and not p.get("review")
                ]
                n = len(kept)
                correct = sum(p["correct"] for p in kept)
                if n >= 10 and correct / n >= 0.95 and wilson_lower(correct, n) >= 0.90:
                    options.append((n, -conf, -margin))
        if not options:
            result[dest] = {"review": True, "reason": "Insufficient gate evidence"}
            continue
        n, neg_conf, neg_margin = max(options)
        conf, margin = -neg_conf, -neg_margin
        kept = [
            p
            for p in final
            if p["predicted"] == dest
            and p["conf"] >= conf
            and p["margin"] >= margin
            and not p.get("review")
        ]
        result[dest] = {
            "reviewConf": conf,
            "reviewMargin": margin,
            "gateN": n,
            "final": accuracy_counts(len(kept), sum(p["correct"] for p in kept)),
        }
    return result
