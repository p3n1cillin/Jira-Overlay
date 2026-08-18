#!/usr/bin/env python3
"""
Jira Triage Shadow Learner
==========================
Watches how your team triages tickets in a Jira Service Management project and
learns to predict where each new ticket will end up — WITHOUT ever touching a
ticket.

It assumes nothing about your workflow. On first run it inspects your own Jira
and works out the shape of your triage process for itself:

  * the intake project            — the service desk the overlay monitors
  * the handoff statuses          — statuses tickets are moved to and then
                                    rest in — "Escalated to Networks",
                                    "3rd Line", "With Development", whatever
                                    yours are called — discovered by watching
                                    where tickets actually settle
  * the resolved statuses         — every status in a "done" category
  * move-target projects          — other projects tickets get moved into,
                                    detected from the issue's own key change
  * custom fields                 — Request Type and any extra context fields
                                    are looked up by name, never by a
                                    hard-coded customfield_NNNNN id

Every one of those can be pinned in jira_config.json if you would rather state
it explicitly than have it discovered (see "triage settings" in the README).

So a ticket's outcome is always one of:
  1. moved to a handoff status inside the intake project  -> that status name
  2. resolved inside the intake project, never handed off -> "Resolved in <PROJ>"
  3. moved out to another project                         -> "Moved to <PROJ>"

What this script does:
  * Backfills from recent triage history to seed its model
  * Predicts an outcome for each NEW ticket at intake and logs
    "what I would have done"
  * When the team actually triages the ticket, compares its prediction to the
    real decision, records hit/miss, and updates the model
  * Re-tunes itself nightly against its own accumulated history
  * Writes a human-readable TRIAGE_REPORT.md and an append-only
    triage_log.jsonl next to this script

STRICTLY READ-ONLY — the only Jira calls are GET and the search endpoint.
It never transitions, comments on, clones, edits or assigns any ticket.

Run:   started by launch.bat alongside the overlay
Stop:  right-click the purple ⊕ tray icon → Quit
"""

from __future__ import annotations

import ctypes
import json
import math
import os
import re
import subprocess
import threading
import time
import traceback
import winreg
from collections import Counter
from datetime import date, datetime, timedelta
from typing import Optional

try:
    import requests
    from requests.auth import HTTPBasicAuth
    from PIL import Image, ImageDraw
    import pystray
except ImportError as _exc:
    # The overlay installs these on its first run; someone starting the
    # learner first would otherwise just get a process that dies instantly
    # with nowhere to print the traceback, pythonw having no console.
    import sys as _sys
    try:
        import tkinter as _tk
        from tkinter import messagebox as _mb
        _root = _tk.Tk(); _root.withdraw()
        _mb.showerror(
            "Triage Learner — missing dependencies",
            f"{_exc}\n\nStart the overlay once and it will install what is "
            "needed, or install them by hand:\n\n"
            f'    "{_sys.executable}" -m pip install requests Pillow pystray')
        _root.destroy()
    except Exception:
        pass
    # 'from None': the dialog already says what is missing, and a chained
    # ImportError traceback behind it only muddies the log.
    raise SystemExit(1) from None

# ---------------------------------------------------------------------------
# Files & constants
# ---------------------------------------------------------------------------

_DIR         = os.path.dirname(os.path.abspath(__file__))
CONFIG_FILE  = os.path.join(_DIR, "jira_config.json")
STATE_FILE   = os.path.join(_DIR, "triage_state.json")
LOG_FILE     = os.path.join(_DIR, "triage_log.jsonl")
REPORT_FILE  = os.path.join(_DIR, "TRIAGE_REPORT.md")
ERROR_LOG    = os.path.join(_DIR, "triage_learner.log")
# suggestions the overlay picks up and shows next to incoming tickets
SUGGEST_FILE = os.path.join(_DIR, "triage_suggestions.json")
# append-only labelled decisions the learner accumulates and self-tunes on
HISTORY_FILE = os.path.join(_DIR, "triage_history.jsonl")

FAST_POLL_SECONDS = 12       # intake predictions — must beat the team routing
POLL_SECONDS   = 60          # heavy routing-resolution + learning cadence
BACKFILL_DAYS  = 90          # seed the model from this much history
BACKFILL_CAP   = 1000        # max historical issues per outcome to learn from
OVERLAP_SECS   = 600         # re-scan overlap so nothing is missed
MIN_EXAMPLES   = 8           # per-destination examples needed before predicting
TUNING_SCHEMA_VERSION = 4    # + reporterWeight in the search space
# Cap on remembered transition IDs. Kept as an *ordered* collection so the
# trim drops the oldest — truncating a set (the old behaviour) kept an
# arbitrary subset, letting already-learned transitions be re-learned.
SEEN_CAP       = 20000
# Decisions the nightly search replays. The search costs
# (history x candidate configs), so an unbounded history turns a
# two-minute job into an hour-long one on a busy desk. The model itself
# is still fitted on the whole history — only the search is windowed.
TUNE_WINDOW    = 6000
# Per slow cycle, how many departed tickets to re-fetch when looking for ones
# that left intake by being MOVED to another project (see _learn_moved_out).
MOVE_LOOKUPS_PER_CYCLE = 20

# Structured Jira fields carry cleaner signals than free text. Field IDs differ
# between Jira sites, so these are resolved by NAME at startup (see
# Discovery.fields) rather than hard-coded. Add your own field names via
# "contextFields" in jira_config.json.
#
# The JSM Request Type field is identified by its schema type, which is stable
# across every Jira Service Management site.
REQUEST_TYPE_SCHEMA = "com.atlassian.servicedesk:vp-origin"
DEFAULT_CONTEXT_FIELDS = ["Affected services", "Organizations", "Organisations"]

# A portal request type that almost always leads to one outcome is a strong
# (not absolute) hint. These are LEARNED from your own data rather than listed
# here — see Learner.request_type_hint. Evidence is added to the probabilistic
# model rather than bypassing it, so contradictory wording can still win and
# low-confidence calls are still sent for review.
HINT_MIN_EXAMPLES = 12     # observations of a request type before it can hint
HINT_MIN_SHARE    = 0.80   # share going to one outcome for it to count

# Learning knobs — tuned offline by triage_eval.py replaying the 90-day
# history chronologically and scoring predictions against what the team did.
# Values chosen by triage_eval.py replaying 1,923 real decisions since the
# routing process began (2026-05-14): 84.7% overall; predictions made at
# >=90% confidence (73.7% of tickets) are right 93.1% of the time.
TUNING = {
    "summaryWeight": 6,   # summary features count this many times vs body
    "mistakeWeight": 3,   # a wrong prediction learns the correction at this weight
    "confTemp": 4.0,      # tokens per unit of softmax temperature (confidence)
    "reqSmooth": 10,      # smoothing for the request-type channel
    "repSmooth": 20,      # smoothing for the reporter channel
    "reporterWeight": 1.0,  # how much who-raised-it counts vs the wording
    "logOdds": 1,         # score tokens against the OTHER destinations, so
                          # vocabulary shared by all classes cancels out
    "priorWeight": 1.0,   # 1 = trust class frequencies, 0 = all classes equal
    "knnThresh": 0.7,     # title similarity needed to reuse a past ticket's
                          # routing directly (0 disables the memory)
    "reviewConf": 75,     # below this confidence a suggestion is flagged "review"
    "reviewMargin": 20,   # or if top-two are within this many points
    "requestTypeHintWeight": 3.0,  # log-score bonus for explicit team portal
    "decayFactor": 0.997, # <1 fades older examples so the model tracks how the
                          # team's routing conventions evolve (1.0 = no forgetting).
                          # 0.997 chosen offline: lifts the weak DEV class from
                          # 56%→65% and confident-band accuracy to ~93%.
}

BALANCED_TUNED_CONFIG = {
    "summaryWeight": 6,
    "mistakeWeight": 2,
    "decayFactor": 0.997,
    "confTemp": 4.0,
    "reviewConf": 75,
    "reviewMargin": 20,
    "knnThresh": 0.7,
    "priorWeight": 0.5,
    "requestTypeHintWeight": 3.0,
    "reporterWeight": 1.0,
}

# ── Outcome labels ─────────────────────────────────────────────────────────
# Outcomes are labels, not Jira statuses. A handoff outcome is simply the
# status name; the other two are generated from your project keys so the
# report reads naturally on any site. Both templates are overridable via
# "resolvedLabel" / "movedLabel" in jira_config.json — which is also how an
# existing install keeps the labels its history was recorded under.
RESOLVED_LABEL_FMT = "Resolved in {project}"
MOVED_LABEL_FMT    = "Moved to {project}"

# How a status qualifies as a "handoff" destination rather than a transient
# workflow step: tickets must actually come to rest in it. "In Progress" is
# passed through and left; a handoff queue is where a ticket sits and waits.
REST_SHARE       = 0.30   # share of arrivals still sitting there at scan time
MIN_DEST_ARRIVALS = 5     # arrivals before a status can qualify at all

_STOPWORDS = {
    "the", "and", "for", "with", "from", "this", "that", "have", "has",
    "are", "was", "were", "been", "being", "will", "would", "should",
    "can", "could", "not", "but", "all", "any", "our", "your", "their",
    "there", "here", "when", "where", "what", "which", "who", "how",
    "please", "thanks", "thank", "hi", "hello", "regards", "kind",
    "need", "needs", "needed", "required", "request", "requested",
    "you", "its", "it's", "into", "onto", "per", "via", "due", "new",
}


def json_objects(value) -> list:
    """The entries of a JSON array that are actually objects.

    Every Jira response this code walks is a list of dicts in the happy
    case, and each level of it — the array itself, and the entries inside —
    can be null or something else entirely on a site with different
    permissions, an older API, or a partial response. Filtering once here
    means the callers can iterate without a guard at every level.
    """
    if not isinstance(value, list):
        return []
    return [v for v in value if isinstance(v, dict)]


def as_name_list(value) -> list[str]:
    """A config setting that must be a list of names, whatever it holds.

    A bare string is the likely hand-edit, and it is the dangerous case:
    list("With DEV") is eight single characters, not one status, so the
    setting silently became eight bogus statuses and every query built from
    it was nonsense. Wrapping it keeps what the user meant.
    """
    if value is None:
        return []
    if isinstance(value, str):
        return [value] if value.strip() else []
    try:
        return [str(v) for v in value if v is not None and str(v).strip()]
    except TypeError:          # not iterable at all
        return []


def as_label_map(value) -> dict:
    """A config setting that must map a project key to a label."""
    return dict(value) if isinstance(value, dict) else {}


def _log_error(msg: str) -> None:
    with open(ERROR_LOG, "a", encoding="utf-8") as f:
        f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S}  {msg}\n")


def _has_association(ext: str) -> bool:
    """Whether Windows has anything registered to open `ext`.

    os.startfile() (ShellExecute under the hood) doesn't raise when there is
    no association — on a plain Windows install with nothing markdown-aware
    installed, it silently resolves to nothing visible: no new window, no
    exception, no error anywhere pythonw could report it. Checking first,
    rather than reacting to a failure that never announces itself, is the
    only way to catch this before the menu click just does nothing.
    """
    try:
        # A per-user choice (Explorer's own "always open with") overrides
        # the class-wide default and is what a real double-click would use.
        with winreg.OpenKey(
                winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts"
                rf"\{ext}\UserChoice") as k:
            if winreg.QueryValueEx(k, "ProgId")[0]:
                return True
    except OSError:
        pass
    try:
        with winreg.OpenKey(winreg.HKEY_CLASSES_ROOT, ext) as k:
            return bool(winreg.QueryValueEx(k, "")[0])
    except OSError:
        return False


