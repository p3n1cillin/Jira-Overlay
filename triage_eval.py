#!/usr/bin/env python3
"""
Offline evaluator/tuner for the triage shadow learner (private local dataset).

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
from source_layout import activate
activate(os.path.dirname(os.path.abspath(__file__)))
from triage_support import threshold_experiment
from datetime import timedelta
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
    try:
        with open(tl.CONFIG_FILE, encoding="utf-8") as f:
            config = json.load(f)
        if "triageStatuses" in config:
            return set(tl.as_name_list(config["triageStatuses"]))
    except (OSError, ValueError, TypeError):
        pass
    # Without discovered configuration, only explicit record evidence is
    # usable. An outcome's English name does not establish its workflow role.
    return {r["dest"] for r in records
            if r.get("cloneProjects") or r.get("isHandoff") is True}


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
    # Same clock as the learner's own windows: Jira evaluates datetime
    # literals against the account's profile timezone, so a workstation-
    # local cutoff would silently shift what "last 90 days" means.
    cutoff = f"{w._jira_now() - timedelta(days=tl.BACKFILL_DAYS):%Y-%m-%d}"
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
    # A status name is a live-discovered value (or a hand-edited config
    # entry), same as everywhere else this query shape is built in
    # triage_learner.pyw - jql_quote keeps a literal quote in one from
    # breaking out of the CHANGED TO (...) clause.
    dest_list = ", ".join(tl.jql_quote(d) for d in w.dests)
    jql = (f'project = {w.project} AND status CHANGED TO ({dest_list}) '
           f'AFTER -{tl.BACKFILL_DAYS}d ORDER BY updated DESC')
    # "status CHANGED TO ()" is not valid JQL; a project where everything is
    # resolved in place simply has nothing for this pass to find.
    skipped = 0
    for issue in (w.jira.search(jql, w._FIELDS, cap=tl.BACKFILL_CAP)
                  if w.dests else []):
        # One unusable issue must not abort a pass that runs to thousands
        # of calls — the same guard the learner's backfill carries.
        try:
            det = w._issue_details(issue)
            routed, _ = w._transitions(det["key"])
            req = det.get("requestType", "")
            for tr in routed:
                if tr["id"] in seen or tr["when"] < cutoff:
                    continue
                seen.add(tr["id"])
                records.append(rec_from(det, tr["dest"], tr["when"], req))
        except Exception:
            skipped += 1
    if skipped:
        print(f"routings: {len(records)} ({skipped} unusable issue(s) skipped)")
    else:
        print(f"routings: {len(records)}")

    # 2. resolved inside the intake project, never handed off
    n0 = len(records)
    resolved_list = ", ".join(tl.jql_quote(s) for s in sorted(w.resolved))
    jql = (f'project = {w.project} AND status CHANGED TO ({resolved_list}) '
           f'AFTER -{tl.BACKFILL_DAYS}d ORDER BY updated DESC')
    for issue in w.jira.search(jql, w._FIELDS, cap=tl.BACKFILL_CAP):
        try:
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
        except Exception:
            skipped += 1
    print(f"resolved in place: {len(records) - n0}")

    # 3. moved out to another project
    n0 = len(records)
    for target in w.move_targets:
        jql = (f'project = {target} AND updated >= -{tl.BACKFILL_DAYS}d '
               f'ORDER BY created DESC')
        label = w.moved_label(target)
        for issue in w.jira.search(jql, w._FIELDS, cap=tl.BACKFILL_CAP):
            try:
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
            except Exception:
                skipped += 1
    if skipped:
        print(f"moved to another project: {len(records) - n0} "
              f"({skipped} unusable issue(s) skipped overall)")
    else:
        print(f"moved to another project: {len(records) - n0}")

    records.sort(key=lambda r: r["when"])
    # Atomic replace: a crash mid-write leaves the previous dataset intact
    # rather than a truncated file every later command trips over.
    tmp = DATASET + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    os.replace(tmp, DATASET)
    print(f"dataset: {len(records)} decisions -> {DATASET}")


# ---------------------------------------------------------------------------
# tune — chronological replay under many configs
# ---------------------------------------------------------------------------

WARMUP = 150     # don't score the first N (model barely trained)


def load_dataset() -> Optional[list[dict]]:
    """Use live history by default; TRIAGE_DATASET selects an explicit dataset."""
    selected = os.environ.get("TRIAGE_DATASET")
    if not selected and os.path.exists(tl.HISTORY_FILE):
        return tl.ShadowWatcher._load_history()
    try:
        with open(selected or DATASET, encoding="utf-8") as f:
            records = [json.loads(line) for line in f if line.strip()]
            return sorted(records, key=lambda r: r.get("when", ""))
    except FileNotFoundError:
        return tl.ShadowWatcher._load_history()
    except json.JSONDecodeError as exc:
        print(f"{DATASET} is corrupt ({exc}). Rebuild it with:   "
              "python triage_eval.py build")
        return None


def replay(records: list[dict], tuning: dict, warmup: int = WARMUP,
           misses: Optional[list] = None, score_start: Optional[int] = None,
           score_end: Optional[int] = None) -> dict:
    watcher = object.__new__(tl.ShadowWatcher)
    watcher.dests = list(handoff_dests(records))
    m = watcher._replay_history(records, tuning,
        score_start=warmup if score_start is None else score_start, score_end=score_end)
    hi = [p for p in m["samples"] if p["conf"] >= 90]
    m["cov90"] = len(hi) / m["n"] if m["n"] else 0.0
    m["acc90"] = sum(p["correct"] for p in hi) / len(hi) if hi else 0.0
    m["reviewCov"] = 1 - m["firmCov"]
    m["confusion"] = {}
    for p in m["samples"]:
        if not p["correct"]:
            pair = f"{p['actual']} predicted-as {p['predicted']}"
            m["confusion"][pair] = m["confusion"].get(pair, 0) + 1
            if misses is not None:
                misses.append((p["actual"], p["predicted"], p["conf"], p["summary"]))
    return m


DEFAULT_CFG = {"logOdds": 1, "summaryWeight": 6, "mistakeWeight": 3,
               "confTemp": 4.0, "reqSmooth": 10, "repSmooth": 20,
               "priorWeight": 0.5, "knnThresh": 0.7, "decayFactor": 0.997,
               "reviewConf": 75, "reviewMargin": 20,
               "requestTypeHintWeight": 3.0, "reporterWeight": 1.0,
               "escalationSpecialist": 1,
               "escalationPriorWeight": 0.25}


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
    if not records or len(records) < 300:
        print("At least 300 labelled decisions are required.")
        return
    saved = dict(tl.TUNING)
    try:
        if os.path.exists(tl.STATE_FILE):
            with open(tl.STATE_FILE, encoding="utf-8") as f:
                tl.TUNING.update(json.load(f).get("tunedConfig", {}))
        watcher = object.__new__(tl.ShadowWatcher)
        watcher.dests = list(handoff_dests(records))
        best = watcher._self_tune(records[-tl.TUNE_WINDOW:])
        if best:
            report = {k: best[k] for k in
                ("cfg", "gateReason", "incumbentGateScore", "challengerGateScore",
                 "holdoutN", "finalHoldoutN", "acc", "macroF1", "segments", "featureOrigins")}
            report["finalAccuracy"] = best["finalHoldout"]["acc"]
            print(json.dumps(report, indent=2))
    finally:
        tl.TUNING.clear()
        tl.TUNING.update(saved)


def thresholds():
    records = load_dataset()
    if not records or len(records) < 600:
        print("At least 600 labelled decisions are needed for separate gate and final windows.")
        return
    cfg = dict(tl.TUNING)
    if os.path.exists(tl.STATE_FILE):
        with open(tl.STATE_FILE, encoding="utf-8") as f:
            cfg.update(json.load(f).get("tunedConfig", {}))
    start = len(records) - min(1200, len(records) // 3)
    final_start = len(records) - min(400, (len(records) - start) // 3)
    gate = replay(records, cfg, score_start=start, score_end=final_start)
    final = replay(records, cfg, score_start=final_start)
    print("Experimental thresholds; these do not change deployed policy.")
    print(json.dumps(threshold_experiment(gate["samples"], final["samples"]), indent=2))


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "tune"
    if mode == "build":
        build()
    elif mode == "tune":
        tune()
    elif mode == "review":
        review_bands()
    elif mode == "thresholds":
        thresholds()
    elif mode == "analyze":
        analyze({"summaryWeight": 4, "mistakeWeight": 2, "confTemp": 12.0,
                 "reqSmooth": 10, "repSmooth": 20})
    else:
        print(__doc__)
