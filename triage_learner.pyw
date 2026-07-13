#!/usr/bin/env python3
"""
Jira Triage Shadow Learner
==========================
Watches how the TAC team routes ITS tickets to "With BI" / "With DATA" /
"With DEV" and learns from those decisions — WITHOUT ever touching a ticket.

Flow being observed:
  1. Every ticket lands in the Technical Assistance Centre (ITS) at First Contact
  2. An agent clones it via Elements Sync to the right team workspace
     (BI-xxxx / DP-xxxxx / DEVW-xxxxx, linked as "Developer Escalations")
  3. Once the clone exists, the ITS master is moved to With BI/DATA/DEV
     and stays in the TAC
  4. Plain-IT tickets are the fourth outcome: they are never cloned or
     moved — they stay in the TAC and are simply Resolved there

What this script does:
  * Backfills from the last 90 days of routing history to seed its model
  * Predicts a destination for each NEW ticket at intake and logs
    "what I would have done"
  * When an agent routes the ticket, compares its prediction to the real
    decision, records hit/miss, and updates the model
  * Writes a human-readable TRIAGE_REPORT.md and an append-only
    triage_log.jsonl next to this script

STRICTLY READ-ONLY — the only Jira calls are GET and the search endpoint.
It never transitions, comments on, clones, edits or assigns any ticket.

Run:   pythonw triage_learner.pyw   (or double-click launch_learner.bat)
Stop:  right-click the purple ⊕ tray icon → Quit
"""

from __future__ import annotations

import ctypes
import json
import math
import os
import re
import sys
import threading
import traceback
from datetime import datetime, timedelta
from typing import Optional

import requests
from requests.auth import HTTPBasicAuth
from PIL import Image, ImageDraw
import pystray

# ---------------------------------------------------------------------------
# Files & constants
# ---------------------------------------------------------------------------

_DIR         = os.path.dirname(os.path.abspath(__file__))
CONFIG_FILE  = os.path.join(_DIR, "jira_config.json")
STATE_FILE   = os.path.join(_DIR, "triage_state.json")
LOG_FILE     = os.path.join(_DIR, "triage_log.jsonl")
REPORT_FILE  = os.path.join(_DIR, "TRIAGE_REPORT.md")
ERROR_LOG    = os.path.join(_DIR, "triage_learner.log")

POLL_SECONDS   = 60          # how often to look for new tickets / routings
BACKFILL_DAYS  = 90          # seed the model from this much history
BACKFILL_CAP   = 300         # max historical issues to learn from
OVERLAP_SECS   = 600         # re-scan overlap so nothing is missed
MIN_EXAMPLES   = 8           # per-destination examples needed before predicting

# Destination statuses on the ITS master ticket (overridable in config)
DEFAULT_DESTS   = ["With BI", "With DATA", "With DEV"]
RESOLVED_STATUS = "Resolved"          # the real Jira status plain-IT tickets end on
# Internal label only (NOT a Jira status): tickets resolved in the TAC that
# were never cloned or moved — just IT work.
TAC_DEST        = "IT (resolved in TAC)"

_STOPWORDS = {
    "the", "and", "for", "with", "from", "this", "that", "have", "has",
    "are", "was", "were", "been", "being", "will", "would", "should",
    "can", "could", "not", "but", "all", "any", "our", "your", "their",
    "there", "here", "when", "where", "what", "which", "who", "how",
    "please", "thanks", "thank", "hi", "hello", "regards", "kind",
    "need", "needs", "needed", "required", "request", "requested",
    "you", "its", "it's", "into", "onto", "per", "via", "due", "new",
}


def _log_error(msg: str) -> None:
    with open(ERROR_LOG, "a", encoding="utf-8") as f:
        f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S}  {msg}\n")


# ---------------------------------------------------------------------------
# Jira client (READ-ONLY)
# ---------------------------------------------------------------------------