def open_in_default_or_notepad(path: str) -> None:
    """Open `path` with whatever Windows has registered for it, falling back
    to Notepad — present on every Windows install by definition — rather
    than risk the menu item that's meant to show the report doing nothing
    with no way for the user to tell why.
    """
    _, ext = os.path.splitext(path)
    if _has_association(ext):
        try:
            os.startfile(path)
            return
        except OSError:
            pass
    try:
        subprocess.Popen(["notepad.exe", path])
    except OSError as exc:
        _log_error(f"could not open {path} with notepad.exe either: {exc}")


def _atomic_json_write(path: str, data, attempts: int = 6) -> None:
    """Write JSON atomically, tolerating short-lived Windows reader locks."""
    tmp = f"{path}.{os.getpid()}.{threading.get_ident()}.tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=1)
        delay = 0.05
        for attempt in range(attempts):
            try:
                os.replace(tmp, path)
                return
            except PermissionError:
                if attempt == attempts - 1:
                    raise
                time.sleep(delay)
                delay = min(delay * 2, 0.8)
    finally:
        try:
            if os.path.exists(tmp):
                os.unlink(tmp)
        except OSError:
            pass


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
        # set by the watcher so a long retry backoff doesn't delay Quit
        self.stop: Optional[threading.Event] = None
        self._refusals: set[int] = set()   # auth/permission codes already logged

    def _pause(self, seconds: float) -> bool:
        """Sleep between retries; True if we were asked to stop instead."""
        if self.stop is not None:
            return self.stop.wait(seconds)
        time.sleep(seconds)
        return False

    def _url(self, path: str) -> str:
        return f"https://{self.domain}{path}"

    def _send(self, method: str, path: str, *, params: Optional[dict] = None,
              json_body: Optional[dict] = None, timeout: int = 20):
        """One request with retry/backoff. Backfill fires thousands of calls,
        so Jira can rate-limit or reset connections (WinError 10054); without
        this the whole cycle would abort and no suggestions get written."""
        url   = self._url(path)
        delay = 1.0
        for attempt in range(5):
            try:
                r = self.session.request(method, url, params=params,
                                         json=json_body, timeout=timeout)
            except (requests.ConnectionError, requests.Timeout):
                if attempt == 4 or self._pause(delay):
                    return None
                delay = min(delay * 2, 20)
                continue
            if r.status_code == 429 or r.status_code >= 500:
                if attempt == 4:
                    return None
                ra = r.headers.get("Retry-After", "")
                if self._pause(float(ra) if ra.isdigit() else delay):
                    return None
                delay = min(delay * 2, 20)
                continue
            self._note_refusal(r)
            return r
        return None

    def _note_refusal(self, response) -> None:
        """Record the first auth/permission refusal of each kind.

        Otherwise a wrong or expired token just makes every search come back
        empty, for ever, with nothing written anywhere — the learner appears
        to run perfectly and simply never produce a suggestion.
        """
        status = response.status_code
        if status not in (401, 403, 404) or status in self._refusals:
            return
        self._refusals.add(status)
        reason = {401: "credentials rejected — check the API token in "
                       "jira_config.json",
                  403: "permission denied — this account cannot read that "
                       "project",
                  404: "not found — check projectKey in jira_config.json"}[status]
        _log_error(f"Jira returned {status} for "
                   f"{response.request.method} {response.request.path_url}: "
                   f"{reason}")

    def get(self, path: str, params: Optional[dict] = None) -> Optional[dict]:
        r = self._send("GET", path, params=params, timeout=15)
        return r.json() if (r is not None and r.ok) else None

    def search(self, jql: str, fields: list[str], cap: int = 100) -> list[dict]:
        """POST /search/jql is Jira's read-only search — no data is modified."""
        issues: list[dict] = []
        token: Optional[str] = None
        while len(issues) < cap:
            body: dict = {"jql": jql, "fields": fields,
                          "maxResults": min(100, cap - len(issues))}
            if token:
                body["nextPageToken"] = token
            r = self._send("POST", "/rest/api/3/search/jql",
                           json_body=body, timeout=20)
            if r is None or not r.ok:
                break
            try:
                data = r.json()
            except ValueError:          # a 200 that isn't JSON at all
                break
            if not isinstance(data, dict):
                break
            # json_objects, not `or []`: an "issues" that is a string would
            # otherwise be iterated one character at a time.
            page = json_objects(data.get("issues"))
            issues.extend(page)
            token = data.get("nextPageToken")
            if not token or data.get("isLast", True) or not page:
                break
        return issues

    def changelog(self, key: str) -> list[dict]:
        """All changelog histories for an issue (paginated GET)."""
        out: list[dict] = []
        start = 0
        while True:
            data = self.get(f"/rest/api/3/issue/{key}/changelog",
                            {"startAt": start, "maxResults": 100})
            if not isinstance(data, dict):
                break
            page = json_objects(data.get("values"))
            out.extend(page)
            # Stop on an empty page as well as isLast: a response that keeps
            # claiming there is more, without returning any, would otherwise
            # page for ever.
            if data.get("isLast", True) or not page:
                break
            start += len(page)
        return out


# ---------------------------------------------------------------------------
# Discovery — work out the shape of THIS Jira rather than assuming ours
# ---------------------------------------------------------------------------

class Discovery:
    """Resolves site-specific ids and workflow shape at startup.

    Everything here has a config override; discovery only fills in what the
    user hasn't pinned. Results are written back to jira_config.json so the
    user can see (and correct) what was inferred.
    """

    def __init__(self, jira: "Jira", cfg: dict) -> None:
        self.jira = jira
        self.cfg  = cfg

    # -- custom fields -------------------------------------------------------

    def fields(self) -> tuple[str, list[str]]:
        """(request-type field id, [context field ids]).

        Field ids are per-site, so we match on the Request Type schema (stable
        across JSM) and on human field NAMES for context fields.
        """
        pinned_rt   = self.cfg.get("requestTypeField") or ""
        configured  = self.cfg.get("contextFields")
        want_names  = [n.lower() for n in
                       (as_name_list(configured) if configured is not None
                        else DEFAULT_CONTEXT_FIELDS)]
        pinned_ctx  = self.cfg.get("contextFieldIds")
        if pinned_rt and pinned_ctx is not None:
            return pinned_rt, as_name_list(pinned_ctx)

        catalogue = json_objects(self.jira.get("/rest/api/3/field"))
        req_field, context = pinned_rt, []
        for f in catalogue:
            fid    = f.get("id", "")
            name   = (f.get("name") or "").lower()
            schema = (f.get("schema") or {}).get("custom", "")
            if not req_field and schema == REQUEST_TYPE_SCHEMA:
                req_field = fid
            if name in want_names and fid not in context:
                context.append(fid)
        if not req_field:
            # Site without the JSM origin field — fall back to the name.
            for f in catalogue:
                if (f.get("name") or "").lower() == "request type":
                    req_field = f.get("id", "")
                    break
        return req_field, context

    # -- workflow shape ------------------------------------------------------

    def status_categories(self, project: str) -> tuple[set[str], set[str]]:
        """(done statuses, open statuses) across every issue type in the
        project. 'Done' drives the resolved-in-place outcome."""
        done: set[str] = set()
        openish: set[str] = set()
        data = self.jira.get(f"/rest/api/3/project/{project}/statuses")
        for issue_type in json_objects(data):
            for st in json_objects(issue_type.get("statuses")):
                name = st.get("name", "")
                if not name:
                    continue
                key = ((st.get("statusCategory") or {}).get("key") or "").lower()
                (done if key == "done" else openish).add(name)
        return done, openish

    def outcome_statuses(self, issues: list[dict],
                         transitions: dict[str, list[str]],
                         done: set[str]) -> tuple[list[str], list[str]]:
        """Work out this project's triage outcomes from observed behaviour.

        Returns (resolved statuses, handoff statuses).

        An *outcome* is a status in a "done" category that tickets actually
        come to rest in — meaning the service desk has finished with the
        ticket. Open-category statuses are steps along the way ("In Progress",
        "On Hold", "Waiting for customer"): the desk still owns the ticket, so
        they are never outcomes no matter how long tickets sit in them.

        Among the outcomes, the highest-volume one is ordinary resolution;
        every other is a handoff to someone else (teams commonly mark those
        "done" too, because they are done as far as the desk is concerned).

        `issues` are recent intake issues carrying their current status, and
        `transitions` maps issue key -> statuses it was moved into.
        """
        arrivals: Counter = Counter()
        resting:  Counter = Counter()
        current = {i.get("key", ""): ((i.get("fields") or {})
                                      .get("status") or {}).get("name", "")
                   for i in issues}
        for key, moved_to in transitions.items():
            for status in set(moved_to):
                if status not in done:
                    continue
                arrivals[status] += 1
                if current.get(key) == status:
                    resting[status] += 1
        picked = [s for s, n in arrivals.items()
                  if n >= MIN_DEST_ARRIVALS and resting[s] / n >= REST_SHARE]
        picked.sort(key=lambda s: -arrivals[s])
        if not picked:
            return [], []
        return picked[:1], picked[1:]


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


def field_names(value) -> list[str]:
    """Extract human-readable names from Jira custom-field values."""
    if not value:
        return []
    if isinstance(value, list):
        return [name for item in value for name in field_names(item)]
    if isinstance(value, dict):
        for key in ("name", "value", "displayName"):
            if value.get(key):
                return [str(value[key])]
        return []
    if isinstance(value, str):
        return [value]
    return []


def request_type_name(value) -> str:
    if not isinstance(value, dict):
        return ""
    request_type = value.get("requestType") or {}
    return str(request_type.get("name") or value.get("name") or "")


# case-reference codes like "hcw11593772keo" / "tc25000123" — individually
# rare and useless, but their PRESENCE is a signal, so collapse to one token
_REF_RE = re.compile(r"^[a-z]{1,6}\d{4,}\w*$")


def tokenize(text: str) -> list[str]:
    words = re.findall(r"[a-z][a-z0-9']{2,}", text.lower())
    return ["#ref" if _REF_RE.match(w) else w
            for w in words if w not in _STOPWORDS]


def features_for(det: dict, req_type: str) -> list[str]:
    """Unigrams + bigrams. The summary counts extra (it's what the team
    triages by); description features are deduplicated so a long email
    body can't drown out the title signal."""
    s_toks = tokenize(" ".join([det["summary"], req_type]))
    d_toks = tokenize(" ".join([det["desc"], " ".join(det["labels"]),
                                " ".join(det["components"]),
                                " ".join(det.get("structured", []))]))
    s_bi = [f"{a}·{b}" for a, b in zip(s_toks, s_toks[1:])]
    d_bi = [f"{a}·{b}" for a, b in zip(d_toks, d_toks[1:])]
    return ((s_toks + s_bi) * int(TUNING["summaryWeight"])
            + sorted(set(d_toks) | set(d_bi)))


# ---------------------------------------------------------------------------
# The learner
# ---------------------------------------------------------------------------

