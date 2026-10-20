"""Local mistake review. Annotations do not relabel training data or edit Jira."""

from __future__ import annotations

import argparse
import json
from collections import Counter
from datetime import datetime
from pathlib import Path


def prioritised_cases(cases, limit=20, per_pair=2):
    """Frequent confusions first; diverse, high-confidence examples per pair."""
    groups = {}
    for key, case in cases.items():
        groups.setdefault((case["predicted"], case["actual"]), []).append((key, case))

    def priority(item):
        key, row = item
        confidence = row.get("confidence")
        confidence = confidence if isinstance(confidence, (int, float)) else -1
        return confidence, str(row.get("ts") or row.get("when") or ""), key

    ordered = sorted(groups.values(), key=lambda items: (len(items), max(priority(i) for i in items)), reverse=True)
    selected = []
    for items in ordered:
        titles = set()
        chosen = 0
        for key, row in sorted(items, key=priority, reverse=True):
            title = str(row.get("summary") or key).strip().casefold()
            if title in titles:
                continue
            titles.add(title)
            selected.append((key, row, len(items)))
            chosen += 1
            if chosen >= per_pair:
                break
    return selected[:max(0, limit)]


def review_markdown(selected, pending_count):
    def clean(value):
        result = str(value if value is not None and value != "" else "unknown")
        result = result.replace("\n", " ").replace("\r", " ").replace("<", "&lt;").replace(">", "&gt;")
        for character in "\\`*_{}[]()#!|":
            result = result.replace(character, "\\" + character)
        return result

    lines = ["# Triage mistake review queue", "",
             f"Generated {datetime.now():%Y-%m-%d %H:%M:%S}; {pending_count} pending mistakes.", "",
             "Frequent confusion pairs first, then higher-confidence examples. Repeated titles are sampled once per pair. "
             "Counts are historical events, not unique tickets. This file contains private ticket data; do not share or commit it.", "",
             "Human checklist: confirm the recorded destination reflects the intended routing; "
             "check whether intake information was sufficient; flag ambiguous or incorrect labels. "
             "Automated notes are not human decisions and never change training labels.", "",
             "Record your conclusion locally with `python triage_review.py --case CASE_ID --note \"Your explanation\" "
             "--verdict confirmed_routing` (alternatives: `ambiguous`, `label_issue`).", ""]
    for key, row, count in selected:
        analysis = row.get("analysis") or {}
        facts = analysis.get("observedFacts", {})
        lines += [f"## {clean(key)}", "",
                  f"- Suggestion: {clean(row['predicted'])}; observed destination: {clean(row['actual'])}.",
                  f"- Confidence: {clean(row.get('confidence'))}; request type: {clean(row.get('requestType'))}.",
                  f"- Observed: {clean(row.get('ts') or row.get('when'))}; {count} mistakes share this confusion pair.",
                  f"- Summary: {clean(row.get('summary'))}",
                  f"- Feature provenance: {clean(facts.get('featureOrigin'))}."]
        if analysis.get("note"):
            lines.append(f"- Automated note (awaiting human confirmation): {clean(analysis['note'])}")
        lines.append("")
    return "\n".join(lines)


def analysis_evidence(history, cases):
    """Aggregate this installation's observations, without interpreting names."""
    request_routes = {}
    for row in history:
        request_type = row.get("requestType", row.get("reqType", ""))
        if request_type and row.get("dest"):
            counts = request_routes.setdefault(request_type, {})
            counts[row["dest"]] = counts.get(row["dest"], 0) + 1
    pairs = Counter((row["predicted"], row["actual"]) for row in cases.values())
    return {"requestRoutes": request_routes, "pairs": pairs}


def analyse_case(case, history, evidence=None):
    """Record reproducible routing facts and a follow-up, not a new label."""
    request_type = history.get("requestType", history.get("reqType", case.get("requestType", "")))
    actual, guess = case["actual"], case["predicted"]
    facts = {"predicted": guess, "actual": actual,
             "requestType": request_type, "confidence": case.get("confidence"),
             "featureOrigin": history.get("featureOrigin", "unrecorded")}
    focus = f"The recorded suggestion was {guess}; the observed destination was {actual}."
    evidence = evidence or {}
    counts = evidence.get("requestRoutes", {}).get(request_type, {})
    if counts:
        facts["requestTypeOutcomes"] = dict(counts)
        focus += f" This request type has {sum(counts.values())} historical routing observations across {len(counts)} outcomes."
    pair_count = evidence.get("pairs", {}).get((guess, actual), 0)
    if pair_count:
        facts["matchingMistakes"] = pair_count
        focus += f" This suggestion/destination pair occurs in {pair_count} mistakes in the selected review window."
    focus += " Compare the original intake features with both outcomes; these retrospective counts do not establish the cause or a routing rule."
    if facts["featureOrigin"] != "intake":
        focus += " Original intake features are not confirmed, so later ticket edits may affect comparisons."
    return {"source": "automated_log_analysis", "requiresHumanReview": True,
            "observedFacts": facts, "note": focus}


