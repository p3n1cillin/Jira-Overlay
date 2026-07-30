#!/usr/bin/env python3
"""
Offline evaluator/tuner for the triage shadow learner (LOCAL ONLY — gitignored).

  python triage_eval.py build    fetch the last 90 days of real triage
                                 decisions into triage_dataset.jsonl (read-only)
  python triage_eval.py tune     replay the dataset chronologically under many
                                 TUNING configs and report which learns best

The replay is honest: each decision is predicted using ONLY the decisions
that happened before it, then the model learns from it — exactly how the
live learner experiences the stream.
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
from datetime import datetime, timedelta
from typing import Optional

_DIR = os.path.dirname(os.path.abspath(__file__))
DATASET = os.path.join(_DIR, "triage_dataset.jsonl")

spec = importlib.util.spec_from_file_location(
    "tl", os.path.join(_DIR, "triage_learner.pyw"))
tl = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tl)

def all_dests(records: list[dict]) -> list[str]:
    """Outcome labels present in the dataset, most common first. Derived from
    the data rather than a fixed list, so this works on any Jira project."""
    counts: dict[str, int] = {}
    for r in records:
        counts[r["dest"]] = counts.get(r["dest"], 0) + 1
    return sorted(counts, key=lambda d: -counts[d])


def handoff_dests(records: list[dict], w=None) -> set[str]:
    """Outcomes that represent a handoff to another team, used to find when
    the triage process started. Taken from the watcher's discovered config."""
    if w is not None:
        return set(w.dests)
    return {r["dest"] for r in records}


# ---------------------------------------------------------------------------
# build — fetch the 90-day decision history (read-only)
# ---------------------------------------------------------------------------

