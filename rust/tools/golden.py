"""Generate golden outputs from the Python learner for the Rust port's tests.

Runs the shipping Python model (triage_learner.pyw + app/triage_evaluation.py)
over a synthetic, seeded ticket history and records every prediction, the
final model, replay metrics and a full nightly self-tune. The Rust engine must
reproduce these exactly. No real ticket data is used or written.

    python rust/tools/golden.py            # regenerate committed fixtures
    python rust/tools/golden.py --real DIR # local-only check against a copy
                                           # of a real data folder (gitignored)

Requires CPython 3.12+ (compensated float sum), as shipped in the installer.
"""
from __future__ import annotations

import argparse
import json
import random
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parents[1]
sys.path.insert(0, str(PROJECT / "tests"))
from _harness import load_learner  # noqa: E402  (isolates every data file)

OUT = PROJECT / "rust" / "crates" / "triage-engine" / "tests" / "golden"
HANDOFFS = ["With DEV", "With BI", "With DATA"]
RESOLVED = "Resolved in SD"
MOVED = ["Moved to PPS", "Moved to IPS"]
DESTS = HANDOFFS + [RESOLVED] + MOVED

ROUTE_WORDS = {
    "With DEV": ["exception", "stack", "deploy", "api", "endpoint", "build", "release", "bug", "crash", "timeout"],
    "With BI": ["report", "dashboard", "powerbi", "metric", "chart", "kpi", "dataset", "refresh", "visual", "filter"],
    "With DATA": ["import", "export", "csv", "extract", "duplicate", "records", "merge", "field", "mapping", "cleanse"],
    RESOLVED: ["password", "reset", "printer", "laptop", "login", "outlook", "teams", "vpn", "monitor", "keyboard"],
    "Moved to PPS": ["proclaim", "matter", "workflow", "screen", "button", "case", "client", "document", "letter", "fee"],
    "Moved to IPS": ["invoice", "payment", "ledger", "posting", "supplier", "remittance", "accounts", "vat", "purchase", "credit"],
}
SHARED = ["issue", "problem", "urgent", "today", "error", "unable", "access", "update", "system", "help", "user", "team"]
REQUEST_TYPES = ["Get IT help", "Report a bug", "Data request", "Reporting request", "Hardware", ""]
REQ_FOR = {"With DEV": "Report a bug", "With BI": "Reporting request", "With DATA": "Data request",
           RESOLVED: "Hardware", "Moved to PPS": "Get IT help", "Moved to IPS": "Get IT help"}
REPORTERS = [f"Person {chr(65 + i)}" for i in range(14)]
WEIGHTS = [0.14, 0.07, 0.22, 0.40, 0.11, 0.06]


def synthetic(n: int, seed: int = 7831) -> list[dict]:
    rng = random.Random(seed)
    records, titles = [], []
    for i in range(n):
        dest = rng.choices(DESTS, WEIGHTS)[0]
        if rng.random() < 0.12:  # occasional cross-talk so the model must weigh evidence
            dest_words = ROUTE_WORDS[rng.choice(DESTS)]
        else:
            dest_words = ROUTE_WORDS[dest]
        if titles and rng.random() < 0.15:  # recurring requests exercise ticket memory
            summary, prev = rng.choice(titles)
            if rng.random() < 0.7:
                dest = prev
        else:
            words = rng.sample(dest_words, 2) + rng.sample(SHARED, rng.randint(0, 2))
            rng.shuffle(words)
            if rng.random() < 0.1:
                words.append(f"tc{rng.randint(10000, 99999)}")
            summary = " ".join(words).capitalize()
            titles.append((summary, dest))
        req = REQ_FOR[dest] if rng.random() < 0.75 else rng.choice(REQUEST_TYPES)
        desc_words = rng.sample(dest_words, 3) + rng.sample(SHARED, 3) + ["please", "thanks", "the"]
        rng.shuffle(desc_words)
        record = {
            "key": f"SD-{1000 + i}",
            "when": f"2026-{1 + i // 300:02d}-{1 + (i // 10) % 28:02d} {9 + i % 8:02d}:{i % 60:02d}",
            "dest": dest,
            "summary": summary,
            "desc": " ".join(desc_words) + (" Kind regards, it's urgent" if rng.random() < 0.2 else ""),
            "labels": rng.sample(["hardware", "software", "finance", "reporting"], rng.randint(0, 2)),
            "components": rng.sample(["Portal", "Backend", "Office"], rng.randint(0, 1)),
            "reporter": rng.choice(REPORTERS[:5]) if dest in HANDOFFS and rng.random() < 0.5 else rng.choice(REPORTERS),
            "requestType": req,
            "structured": [f"customfield_10010 {rng.choice(['Head Office', 'Branch', 'Remote'])}"],
            "cloneProjects": ["DEVPROJ"] if dest == "With DEV" and rng.random() < 0.6 else [],
            "featureOrigin": "intake" if rng.random() < 0.5 else "outcome",
        }
        if i % 97 == 0:  # legacy rows: reqType instead of requestType, no origin
            record["reqType"] = record.pop("requestType")
            record.pop("featureOrigin")
        records.append(record)
    return records


def watcher(tl, dests):
    w = object.__new__(tl.ShadowWatcher)
    w.dests = list(dests)
    return w