class Jira:
    """Thin read-only wrapper. GET + search only — no mutations, ever."""

    def __init__(self, cfg: dict) -> None:
        self.domain  = cfg["domain"]
        self.session = requests.Session()
        self.session.auth = HTTPBasicAuth(cfg["email"], cfg["token"])
        self.session.headers["Accept"] = "application/json"

    def _url(self, path: str) -> str:
        return f"https://{self.domain}{path}"

    def get(self, path: str, params: Optional[dict] = None) -> Optional[dict]:
        r = self.session.get(self._url(path), params=params, timeout=15)
        return r.json() if r.ok else None

    def search(self, jql: str, fields: list[str], cap: int = 100) -> list[dict]:
        """POST /search/jql is Jira's read-only search — no data is modified."""
        issues: list[dict] = []
        token: Optional[str] = None
        while len(issues) < cap:
            body: dict = {"jql": jql, "fields": fields,
                          "maxResults": min(100, cap - len(issues))}
            if token:
                body["nextPageToken"] = token
            r = self.session.post(self._url("/rest/api/3/search/jql"),
                                  json=body, timeout=20)
            if not r.ok:
                break
            data = r.json()
            issues.extend(data.get("issues", []))
            token = data.get("nextPageToken")
            if not token or data.get("isLast", True):
                break
        return issues

    def changelog(self, key: str) -> list[dict]:
        """All changelog histories for an issue (paginated GET)."""
        out: list[dict] = []
        start = 0
        while True:
            data = self.get(f"/rest/api/3/issue/{key}/changelog",
                            {"startAt": start, "maxResults": 100})
            if not data:
                break
            out.extend(data.get("values", []))
            if data.get("isLast", True) or not data.get("values"):
                break
            start += len(data["values"])
        return out

    def request_type(self, key: str) -> str:
        """JSM request type name, or '' if not a customer request."""
        try:
            data = self.get(f"/rest/servicedeskapi/request/{key}")
            return (data or {}).get("requestType", {}).get("name", "")
        except Exception:
            return ""


# ---------------------------------------------------------------------------
# Text helpers
# ---------------------------------------------------------------------------

def flatten_adf(node) -> str:
    """Collapse an Atlassian Document Format tree to plain text."""
    if node is None:
        return ""
    if isinstance(node, str):
        return node
    parts: list[str] = []
    if isinstance(node, dict):
        if node.get("text"):
            parts.append(node["text"])
        for child in node.get("content", []):
            parts.append(flatten_adf(child))
    elif isinstance(node, list):
        parts.extend(flatten_adf(c) for c in node)
    return " ".join(p for p in parts if p)


def tokenize(text: str) -> list[str]:
    words = re.findall(r"[a-z][a-z0-9']{2,}", text.lower())
    return [w for w in words if w not in _STOPWORDS]


# ---------------------------------------------------------------------------
# The learner
# ---------------------------------------------------------------------------