class Learner:
    """Naive-Bayes-style model over routing destinations.

    Features are unigrams + bigrams; the summary counts double because it's
    what the team actually triages by. The reporter and request type get
    their own channels. When a prediction turns out wrong, the correcting
    example is learned at double weight so mistakes sting.

    Recency decay is applied *lazily*. The naive version multiplied every
    stored count by `decayFactor` on every single learn() — an O(vocabulary)
    sweep per example, which at ~43k tokens dominated both the live loop and
    the nightly self-tune. Instead we keep one global multiplier `scale`
    (= decayFactor**n after n examples) and store every count pre-divided by
    it, so `true value == stored * scale`. Decaying is then a single float
    multiply, and the O(vocabulary) sweep only runs when `scale` gets small
    enough to risk float overflow — roughly once per 7,000 examples.
    """

    # Renormalise once the implicit multiplier reaches this; 1/scale stays
    # below 1e9, far inside float64 range, and the counts it would drop are
    # ~1e-16 of current weight — i.e. already decayed into irrelevance.
    _RENORM_FLOOR = 1e-9
    _EX_CAP   = 5000    # exemplars kept for the ticket-memory lookup
    _EX_SLACK = 1000    # trim in batches so the index is rebuilt rarely

    # Buckets that carry decayed per-destination token tables.
    _DECAYED_TABLES = ("tokens", "reqTypes", "reporters")

    def __init__(self, state: dict, plans: Optional[dict] = None) -> None:
        # dest -> plain-English plan, owned by the watcher (which knows this
        # site's workflow) and shared by reference so newly discovered
        # outcomes describe themselves without rebuilding the model.
        self._plans = plans if plans is not None else {}
        # setdefault only fills a key that is MISSING, so a key present with
        # a null — or holding the wrong type after a version change or a
        # damaged write — went straight through and crashed on first use.
        # This file carries every decision the model has learned, so it is
        # worth reading defensively: an unusable section is rebuilt empty
        # and relearned, rather than taking the whole learner down.
        if not isinstance(state.get("model"), dict):
            state["model"] = {}
        m = state["model"]
        for bucket in ("priors", "tokens", "totals", "reqTypes", "reporters",
                       "cloneProjects", "seen"):
            if not isinstance(m.get(bucket), dict):
                m[bucket] = {}
        if not isinstance(m.get("exemplars"), list):
            m["exemplars"] = []             # [[title tokens...], dest] memory
        # Absent in models written before lazy decay — 1.0 is exactly right
        # for them, since their stored counts are already true values. A
        # non-positive scale would divide by zero on the next example.
        scale = m.get("scale")
        if not isinstance(scale, (int, float)) or scale <= 0:
            m["scale"] = 1.0
        self.m = m
        self._ex_index: Optional[dict] = None
        self._ex_lens: Optional[list[int]] = None
        self._vocab: Optional[dict] = None   # token -> #destinations using it
        # Bumped by learn(); lets the watcher skip rewriting a
        # multi-megabyte state file when nothing was actually learned.
        self._mutations = 0

    # -- lazy decay ----------------------------------------------------------

    def _renormalise(self) -> None:
        """Fold the implicit multiplier back into the stored counts."""
        m = self.m
        s = m["scale"]
        for d in list(m["priors"]):
            m["priors"][d] *= s
            m["totals"][d] = m["totals"].get(d, 0) * s
            for bucket in self._DECAYED_TABLES:
                tbl = m[bucket].get(d)
                if tbl:
                    for k in tbl:
                        tbl[k] *= s
        m["scale"] = 1.0

    def _advance_decay(self) -> float:
        """Age all existing evidence by one example; return the weight
        multiplier the *new* example must be stored with."""
        f = TUNING.get("decayFactor", 1.0)
        if f < 1.0:
            self.m["scale"] *= f
            if self.m["scale"] < self._RENORM_FLOOR:
                self._renormalise()
        return 1.0 / self.m["scale"]

    # -- vocabulary ----------------------------------------------------------

    def _vocab_table(self) -> dict:
        """token -> number of destinations whose table holds it. Maintained
        incrementally; rebuilding it per prediction (the old behaviour) cost
        more than the rest of scoring combined."""
        if self._vocab is None:
            v: dict = {}
            for tbl in self.m["tokens"].values():
                for t in tbl:
                    v[t] = v.get(t, 0) + 1
            self._vocab = v
        return self._vocab

    def learn(self, dest: str, tokens: list[str], req_type: str,
              clone_projects: list[str], reporter: str = "",
              weight: int = 1, exemplar: Optional[list[str]] = None) -> None:
        m = self.m
        self._mutations += 1
        boost = self._advance_decay()
        w = weight * boost
        m["priors"][dest] = m["priors"].get(dest, 0) + boost
        m["seen"][dest]   = m["seen"].get(dest, 0) + 1   # raw count, undecayed
        tok = m["tokens"].setdefault(dest, {})
        vocab = self._vocab
        for t, k in Counter(tokens).items():
            if vocab is not None and t not in tok:
                vocab[t] = vocab.get(t, 0) + 1
            tok[t] = tok.get(t, 0) + w * k
        m["totals"][dest] = m["totals"].get(dest, 0) + w * len(tokens)
        if req_type:
            rt = m["reqTypes"].setdefault(dest, {})
            rt[req_type] = rt.get(req_type, 0) + w
        if reporter:
            rp = m["reporters"].setdefault(dest, {})
            rp[reporter] = rp.get(reporter, 0) + w
        for proj in clone_projects:
            cp = m["cloneProjects"].setdefault(dest, {})
            cp[proj] = cp.get(proj, 0) + 1
        if exemplar:
            ex = m["exemplars"]
            ex.append([sorted(set(exemplar)), dest])
            if len(ex) > self._EX_CAP + self._EX_SLACK:
                # Trim in batches. Dropping one per call (the old behaviour)
                # invalidated the index on every learn, so every prediction
                # then rebuilt it from all 5,000 exemplars.
                del ex[: len(ex) - self._EX_CAP]
                self._ex_index = None
                self._ex_lens  = None
            elif self._ex_index is not None:
                toks = ex[-1][0]
                for t in toks:
                    self._ex_index.setdefault(t, []).append(len(ex) - 1)
                self._ex_lens.append(len(toks))   # type: ignore[union-attr]

    def _nearest(self, title_toks: list[str]) -> tuple[Optional[str], float]:
        """Most similar previously-seen ticket title (Jaccard overlap).

        Intersection sizes come straight off the inverted index, so we never
        materialise a set per candidate."""
        ex = self.m.get("exemplars", [])
        if not ex or not title_toks:
            return None, 0.0
        if self._ex_index is None:
            index: dict = {}
            lens: list[int] = []
            for i, (tks, _) in enumerate(ex):
                for t in tks:
                    index.setdefault(t, []).append(i)
                lens.append(len(tks))
            self._ex_index, self._ex_lens = index, lens
        index, lens = self._ex_index, self._ex_lens
        qs = set(title_toks)
        overlap: dict[int, int] = {}
        for t in qs:
            for i in index.get(t, ()):
                overlap[i] = overlap.get(i, 0) + 1
        nq = len(qs)
        best_dest, best_j = None, 0.0
        # Newest first, so on a tie — the common "same title, routed twice"
        # case — the team's most recent decision wins. The old code iterated
        # a set of indices, making the winner depend on hash order.
        for i in sorted(overlap, reverse=True):
            shared = overlap[i]
            j = shared / (nq + lens[i] - shared)   # |q∩s| / |q∪s|
            if j > best_j:
                best_j, best_dest = j, ex[i][1]
        return best_dest, best_j

    def plan_for(self, dest: str) -> str:
        """Plain-English description of an outcome.

        Outcome wording is supplied by the watcher, which knows this site's
        workflow. Anything not described there is a handoff status, so the
        phrasing is built from the status name itself — plus, where tickets
        sent there are habitually linked to an issue in another project, that
        observed convention.
        """
        plan = self._plans.get(dest)
        if plan:
            return plan
        proj = self.clone_project_for(dest)
        return (f"raise a linked {proj} issue, then move to {dest}"
                if proj else f"move to {dest}")

    def request_type_hint(self, req_type: str) -> Optional[str]:
        """The outcome a portal request type nearly always leads to, or None.

        Learned from observation rather than configured: if a request type has
        been seen often enough and overwhelmingly leads to one outcome, that
        is a strong hint. Works on any site's portal without anyone having to
        list their request types.
        """
        if not req_type:
            return None
        scale  = self.m.get("scale", 1.0)
        counts = {d: table.get(req_type, 0) * scale
                  for d, table in self.m["reqTypes"].items()}
        total = sum(counts.values())
        if total < HINT_MIN_EXAMPLES:
            return None
        best = max(counts, key=counts.get)          # type: ignore[arg-type]
        return best if counts[best] / total >= HINT_MIN_SHARE else None

    def predict_ex(self, tokens: list[str], req_type: str, reporter: str = "",
                   exemplar: Optional[list[str]] = None) -> dict:
        """Full prediction: best destination, its confidence, the runner-up
        and the margin between them, plus whether the call is close enough
        that a human should double-check. Margin/second-choice is what makes
        the suggestions honest on the BI/DATA/DEV overlap cases."""
        m      = self.m
        # readiness uses the RAW (undecayed) count so recency decay can't push
        # an infrequent-but-valid but low-volume class below the gate
        seen   = m.get("seen", {})
        ready  = [d for d in m["priors"]
                  if seen.get(d, m["priors"][d]) >= MIN_EXAMPLES]
        if len(ready) < 2:
            return {"dest": None, "conf": 0, "second": None, "secondConf": 0,
                    "margin": 0, "review": True, "source": "cold",
                    "note": "still learning — not enough observed examples yet"}

        # Stored counts are pre-divided by the lazy-decay multiplier; fold it
        # back in here, where it only touches the handful of tokens in play.
        S = m.get("scale", 1.0)
        total_examples = sum(m["priors"][d] for d in ready)
        tok_tables = [m["tokens"].get(d) or {} for d in ready]
        if len(ready) == len(m["tokens"]):
            vocab = len(self._vocab_table()) or 1
        else:   # cold start: not every destination is predictable yet
            vocab = len({t for tbl in tok_tables for t in tbl}) or 1

        # Tokens repeat (the summary is weighted by duplication), so score
        # each distinct token once and multiply — identical result, far less
        # work than walking the expanded list.
        counts = Counter(tokens)
        log_odds = bool(TUNING.get("logOdds"))
        if log_odds:
            tot_all = sum(m["totals"].get(d, 0) for d in ready) * S
            all_cnt = {}
            for t in counts:
                c = 0.0
                for tbl in tok_tables:
                    c += tbl.get(t, 0)
                all_cnt[t] = c * S
        prior_w   = TUNING.get("priorWeight", 1.0)
        req_smooth = TUNING["reqSmooth"]
        rep_smooth = TUNING["repSmooth"]
        rep_w      = TUNING.get("reporterWeight", 1.0)
        hint_for_type = self.request_type_hint(req_type)
        scores: dict[str, float] = {}
        for d, tok in zip(ready, tok_tables):
            score = prior_w * math.log(m["priors"][d] / total_examples)
            tot = m["totals"].get(d, 0) * S
            if log_odds:
                # Complement log-odds: is this token MORE likely here than in
                # the other destinations combined? Vocabulary shared by every
                # class ("cases", "client") cancels out; only discriminative
                # wording moves the score.
                den_in  = tot + vocab
                den_out = (tot_all - tot) + vocab
                for t, k in counts.items():
                    c_in  = tok.get(t, 0) * S
                    p_in  = (c_in + 1) / den_in
                    p_out = (all_cnt[t] - c_in + 1) / den_out
                    score += k * math.log(p_in / p_out)
            else:
                den_in = tot + vocab
                for t, k in counts.items():
                    score += k * math.log((tok.get(t, 0) * S + 1) / den_in)
            if req_type:
                rt = m["reqTypes"].get(d, {})
                n_rt = sum(rt.values()) * S
                score += math.log((rt.get(req_type, 0) * S + 1)
                                  / (n_rt + req_smooth))
                if hint_for_type == d:
                    score += TUNING.get("requestTypeHintWeight", 3.0)
            if reporter:
                rp = m.get("reporters", {}).get(d, {})
                n_rp = sum(rp.values()) * S
                # Who raised it is one log term against one per token, and
                # the summary's tokens are repeated summaryWeight times — so
                # a strong reporter signal is easily drowned by wording.
                # Weighting it lets the tuner decide how much it is worth.
                score += rep_w * math.log((rp.get(reporter, 0) * S + 1)
                                          / (n_rp + rep_smooth))
            scores[d] = score

        # Temperature-scaled softmax: raw NB log-score gaps grow with text
        # length, which reads as false 100% certainty. Scaling by token
        # count keeps confidence honest.
        ranked = sorted(scores, key=scores.get, reverse=True)  # type: ignore[arg-type]
        best, second = ranked[0], ranked[1]
        mx    = scores[best]
        temp  = max(1.0, len(tokens) / TUNING["confTemp"])
        exps  = {d: math.exp((s - mx) / temp) for d, s in scores.items()}
        tot_e = sum(exps.values())
        conf  = int(round(100 * exps[best] / tot_e))
        conf2 = int(round(100 * exps[second] / tot_e))
        source = "model"
        note   = self.plan_for(best)
        hint_dest = hint_for_type
        hint_conf = (int(round(100 * exps.get(hint_dest, 0) / tot_e))
                     if hint_dest else 0)

        # Ticket memory: if the title closely matches a previous ticket,
        # route the way that one actually went — recurring requests are
        # far better matched whole than word-by-word.
        thresh = TUNING.get("knnThresh", 0)
        if exemplar and thresh:
            k_dest, sim = self._nearest(exemplar)
            if k_dest and sim >= thresh:
                k_conf = int(round(50 + 50 * sim))
                if k_dest != best or k_conf > conf:
                    if k_dest != best:
                        second, conf2 = best, conf
                    best, conf = k_dest, max(k_conf, conf if k_dest == best else 0)
                    source = "memory"
                    note   = self.plan_for(best) + " (matches a previous ticket)"

        hint_conflict = bool(hint_dest in ready and best != hint_dest)
        if hint_conflict:
            if second != hint_dest:
                second, conf2 = hint_dest, hint_conf
            else:
                conf2 = max(conf2, hint_conf)
            note += f"; review because Request Type points to {hint_dest}"

        margin = conf - conf2
        review = (conf < TUNING.get("reviewConf", 75)
                  or margin < TUNING.get("reviewMargin", 20)
                  or hint_conflict)
        return {"dest": best, "conf": conf, "second": second,
                "secondConf": conf2, "margin": margin, "review": review,
                "source": source, "note": note}

    def clone_project_for(self, dest: str) -> str:
        cp = self.m["cloneProjects"].get(dest, {})
        return max(cp, key=cp.get) if cp else ""  # type: ignore[arg-type]

    def top_keywords(self, dest: str, n: int = 12) -> list[str]:
        tok = self.m["tokens"].get(dest, {})
        return [t.replace("·", " ") for t, _ in
                sorted(tok.items(), key=lambda kv: -kv[1])[:n]]