SCENARIOS = {
    "default": {},
    "aggressive": {"decayFactor": 0.98, "escalationSpecialist": 1, "routeBalanceWeight": 1.0,
                   "priorWeight": 0.5, "knnThresh": 0.6, "reporterWeight": 2.0, "summaryWeight": 4},
    "plain": {"logOdds": 0, "knnThresh": 0, "decayFactor": 1.0, "summaryWeight": 3, "mistakeWeight": 4},
}
PLANS = {RESOLVED: "resolve in SD — no handoff", "Moved to PPS": "move the ticket to the PPS project"}
SAFETY = {"With BI": {"forceReview": True, "reason": "only 4 confident predictions"}}


def sequential(tl, records, overrides):
    saved = dict(tl.TUNING)
    tl.TUNING.update(overrides)
    try:
        lr = tl.Learner({}, plans=dict(PLANS), escalation_dests=HANDOFFS, route_safety=dict(SAFETY))
        w = watcher(tl, HANDOFFS)
        predictions = [w._fit_example(lr, r) for r in records]
        return {"predictions": predictions, "model": lr.m, "mutations": lr._mutations}
    finally:
        tl.TUNING.clear()
        tl.TUNING.update(saved)


def write(name, value):
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / name).write_text(json.dumps(value, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")


def strip_samples(metrics):
    return {k: v for k, v in metrics.items() if k != "samples"} | {"samples": metrics["samples"][:50]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--real", type=Path, help="data folder copy for a local-only comparison")
    args = parser.parse_args()
    tl = load_learner()
    if args.real:
        return real(tl, args.real)

    records = synthetic(1200)
    (OUT.parent).mkdir(parents=True, exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    with (OUT / "synthetic_history.jsonl").open("w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")

    texts = ["Hi, PLEASE reset it's password for HCW11593772KEO and tc25000123 ab1234's",
             "Ünïcödé ﬁle naïve café Kelvin İstanbul ΣΑΣ", "x1 ab a1b2c3 don't won't 'quoted'",
             "", "AAAA1234 b12345 abcdefg1234 abcdef1234x_y"]
    write("text.json", {
        "tokenize": [[t, tl.tokenize(t)] for t in texts],
        "features": [[r["summary"], tl.features_for(r, r.get("requestType", r.get("reqType", "")))]
                     for r in records[:40]],
        "round": [[x, n, round(x, n)] for x in (0.12345, 0.5, 0.0005, 0.9995, 2.675, 1/3, 2/3, 0.1 + 0.2, 0.83349999)
                  for n in (1, 3, 4)],
    })
    for name, overrides in SCENARIOS.items():
        write(f"sequential_{name}.json", {"overrides": overrides, **sequential(tl, records, overrides)})

    w = watcher(tl, HANDOFFS)
    full = w._replay_history(records, {})
    window = w._replay_history(records, {"routeBalanceWeight": 0.5}, score_start=600, score_end=1000)
    write("replay.json", {"full": strip_samples(full), "window": strip_samples(window),
                          "scores": [list(w._tune_score(full)), list(w._tune_score(window))]})

    best = w._self_tune(records)
    keep = ("cfg", "score", "split", "evals", "validationWindows", "challengerRejected", "gateReason",
            "incumbentGateScore", "challengerGateScore", "challengerCfg", "gateN", "trainN", "holdoutN",
            "finalHoldoutN", "acc", "balanced", "macroF1", "firmCov")
    write("self_tune.json", {k: best[k] for k in keep}
          | {"finalAcc": best["finalHoldout"]["acc"], "gateAcc": best["gate"]["acc"]})
    print("golden fixtures written to", OUT)


def real(tl, folder: Path):
    """Replay a copy of a real history and dump predictions for a local diff."""
    out = HERE / "golden-local"
    out.mkdir(exist_ok=True)
    cfg = json.loads((folder / "jira_config.json").read_text(encoding="utf-8"))
    state = json.loads((folder / "triage_state.json").read_text(encoding="utf-8"))
    dests = list(dict.fromkeys(cfg.get("triageStatuses") or []))
    recs = []
    for line in (folder / "triage_history.jsonl").read_text(encoding="utf-8").splitlines():
        try:
            recs.append(json.loads(line))
        except ValueError:
            continue
    recs.sort(key=lambda r: r.get("when", ""))
    tuned = dict(tl.BALANCED_TUNED_CONFIG, **(state.get("tunedConfig") or {}))
    tl.TUNING.update(tuned)
    lr = tl.Learner({}, escalation_dests=dests)
    w = watcher(tl, dests)
    preds = [w._fit_example(lr, r) for r in recs]
    live = tl.Learner(state, escalation_dests=dests, route_safety=state.get("routeSafety") or {})
    live_preds = []
    for r in recs[-500:]:
        req = r.get("requestType", r.get("reqType", ""))
        live_preds.append(live.predict_ex(tl.features_for(r, req), req, r.get("reporter", ""),
                                          exemplar=tl.tokenize(r["summary"])))
    (out / "real.json").write_text(json.dumps({"dests": dests, "tuning": tuned, "predictions": preds,
                                               "livePredictions": live_preds}, ensure_ascii=False),
                                   encoding="utf-8")
    print(f"{len(recs)} records replayed; output in {out}")


if __name__ == "__main__":
    main()