class Learner:
    """Naive-Bayes-style keyword model over routing destinations."""

    def __init__(self, state: dict) -> None:
        m = state.setdefault("model", {})
        m.setdefault("priors", {})         # dest -> #examples
        m.setdefault("tokens", {})         # dest -> {token: count}
        m.setdefault("totals", {})         # dest -> total token count
        m.setdefault("reqTypes", {})       # dest -> {request type: count}
        m.setdefault("cloneProjects", {})  # dest -> {clone project key: count}
        self.m = m

    def learn(self, dest: str, tokens: list[str],
              req_type: str, clone_projects: list[str]) -> None:
        m = self.m
        m["priors"][dest] = m["priors"].get(dest, 0) + 1
        tok = m["tokens"].setdefault(dest, {})
        for t in tokens:
            tok[t] = tok.get(t, 0) + 1
        m["totals"][dest] = m["totals"].get(dest, 0) + len(tokens)
        if req_type:
            rt = m["reqTypes"].setdefault(dest, {})
            rt[req_type] = rt.get(req_type, 0) + 1
        for proj in clone_projects:
            cp = m["cloneProjects"].setdefault(dest, {})
            cp[proj] = cp.get(proj, 0) + 1

    def predict(self, tokens: list[str],
                req_type: str) -> tuple[Optional[str], int, str]:
        """Return (destination, confidence %, note). Destination is None
        while the model doesn't have enough examples to be worth logging."""
        m      = self.m
        ready  = [d for d, n in m["priors"].items() if n >= MIN_EXAMPLES]
        if len(ready) < 2:
            return None, 0, "still learning — not enough observed examples yet"

        total_examples = sum(m["priors"][d] for d in ready)
        vocab = len({t for d in ready for t in m["tokens"].get(d, {})}) or 1
        scores: dict[str, float] = {}
        for d in ready:
            score = math.log(m["priors"][d] / total_examples)
            tok, tot = m["tokens"].get(d, {}), m["totals"].get(d, 0)
            for t in tokens:
                score += math.log((tok.get(t, 0) + 1) / (tot + vocab))
            if req_type:
                rt = m["reqTypes"].get(d, {})
                n_rt = sum(rt.values())
                score += math.log((rt.get(req_type, 0) + 1) / (n_rt + 10))
            scores[d] = score

        best = max(scores, key=scores.get)  # type: ignore[arg-type]
        # softmax over log scores -> a rough relative confidence
        mx    = scores[best]
        exps  = {d: math.exp(s - mx) for d, s in scores.items()}
        conf  = int(round(100 * exps[best] / sum(exps.values())))
        if best == TAC_DEST:
            note = "keep in TAC — plain IT, resolve here, no clone"
        else:
            proj = self.clone_project_for(best)
            note = (f"clone to {proj} then move master to {best}"
                    if proj else f"move master to {best}")
        return best, conf, note

    def clone_project_for(self, dest: str) -> str:
        cp = self.m["cloneProjects"].get(dest, {})
        return max(cp, key=cp.get) if cp else ""  # type: ignore[arg-type]

    def top_keywords(self, dest: str, n: int = 12) -> list[str]:
        tok = self.m["tokens"].get(dest, {})
        return [t for t, _ in
                sorted(tok.items(), key=lambda kv: -kv[1])[:n]]


# ---------------------------------------------------------------------------
# Shadow watcher
# ---------------------------------------------------------------------------