def cases_from_log(path):
    cases = {}
    for source in [Path(str(path) + ".old"), Path(path)]:
        if not source.exists():
            continue
        for line in source.read_text(encoding="utf-8").splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if (
                isinstance(row, dict)
                and row.get("event") == "routed"
                and row.get("predicted")
                and row.get("predictionCorrect") is False
            ):
                case = dict(row, actual=row.get("dest"))
                cases[f"{row['key']}@{row.get('when', row.get('ts', ''))}"] = case
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--case", help="Case ID printed in the queue")
    parser.add_argument("--note", help="Human explanation of the confusion")
    parser.add_argument("--verdict", choices=("confirmed_routing", "ambiguous", "label_issue"),
                        help="Human assessment, saved with --case and --note; never relabels training data")
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--per-pair", type=int, default=2, help="Maximum representative examples per confusion pair")
    parser.add_argument("--export", help="Write the prioritised queue to a private Markdown (.md) file")
    parser.add_argument("--since", default="", help="Include cases observed on/after YYYY-MM-DD")
    parser.add_argument("--analyse", "--analyze", action="store_true",
                        help="Save evidence-based mistake analyses; cases remain pending human review")
    parser.add_argument("--predicted", help="Optionally analyse only suggestions for this exact outcome label")
    args = parser.parse_args()
    if args.per_pair < 1:
        parser.error("--per-pair must be at least 1")
    root = Path(__file__).resolve().parent
    path = root / "triage_reviews.json"
    notes = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    cases = cases_from_log(root / "triage_log.jsonl")
    state_path = root / "triage_state.json"
    if state_path.exists():
        state = json.loads(state_path.read_text(encoding="utf-8"))
        for row in state.get("reviewCases", []):
            cases.setdefault(f"{row['key']}@{row['when']}", dict(row, ts=row["when"]))
    if args.case or args.note or args.verdict:
        if args.case not in cases or not args.note:
            parser.error("Supply a known --case ID and a non-empty --note")
        # Import only for an explicit annotation write, preserving the app's
        # tested atomic writer and keeping listing independent of GUI imports.
        from triage_eval import tl

        existing = dict(notes.get(args.case, {}))
        existing.update(note=args.note, at=datetime.now().isoformat(), source="human")
        if args.verdict:
            existing["verdict"] = args.verdict
        notes[args.case] = existing
        tl._atomic_json_write(str(path), notes)
        print("Annotation saved locally.")
    if args.since:
        try:
            datetime.strptime(args.since, "%Y-%m-%d")
        except ValueError:
            parser.error("--since must be YYYY-MM-DD")
        cases = {k: v for k, v in cases.items() if v.get("ts", v.get("when", ""))[:10] >= args.since}
    if args.analyse:
        from triage_eval import tl

        records = tl.ShadowWatcher._load_history() or []
        history = {(r.get("key"), r.get("when")): r for r in records}
        evidence = analysis_evidence(records, cases)
        count = 0
        for key, case in cases.items():
            if (args.predicted and case["predicted"] != args.predicted) or notes.get(key, {}).get("note"):
                continue
            saved = dict(notes.get(key, {}))
            saved["analysis"] = dict(analyse_case(case, history.get((case.get("key"), case.get("when")), {}), evidence),
                                     at=datetime.now().isoformat())
            notes[key] = saved
            count += 1
        tl._atomic_json_write(str(path), notes)
        print(f"Saved {count} automated analyses; human review is still pending.")
    pending = {k: dict(v, analysis=notes.get(k, {}).get("analysis"))
               for k, v in cases.items() if not notes.get(k, {}).get("note")}
    pairs = Counter((v["predicted"], v["actual"]) for v in pending.values())
    human = sum(bool(row.get("note")) for row in notes.values())
    automated = sum(bool(row.get("analysis")) for row in notes.values())
    print(f"{len(pending)} unreviewed mistakes; {human} human annotations; {automated} automated analyses")
    for (guess, actual), n in pairs.most_common(8):
        print(f"  {guess} -> {actual}: {n}")
    selected = prioritised_cases(pending, args.limit, args.per_pair)
    if args.export:
        target = Path(args.export).resolve()
        if target.suffix.lower() != ".md":
            parser.error("--export must name a Markdown (.md) file")
        if target.exists() and not target.read_text(encoding="utf-8").startswith("# Triage mistake review queue\n"):
            parser.error("Refusing to overwrite a file that is not a generated review queue")
        target.write_text(review_markdown(selected, len(pending)), encoding="utf-8")
        print(f"Private review queue saved to {target}; do not share or commit it.")
    for key, row, count in selected:
        print(
            f"\n{key}\n  {row['predicted']} -> {row['actual']}\n  {row.get('summary', '')[:160]}"
        )
        print(f"  Confidence: {row.get('confidence', 'unknown')}; {count} mistakes share this pair")
        if notes.get(key, {}).get("analysis"):
            print("  Analysis (awaiting human review): " + notes[key]["analysis"]["note"])


if __name__ == "__main__":
    main()