def build() -> None:
    w = tl.ShadowWatcher()          # reuses config, Jira client, changelog logic
    # The watcher only works out this site's statuses when its main loop
    # starts, so do it here too — otherwise a fresh install has no handoff or
    # resolved statuses and every query below is malformed.
    if not w.ensure_workflow_known():
        print("Could not work out this project's workflow. Check projectKey "
              "and your credentials in jira_config.json, or set "
              "triageStatuses / resolvedStatuses by hand.")
        return
    print(f"project {w.project}: handoffs={w.dests or 'none'} "
          f"resolved={sorted(w.resolved)} moves={w.move_targets or 'none'}")
    cutoff = f"{datetime.now() - timedelta(days=tl.BACKFILL_DAYS):%Y-%m-%d}"
    records: list[dict] = []
    seen: set[str] = set()

    def rec_from(det: dict, dest: str, when: str, req_type: str) -> dict:
        return {"key": det["key"], "dest": dest, "when": when,
                "summary": det["summary"], "desc": det["desc"],
                "labels": det["labels"], "components": det["components"],
                "structured": det.get("structured", []),
                "reporter": det["reporter"], "reqType": req_type,
                "cloneProjects": [c["key"].split("-")[0]
                                  for c in det["clones"]]}

    # 1. handoffs to another team
    dest_list = ", ".join(f'"{d}"' for d in w.dests)
    jql = (f'project = {w.project} AND status CHANGED TO ({dest_list}) '
           f'AFTER -{tl.BACKFILL_DAYS}d ORDER BY updated DESC')
    # "status CHANGED TO ()" is not valid JQL; a project where everything is
    # resolved in place simply has nothing for this pass to find.
    for issue in (w.jira.search(jql, w._FIELDS, cap=tl.BACKFILL_CAP)
                  if w.dests else []):
        det = w._issue_details(issue)
        routed, _ = w._transitions(det["key"])
        req = det.get("requestType", "")
        for tr in routed:
            if tr["id"] in seen or tr["when"] < cutoff:
                continue
            seen.add(tr["id"])
            records.append(rec_from(det, tr["dest"], tr["when"], req))
    print(f"routings: {len(records)}")

    # 2. resolved inside the intake project, never handed off
    n0 = len(records)
    resolved_list = ", ".join(f'"{s}"' for s in sorted(w.resolved))
    jql = (f'project = {w.project} AND status CHANGED TO ({resolved_list}) '
           f'AFTER -{tl.BACKFILL_DAYS}d ORDER BY updated DESC')
    for issue in w.jira.search(jql, w._FIELDS, cap=tl.BACKFILL_CAP):
        det = w._issue_details(issue)
        routed, resolutions = w._transitions(det["key"])
        if routed:
            continue
        req = det.get("requestType", "")
        for tr in resolutions:
            if tr["id"] in seen or tr["when"] < cutoff:
                continue
            seen.add(tr["id"])
            records.append(rec_from(det, w.resolved_label, tr["when"], req))
    print(f"resolved in place: {len(records) - n0}")

    # 3. moved out to another project
    n0 = len(records)
    for target in w.move_targets:
        jql = (f'project = {target} AND updated >= -{tl.BACKFILL_DAYS}d '
               f'ORDER BY created DESC')
        label = w.moved_label(target)
        for issue in w.jira.search(jql, w._FIELDS, cap=tl.BACKFILL_CAP):
            det = w._issue_details(issue)
            moves, orig_summary = w._project_moves(det["key"])
            if not moves:
                continue
            if orig_summary:
                det["summary"] = f"{orig_summary} — {det['summary']}"
            for mv in moves:
                if mv["id"] in seen or mv["when"] < cutoff:
                    continue
                seen.add(mv["id"])
                records.append(rec_from(det, label, mv["when"],
                                        det.get("requestType", "")))
    print(f"moved to another project: {len(records) - n0}")

    records.sort(key=lambda r: r["when"])
    with open(DATASET, "w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"dataset: {len(records)} decisions -> {DATASET}")


# ---------------------------------------------------------------------------
# tune — chronological replay under many configs
# ---------------------------------------------------------------------------

WARMUP = 150     # don't score the first N (model barely trained)


def load_dataset() -> Optional[list[dict]]:
    """Records from the last `build`, or None with an explanation."""
    try:
        with open(DATASET, encoding="utf-8") as f:
            return [json.loads(line) for line in f if line.strip()]
    except FileNotFoundError:
        print(f"No dataset yet at {DATASET}\n"
              "Run this first:   python triage_eval.py build")
        return None
    except json.JSONDecodeError as exc:
        print(f"{DATASET} is corrupt ({exc}). Rebuild it with:   "
              "python triage_eval.py build")
        return None


def replay(records: list[dict], tuning: dict, warmup: int = WARMUP,
           misses: Optional[list] = None, score_start: Optional[int] = None,
           score_end: Optional[int] = None) -> dict:
    saved = dict(tl.TUNING)
    tl.TUNING.update(tuning)
    try:
        learner = tl.Learner({})
        scored: list[tuple[bool, int, str]] = []   # (correct, conf, actual)
        firm: list[tuple[bool, str]] = []          # non-review calls
        review_n = 0               # how many were flagged for human review
        confusion: dict[str, int] = {}
        start = warmup if score_start is None else score_start
        end = len(records) if score_end is None else score_end
        for i, r in enumerate(records):
            req_type = r.get("reqType", "")
            det = {"summary": r["summary"], "desc": r["desc"],
                   "labels": r["labels"], "components": r["components"],
                   "structured": r.get("structured", []),
                   "reporter": r["reporter"], "key": r["key"], "clones": []}
            toks = tl.features_for(det, req_type)
            title = tl.tokenize(r["summary"])
            p = learner.predict_ex(toks, req_type, r["reporter"], exemplar=title)
            dest, conf = p["dest"], p["conf"]
            correct = dest == r["dest"] if dest is not None else None
            if dest is not None and start <= i < end:
                scored.append((bool(correct), conf, r["dest"]))
                if p["review"]:
                    review_n += 1
                else:
                    firm.append((bool(correct), r["dest"]))
                if not correct:
                    pair = f"{r['dest']} predicted-as {dest}"
                    confusion[pair] = confusion.get(pair, 0) + 1
                    if misses is not None:
                        misses.append((r["dest"], dest, conf, r["summary"]))
            w = (int(tl.TUNING["mistakeWeight"])
                 if correct is False else 1)
            learner.learn(r["dest"], toks, req_type,
                          r.get("cloneProjects", []),
                          r["reporter"], weight=w, exemplar=title)
    finally:
        tl.TUNING.clear()
        tl.TUNING.update(saved)

    n = len(scored)
    acc = sum(c for c, _, _ in scored) / n if n else 0.0
    hi = [(c, conf) for c, conf, _ in scored if conf >= 90]
    cov90 = len(hi) / n if n else 0.0
    acc90 = (sum(c for c, _ in hi) / len(hi)) if hi else 0.0
    per: dict[str, list[int]] = {}
    for c, _, actual in scored:
        row = per.setdefault(actual, [0, 0])
        row[0] += 1
        row[1] += int(c)
    firm_per: dict[str, list[int]] = {}
    for c, actual in firm:
        row = firm_per.setdefault(actual, [0, 0])
        row[0] += 1
        row[1] += int(c)
    per_acc = {k: v[1] / v[0] for k, v in per.items()}
    firm_per_acc = {k: v[1] / v[0] for k, v in firm_per.items()}
    firm_acc = (sum(c for c, _ in firm) / len(firm)) if firm else 0.0
    return {"n": n, "acc": acc, "cov90": cov90, "acc90": acc90,
            "per": per_acc,
            "balanced": (sum(per_acc.values()) / len(per_acc))
            if per_acc else 0.0,
            "worst": min(per_acc.values(), default=0.0),
            "confusion": confusion,
            "firmCov": (len(firm) / n) if n else 0.0, "firmAcc": firm_acc,
            "firmBalanced": (sum(firm_per_acc.values()) / len(firm_per_acc))
            if firm_per_acc else 0.0,
            "firmWorst": min(firm_per_acc.values(), default=0.0),
            "reviewCov": (review_n / n) if n else 0.0}


DEFAULT_CFG = {"logOdds": 1, "summaryWeight": 6, "mistakeWeight": 3,
               "confTemp": 4.0, "reqSmooth": 10, "repSmooth": 20,
               "priorWeight": 0.5, "knnThresh": 0.7, "decayFactor": 0.997,
               "reviewConf": 75, "reviewMargin": 20,
               "requestTypeHintWeight": 3.0}


def review_bands() -> None:
    """Show how the 'review' flag splits reliable suggestions from the
    error-prone ones, across a few thresholds."""
    records = load_dataset()
    if records is None:
        return
    handoffs = handoff_dests(records)
    team = [r["when"] for r in records if r["dest"] in handoffs]
    if team:
        records = [r for r in records if r["when"] >= min(team)]
    warmup = min(WARMUP, len(records) // 5)

    print(f"replaying {len(records)} decisions "
          f"(scoring after {warmup}); overall vs the confident (non-review) band\n")
    print("reviewConf reviewMargin |  overall  | firm-cov firm-acc | review-cov")
    for rc in (70, 75, 80):
        for rm in (15, 20, 25):
            cfg = dict(DEFAULT_CFG, reviewConf=rc, reviewMargin=rm)
            m = replay(records, cfg, warmup)
            print(f"    {rc:>3}       {rm:>3}      | {m['acc']:6.1%}   | "
                  f"{m['firmCov']:6.1%}  {m['firmAcc']:6.1%}  | {m['reviewCov']:6.1%}")


def analyze(cfg: dict) -> None:
    records = load_dataset()
    if records is None:
        return
    handoffs = handoff_dests(records)
    team = [r["when"] for r in records if r["dest"] in handoffs]
    if team:
        start = min(team)
        records = [r for r in records if r["when"] >= start]
    warmup = min(WARMUP, len(records) // 5)
    misses: list = []
    m = replay(records, cfg, warmup, misses)
    print(f"acc={m['acc']:.1%}  cov90={m['cov90']:.1%}  acc90={m['acc90']:.1%}")
    print("\nconfusion (actual predicted-as guess):")
    for pair, n in sorted(m["confusion"].items(), key=lambda kv: -kv[1]):
        print(f"  {pair}: {n}")
    # Drill into whichever outcome this site gets wrong most often.
    per_dest: dict[str, int] = {}
    for actual, _guess, _conf, _summary in misses:
        per_dest[actual] = per_dest.get(actual, 0) + 1
    focus_dest = max(per_dest, key=per_dest.get) if per_dest else ""  # type: ignore[arg-type]
    print(f"\nsample misses for the weakest outcome ({focus_dest or 'n/a'}):")
    shown = 0
    for actual, guess, conf, summary in misses:
        if actual == focus_dest and shown < 20:
            print(f"  [{conf:>3}% {guess}] {summary[:80]}")
            shown += 1


def tune() -> None:
    records = load_dataset()
    if records is None:
        return

    # Handoffs may have started only recently: train from the first
    # handoff onward, else earlier resolutions teach the wrong lesson.
    handoffs = handoff_dests(records)
    team = [r["when"] for r in records if r["dest"] in handoffs]
    if team:
        start = min(team)
        before = len(records)
        records = [r for r in records if r["when"] >= start]
        print(f"process start detected: {start} "
              f"(dropped {before - len(records)} pre-process decisions)")

    if len(records) < 200:
        print(f"only {len(records)} decisions — too few to hold any out; "
              "run 'build' once the team has triaged more tickets")
        return
    warmup = min(WARMUP, len(records) // 5)
    validation_n = max(100, min(400, len(records) // 5))
    split = len(records) - validation_n
    tuning_records = records[:split]
    tuning_warmup = min(WARMUP, len(tuning_records) // 5)
    print(f"replaying {len(records)} decisions; selecting on {split} older "
          f"decisions and holding out {validation_n} recent decisions\n")

    # Same coordinate-descent search the live learner runs nightly, over the
    # same axes and the same ranking — so this tool and the learner can never
    # disagree about which config is best.
    axes  = tl.ShadowWatcher._TUNE_AXES
    score = tl.ShadowWatcher._tune_score
    seen: dict[tuple, tuple] = {}
    results = []

    def evaluate(cfg: dict):
        sig = tuple(cfg[k] for k in axes)
        if sig not in seen:
            m = replay(tuning_records, cfg, tuning_warmup)
            seen[sig] = m
            results.append((dict(cfg), m))
        return seen[sig]

    cur  = dict(DEFAULT_CFG)
    best = score(evaluate(cur))
    for _ in range(tl.ShadowWatcher._TUNE_PASSES):
        improved = False
        for axis, values in axes.items():
            for value in values:
                if cur.get(axis) == value:
                    continue
                trial = dict(cur, **{axis: value})
                s = score(evaluate(trial))
                if s > best:
                    best, cur, improved = s, trial, True
            if len(seen) >= tl.ShadowWatcher._TUNE_BUDGET:
                break
        if not improved or len(seen) >= tl.ShadowWatcher._TUNE_BUDGET:
            break
    print(f"searched {len(seen)} configs by coordinate descent\n")

    # Rank balanced first so small buckets such as BI/DEV cannot be hidden by
    # high-volume IT/DATA wins; confident-band quality breaks ties.
    results.sort(key=lambda cm: score(cm[1]), reverse=True)
    print("top configs  (bal = mean per-route accuracy | worst = weakest route | "
          "firm = non-review suggestions)")
    for cfg, m in results[:10]:
        print(f"  sw={cfg['summaryWeight']:>2} mw={cfg['mistakeWeight']} "
              f"ct={cfg['confTemp']:>4} kt={cfg['knnThresh']} "
              f"decay={cfg['decayFactor']} rc={cfg['reviewConf']} "
              f"rm={cfg['reviewMargin']} | bal={m['balanced']:.1%} "
              f"worst={m['worst']:.1%} acc={m['acc']:.1%} "
              f"firm={m['firmAcc']:.1%}/{m['firmCov']:.1%}")

    best_cfg, best = results[0]
    holdout = replay(records, best_cfg, warmup, score_start=split)
    print(f"\nBEST: {best_cfg}")
    print(f"  selected on {best['n']} older decisions")
    print(f"  recent holdout: acc={holdout['acc']:.1%} "
          f"bal={holdout['balanced']:.1%} worst={holdout['worst']:.1%} "
          f"firm={holdout['firmAcc']:.1%}/{holdout['firmCov']:.1%} "
          f"n={holdout['n']}")
    for dest in all_dests(records):
        if dest in holdout["per"]:
            print(f"  {dest}: {holdout['per'][dest]:.1%}")


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "tune"
    if mode == "build":
        build()
    elif mode == "tune":
        tune()
    elif mode == "review":
        review_bands()
    elif mode == "analyze":
        analyze({"summaryWeight": 4, "mistakeWeight": 2, "confTemp": 12.0,
                 "reqSmooth": 10, "repSmooth": 20})
    else:
        print(__doc__)