# ---------------------------------------------------------------------------
# Shadow watcher
# ---------------------------------------------------------------------------

class ShadowWatcher:
    # Incremental report-metrics cache (see _live_metrics). Declared at class
    # level so the accessors are safe even before __init__ has run.
    _metrics: Optional[dict] = None
    _metrics_offset: int = 0
    _metrics_pending: dict = {}

    def __init__(self) -> None:
        with open(CONFIG_FILE, encoding="utf-8") as f:
            self.cfg = json.load(f)
        self.project = self.cfg.get("projectKey", "")
        if not self.project:
            raise SystemExit(
                "No projectKey in jira_config.json — run the overlay first so "
                "it can discover your service desk.")
        self.jira  = Jira(self.cfg)
        self.state = self._load_state()

        # Everything below is discovered from your Jira unless pinned in the
        # config. Discovery results are written back so they are visible.
        self.discovery = Discovery(self.jira, self.cfg)
        self.req_field, self.context_fields = self.discovery.fields()
        # Remember the ids so the field catalogue isn't re-fetched every start.
        if self.cfg.get("requestTypeField") != self.req_field or \
                self.cfg.get("contextFieldIds") != self.context_fields:
            self._save_cfg({"requestTypeField": self.req_field,
                            "contextFieldIds": self.context_fields})
        # These are hand-editable, so none of them can be trusted to hold the
        # type they are documented with — see as_name_list for why a bare
        # string is worse than a null here.
        self.dests     = as_name_list(self.cfg.get("triageStatuses"))
        self.resolved  = set(as_name_list(self.cfg.get("resolvedStatuses"))
                             or as_name_list(self.cfg.get("resolvedStatus")))
        self.move_targets = as_name_list(self.cfg.get("moveTargetProjects"))
        self.resolved_label = (self.cfg.get("resolvedLabel")
                               or RESOLVED_LABEL_FMT.format(project=self.project))
        self.moved_label_fmt = self.cfg.get("movedLabel") or MOVED_LABEL_FMT
        self.moved_label_overrides = as_label_map(self.cfg.get("movedLabels"))
        self._issue_fields = ["summary", "description", "labels", "components",
                              "issuelinks", "reporter", "status"]
        if self.req_field:
            self._issue_fields.append(self.req_field)
        self._issue_fields.extend(self.context_fields)
        # adopt any hyper-parameters chosen by a previous nightly self-tune
        if isinstance(self.state.get("tunedConfig"), dict):
            upgraded = dict(BALANCED_TUNED_CONFIG,
                            **self.state["tunedConfig"])
            if upgraded != self.state["tunedConfig"]:
                self.state["tunedConfig"] = upgraded
                self._save_state()
            TUNING.update(self.state["tunedConfig"])
        if self.state.get("tuningSchemaVersion", 0) < TUNING_SCHEMA_VERSION:
            self.state["tuningSchemaVersion"] = TUNING_SCHEMA_VERSION
            self.state["lastTuneDate"] = ""  # run the upgraded tuner once now
            self._save_state()
        # Outcome -> plain-English plan. Shared by reference with the Learner
        # so outcomes discovered later describe themselves too.
        self.plans: dict[str, str] = {
            self.resolved_label: f"resolve in {self.project} — no handoff",
        }
        for target in self.move_targets:
            self.plans[self.moved_label(target)] = (
                f"move the ticket to the {target} project")
        self.learner = Learner(self.state, plans=self.plans)
        self._saved_mutations = 0
        self._last_state_save = 0.0
        self.stop    = threading.Event()
        self.jira.stop = self.stop     # let retry backoff abort on Quit
        # incremental report metrics (see _live_metrics)
        self._metrics: Optional[dict] = None
        self._metrics_offset = 0
        self._metrics_pending: dict[str, dict] = {}

    # -- discovery -----------------------------------------------------------

    def _save_cfg(self, discovered: dict) -> None:
        """Persist inferred settings next to the user's own, so what was
        discovered is visible and can be corrected by hand."""
        self.cfg.update(discovered)
        try:
            with open(CONFIG_FILE, encoding="utf-8") as f:
                on_disk = json.load(f)
        except (OSError, json.JSONDecodeError):
            on_disk = {}
        on_disk.update(discovered)
        _atomic_json_write(CONFIG_FILE, on_disk)

    def ensure_workflow_known(self) -> bool:
        """Fill in whatever the config didn't pin. True once we know enough
        to learn: at minimum a set of resolved statuses.

        Whether discovery has run is tracked explicitly rather than inferred
        from `self.dests` being non-empty — a project where everything is
        resolved in place legitimately has no handoff statuses, and testing
        for truthiness made it repeat the whole 250-changelog scan on every
        single startup.
        """
        if self.resolved and (self.dests
                              or self.state.get("workflowDiscovered")):
            return True

        done, _open = self.discovery.status_categories(self.project)
        if not done:
            _log_error(f"could not read statuses for project {self.project} "
                       "— cannot start")
            return False

        # Sample how recent tickets actually moved, then read the outcomes off
        # that behaviour rather than guessing from status names.
        issues = self.jira.search(
            f"project = {self.project} AND updated >= -{BACKFILL_DAYS}d "
            "ORDER BY updated DESC", ["summary", "status"], cap=250)
        transitions: dict[str, list[str]] = {}
        for issue in issues:
            if self.stop.is_set():
                return False
            key = issue.get("key", "")
            moved_to = [item.get("toString", "")
                        for h in self.jira.changelog(key)
                        for item in h.get("items", [])
                        if item.get("field") == "status"]
            if moved_to:
                transitions[key] = moved_to
        resolved, handoffs = self.discovery.outcome_statuses(
            issues, transitions, done)

        found: dict = {}
        if not self.resolved:
            if not resolved:
                _log_error(f"no settled outcome status found in "
                           f"{self.project} — set resolvedStatuses in "
                           "jira_config.json")
                return False
            self.resolved = set(resolved)
            self.resolved_label = (self.cfg.get("resolvedLabel")
                                   or RESOLVED_LABEL_FMT.format(
                                       project=self.project))
            found["resolvedStatuses"] = resolved
        if not self.dests:
            self.dests = handoffs
            found["triageStatuses"] = handoffs

        if found:
            self._save_cfg(found)
            _log_error(f"discovered outcomes for {self.project}: "
                       f"resolved={sorted(self.resolved)} "
                       f"handoffs={self.dests or 'none'}")
        if self.resolved:
            self.state["workflowDiscovered"] = True
            self._save_state()
        return bool(self.resolved)

    def _remember_move_target(self, project_key: str) -> None:
        """A project we've now seen a ticket moved into; worth scanning on
        future backfills."""
        if project_key and project_key not in self.move_targets:
            self.move_targets.append(project_key)
            self.plans[self.moved_label(project_key)] = (
                f"move the ticket to the {project_key} project")
            self._save_cfg({"moveTargetProjects": self.move_targets})
            _log_error(f"discovered move-target project: {project_key}")

    def moved_label(self, project_key: str) -> str:
        """Outcome label for a ticket moved into `project_key`.

        `movedLabels` lets a single project keep a hand-written name (useful
        when an install already has history recorded under one); anything not
        listed falls back to the generic template.
        """
        explicit = self.moved_label_overrides.get(project_key)
        return explicit or self.moved_label_fmt.format(project=project_key)

    @property
    def all_dests(self) -> list[str]:
        """Every outcome label the report should account for."""
        return (list(self.dests) + [self.resolved_label]
                + [self.moved_label(p) for p in self.move_targets])

    # -- state ---------------------------------------------------------------

    def _load_state(self) -> dict:
        try:
            with open(STATE_FILE, encoding="utf-8") as f:
                return json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            return {}

    # Bookkeeping-only saves are throttled to this. A crash loses at most
    # this much prediction bookkeeping, which costs a little scoring
    # accuracy and nothing that can't be re-derived.
    _IDLE_SAVE_SECS = 300

    def _save_state(self, force: bool = True) -> None:
        """Persist state. `force=False` skips the write when only the poll
        clock has moved on.

        The state file holds the whole model, so every save rewrites a few
        megabytes. lastPollEpoch is stamped every cycle, which made the state
        look changed every single time — measured at 1.2 writes a minute,
        about 4GB of disk a day, on a tool that is usually idle. Learning is
        always written immediately; a cycle that merely ticked the clock is
        not worth the write.
        """
        # Read through getattr: this runs from shutdown and error paths too,
        # and a save is the last thing that should introduce a failure.
        if not force:
            mutations = getattr(self.learner, "_mutations", 0)
            unchanged = mutations == getattr(self, "_saved_mutations", None)
            since = time.time() - getattr(self, "_last_state_save", 0.0)
            if unchanged and since < self._IDLE_SAVE_SECS:
                return
        _atomic_json_write(STATE_FILE, self.state)
        self._saved_mutations = getattr(self.learner, "_mutations", 0)
        self._last_state_save = time.time()

    def _log(self, record: dict) -> None:
        record["ts"] = f"{datetime.now():%Y-%m-%d %H:%M:%S}"
        with open(LOG_FILE, "a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")

    @staticmethod
    def _history_record(det: dict, dest: str, when: str) -> dict:
        """A self-contained labelled example the nightly self-tune replays."""
        return {"key": det.get("key", ""), "when": when, "dest": dest,
                "summary": det["summary"],
                "desc": det["desc"], "labels": det["labels"],
                "components": det["components"], "reporter": det["reporter"],
                "requestType": det.get("requestType", ""),
                "structured": det.get("structured", []),
                "cloneProjects": [c["key"].split("-")[0] for c in det["clones"]]}

    def _append_history(self, det: dict, dest: str, when: str,
                        handle=None) -> None:
        rec = self._history_record(det, dest, when)
        if handle is not None:
            handle.write(json.dumps(rec, ensure_ascii=False) + "\n")
        else:
            with open(HISTORY_FILE, "a", encoding="utf-8") as f:
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")

    # -- issue helpers ---------------------------------------------------------

    @property
    def _FIELDS(self) -> list[str]:
        """Issue fields to request — built from what discovery found on this
        site, so no customfield id is ever hard-coded."""
        return self._issue_fields

    def _issue_details(self, issue: dict) -> dict:
        """Normalise the bits we learn from.

        Every field here is treated as possibly null rather than merely
        possibly absent. Jira sends an explicit null for a field the
        requesting account cannot see or that doesn't apply to the issue
        type, and `.get(key, default)` only substitutes the default when the
        key is MISSING — a present null passes straight through. Since field
        visibility varies by account and by site, a null that never appears
        here can be routine on someone else's desk, and the resulting
        TypeError would fail every cycle while the learner still looked like
        it was running.
        """
        f       = issue.get("fields") or {}
        desc    = flatten_adf(f.get("description"))
        comps   = [c.get("name", "") for c in (f.get("components") or [])
                   if isinstance(c, dict)]
        labels  = [str(x) for x in (f.get("labels") or [])]
        req_type = (request_type_name(f.get(self.req_field))
                    if self.req_field else "")
        # Extra context fields are user-chosen; tag each value with its field
        # name so "Payroll" as a service and "Payroll" as an org stay distinct.
        structured = []
        for fid in self.context_fields:
            for name in field_names(f.get(fid)):
                structured.append(f"{fid} {name}")
        clones  = []
        for link in (f.get("issuelinks") or []):
            if not isinstance(link, dict):
                continue
            other = link.get("outwardIssue") or link.get("inwardIssue") or {}
            key   = other.get("key", "")
            if key and not key.startswith(self.project + "-"):
                clones.append({"key": key,
                               "linkType": (link.get("type") or {})
                               .get("name", "")})
        return {
            "key":      issue.get("key") or "",
            "summary":  str(f.get("summary") or ""),
            "desc":     desc[:800],
            "labels":   labels,
            "components": comps,
            "requestType": req_type,
            "structured": structured,
            "clones":   clones,
            "reporter": (f.get("reporter") or {}).get("displayName", ""),
        }

    def _features_for(self, det: dict, req_type: str) -> list[str]:
        return features_for(det, req_type)

    def _transitions(self, key: str) -> tuple[list[dict], list[dict]]:
        """(handoffs into a destination status, transitions into a resolved
        status) from the full changelog — a ticket that was ever handed off is
        never counted as a resolved-in-place outcome."""
        routed: list[dict] = []
        resolved: list[dict] = []
        for h in json_objects(self.jira.changelog(key)):
            for item in json_objects(h.get("items")):
                if item.get("field") != "status":
                    continue
                to = item.get("toString", "")
                rec = {
                    "id":   f"{key}:{h.get('id', '')}",
                    "dest": to,
                    "by":   (h.get("author") or {}).get("displayName", ""),
                    "when": (h.get("created") or "")[:16].replace("T", " "),
                }
                if to in self.dests:
                    routed.append(rec)
                elif to in self.resolved:
                    resolved.append(rec)
        return routed, resolved

    def _project_moves(self, key: str) -> tuple[list[dict], str]:
        """(moves of an intake ticket out to another project, its original
        summary).

        When an issue is moved between projects Jira changes its key and
        records that as a "Key" change in the *destination* issue's
        changelog, so the old intake key is recoverable. Any later rename is
        captured too, because the wording at intake — not the tidied-up title
        — is what the team actually triaged on.
        """
        moves: list[dict] = []
        first_summary = ""
        for h in json_objects(self.jira.changelog(key)):
            for item in json_objects(h.get("items")):
                field = item.get("field", "")
                if field == "Key" and (item.get("fromString") or "") \
                        .startswith(self.project + "-"):
                    moves.append({
                        "id":     f"{key}:{h.get('id', '')}",
                        "oldKey": item.get("fromString", ""),
                        "by":     (h.get("author") or {}).get("displayName", ""),
                        "when":   h.get("created", "")[:16].replace("T", " "),
                    })
                elif field == "summary" and not first_summary:
                    first_summary = item.get("fromString") or ""
        return moves, first_summary

    def _find_moved_issue(self, old_key: str) -> Optional[dict]:
        """Resolve an intake key that has vanished from the project.

        Jira keeps the old key as an alias after a move, so fetching it
        returns the issue under its new key — which tells us, with no
        configuration at all, which project the ticket was moved into.
        """
        data = self.jira.get(f"/rest/api/3/issue/{old_key}",
                             {"fields": ",".join(self._FIELDS)})
        if not data:
            return None
        new_key = data.get("key", "")
        if not new_key or new_key.startswith(self.project + "-"):
            return None          # still in the intake project — not a move
        return data

    # -- backfill --------------------------------------------------------------

    def _backfill_checkpoint(self, seen: dict) -> None:
        """Save progress so an interrupted backfill doesn't discard it.

        This is a one-time, potentially slow pass over a project's whole
        history — one Jira call per issue just for its changelog, easily
        thousands of calls on a busy desk. A crash, a forced restart or the
        machine sleeping partway through used to lose everything learned so
        far, since seenTransitions and the history file were only written
        once, right at the end; backfillDone correctly stays False so it
        retries, but from zero. Checkpointing here means a retry resumes
        with what was already learned kept, rather than starting over.
        """
        self.state["seenTransitions"] = list(seen)
        self._save_state()

    def backfill(self) -> None:
        if self.state.get("backfillDone"):
            return
        # train strictly off the last BACKFILL_DAYS of decisions
        cutoff = f"{datetime.now() - timedelta(days=BACKFILL_DAYS):%Y-%m-%d}"
        jql = (f'project = {self.project} AND status CHANGED TO '
               f'({", ".join(chr(34) + d + chr(34) for d in self.dests)}) '
               f'AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC')
        # No handoff statuses on this site — every ticket is resolved in place
        # or moved out, so there is nothing for this pass to find.
        issues = (self.jira.search(jql, self._FIELDS, cap=BACKFILL_CAP)
                  if self.dests else [])
        seen: dict[str, None] = dict.fromkeys(
            self.state.setdefault("seenTransitions", []))
        learned = 0
        first_routing = "9999"
        for issue in issues:
            if self.stop.is_set():
                self._backfill_checkpoint(seen)
                return
            det = self._issue_details(issue)
            routed, _ = self._transitions(det["key"])
            for tr in routed:
                if tr["id"] in seen or tr["when"] < cutoff:
                    continue
                first_routing = min(first_routing, tr["when"])
                seen[tr["id"]] = None
                req_type = det.get("requestType", "")
                toks     = self._features_for(det, req_type)
                projs    = [c["key"].split("-")[0] for c in det["clones"]]
                self.learner.learn(tr["dest"], toks, req_type, projs,
                                   reporter=det["reporter"],
                                   exemplar=tokenize(det["summary"]))
                learned += 1
                self._append_history(det, tr["dest"], tr["when"])
                self._log({"event": "backfill", "key": det["key"],
                           "summary": det["summary"], "dest": tr["dest"],
                           "by": tr["by"], "when": tr["when"],
                           "requestType": req_type,
                           "clones": [c["key"] for c in det["clones"]]})

        # Handoffs may only have started recently — anchor the training window
        # to the first one observed, so resolutions from before the process
        # existed can't teach "everything gets resolved in place".
        if first_routing != "9999":
            cutoff = max(cutoff, first_routing[:10])
            self.state["processStart"] = cutoff

        # Phase 2: tickets resolved inside the intake project, never handed off
        resolved_list = ", ".join(chr(34) + s + chr(34)
                                  for s in sorted(self.resolved))
        jql2 = (f'project = {self.project} AND status CHANGED TO '
                f'({resolved_list}) AFTER -{BACKFILL_DAYS}d '
                f'ORDER BY updated DESC')
        for issue in self.jira.search(jql2, self._FIELDS, cap=BACKFILL_CAP):
            if self.stop.is_set():
                self._backfill_checkpoint(seen)
                return
            det = self._issue_details(issue)
            routed, resolutions = self._transitions(det["key"])
            if routed:      # handed off at some point — learned above
                continue
            for tr in resolutions:
                if tr["id"] in seen or tr["when"] < cutoff:
                    continue
                seen[tr["id"]] = None
                req_type = det.get("requestType", "")
                toks     = self._features_for(det, req_type)
                self.learner.learn(self.resolved_label, toks, req_type, [],
                                   reporter=det["reporter"],
                                   exemplar=tokenize(det["summary"]))
                learned += 1
                self._append_history(det, self.resolved_label, tr["when"])
                self._log({"event": "backfill", "key": det["key"],
                           "summary": det["summary"],
                           "dest": self.resolved_label,
                           "by": tr["by"], "when": tr["when"],
                           "requestType": req_type, "clones": []})

        # Phase 3: tickets moved out into another project entirely. Each
        # known move-target project is scanned for issues whose changelog
        # shows they arrived from the intake project.
        for target in list(self.move_targets):
            jql3 = (f'project = {target} AND updated >= -{BACKFILL_DAYS}d '
                    f'ORDER BY created DESC')
            label = self.moved_label(target)
            for issue in self.jira.search(jql3, self._FIELDS,
                                          cap=BACKFILL_CAP):
                if self.stop.is_set():
                    self._backfill_checkpoint(seen)
                    return
                det = self._issue_details(issue)
                moves, orig_summary = self._project_moves(det["key"])
                if not moves:
                    continue
                if orig_summary:  # learn the intake wording, not the rename
                    det["summary"] = f"{orig_summary} — {det['summary']}"
                req_type = det.get("requestType", "")
                toks = self._features_for(det, req_type)
                for mv in moves:
                    if mv["id"] in seen or mv["when"] < cutoff:
                        continue
                    seen[mv["id"]] = None
                    self.learner.learn(label, toks, req_type, [],
                                       reporter=det["reporter"],
                                       exemplar=tokenize(det["summary"]))
                    learned += 1
                    self._append_history(det, label, mv["when"])
                    self._log({"event": "backfill", "key": mv["oldKey"],
                               "summary": det["summary"], "dest": label,
                               "by": mv["by"], "when": mv["when"],
                               "requestType": req_type,
                               "clones": [det["key"]]})

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

    def predict_intake(self) -> None:
        """FAST loop: suggest a destination for every ticket currently at
        intake (unresolved, New status category — the same tickets the overlay
        shows in its alert queue). Runs every ~12 s so a suggestion is ready
        before the team routes the ticket out of the queue. Cheap: one search,
        no per-ticket calls, no model mutation."""
        predicted: dict = self.state.setdefault("predicted", {})
        changed = False
        intake_keys: set[str] = set()
        for issue in self.jira.search(
                f'project = {self.project} AND resolution = Unresolved '
                f'AND statusCategory = New', self._FIELDS, cap=100):
            key = issue.get("key", "")
            if key:
                intake_keys.add(key)
            if not key or key in predicted:
                continue
            det  = self._issue_details(issue)
            req_type = det.get("requestType", "")
            toks = self._features_for(det, req_type)
            p = self.learner.predict_ex(
                toks, req_type, det["reporter"],
                exemplar=tokenize(det["summary"]))
            predicted[key] = {"dest": p["dest"], "conf": p["conf"],
                              "plan": p["note"], "second": p["second"],
                              "secondConf": p["secondConf"],
                              "review": p["review"],
                              "source": p["source"],
                              "requestType": req_type,
                              "summary": det["summary"][:120],
                              "at": f"{datetime.now():%Y-%m-%d %H:%M}"}
            changed = True
            self._log({"event": "would_have_done", "key": key,
                       "summary": det["summary"],
                       "prediction": p["dest"], "confidence": p["conf"],
                       "second": p["second"], "review": p["review"],
                       "source": p["source"], "requestType": req_type,
                       "plan": p["note"]})
        # Tickets still queued at intake — _learn_moved_out skips these,
        # since a ticket that hasn't left can't have been moved.
        self._intake_keys = intake_keys
        # always refresh the file so its mtime stays current (overlay ignores
        # files older than 15 min); persist state only when something changed
        self._write_suggestions(predicted)
        if changed:
            self._save_state()

    def resolve_and_learn(self) -> None:
        """SLOW loop: watch how tickets were actually routed, score our
        earlier suggestions against the team's decision, and train the model."""
        since = self._window_jql_time()
        predicted: dict = self.state.setdefault("predicted", {})

        # 2. Actual routings — resolve predictions and learn
        seen: dict[str, None] = dict.fromkeys(
            self.state.setdefault("seenTransitions", []))
        stats = self.state.setdefault("stats", {"predictions": 0, "correct": 0})
        # A project where everything is resolved in place has no handoff
        # statuses; "status CHANGED TO ()" is not valid JQL, so skip the pass
        # rather than issuing a query that fails every cycle.
        dest_list = ", ".join(chr(34) + d + chr(34) for d in self.dests)
        for issue in (self.jira.search(
                f'project = {self.project} AND status CHANGED TO ({dest_list}) '
                f'AFTER {since}', self._FIELDS, cap=50) if self.dests else []):
            det = self._issue_details(issue)
            routed, _ = self._transitions(det["key"])
            for tr in routed:
                if tr["id"] in seen:
                    continue
                seen[tr["id"]] = None
                req_type = det.get("requestType", "")
                toks     = self._features_for(det, req_type)
                projs    = [c["key"].split("-")[0] for c in det["clones"]]
                correct  = self._resolve_prediction(
                    det, tr["dest"], tr, req_type, predicted, stats,
                    clones=[c["key"] for c in det["clones"]])
                self.learner.learn(tr["dest"], toks, req_type, projs,
                                   reporter=det["reporter"],
                                   exemplar=tokenize(det["summary"]),
                                   weight=int(TUNING["mistakeWeight"])
                                   if correct is False else 1)
                self._append_history(det, tr["dest"], tr["when"])

        # 3. Tickets resolved inside the intake project, never handed off
        resolved_list = ", ".join(chr(34) + s + chr(34)
                                  for s in sorted(self.resolved))
        for issue in self.jira.search(
                f'project = {self.project} AND status CHANGED TO '
                f'({resolved_list}) AFTER {since}', self._FIELDS, cap=50):
            det = self._issue_details(issue)
            routed, resolutions = self._transitions(det["key"])
            if routed:      # handed off — covered by the pass above
                continue
            for tr in resolutions:
                if tr["id"] in seen:
                    continue
                seen[tr["id"]] = None
                req_type = det.get("requestType", "")
                toks     = self._features_for(det, req_type)
                correct  = self._resolve_prediction(det, self.resolved_label,
                                                    tr, req_type, predicted,
                                                    stats, clones=[])
                self.learner.learn(self.resolved_label, toks, req_type, [],
                                   reporter=det["reporter"],
                                   exemplar=tokenize(det["summary"]),
                                   weight=int(TUNING["mistakeWeight"])
                                   if correct is False else 1)
                self._append_history(det, self.resolved_label, tr["when"])

        # 4. Tickets moved out to another project. Rather than scanning
        #    configured target projects, look up the ones we predicted on that
        #    have since vanished from intake: Jira keeps the old key as an
        #    alias, so it resolves to wherever the ticket went. This needs no
        #    configuration and finds move targets we've never seen before.
        self._learn_moved_out(predicted, seen, stats)

        # housekeeping
        self.state["seenTransitions"] = list(seen)[-SEEN_CAP:]
        cutoff = f"{datetime.now() - timedelta(days=30):%Y-%m-%d}"
        self.state["predicted"] = {k: v for k, v in predicted.items()
                                   if v.get("at", "9999") >= cutoff}
        self.state["lastPollEpoch"] = datetime.now().timestamp()
        self._save_state(force=False)
        self._write_suggestions(self.state["predicted"])
        self.write_report()

    def _learn_moved_out(self, predicted: dict, seen: dict,
                         stats: dict) -> None:
        """Catch tickets that left the intake project by being MOVED.

        A move changes the issue key, so such a ticket simply stops appearing
        in intake searches — no status transition marks it. We therefore
        re-fetch keys we predicted on but haven't yet scored; Jira resolves an
        old key to the moved issue, revealing the destination project.
        """
        # Tickets still sitting in intake haven't gone anywhere yet, so there
        # is nothing to look up for them.
        still_in_intake = getattr(self, "_intake_keys", set())
        unscored = [k for k, p in predicted.items()
                    if p.get("dest") is not None and not p.get("scored")
                    and k not in still_in_intake]
        # Oldest first, and bounded so a backlog can't stall the cycle.
        unscored.sort(key=lambda k: predicted[k].get("at", ""))
        for old_key in unscored[:MOVE_LOOKUPS_PER_CYCLE]:
            if self.stop.is_set():
                return
            moved = self._find_moved_issue(old_key)
            if moved is None:
                continue
            det = self._issue_details(moved)
            new_project = moved.get("key", "").split("-")[0]
            moves, orig_summary = self._project_moves(moved.get("key", ""))
            move_id = moves[0]["id"] if moves else f"{old_key}:moved"
            if move_id in seen:
                continue
            seen[move_id] = None
            self._remember_move_target(new_project)
            label = self.moved_label(new_project)
            if orig_summary:
                det["summary"] = f"{orig_summary} — {det['summary']}"
            req_type = det.get("requestType", "")
            toks = self._features_for(det, req_type)
            when = (moves[0]["when"] if moves
                    else f"{datetime.now():%Y-%m-%d %H:%M}")
            by = moves[0]["by"] if moves else ""
            correct = self._resolve_prediction(
                dict(det, key=old_key), label, {"by": by, "when": when},
                req_type, predicted, stats, clones=[moved.get("key", "")])
            self.learner.learn(label, toks, req_type, [],
                               reporter=det["reporter"],
                               exemplar=tokenize(det["summary"]),
                               weight=int(TUNING["mistakeWeight"])
                               if correct is False else 1)
            self._append_history(det, label, when)

    # -- daily self-improvement ------------------------------------------------

    @staticmethod
    def _load_history() -> Optional[list[dict]]:
        """Labelled decisions in chronological order, or None if there is no
        history file yet. A truncated final line (power loss mid-append) is
        skipped rather than aborting the whole nightly tune."""
        recs: list[dict] = []
        bad = 0
        try:
            with open(HISTORY_FILE, encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        recs.append(json.loads(line))
                    except json.JSONDecodeError:
                        bad += 1
        except FileNotFoundError:
            return None
        if bad:
            _log_error(f"history: skipped {bad} unparseable line(s)")
        recs.sort(key=lambda r: r.get("when", ""))
        return recs

    def _replay_history(self, recs: list[dict], cfg: dict,
                        score_start: Optional[int] = None,
                        score_end: Optional[int] = None) -> dict:
        """Chronologically predict-then-learn over the labelled history under
        `cfg`.

        Return headline accuracy plus balanced/worst-class accuracy so tuning
        cannot hide weak BI/DEV performance behind high-volume IT/DATA wins.
        """
        saved = dict(TUNING)
        TUNING.update(cfg)
        try:
            lr = Learner({})
            warm = min(150, len(recs) // 5)
            start = warm if score_start is None else score_start
            end = len(recs) if score_end is None else score_end
            n = ok = firm = firm_ok = 0
            per: dict[str, list[int]] = {}
            firm_per: dict[str, list[int]] = {}
            source_per: dict[str, list[int]] = {}
            for i, r in enumerate(recs):
                det = {"summary": r["summary"], "desc": r["desc"],
                       "labels": r["labels"], "components": r["components"],
                       "structured": r.get("structured", []),
                       "reporter": r["reporter"], "key": "", "clones": []}
                req_type = r.get("requestType", r.get("reqType", ""))
                toks  = features_for(det, req_type)
                title = tokenize(r["summary"])
                p = lr.predict_ex(toks, req_type, r["reporter"], exemplar=title)
                if p["dest"] is not None and start <= i < end:
                    hit = int(p["dest"] == r["dest"])
                    n += 1; ok += hit
                    row = per.setdefault(r["dest"], [0, 0])
                    row[0] += 1; row[1] += hit
                    srow = source_per.setdefault(p.get("source", "model"), [0, 0])
                    srow[0] += 1; srow[1] += hit
                    if not p["review"]:
                        firm += 1; firm_ok += hit
                        frow = firm_per.setdefault(r["dest"], [0, 0])
                        frow[0] += 1; frow[1] += hit
                w = int(cfg.get("mistakeWeight", 1)) if (
                    p["dest"] is not None and p["dest"] != r["dest"]) else 1
                lr.learn(r["dest"], toks, req_type, r["cloneProjects"],
                         r["reporter"], weight=w, exemplar=title)
            per_acc = {k: v[1] / v[0] for k, v in per.items() if v[0]}
            firm_per_acc = {k: v[1] / v[0] for k, v in firm_per.items() if v[0]}
            source_acc = {k: {"n": v[0], "acc": v[1] / v[0]}
                          for k, v in source_per.items() if v[0]}
            return {"acc": ok / n if n else 0.0,
                    "firmAcc": firm_ok / firm if firm else 0.0,
                    "firmCov": firm / n if n else 0.0, "n": n,
                    "balanced": (sum(per_acc.values()) / len(per_acc))
                    if per_acc else 0.0,
                    "worst": min(per_acc.values(), default=0.0),
                    "per": per_acc,
                    "perCounts": per,
                    "firmBalanced": (sum(firm_per_acc.values()) / len(firm_per_acc))
                    if firm_per_acc else 0.0,
                    "firmWorst": min(firm_per_acc.values(), default=0.0),
                    "firmPer": firm_per_acc,
                    "firmPerCounts": firm_per,
                    "sources": source_acc}
        finally:
            TUNING.clear(); TUNING.update(saved)

    # Values the nightly tune is allowed to explore, per knob. Searched by
    # coordinate descent rather than the old fixed list of 13 hand-written
    # combinations, which could only ever move one or two knobs at a time.
    _TUNE_AXES = {
        "summaryWeight":         [3, 4, 5, 6, 8, 10],
        "mistakeWeight":         [1, 2, 3, 4],
        "decayFactor":           [1.0, 0.999, 0.997, 0.995, 0.99],
        "confTemp":              [2.0, 3.0, 4.0, 6.0, 8.0],
        "priorWeight":           [0.0, 0.25, 0.5, 0.75, 1.0],
        "knnThresh":             [0.0, 0.6, 0.7, 0.8, 0.9],
        "requestTypeHintWeight": [0.0, 1.5, 3.0, 5.0],
        # Who raised a ticket predicts the receiving team on its own far
        # better than chance, but contributes a single log term against one
        # per token. Whether amplifying it actually helps was inconclusive on
        # a 400-ticket holdout — a real peak, but worth about one ticket — so
        # it is offered to the tuner at a default of 1.0 (today's behaviour)
        # rather than hard-coded, and adopted only if the evidence grows.
        "reporterWeight":        [0.0, 1.0, 2.0, 3.0, 5.0],
        "reviewConf":            [65, 70, 75, 80, 85],
        "reviewMargin":          [10, 15, 20, 25],
    }
    _TUNE_KEEP   = tuple(_TUNE_AXES)
    _TUNE_BUDGET = 120          # replays per night — ~1.5 min at current size
    _TUNE_PASSES = 3

    @staticmethod
    def _tune_score(m: dict) -> tuple:
        """Rank candidates: balanced accuracy first so a config cannot win by
        sacrificing the low-volume BI/DEV classes for the high-volume ones."""
        return (round(m["balanced"], 3),
                round(m["worst"], 3),
                round(m["firmBalanced"], 3),
                round(m["firmWorst"], 3),
                round(m["firmAcc"], 3),
                m["firmCov"],
                round(m["acc"], 3))

    def _self_tune(self, recs: list[dict]) -> Optional[dict]:
        """Tune on older decisions, then report on an untouched recent tail."""
        validation_n = max(100, min(400, len(recs) // 5))
        split = len(recs) - validation_n
        tuning_recs = recs[:split]

        cache: dict[tuple, tuple] = {}
        spent = 0

        def evaluate(cfg: dict, budget: list[int]):
            """Scored replay of `cfg`, memoised; None once out of budget.

            `budget` is this descent's own remaining allowance, not the
            shared total — a cache hit costs nothing, but a fresh replay
            draws only from the caller's share, so one hungry descent can't
            spend the whole night's budget before the other gets to run.
            """
            nonlocal spent
            sig = tuple(cfg[k] for k in self._TUNE_KEEP)
            if sig in cache:
                return cache[sig]
            if budget[0] <= 0:
                return None
            budget[0] -= 1
            spent += 1
            m = self._replay_history(tuning_recs, cfg)
            cache[sig] = (self._tune_score(m), m)
            return cache[sig]

        def descend(seed: dict, budget: list[int]):
            """Coordinate descent from `seed`: sweep each knob against the
            current best, keep any improvement, repeat until a full pass
            changes nothing."""
            cur = dict(seed)
            got = evaluate(cur, budget)
            if got is None:
                return None
            best_score, best_m = got
            for _ in range(self._TUNE_PASSES):
                improved = False
                for axis, values in self._TUNE_AXES.items():
                    for value in values:
                        if cur.get(axis) == value:
                            continue
                        trial = dict(cur, **{axis: value})
                        got = evaluate(trial, budget)
                        if got is None:
                            break
                        score, m = got
                        if score > best_score:
                            best_score, best_m, cur = score, m, trial
                            improved = True
                    if budget[0] <= 0:
                        break
                if not improved or budget[0] <= 0:
                    break
            return best_score, best_m, cur

        # Greedy descent only ever explores the basin it starts in: once a
        # knob such as decayFactor settles near last night's value, testing
        # it alone rarely looks better, even when a very different value
        # would win once paired with other knobs the search already moved
        # away from. Descending twice - once warm-started from tonight's
        # live config, once from the fixed, offline-validated baseline -
        # and keeping whichever finishes with the better score is what
        # surfaced decayFactor=1.0 (never forget) as far better for the
        # low-volume BI class than the 0.999 the live config was stuck on.
        # A single descent alone can exhaust the whole budget before
        # converging, so each gets its own guaranteed half rather than
        # sharing one pool the first descent could spend in full.
        half = max(1, self._TUNE_BUDGET // 2)
        runs = [descend(dict(TUNING), [half]),
                descend(dict(BALANCED_TUNED_CONFIG),
                        [self._TUNE_BUDGET - half])]
        runs = [r for r in runs if r is not None]
        if not runs:
            return None
        best_score, best_m, cur = max(runs, key=lambda r: r[0])

        best = {"score": best_score,
                "cfg": {k: cur[k] for k in self._TUNE_KEEP},
                "train": best_m, "trainN": best_m["n"], "split": split,
                "evals": spent}
        if best is not None:
            holdout = self._replay_history(recs, best["cfg"], score_start=split)
            best["holdout"] = holdout
            best["holdoutN"] = holdout["n"]
            for key in ("acc", "balanced", "worst", "firmAcc", "firmBalanced",
                        "firmWorst", "firmCov", "n", "per", "firmPer",
                        "perCounts", "firmPerCounts", "sources"):
                best[key] = holdout[key]
        return best

    def _rebuild_model(self, recs: list[dict]) -> None:
        """Re-fit a clean model over the whole history with the current TUNING
        (called after adopting a freshly-tuned config)."""
        # Pass the plans through: without them the nightly re-fit silently
        # replaced the learner with one that had no outcome descriptions, so
        # the report and suggestions lost their wording until the next restart.
        lr = Learner({}, plans=self.plans)
        skipped = 0
        for r in recs:
            # One unusable record must not cost the entire re-fit. These are
            # read back from a file written by earlier versions, so fields
            # can be absent, null, or the wrong type; anything that still
            # can't be used is skipped and the other few thousand decisions
            # are kept.
            try:
                det = {"summary": str(r.get("summary") or ""),
                       "desc": str(r.get("desc") or ""),
                       "labels": as_name_list(r.get("labels")),
                       "components": as_name_list(r.get("components")),
                       "structured": as_name_list(r.get("structured")),
                       "reporter": str(r.get("reporter") or ""),
                       "key": "", "clones": []}
                dest = r.get("dest")
                if not dest:
                    skipped += 1
                    continue
                req_type = str(r.get("requestType")
                               or r.get("reqType") or "")
                toks = features_for(det, req_type)
                lr.learn(dest, toks, req_type,
                         as_name_list(r.get("cloneProjects")),
                         det["reporter"], exemplar=tokenize(det["summary"]))
            except Exception:                       # noqa: BLE001
                skipped += 1
        if skipped:
            _log_error(f"model rebuild skipped {skipped} unusable "
                       f"record(s) of {len(recs)}")
        self.state["model"] = lr.m
        self.learner = lr

    def maybe_daily_tune(self) -> None:
        """Once a day, replay the accumulated history under candidate configs,
        adopt the best, and re-fit the model — so the learner measurably sharpens
        itself as more real decisions accrue. Fully self-contained; never writes
        to Jira. Any failure is logged and left to the next day."""
        today = date.today().isoformat()
        if self.state.get("lastTuneDate") == today:
            return
        recs = self._load_history()
        if recs is None:
            return
        if len(recs) < 300:                 # too little to tune reliably yet
            self.state["lastTuneDate"] = today
            self._save_state()
            return
        # Search over a bounded window, but fit the final model on everything.
        # The search replays the history once per candidate config, so its
        # cost is (history x budget): fine at a few thousand decisions,
        # roughly an hour of CPU a night at twenty-five thousand — on a
        # machine somebody is trying to work on. Recency decay already means
        # the oldest decisions barely move the result, so the tail they would
        # contribute isn't worth the time it costs a busy desk.
        search_recs = recs[-TUNE_WINDOW:]
        if len(search_recs) < len(recs):
            _log_error(f"self-tune searching the most recent "
                       f"{len(search_recs)} of {len(recs)} decisions")
        best = self._self_tune(search_recs)
        if best:
            TUNING.update(best["cfg"])
            self.state["tunedConfig"] = best["cfg"]
            self._rebuild_model(recs)
            hist = self.state.setdefault("tuneHistory", [])
            hist.append({"date": today, "acc": round(best["acc"], 4),
                         "balanced": round(best["balanced"], 4),
                         "worst": round(best["worst"], 4),
                         "firmAcc": round(best["firmAcc"], 4),
                         "firmBalanced": round(best["firmBalanced"], 4),
                         "firmWorst": round(best["firmWorst"], 4),
                         "firmCov": round(best["firmCov"], 4),
                         "n": best["n"], "trainN": best["trainN"],
                         "holdoutN": best["holdoutN"],
                         "per": {k: round(v, 4)
                                 for k, v in best["per"].items()},
                         "firmPer": {k: round(v, 4)
                                     for k, v in best["firmPer"].items()},
                         "cfg": best["cfg"]})
            self.state["tuneHistory"] = hist[-60:]
            _log_error(f"daily self-tune {today}: holdoutFirm={best['firmAcc']:.1%} "
                       f"balanced={best['balanced']:.1%} "
                       f"worst={best['worst']:.1%} "
                       f"holdoutAcc={best['acc']:.1%} n={best['holdoutN']} "
                       f"cfg={best['cfg']}")
        self.state["lastTuneDate"] = today
        self._save_state()
        self.write_report()

    def _dest_phrase(self, dest: str) -> str:
        """Short outcome name for the overlay's one-line suggestion. Labels
        already read naturally (a status name, "Resolved in <project>",
        "Moved to <project>"), so only the resolved label is shortened, to
        fit the overlay's narrow popup."""
        if dest == self.resolved_label:
            return f"resolve in {self.project}"
        return dest

    def _short_label(self, p: dict) -> str:
        """One-line suggestion. On a close call, show both candidates and
        ask for a human check instead of a confidently-wrong single answer."""
        best = self._dest_phrase(p["dest"])
        if p.get("review") and p.get("second"):
            second = self._dest_phrase(p["second"])
            return (f"review — likely {best} ({p['conf']}%), "
                    f"or {second} ({p.get('secondConf', 0)}%)")
        return f"{best} · {p['conf']}%"

    def _write_suggestions(self, predicted: dict) -> None:
        """Publish current suggestions for the overlay to display.
        Suggestions only — nothing here ever touches Jira."""
        cutoff = f"{datetime.now() - timedelta(days=7):%Y-%m-%d}"
        out = {}
        for key, p in predicted.items():
            if p.get("dest") and p.get("at", "") >= cutoff:
                out[key] = {"label": self._short_label(p),
                            "review": bool(p.get("review")),
                            "plan": p.get("plan", ""), "at": p["at"]}
        _atomic_json_write(SUGGEST_FILE, out)

    def _resolve_prediction(self, det: dict, dest: str, tr: dict,
                            req_type: str, predicted: dict, stats: dict,
                            clones: list[str]) -> Optional[bool]:
        """Score our earlier guess against the human decision and log it.
        Returns True/False for a scored prediction, None if we never made
        one (e.g. the ticket predates the learner)."""
        pred    = predicted.get(det["key"], {})
        p_dest  = pred.get("dest")
        correct = None
        if pred:
            # Outcome known — stop re-probing this key for a project move.
            pred["scored"] = True
        if p_dest is not None:
            correct = (p_dest == dest)
            stats["predictions"] += 1
            stats["correct"]     += int(correct)
            confusion = self.state.setdefault("confusion", {})
            pair = f"{p_dest} → {dest}"
            confusion[pair] = confusion.get(pair, 0) + 1
            # per-day tally so the report can show the accuracy trend
            daily = self.state.setdefault("dailyStats", {})
            day   = daily.setdefault(date.today().isoformat(), [0, 0])
            day[0] += 1
            day[1] += int(correct)
        self._log({"event": "routed", "key": det["key"],
                   "summary": det["summary"], "dest": dest,
                   "by": tr["by"], "when": tr["when"],
                   "requestType": req_type, "clones": clones,
                   "predicted": p_dest,
                   "confidence": pred.get("conf"),
                   "second": pred.get("second"),
                   "secondConfidence": pred.get("secondConf"),
                   "review": pred.get("review"),
                   "source": pred.get("source"),
                   "predictionCorrect": correct})
        return correct

    # -- report --------------------------------------------------------------

    _PENDING_CAP = 4000    # unresolved intake predictions kept for context

    def _reset_metrics(self) -> None:
        self._metrics = {"routed": 0, "predicted": 0, "correct": 0,
                         "confident": [0, 0], "review": [0, 0],
                         "byActual": {}, "confidentByActual": {},
                         "bySource": {}, "withRequestType": 0}
        self._metrics_offset = 0
        self._metrics_pending: dict[str, dict] = {}

    def _live_metrics(self) -> dict:
        """Coverage and calibration reconstructed from the append-only log.

        The log only ever grows, so we fold in whatever is new since the last
        call rather than re-reading the whole file — the report is rewritten
        every minute and a full reparse got steadily more expensive as the
        log accumulated."""
        if self._metrics is None:
            self._reset_metrics()
        result  = self._metrics
        pending = self._metrics_pending
        try:
            size = os.path.getsize(LOG_FILE)
        except OSError:
            return result
        if size < self._metrics_offset:      # truncated or replaced — restart
            self._reset_metrics()
            result, pending = self._metrics, self._metrics_pending
        if size == self._metrics_offset:
            return result
        try:
            # Binary + explicit decode: text-mode tell() is unusable once the
            # handle has been iterated, and we must remember where we stopped.
            with open(LOG_FILE, "rb") as f:
                f.seek(self._metrics_offset)
                data = f.read()
        except OSError:
            return result
        cut = data.rfind(b"\n") + 1          # ignore a half-written last line
        if not cut:
            return result
        self._metrics_offset += cut

        for raw in data[:cut].decode("utf-8", "replace").splitlines():
            if not raw.strip():
                continue
            try:
                rec = json.loads(raw)
            except json.JSONDecodeError:
                continue
            key = rec.get("key", "")
            if rec.get("event") == "would_have_done":
                pending[key] = rec
                if len(pending) > self._PENDING_CAP:
                    for stale in list(pending)[:len(pending) - self._PENDING_CAP]:
                        del pending[stale]
                continue
            if rec.get("event") != "routed":
                continue
            result["routed"] += 1
            if rec.get("requestType"):
                result["withRequestType"] += 1
            if rec.get("predicted") is None:
                continue
            result["predicted"] += 1
            hit = int(bool(rec.get("predictionCorrect")))
            result["correct"] += hit
            actual = rec.get("dest", "unknown")
            row = result["byActual"].setdefault(actual, [0, 0])
            row[0] += 1; row[1] += hit
            context = rec if rec.get("review") is not None \
                else pending.get(key, {})
            band = "review" if context.get("review") else "confident"
            result[band][0] += 1; result[band][1] += hit
            if band == "confident":
                crow = result["confidentByActual"].setdefault(actual, [0, 0])
                crow[0] += 1; crow[1] += hit
            source = context.get("source")
            if source:
                srow = result["bySource"].setdefault(source, [0, 0])
                srow[0] += 1; srow[1] += hit
        return result

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
            f"## Learned triage map for `{self.project}`",
            "",
            "| Outcome | What the team does | Observed examples |",
            "|---|---|---|",
        ]
        seen = m.get("seen", {})
        for dest in self.all_dests:
            lines.append(f"| {dest} | {self.learner.plan_for(dest)} | "
                         f"{seen.get(dest, 0)} |")

        live = self._live_metrics()
        lines += ["", f"## Prediction accuracy: {acc}"]
        if live["routed"]:
            coverage = 100 * live["predicted"] // live["routed"]
            lines.append(f"- **Prediction coverage:** {coverage}% "
                         f"({live['predicted']}/{live['routed']} routed tickets "
                         "received an intake suggestion)")
        for key, label in (("confident", "Confident suggestions"),
                           ("review", "Review-flagged suggestions")):
            total, right = live[key]
            if total:
                lines.append(f"- **{label}:** {100 * right // total}% "
                             f"({right}/{total})")
        if live["bySource"]:
            source_bits = []
            for source, (total, right) in sorted(live["bySource"].items()):
                source_bits.append(f"{source} {100 * right // total}% "
                                   f"({right}/{total})")
            lines.append("- **Novel vs recurring:** " + "; ".join(source_bits))
        lines.append("- **Structured signals:** Request Type, affected service, "
                     "and organisation are enabled for new observations")
        if live["confidentByActual"]:
            lines += ["", "### Confident accuracy by actual route", "",
                      "| Actual route | Correct confident calls | Accuracy |",
                      "|---|---:|---:|"]
            for dest in self.all_dests:
                if dest in live["confidentByActual"]:
                    total, right = live["confidentByActual"][dest]
                    lines.append(f"| {dest} | {right}/{total} | "
                                 f"{100 * right // total}% |")

        # Daily accuracy trend — shows it sharpening over time
        daily = self.state.get("dailyStats", {})
        if daily:
            lines += ["", "## Daily accuracy (live suggestions vs team)", ""]
            for day in sorted(daily)[-14:]:
                dn, dc = daily[day]
                if dn:
                    bar = "█" * round(10 * dc / dn)
                    lines.append(f"- {day}: {100 * dc // dn:>3}% "
                                 f"({dc}/{dn})  {bar}")

        # Nightly self-tune status
        tuned = self.state.get("tunedConfig")
        th    = self.state.get("tuneHistory", [])
        if tuned or th:
            lines += ["", "## Self-tuning (runs daily on accumulated history)", ""]
            if tuned:
                lines.append(f"- **Current auto-tuned config:** `{tuned}`")
            for t in th[-5:]:
                prefix = (f"- {t['date']}: recent holdout "
                          if t.get("holdoutN") else f"- {t['date']}: ")
                parts = [prefix + "confident-band "
                         f"{t['firmAcc']*100:.0f}% over {t['firmCov']*100:.0f}% "
                         "of tickets"]
                if t.get("balanced") and t.get("worst"):
                    parts.append(f"balanced {t['balanced']*100:.0f}% "
                                 f"(weakest {t['worst']*100:.0f}%)")
                if t.get("holdoutN"):
                    parts.append(f"overall {t['acc']*100:.0f}% "
                                 f"({t['holdoutN']} unseen recent decisions; "
                                 f"selected on {t['trainN']})")
                else:
                    parts.append(f"overall {t['acc']*100:.0f}% "
                                 f"(replayed {t['n']})")
                lines.append(" · ".join(parts))

        confusion = self.state.get("confusion", {})
        if confusion:
            per: dict[str, list[int]] = {}
            for pair, count in confusion.items():
                p_dest, actual = pair.split(" → ")
                row = per.setdefault(actual, [0, 0])
                row[0] += count
                if p_dest == actual:
                    row[1] += count
            lines.append("")
            for dest in self.all_dests:
                if dest in per:
                    total, right = per[dest]
                    lines.append(f"- {dest}: {100 * right // total}% "
                                 f"({right}/{total})")
            wrong = sorted(((pair, n) for pair, n in confusion.items()
                            if pair.split(" → ")[0] != pair.split(" → ")[1]),
                           key=lambda kv: -kv[1])
            if wrong:
                lines += ["", "## Mistakes learned from (said → team chose)",
                          ""]
                for pair, n in wrong[:10]:
                    lines.append(f"- {pair} — {n}×")

        lines += ["", "## Signals per destination", ""]
        for dest in self.all_dests:
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
                # A line can be truncated (the writer is a plain append, not
                # atomic) or from an older schema missing a field the report
                # expects. One bad line among thousands must not blank the
                # whole report — _live_metrics and _load_history skip a bad
                # line the same way when reading this file.
                try:
                    rec = json.loads(raw)
                except json.JSONDecodeError:
                    continue
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
            if not self.ensure_workflow_known():
                _log_error("could not determine this project's workflow — "
                           "set triageStatuses / resolvedStatuses in "
                           "jira_config.json and restart")
                return
        except Exception:
            _log_error("workflow discovery failed:\n" + traceback.format_exc())
            return
        try:
            self.backfill()
        except Exception:
            _log_error("backfill failed:\n" + traceback.format_exc())
        last_slow = 0.0
        while not self.stop.is_set():
            # fast: keep suggestions fresh for tickets currently at intake
            try:
                self.predict_intake()
            except Exception:
                _log_error("intake failed:\n" + traceback.format_exc())
            # slow: score past suggestions against real routings and learn
            if time.time() - last_slow >= POLL_SECONDS:
                try:
                    self.resolve_and_learn()
                    last_slow = time.time()
                except Exception:
                    _log_error("resolve/learn failed:\n" + traceback.format_exc())
                # once a day, self-tune on the accumulated history
                try:
                    self.maybe_daily_tune()
                except Exception:
                    _log_error("daily tune failed:\n" + traceback.format_exc())
            self.stop.wait(FAST_POLL_SECONDS)


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


def _report_fatal(message: str) -> None:
    """Tell the user why we couldn't start.

    This runs under pythonw, so there is no console: an unhandled exception
    here used to kill the process with no tray icon, no message and nothing
    in the log — indistinguishable from "it just doesn't work".
    """
    _log_error(message)
    try:
        import tkinter as tk
        from tkinter import messagebox
        root = tk.Tk()
        root.withdraw()
        messagebox.showerror("Triage Learner — cannot start", message)
        root.destroy()
    except Exception:
        pass


def main() -> None:
    # single-instance guard
    ctypes.windll.kernel32.CreateMutexW(None, False, "JiraTriageLearner")
    if ctypes.windll.kernel32.GetLastError() == 183:        # ERROR_ALREADY_EXISTS
        return

    try:
        watcher = ShadowWatcher()
    except FileNotFoundError:
        _report_fatal(f"No {os.path.basename(CONFIG_FILE)} found next to this "
                      "script.\n\nRun the overlay first so it can set up your "
                      "Jira credentials and find your service desk.")
        return
    except SystemExit as exc:
        _report_fatal(str(exc))
        return
    except Exception:
        _report_fatal("Failed to start:\n\n" + traceback.format_exc())
        return

    worker = threading.Thread(target=watcher.run_forever, daemon=True)
    worker.start()

    def _open_report() -> None:
        if os.path.exists(REPORT_FILE):
            open_in_default_or_notepad(REPORT_FILE)

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