class ShadowWatcher:
    def __init__(self) -> None:
        with open(CONFIG_FILE, encoding="utf-8") as f:
            self.cfg = json.load(f)
        self.project  = self.cfg.get("projectKey", "ITS")
        self.dests    = self.cfg.get("triageStatuses", DEFAULT_DESTS)
        self.resolved = self.cfg.get("resolvedStatus", RESOLVED_STATUS)
        self.jira    = Jira(self.cfg)
        self.state   = self._load_state()
        self.learner = Learner(self.state)
        self.stop    = threading.Event()

    # -- state ---------------------------------------------------------------

    def _load_state(self) -> dict:
        try:
            with open(STATE_FILE, encoding="utf-8") as f:
                return json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            return {}

    def _save_state(self) -> None:
        tmp = STATE_FILE + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(self.state, f, indent=1)
        os.replace(tmp, STATE_FILE)

    def _log(self, record: dict) -> None:
        record["ts"] = f"{datetime.now():%Y-%m-%d %H:%M:%S}"
        with open(LOG_FILE, "a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")

    # -- issue helpers ---------------------------------------------------------

    _FIELDS = ["summary", "description", "labels", "components",
               "issuelinks", "reporter", "status"]

    def _issue_details(self, issue: dict) -> dict:
        """Normalise the bits we learn from."""
        f       = issue.get("fields", {})
        desc    = flatten_adf(f.get("description"))
        comps   = [c.get("name", "") for c in f.get("components", [])]
        labels  = f.get("labels", [])
        clones  = []
        for link in f.get("issuelinks", []):
            other = link.get("outwardIssue") or link.get("inwardIssue") or {}
            key   = other.get("key", "")
            if key and not key.startswith(self.project + "-"):
                clones.append({"key": key,
                               "linkType": link.get("type", {}).get("name", "")})
        return {
            "key":      issue.get("key", ""),
            "summary":  f.get("summary", ""),
            "desc":     desc[:800],
            "labels":   labels,
            "components": comps,
            "clones":   clones,
            "reporter": (f.get("reporter") or {}).get("displayName", ""),
        }

    def _tokens_for(self, det: dict, req_type: str) -> list[str]:
        text = " ".join([det["summary"], det["desc"], req_type,
                         " ".join(det["labels"]), " ".join(det["components"])])
        return tokenize(text)

    def _transitions(self, key: str) -> tuple[list[dict], list[dict]]:
        """(transitions into a destination status, transitions into Resolved)
        from the full changelog — a ticket that was ever routed to a team is
        never counted as a plain-TAC resolution."""
        routed: list[dict] = []
        resolved: list[dict] = []
        for h in self.jira.changelog(key):
            for item in h.get("items", []):
                if item.get("field") != "status":
                    continue
                to = item.get("toString", "")
                rec = {
                    "id":   f"{key}:{h.get('id', '')}",
                    "dest": to,
                    "by":   (h.get("author") or {}).get("displayName", ""),
                    "when": h.get("created", "")[:16].replace("T", " "),
                }
                if to in self.dests:
                    routed.append(rec)
                elif to == self.resolved:
                    resolved.append(rec)
        return routed, resolved

    # -- backfill --------------------------------------------------------------

    def backfill(self) -> None:
        if self.state.get("backfillDone"):
            return
        jql = (f'project = {self.project} AND status CHANGED TO '
               f'({", ".join(chr(34) + d + chr(34) for d in self.dests)}) '
               f'AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC')
        issues = self.jira.search(jql, self._FIELDS, cap=BACKFILL_CAP)
        seen: set[str] = set(self.state.setdefault("seenTransitions", []))
        learned = 0
        for issue in issues:
            if self.stop.is_set():
                return
            det = self._issue_details(issue)
            routed, _ = self._transitions(det["key"])
            for tr in routed:
                if tr["id"] in seen:
                    continue
                seen.add(tr["id"])
                req_type = self.jira.request_type(det["key"])
                toks     = self._tokens_for(det, req_type)
                projs    = [c["key"].split("-")[0] for c in det["clones"]]
                self.learner.learn(tr["dest"], toks, req_type, projs)
                learned += 1
                self._log({"event": "backfill", "key": det["key"],
                           "summary": det["summary"], "dest": tr["dest"],
                           "by": tr["by"], "when": tr["when"],
                           "requestType": req_type,
                           "clones": [c["key"] for c in det["clones"]]})

        # Phase 2: plain-IT tickets — resolved inside the TAC, never
        # cloned or moved to a team
        jql2 = (f'project = {self.project} AND status CHANGED TO '
                f'"{self.resolved}" AFTER -{BACKFILL_DAYS}d '
                f'ORDER BY updated DESC')
        for issue in self.jira.search(jql2, self._FIELDS, cap=BACKFILL_CAP):
            if self.stop.is_set():
                return
            det = self._issue_details(issue)
            routed, resolutions = self._transitions(det["key"])
            if routed:      # went to a team at some point — learned above
                continue
            for tr in resolutions:
                if tr["id"] in seen:
                    continue
                seen.add(tr["id"])
                req_type = self.jira.request_type(det["key"])
                toks     = self._tokens_for(det, req_type)
                self.learner.learn(TAC_DEST, toks, req_type, [])
                learned += 1
                self._log({"event": "backfill", "key": det["key"],
                           "summary": det["summary"], "dest": TAC_DEST,
                           "by": tr["by"], "when": tr["when"],
                           "requestType": req_type, "clones": []})
        self.state["seenTransitions"] = list(seen)
        self.state["backfillDone"]    = True
        self.state["backfillCount"]   = learned
        self._save_state()
        self.write_report()

    # -- live cycle --------------------------------------------------------------

    def _window_jql_time(self) -> str:
        last = self.state.get("lastPollEpoch", 0)
        start = max(datetime.now() - timedelta(days=7),
                    datetime.fromtimestamp(last) - timedelta(seconds=OVERLAP_SECS)
                    if last else datetime.now() - timedelta(hours=2))
        return f'"{start:%Y/%m/%d %H:%M}"'

    def poll_once(self) -> None:
        since = self._window_jql_time()

        # 1. NEW tickets at intake — log what we WOULD have done
        predicted: dict = self.state.setdefault("predicted", {})
        for issue in self.jira.search(
                f'project = {self.project} AND created >= {since}',
                self._FIELDS, cap=50):
            key = issue.get("key", "")
            if not key or key in predicted:
                continue
            det      = self._issue_details(issue)
            req_type = self.jira.request_type(key)
            toks     = self._tokens_for(det, req_type)
            dest, conf, note = self.learner.predict(toks, req_type)
            predicted[key] = {"dest": dest, "conf": conf,
                              "at": f"{datetime.now():%Y-%m-%d %H:%M}"}
            self._log({"event": "would_have_done", "key": key,
                       "summary": det["summary"], "requestType": req_type,
                       "prediction": dest, "confidence": conf, "plan": note})

        # 2. Actual routings — resolve predictions and learn
        seen: set[str] = set(self.state.setdefault("seenTransitions", []))
        stats = self.state.setdefault("stats", {"predictions": 0, "correct": 0})
        dest_list = ", ".join(chr(34) + d + chr(34) for d in self.dests)
        for issue in self.jira.search(
                f'project = {self.project} AND status CHANGED TO ({dest_list}) '
                f'AFTER {since}', self._FIELDS, cap=50):
            det = self._issue_details(issue)
            routed, _ = self._transitions(det["key"])
            for tr in routed:
                if tr["id"] in seen:
                    continue
                seen.add(tr["id"])
                req_type = self.jira.request_type(det["key"])
                toks     = self._tokens_for(det, req_type)
                projs    = [c["key"].split("-")[0] for c in det["clones"]]
                self.learner.learn(tr["dest"], toks, req_type, projs)
                self._resolve_prediction(det, tr["dest"], tr, req_type,
                                         predicted, stats,
                                         clones=[c["key"] for c in det["clones"]])

        # 3. Plain-IT tickets resolved inside the TAC (never cloned/moved)
        #    — the fourth outcome we learn from
        for issue in self.jira.search(
                f'project = {self.project} AND status CHANGED TO '
                f'"{self.resolved}" AFTER {since}', self._FIELDS, cap=50):
            det = self._issue_details(issue)
            routed, resolutions = self._transitions(det["key"])
            if routed:      # team ticket — handled by the routing pass
                continue
            for tr in resolutions:
                if tr["id"] in seen:
                    continue
                seen.add(tr["id"])
                req_type = self.jira.request_type(det["key"])
                toks     = self._tokens_for(det, req_type)
                self.learner.learn(TAC_DEST, toks, req_type, [])
                self._resolve_prediction(det, TAC_DEST, tr, req_type,
                                         predicted, stats, clones=[])

        # housekeeping
        self.state["seenTransitions"] = list(seen)[-8000:]
        cutoff = f"{datetime.now() - timedelta(days=30):%Y-%m-%d}"
        self.state["predicted"] = {k: v for k, v in predicted.items()
                                   if v.get("at", "9999") >= cutoff}
        self.state["lastPollEpoch"] = datetime.now().timestamp()
        self._save_state()
        self.write_report()

    def _resolve_prediction(self, det: dict, dest: str, tr: dict,
                            req_type: str, predicted: dict, stats: dict,
                            clones: list[str]) -> None:
        """Score our earlier guess against the human decision and log it."""
        pred    = predicted.get(det["key"], {})
        p_dest  = pred.get("dest")
        correct = None
        if p_dest is not None:
            correct = (p_dest == dest)
            stats["predictions"] += 1
            stats["correct"]     += int(correct)
        self._log({"event": "routed", "key": det["key"],
                   "summary": det["summary"], "dest": dest,
                   "by": tr["by"], "when": tr["when"],
                   "requestType": req_type, "clones": clones,
                   "predicted": p_dest, "predictionCorrect": correct})

    # -- report --------------------------------------------------------------

    def write_report(self) -> None:
        m     = self.learner.m
        stats = self.state.get("stats", {"predictions": 0, "correct": 0})
        n, c  = stats["predictions"], stats["correct"]
        acc   = f"{100 * c // n}% ({c}/{n})" if n else "no resolved predictions yet"

        lines = [
            "# Triage Shadow Learner — report",
            "",
            f"_Updated {datetime.now():%Y-%m-%d %H:%M:%S} · shadow mode — "
            "no tickets are ever actioned._",
            "",
            "## Learned routing map",
            "",
            "| Outcome | Clone goes to | Observed examples |",
            "|---|---|---|",
        ]
        for dest in self.dests:
            proj = self.learner.clone_project_for(dest) or "?"
            lines.append(f"| {dest} | `{proj}-…` | {m['priors'].get(dest, 0)} |")
        lines.append(f"| {TAC_DEST} | stays in TAC — no clone | "
                     f"{m['priors'].get(TAC_DEST, 0)} |")

        lines += ["", f"## Prediction accuracy: {acc}", "",
                  "## Signals per destination", ""]
        for dest in self.dests + [TAC_DEST]:
            kws = ", ".join(self.learner.top_keywords(dest)) or "—"
            rts = m["reqTypes"].get(dest, {})
            top_rt = ", ".join(f"{k} ({v})" for k, v in
                               sorted(rts.items(), key=lambda kv: -kv[1])[:4]) or "—"
            lines += [f"### {dest}",
                      f"- **Top keywords:** {kws}",
                      f"- **Request types:** {top_rt}", ""]

        # Recent decisions from the tail of the jsonl log
        lines += ["## Recent activity (newest first)", ""]
        try:
            with open(LOG_FILE, encoding="utf-8") as f:
                tail = f.readlines()[-60:]
            shown = 0
            for raw in reversed(tail):
                if shown >= 15:
                    break
                rec = json.loads(raw)
                ev  = rec.get("event")
                if ev == "routed":
                    p = rec.get("predicted")
                    mark = ("✅" if rec.get("predictionCorrect")
                            else "❌" if rec.get("predictionCorrect") is False
                            else "•")
                    pred_txt = f" — I said **{p}** {mark}" if p else ""
                    lines.append(
                        f"- `{rec['key']}` **{rec['dest']}** by {rec.get('by', '?')}"
                        f" (clones: {', '.join(rec.get('clones', [])) or 'none'})"
                        f"{pred_txt}  \n  {rec.get('summary', '')[:90]}")
                    shown += 1
                elif ev == "would_have_done" and rec.get("prediction"):
                    lines.append(
                        f"- `{rec['key']}` would have done: **{rec['plan']}**"
                        f" ({rec['confidence']}% confident)  \n"
                        f"  {rec.get('summary', '')[:90]}")
                    shown += 1
        except FileNotFoundError:
            lines.append("_No activity logged yet._")

        with open(REPORT_FILE, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")

    # -- main loop --------------------------------------------------------------

    def run_forever(self) -> None:
        try:
            self.backfill()
        except Exception:
            _log_error("backfill failed:\n" + traceback.format_exc())
        while not self.stop.is_set():
            try:
                self.poll_once()
            except Exception:
                _log_error("poll failed:\n" + traceback.format_exc())
            self.stop.wait(POLL_SECONDS)


# ---------------------------------------------------------------------------
# Tray icon + entry point
# ---------------------------------------------------------------------------

def _tray_image() -> Image.Image:
    img  = Image.new("RGBA", (128, 128), (0, 0, 0, 0))
    d    = ImageDraw.Draw(img)
    d.ellipse([4, 4, 124, 124], fill=(128, 90, 213))       # purple = learner
    d.ellipse([40, 40, 88, 88], outline=(255, 255, 255), width=8)
    d.line([64, 16, 64, 44], fill=(255, 255, 255), width=8)
    return img


def main() -> None:
    # single-instance guard
    ctypes.windll.kernel32.CreateMutexW(None, False, "JiraTriageLearner")
    if ctypes.windll.kernel32.GetLastError() == 183:        # ERROR_ALREADY_EXISTS
        return

    watcher = ShadowWatcher()
    worker  = threading.Thread(target=watcher.run_forever, daemon=True)
    worker.start()

    def _open_report() -> None:
        if os.path.exists(REPORT_FILE):
            os.startfile(REPORT_FILE)

    def _quit(icon: pystray.Icon) -> None:
        watcher.stop.set()
        icon.stop()

    icon = pystray.Icon(
        "TriageLearner", _tray_image(), "Triage Learner (shadow mode)",
        pystray.Menu(
            pystray.MenuItem("Open report", lambda: _open_report(), default=True),
            pystray.MenuItem("Open log folder", lambda: os.startfile(_DIR)),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem("Quit", _quit),
        ))
    icon.run()


if __name__ == "__main__":
    main()
