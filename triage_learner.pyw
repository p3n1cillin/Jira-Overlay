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

import base64
import ctypes
import hashlib
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import winreg
from collections import Counter
from datetime import date, datetime, timedelta
from typing import Optional
from zoneinfo import ZoneInfo
from source_layout import activate
activate(os.path.dirname(os.path.abspath(__file__)))
import app_updates as updates
from app_release import VERSION as APP_VERSION
from triage_support import (accuracy_counts, wilson_lower, segment_name, rolling_metrics,
                            coverage_reason, route_balance_factors, performance_drift)
from triage_evaluation import bind_engine
from triage_storage import daily_backup
from triage_jira import make_client

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
            f'    "{_sys.executable}" -m pip install '
            "requests Pillow pystray tzdata")
        _root.destroy()
    except Exception:
        pass
    # 'from None': the dialog already says what is missing, and a chained
    # ImportError traceback behind it only muddies the log.
    raise SystemExit(1) from None

# ---------------------------------------------------------------------------
# Files & constants
# ---------------------------------------------------------------------------

# Bump on a meaningful change — see jira_overlay.pyw for why this is a
# plain constant rather than something derived from git or a build step.
_DIR         = os.path.dirname(os.path.abspath(__file__))
CONFIG_FILE  = os.path.join(_DIR, "jira_config.json")
STATE_FILE   = os.path.join(_DIR, "triage_state.json")
LOG_FILE     = os.path.join(_DIR, "triage_log.jsonl")
REPORT_FILE  = os.path.join(_DIR, "TRIAGE_REPORT.md")
# machine-readable twin of the report's health/provenance section, for
# external monitoring — nobody should have to scrape markdown to find out
# whether this is still getting real data from Jira
STATUS_FILE  = os.path.join(_DIR, "triage_status.json")
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
TUNING_SCHEMA_VERSION = 8    # recent-window tuning + learned route balance
# Cap on remembered transition IDs. Kept as an *ordered* collection so the
# trim drops the oldest — truncating a set (the old behaviour) kept an
# arbitrary subset, letting already-learned transitions be re-learned.
SEEN_CAP       = 20000
# Decisions the nightly search replays. The search costs
# (history x candidate configs), so an unbounded history turns a
# two-minute job into an hour-long one on a busy desk. The model itself
# is still fitted on the whole history — only the search is windowed.
TUNE_WINDOW    = 6000
# A per-destination holdout bucket below this size is too small for its
# accuracy to mean anything night to night — measured directly on this desk:
# a 12-ticket bucket for the smallest destination swung 30+ points on one
# ticket flipping right or wrong. Below this, a swing is sampling noise, not
# drift, and flagging it would just be alert fatigue.
MIN_DRIFT_SAMPLE = 30
# Per slow cycle, how many departed tickets to re-fetch when looking for ones
# that left intake by being MOVED to another project (see _learn_moved_out).
MOVE_LOOKUPS_PER_CYCLE = 20

# Jira evaluates bare datetime literals in JQL against the *account's*
# profile timezone — not this machine's — and stamps changelog timestamps
# in that zone too. Windows and cutoffs were built from workstation-local
# time, so any mismatch (a different timezone, or DST handled differently)
# silently shifted every learning window. The account's own zone is read
# once and used wherever a wall-clock feeds a query or a comparison.
LOG_ROTATE_BYTES = 20 * 1024 * 1024   # rotate triage_log.jsonl past this size

# A route is allowed to call itself confident only when recent chronological
# validation supports that claim. Sparse/dormant outcomes remain visible as
# suggestions but are automatically marked for human review.
ROUTE_ACTIVITY_DAYS = 21
MIN_ROUTE_RECENT_ACTUAL = 3
MIN_ROUTE_FIRM_PREDICTIONS = 10
MIN_ROUTE_FIRM_PRECISION = 0.95

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
    "routeBalanceWeight": 0.0,  # neutral unless chronological validation earns a correction
    "summaryWeight": 6,   # summary features count this many times vs body
    "mistakeWeight": 3,   # a wrong prediction learns the correction at this weight
    "confTemp": 4.0,      # tokens per unit of softmax temperature (confidence)
    "reqSmooth": 10,      # smoothing for the request-type channel
    "repSmooth": 20,      # smoothing for the reporter channel
    "reporterWeight": 1.0,  # how much who-raised-it counts vs the wording
    "escalationSpecialist": 0,   # enabled only when validation beats incumbent
    "escalationPriorWeight": 0.25, # reduce high-volume-team bias in that rerank
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
    "routeBalanceWeight": 0.0,
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
    "escalationSpecialist": 0,
    "escalationPriorWeight": 0.25,
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
    try:
        with open(ERROR_LOG, "a", encoding="utf-8") as f:
            f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S}  {msg}\n")
    except OSError:
        pass   # an unwritable log must never take the app down — this runs
               # from error paths (including _report_fatal), where raising
               # would replace the real failure with a crash. The overlay's
               # own _log_error guards the same way.


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


def _system_binary(relative_path: str) -> str:
    """Fully-qualified path to a well-known Windows system binary.

    Launching by bare name ("notepad.exe") lets Windows' normal search
    order run whatever answers to that name first — a directory earlier in
    PATH than System32, planted deliberately or by another install, runs
    instead of the real thing. Falls back to the bare name only if
    System32 genuinely doesn't have it there, so an unusual Windows
    install still works rather than silently doing nothing.
    """
    candidate = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"),
                             "System32", relative_path)
    return candidate if os.path.exists(candidate) else relative_path


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
        subprocess.Popen([_system_binary("notepad.exe"), path])
    except OSError as exc:
        _log_error(f"could not open {path} with notepad.exe either: {exc}")


def _atomic_json_write(path: str, data, attempts: int = 6,
                       fallback_in_place: bool = False) -> None:
    """Write JSON atomically, tolerating short-lived Windows reader locks.

    mkstemp, not open(f"{path}.{pid}.{tid}.tmp", "w"): a PID and thread id
    are guessable, and the old name followed whatever already sat there -
    a symlink or hard link, which NTFS lets an unprivileged local process
    create - writing this file's contents through it into somewhere else
    entirely rather than failing. mkstemp's O_CREAT|O_EXCL fails outright
    if anything is already at the name instead of writing through it.
    """
    payload = json.dumps(data, ensure_ascii=False, indent=1)
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path) or ".",
                               prefix=os.path.basename(path) + ".",
                               suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(payload)
        delay = 0.05
        for attempt in range(attempts):
            try:
                os.replace(tmp, path)
                return
            except PermissionError:
                if attempt == attempts - 1:
                    if not fallback_in_place:
                        raise
                    # Some Windows readers allow the existing file to be
                    # opened for writing but deny the delete/rename access
                    # os.replace needs. Suggestions are ephemeral and the
                    # overlay already treats a half-read JSON document as
                    # "try again next refresh", so an in-place final attempt
                    # is safer than leaving advice stale indefinitely.
                    with open(path, "w", encoding="utf-8") as f:
                        f.write(payload)
                    return
                time.sleep(delay)
                delay = min(delay * 2, 0.8)
    finally:
        try:
            if os.path.exists(tmp):
                os.unlink(tmp)
        except OSError:
            pass


# ── Credential protection (DPAPI) ────────────────────────────────────────
# jira_config.json is owned by the overlay, which stores the API token as a
# DPAPI blob under tokenProtected rather than plain text — see jira_overlay
# .pyw for why. This side only ever needs to decrypt it; identical to the
# overlay's own implementation so a blob either app's Windows login produced
# decrypts the same way.
class _DPAPI_BLOB(ctypes.Structure):
    _fields_ = [("cbData", ctypes.c_uint32),
                ("pbData", ctypes.POINTER(ctypes.c_char))]


def _dpapi_call(fn, data: bytes) -> bytes:
    buf = ctypes.create_string_buffer(data, len(data))
    inblob  = _DPAPI_BLOB(len(data),
                          ctypes.cast(buf, ctypes.POINTER(ctypes.c_char)))
    outblob = _DPAPI_BLOB()
    if not fn(ctypes.byref(inblob), None, None, None, None, 0,
             ctypes.byref(outblob)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return ctypes.string_at(outblob.pbData, outblob.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(outblob.pbData)


def unprotect_secret(encoded: str) -> Optional[str]:
    """None on any failure rather than raising — a blob from a different
    Windows profile, or simply corrupted. The caller treats that as an
    empty token, which fails sign-in visibly instead of silently."""
    try:
        decrypted = _dpapi_call(ctypes.windll.crypt32.CryptUnprotectData,
                                base64.b64decode(encoded))
        return decrypted.decode("utf-8")
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Jira client (READ-ONLY)
# ---------------------------------------------------------------------------

Jira = make_client(requests, HTTPBasicAuth, json_objects, lambda message: _log_error(message))


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
        # A leaf node can carry an explicit "content": null rather than
        # omitting the key — the same present-but-null shape as everywhere
        # else Jira sends data — and .get(key, []) only falls back to []
        # when the key is MISSING, not when it's there and null.
        for child in node.get("content") or []:
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


# A real Jira project key: a letter, then letters/digits/underscore. Used
# to validate a project key before it's trusted as a bare JQL identifier
# (`project = {key}`) rather than a quoted literal — there's no quoting
# syntax for "this is a field/identifier", so the only defence is
# restricting it to characters a real key can ever contain.
_SAFE_PROJECT_KEY = re.compile(r"^[A-Za-z][A-Za-z0-9_]*$")


def jql_quote(value: str) -> str:
    """Quote a value for use as a JQL string literal.

    Status names come from this site's own Jira workflow, or from a
    hand-editable config, so they can contain quotes and backslashes -
    interpolated raw, one breaks the query outright, and a crafted one
    changes what it matches. Mirrors jira_overlay.pyw's own jql_quote();
    kept in step with it deliberately.
    """
    escaped = (value or "").replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


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

    def __init__(self, state: dict, plans: Optional[dict] = None,
                 escalation_dests: Optional[list[str]] = None,
                 route_safety: Optional[dict] = None) -> None:
        # dest -> plain-English plan, owned by the watcher (which knows this
        # site's workflow) and shared by reference so newly discovered
        # outcomes describe themselves without rebuilding the model.
        self._plans = plans if plans is not None else {}
        self._escalation_dests = list(escalation_dests or [])
        # Shared by reference with the watcher, which refreshes it after each
        # nightly validation without needing to rebuild the live learner.
        self._route_safety = route_safety if route_safety is not None else {}
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
        balance = m.get("routeBalance")
        if not isinstance(balance, dict):
            balance = {}
        m["routeBalance"] = balance
        for name in ("actual", "predicted"):
            values = balance.get(name)
            balance[name] = {k: v for k, v in values.items()
                             if isinstance(v, (int, float)) and math.isfinite(v) and v >= 0} if isinstance(values, dict) else {}
        observations = balance.get("observations", 0)
        if not isinstance(observations, (int, float)) or not math.isfinite(observations) or observations < 0:
            observations = 0
        balance["observations"] = observations
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
              weight: int = 1, exemplar: Optional[list[str]] = None,
              prediction: Optional[dict] = None) -> None:
        m = self.m
        # Record a pre-learning prediction, never score after seeing this label.
        # Replay supplies the prediction it already made to avoid a second fit.
        if prediction is None:
            prediction = self.predict_ex(tokens, req_type, reporter, exemplar=exemplar)
        if prediction.get("dest") is not None:
            balance = m["routeBalance"]
            for table in ("actual", "predicted"):
                for route in list(balance[table]):
                    balance[table][route] *= 0.995
            for table, route in (("actual", dest), ("predicted", prediction["dest"])):
                balance[table][route] = balance[table].get(route, 0) + 1
            balance["observations"] += 1
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

    _WHO_MIN_SEEN = 3     # times this reporter must appear in either channel
    _WHO_MIN_RATIO = 2.0  # ...and favour `best` by at least this much

    def _why(self, counts: "Counter[str]", best: str, second: str,
             reporter: str = "") -> list[str]:
        """The few words in this ticket that most favoured `best` over the
        runner-up — so a suggestion is something a reviewer can check, not
        just a number to trust. Same per-token smoothing predict_ex already
        uses, just compared head-to-head against the second choice instead
        of against every destination at once, which is cheap: two
        destinations, not the whole ready set.

        Wording isn't the whole story — reporter alone is a strong signal
        (measured at 82.6% discriminative power on this desk's real
        history) and a call driven mostly by who raised it, with an
        otherwise generic ticket, would otherwise look unexplained. Only
        mentioned once this reporter has raised enough tickets for the
        pattern to be more than a coincidence.
        """
        S     = self.m.get("scale", 1.0)
        vocab = len(self._vocab_table()) or 1
        tok_best   = self.m["tokens"].get(best) or {}
        tok_second = self.m["tokens"].get(second) or {}
        den_best   = self.m["totals"].get(best, 0) * S + vocab
        den_second = self.m["totals"].get(second, 0) * S + vocab
        weighed = []
        for t, k in counts.items():
            rate_best   = (tok_best.get(t, 0) * S + 1) / den_best
            rate_second = (tok_second.get(t, 0) * S + 1) / den_second
            weight = k * math.log(rate_best / rate_second)
            if weight > 0:
                weighed.append((weight, t))
        weighed.sort(key=lambda w: -w[0])
        reasons = [t.replace("·", " ") for _, t in weighed[:4]]

        if reporter:
            rep_best   = self.m.get("reporters", {}).get(best, {})
            rep_second = self.m.get("reporters", {}).get(second, {})
            seen = rep_best.get(reporter, 0) + rep_second.get(reporter, 0)
            if seen * S >= self._WHO_MIN_SEEN:
                rep_smooth = TUNING["repSmooth"]
                n_best   = sum(rep_best.values()) * S
                n_second = sum(rep_second.values()) * S
                rate_best   = (rep_best.get(reporter, 0) * S + 1) / (n_best + rep_smooth)
                rate_second = (rep_second.get(reporter, 0) * S + 1) / (n_second + rep_smooth)
                if rate_best >= rate_second * self._WHO_MIN_RATIO:
                    reasons.append(f"usually raised by {reporter}")
        return reasons

    def _subset_scores(self, counts: "Counter[str]", req_type: str,
                       reporter: str, candidates: list[str],
                       prior_weight: float) -> dict[str, float]:
        """Score a closed set of outcomes against each other.

        Complement log odds depends on what counts as "the other classes".
        Reusing the global score and merely changing its prior left DATA's
        comparison against TAC/PPS baked into every team score, so the old
        specialist could not alter a single historical decision. Recomputing
        within BI/DATA/DEV makes the second stage genuinely conditional.
        """
        m = self.m
        S = m.get("scale", 1.0)
        total_examples = sum(m["priors"][d] for d in candidates)
        tables = [m["tokens"].get(d) or {} for d in candidates]
        vocab = len({t for table in tables for t in table}) or 1
        tot_all = sum(m["totals"].get(d, 0) for d in candidates) * S
        all_cnt = {t: sum(table.get(t, 0) for table in tables) * S
                   for t in counts}
        req_smooth = TUNING["reqSmooth"]
        rep_smooth = TUNING["repSmooth"]
        rep_w = TUNING.get("reporterWeight", 1.0)
        hint = self.request_type_hint(req_type)
        scores = {}
        for d, table in zip(candidates, tables):
            score = prior_weight * math.log(m["priors"][d] / total_examples)
            total = m["totals"].get(d, 0) * S
            den_in = total + vocab
            den_out = (tot_all - total) + vocab
            for token, repetitions in counts.items():
                inside = table.get(token, 0) * S
                p_in = (inside + 1) / den_in
                p_out = (all_cnt[token] - inside + 1) / den_out
                score += repetitions * math.log(p_in / p_out)
            if req_type:
                request_counts = m["reqTypes"].get(d, {})
                request_total = sum(request_counts.values()) * S
                score += math.log((request_counts.get(req_type, 0) * S + 1)
                                  / (request_total + req_smooth))
                if hint == d:
                    score += TUNING.get("requestTypeHintWeight", 3.0)
            if reporter:
                reporter_counts = m.get("reporters", {}).get(d, {})
                reporter_total = sum(reporter_counts.values()) * S
                score += rep_w * math.log(
                    (reporter_counts.get(reporter, 0) * S + 1)
                    / (reporter_total + rep_smooth))
            scores[d] = score
        return scores

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
                    "margin": 0, "review": True, "source": "cold", "why": [],
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
        global_best = ranked[0]
        mx    = scores[global_best]
        temp  = max(1.0, len(tokens) / TUNING["confTemp"])
        exps  = {d: math.exp((s - mx) / temp) for d, s in scores.items()}
        tot_e = sum(exps.values())
        probs = {d: e / tot_e for d, e in exps.items()}
        source = "model"

        # Once the general model has decided this is a team handoff, compare
        # only the actual handoff teams. This conditional second stage keeps
        # the strong TAC/moved-ticket boundary intact while preventing the
        # high-volume DATA class from overwhelming low-volume BI/DEV merely
        # through its prior. The overall probability mass assigned to
        # "handoff" is preserved; only its split between teams is reranked.
        escalation = [d for d in self._escalation_dests if d in scores]
        if TUNING.get("escalationSpecialist", 1) and \
                global_best in escalation and len(escalation) >= 2:
            handoff_mass = sum(probs[d] for d in escalation)
            esc_prior_w = TUNING.get("escalationPriorWeight", 0.25)
            esc_scores = self._subset_scores(
                counts, req_type, reporter, escalation, esc_prior_w)
            esc_mx = max(esc_scores.values())
            esc_exps = {d: math.exp((s - esc_mx) / temp)
                        for d, s in esc_scores.items()}
            esc_total_e = sum(esc_exps.values())
            for d in escalation:
                probs[d] = handoff_mass * esc_exps[d] / esc_total_e
            source = "escalation"

        # Correct systematic route-frequency imbalance using past observations
        # only. No route name or request type receives a hand-written penalty.
        factors = route_balance_factors(self.m["routeBalance"], TUNING.get("routeBalanceWeight", 0))
        if factors:
            adjusted = {dest: probability * factors.get(dest, 1.0) for dest, probability in probs.items()}
            mass = sum(adjusted.values())
            probs = {dest: probability / mass for dest, probability in adjusted.items()}
        ranked = sorted(probs, key=probs.get, reverse=True)  # type: ignore[arg-type]
        best, second = ranked[0], ranked[1]
        conf  = int(round(100 * probs[best]))
        conf2 = int(round(100 * probs[second]))
        note   = self.plan_for(best)
        hint_dest = hint_for_type
        hint_conf = (int(round(100 * probs.get(hint_dest, 0)))
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

        if source != "memory" and abs(factors.get(best, 1.0) - 1.0) > 1e-6:
            note += "; ranking adjusted using earlier routing outcomes"
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
        safety = self._route_safety.get(best, {})
        if safety.get("forceReview"):
            review = True
            reason = safety.get("reason") or "route confidence needs more evidence"
            note += f"; review because {reason}"
        why = self._why(counts, best, second, reporter) if second else []
        return {"dest": best, "conf": conf, "second": second,
                "secondConf": conf2, "margin": margin, "review": review,
                "source": source, "note": note, "why": why}

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

class ShadowWatcher(bind_engine(Learner, TUNING, features_for, tokenize, BALANCED_TUNED_CONFIG)):
    # Incremental report-metrics cache (see _live_metrics). Declared at class
    # level so the accessors are safe even before __init__ has run.
    _metrics: Optional[dict] = None
    _metrics_offset: int = 0
    _metrics_pending: dict = {}

    def __init__(self) -> None:
        with open(CONFIG_FILE, encoding="utf-8") as f:
            self.cfg = json.load(f)
        if "tokenProtected" in self.cfg:
            self.cfg["token"] = unprotect_secret(
                self.cfg.pop("tokenProtected")) or ""
        self.project = self.cfg.get("projectKey", "")
        # projectKey is interpolated as a bare JQL identifier
        # (`project = {self.project}`) in every query this file builds,
        # not a quoted literal jql_quote() could protect - config is
        # hand-editable, so this is validated the same way a column or
        # table name reaching SQL would need to be, not just checked for
        # presence.
        if not isinstance(self.project, str) \
                or not _SAFE_PROJECT_KEY.match(self.project):
            raise SystemExit(
                "No usable projectKey in jira_config.json — run the "
                "overlay first so it can discover your service desk.")
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
        # Each of these is interpolated the same way self.project is
        # (`project = {target}`), so the same identifier allowlist applies;
        # an invalid entry is dropped rather than treated as fatal, since
        # unlike the intake project this list is never required to be
        # non-empty.
        self.move_targets = [p for p in as_name_list(self.cfg.get("moveTargetProjects"))
                             if _SAFE_PROJECT_KEY.match(p)]
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
        if not isinstance(self.state.get("routeSafety"), dict):
            self.state["routeSafety"] = {}
        self.route_safety = self.state["routeSafety"]
        self.learner = Learner(self.state, plans=self.plans,
                               escalation_dests=self.dests,
                               route_safety=self.route_safety)
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
            # One unusable issue here must not cost the ability to start at
            # all - unlike a bad issue reaching the live cycles below, which
            # only costs that one cycle, this runs once at startup with
            # nothing to fall back on if it raises out uncaught.
            try:
                key = issue.get("key", "")
                # json_objects, not h.get("items", []): a history entry can
                # carry an explicit "items": null rather than omitting the
                # key (the same present-but-null shape Jira sends everywhere
                # else), and .get(key, default) only falls back when the key
                # is missing. _transitions already guards this same data the
                # same way; this inline scan had drifted out of step with it.
                moved_to = [item.get("toString", "")
                            for h in self.jira.changelog(key)
                            for item in json_objects(h.get("items"))
                            if item.get("field") == "status"]
                if moved_to:
                    transitions[key] = moved_to
            except Exception:
                continue
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
        if project_key and _SAFE_PROJECT_KEY.match(project_key) \
                and project_key not in self.move_targets:
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

    def canonical_dest(self, dest: str) -> str:
        """Map labels written by older versions to today's outcome names."""
        for project_key in getattr(self, "move_targets", []):
            legacy = f"Proclaim bug (moved to {project_key})"
            current = self.moved_label(project_key)
            if dest == legacy and current != legacy:
                return current
        return dest

    @property
    def all_dests(self) -> list[str]:
        """Every outcome label the report should account for.

        Deduplicated: dests come from real Jira status names, resolved_label
        and the per-project move labels are synthetic ones a hand-edited
        config can accidentally make collide with a real status name. Report
        tables index by this list, so a collision would otherwise show the
        same outcome twice with the same figures rather than once.
        """
        return list(dict.fromkeys(
            list(self.dests) + [self.resolved_label]
            + [self.moved_label(p) for p in self.move_targets]))

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
        learner = getattr(self, "learner", None)
        if not force:
            mutations = getattr(learner, "_mutations", 0)
            unchanged = mutations == getattr(self, "_saved_mutations", None)
            since = time.time() - getattr(self, "_last_state_save", 0.0)
            if unchanged and since < self._IDLE_SAVE_SECS:
                return
        _atomic_json_write(STATE_FILE, self.state)
        try:
            daily_backup(STATE_FILE, HISTORY_FILE, LOG_FILE)
        except (OSError, ValueError) as exc:
            _log_error(f"Recovery backup failed: {exc}")
        self._saved_mutations = getattr(learner, "_mutations", 0)
        self._last_state_save = time.time()

    def _log(self, record: dict) -> None:
        record["ts"] = f"{datetime.now():%Y-%m-%d %H:%M:%S}"
        line = json.dumps(record, ensure_ascii=False) + "\n"
        try:
            if os.path.getsize(LOG_FILE) > LOG_ROTATE_BYTES:
                # Persist the folded prefix before rotation. Its fingerprint
                # also makes restart safe if replacement fails or power drops.
                metrics = self._live_metrics()
                prefix_size = self._metrics_offset
                with open(LOG_FILE, "rb") as source:
                    digest = hashlib.sha256(source.read(prefix_size)).hexdigest()
                _atomic_json_write(LOG_FILE + ".metrics.json", {
                    "metrics": metrics, "pending": self._metrics_pending,
                    "prefixSize": prefix_size, "prefixHash": digest})
                # One rotating generation: the live log becomes .old (any
                # previous .old is overwritten). _live_metrics notices the
                # shrink on its next read and restarts its offset from zero.
                try:
                    os.replace(LOG_FILE, LOG_FILE + ".old")
                    self._metrics = None
                    self._metrics_offset = 0
                except OSError:
                    pass
        except OSError:
            pass
        with open(LOG_FILE, "a", encoding="utf-8") as f:
            f.write(line)

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
        snapshot = self.state.get("predicted", {}).get(det.get("key"), {}).get("intakeFeatures")
        if isinstance(snapshot, dict):
            rec.update(snapshot)
            rec["featureOrigin"] = "intake"
        else:
            rec["featureOrigin"] = "outcome"
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

    def _learning_details(self, det: dict) -> dict:
        snapshot = self.state.get("predicted", {}).get(det.get("key"), {}).get("intakeFeatures")
        return dict(det, **snapshot) if isinstance(snapshot, dict) else det

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
        cutoff = f"{self._jira_now() - timedelta(days=BACKFILL_DAYS):%Y-%m-%d}"
        jql = (f'project = {self.project} AND status CHANGED TO '
               f'({", ".join(jql_quote(d) for d in self.dests)}) '
               f'AFTER -{BACKFILL_DAYS}d ORDER BY updated DESC')
        # No handoff statuses on this site — every ticket is resolved in place
        # or moved out, so there is nothing for this pass to find.
        issues = (self.jira.search(jql, self._FIELDS, cap=BACKFILL_CAP)
                  if self.dests else [])
        seen: dict[str, None] = dict.fromkeys(
            self.state.setdefault("seenTransitions", []))
        learned = 0
        skipped = 0
        first_routing = "9999"
        for issue in issues:
            if self.stop.is_set():
                self._backfill_checkpoint(seen)
                return
            try:
                det = self._issue_details(issue)
                routed, _ = self._transitions(det["key"])
                det = self._learning_details(det)
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
            except Exception:
                # One unusable issue must not abort a pass that can run to
                # thousands of calls — and losing the whole run would also
                # skip every _backfill_checkpoint below it, discarding
                # progress the checkpoint exists specifically to keep.
                skipped += 1

        # Handoffs may only have started recently — anchor the training window
        # to the first one observed, so resolutions from before the process
        # existed can't teach "everything gets resolved in place".
        if first_routing != "9999":
            cutoff = max(cutoff, first_routing[:10])
            self.state["processStart"] = cutoff

        # Phase 2: tickets resolved inside the intake project, never handed off
        resolved_list = ", ".join(jql_quote(s) for s in sorted(self.resolved))
        jql2 = (f'project = {self.project} AND status CHANGED TO '
                f'({resolved_list}) AFTER -{BACKFILL_DAYS}d '
                f'ORDER BY updated DESC')
        for issue in self.jira.search(jql2, self._FIELDS, cap=BACKFILL_CAP):
            if self.stop.is_set():
                self._backfill_checkpoint(seen)
                return
            try:
                det = self._issue_details(issue)
                routed, resolutions = self._transitions(det["key"])
                det = self._learning_details(det)
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
            except Exception:
                skipped += 1

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
                try:
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
                except Exception:
                    skipped += 1

        if skipped:
            _log_error(f"backfill skipped {skipped} unusable issue(s) — "
                       f"{learned} decision(s) learned from the rest")
        self.state["seenTransitions"] = list(seen)
        self.state["backfillDone"]    = True
        self.state["backfillCount"]   = learned
        self._save_state()
        self.write_report()

    # -- live cycle --------------------------------------------------------------

    def _jira_tz(self):
        """The Jira account's profile timezone, resolved once per process."""
        tz = getattr(self, "_tz_cache", None)
        if tz is None:
            name = ""
            try:
                me = self.jira.get("/rest/api/3/myself")
                if isinstance(me, dict):
                    name = str(me.get("timeZone") or "")
            except Exception:
                name = ""
            try:
                tz = ZoneInfo(name) if name else None
            except Exception:
                tz = None          # zone id unknown on this machine
            # Fall back to this machine's zone: a slightly wrong window is
            # strictly better than crashing, and equals the old behaviour.
            self._tz_cache = tz or datetime.now().astimezone().tzinfo
        return self._tz_cache

    def _jira_now(self) -> datetime:
        """Current wall-clock in the Jira account's timezone — naive, so it
        formats and string-compares exactly like the timestamps Jira's own
        API returns."""
        return datetime.now(self._jira_tz()).replace(tzinfo=None)

    def _window_jql_time(self) -> str:
        now = self._jira_now()
        last = self.state.get("lastPollEpoch", 0)
        if last:
            # lastPollEpoch is absolute epoch seconds, so converting it in
            # the account's zone reproduces the wall-clock the earlier
            # window was written against, whatever zone that was.
            prev = datetime.fromtimestamp(last, self._jira_tz())
            start = max(now - timedelta(days=7),
                        prev.replace(tzinfo=None)
                        - timedelta(seconds=OVERLAP_SECS))
        else:
            start = now - timedelta(hours=2)
        return f'"{start:%Y/%m/%d %H:%M}"'

    def predict_intake(self) -> None:
        """FAST loop: suggest a destination for every ticket currently at
        intake (unresolved, New status category — the same tickets the overlay
        shows in its alert queue). Runs every ~12 s so a suggestion is ready
        before the team routes the ticket out of the queue. Cheap: one search,
        no per-ticket calls, no model mutation."""
        predicted: dict = self.state.setdefault("predicted", {})
        changed = False
        skipped = 0
        intake_keys: set[str] = set()
        # cap=500, not 100: a desk whose intake backlog exceeds one page
        # left tickets beyond it permanently unpredicted — no suggestion
        # until the backlog dropped, which is exactly when routing help
        # matters most. search() paginates, so the larger cap just costs
        # more pages on the rare big-backlog cycle.
        for issue in self.jira.search(
                f'project = {self.project} AND resolution = Unresolved '
                f'AND statusCategory = New', self._FIELDS, cap=500):
            key = issue.get("key", "")
            if key:
                intake_keys.add(key)
            if not key or key in predicted:
                continue
            # This queue can hold customer-submitted requests, so one
            # malformed ticket is a real possibility, not just a hand-edit -
            # and with no per-issue guard, it silently blocked a suggestion
            # for every ticket behind it in the list, every ~12s, for as
            # long as it sat in intake.
            try:
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
                                  "source": p["source"], "why": p.get("why", []),
                                  "requestType": req_type,
                                  "intakeFeatures": {k: det.get(k) for k in
                                      ("summary", "desc", "labels", "components", "structured", "reporter", "requestType")},
                                  "summary": det["summary"][:120],
                                  "at": f"{self._jira_now():%Y-%m-%d %H:%M}"}
                changed = True
                self._log({"event": "would_have_done", "key": key,
                           "summary": det["summary"],
                           "prediction": p["dest"], "confidence": p["conf"],
                           "second": p["second"], "review": p["review"],
                           "source": p["source"], "requestType": req_type,
                           "plan": p["note"]})
            except Exception:
                skipped += 1
        if skipped:
            _log_error(f"predict_intake skipped {skipped} unusable "
                       "issue(s) this cycle")
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
        poll_started = time.time()
        reads_before = getattr(self.jira, "incomplete_reads", 0)
        predicted: dict = self.state.setdefault("predicted", {})

        # 2. Actual routings — resolve predictions and learn
        seen: dict[str, None] = dict.fromkeys(
            self.state.setdefault("seenTransitions", []))
        stats = self.state.setdefault("stats", {"predictions": 0, "correct": 0})
        # A project where everything is resolved in place has no handoff
        # statuses; "status CHANGED TO ()" is not valid JQL, so skip the pass
        # rather than issuing a query that fails every cycle.
        dest_list = ", ".join(jql_quote(d) for d in self.dests)
        skipped = 0
        # cap=500, not 50: `since` spans everything since the last poll plus
        # overlap — which after a weekend asleep or a long backoff is days of
        # transitions. A 50-issue cap silently dropped everything past the
        # first page, and because lastPollEpoch advances regardless, those
        # decisions were lost permanently: never scored, never learned.
        for issue in (self.jira.search(
                f'project = {self.project} AND status CHANGED TO ({dest_list}) '
                f'AFTER {since}', self._FIELDS, cap=None) if self.dests else []):
            # One bad issue must not stall learning for every ticket in this
            # cycle, or worse — since seenTransitions only saves once, at the
            # end, an uncaught exception here re-fetches and re-fails on the
            # same issue every poll until it ages out of the JQL window.
            try:
                det = self._issue_details(issue)
                routed, _ = self._transitions(det["key"])
                det = self._learning_details(det)
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
            except Exception:
                skipped += 1

        # 3. Tickets resolved inside the intake project, never handed off
        resolved_list = ", ".join(jql_quote(s) for s in sorted(self.resolved))
        for issue in self.jira.search(
                f'project = {self.project} AND status CHANGED TO '
                f'({resolved_list}) AFTER {since}', self._FIELDS, cap=None):
            try:
                det = self._issue_details(issue)
                routed, resolutions = self._transitions(det["key"])
                det = self._learning_details(det)
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
            except Exception:
                skipped += 1

        # 4. Tickets moved out to another project. Rather than scanning
        #    configured target projects, look up the ones we predicted on that
        #    have since vanished from intake: Jira keeps the old key as an
        #    alias, so it resolves to wherever the ticket went. This needs no
        #    configuration and finds move targets we've never seen before.
        skipped += self._learn_moved_out(predicted, seen, stats)
        if skipped:
            _log_error(f"resolve/learn skipped {skipped} unusable issue(s) "
                       "this cycle")

        # housekeeping
        self.state["seenTransitions"] = list(seen)[-SEEN_CAP:]
        cutoff = f"{self._jira_now() - timedelta(days=30):%Y-%m-%d}"
        self.state["predicted"] = {k: v for k, v in predicted.items()
                                   if v.get("at", "9999") >= cutoff}
        complete = getattr(self.jira, "incomplete_reads", 0) == reads_before
        self.state["pollIncomplete"] = not complete
        if complete:
            self.state["lastPollEpoch"] = poll_started
            self.state["lastSuccessfulPoll"] = poll_started
        self._save_state(force=False)
        self._write_suggestions(self.state["predicted"])
        self.write_report()

    def _learn_moved_out(self, predicted: dict, seen: dict,
                         stats: dict) -> int:
        """Catch tickets that left the intake project by being MOVED.

        A move changes the issue key, so such a ticket simply stops appearing
        in intake searches — no status transition marks it. We therefore
        re-fetch keys we predicted on but haven't yet scored; Jira resolves an
        old key to the moved issue, revealing the destination project.

        Returns the number of keys skipped as unusable.
        """
        # Tickets still sitting in intake haven't gone anywhere yet, so there
        # is nothing to look up for them.
        still_in_intake = getattr(self, "_intake_keys", set())
        unscored = [k for k, p in predicted.items()
                    if p.get("dest") is not None and not p.get("scored")
                    and k not in still_in_intake]
        # Oldest first, and bounded so a backlog can't stall the cycle.
        unscored.sort(key=lambda k: predicted[k].get("at", ""))
        skipped = 0
        for old_key in unscored[:MOVE_LOOKUPS_PER_CYCLE]:
            if self.stop.is_set():
                return skipped
            try:
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
                det = self._learning_details(dict(det, key=old_key))
                req_type = det.get("requestType", "")
                toks = self._features_for(det, req_type)
                when = (moves[0]["when"] if moves
                        else f"{self._jira_now():%Y-%m-%d %H:%M}")
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
            except Exception:
                skipped += 1
        return skipped

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
                        row = json.loads(line)
                        if isinstance(row, dict):
                            recs.append(row)
                        else:
                            bad += 1
                    except json.JSONDecodeError:
                        bad += 1
        except FileNotFoundError:
            return None
        if bad:
            _log_error(f"history: skipped {bad} unparseable line(s)")
        recs.sort(key=lambda r: r.get("when", ""))
        return recs


    def _rebuild_model(self, recs: list[dict]) -> None:
        """Re-fit a clean model over the whole history with the current TUNING
        (called after adopting a freshly-tuned config)."""
        # Pass the plans through: without them the nightly re-fit silently
        # replaced the learner with one that had no outcome descriptions, so
        # the report and suggestions lost their wording until the next restart.
        lr = Learner({}, plans=self.plans,
                     escalation_dests=getattr(self, "dests", []),
                     route_safety=getattr(self, "route_safety", {}))
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
                example = dict(det, dest=dest, requestType=req_type,
                               cloneProjects=as_name_list(r.get("cloneProjects")))
                self._fit_example(lr, example)
            except Exception:                       # noqa: BLE001
                skipped += 1
        if skipped:
            _log_error(f"model rebuild skipped {skipped} unusable "
                       f"record(s) of {len(recs)}")
        self.state["model"] = lr.m
        self.learner = lr

    def _derive_route_safety(self, recs: list[dict], validation: dict) -> dict:
        """Decide which outcomes have earned a confident live label.

        This deliberately uses precision by *predicted* route. A class can
        have perfect recall yet still be unsafe if most tickets it claims are
        false positives — exactly what the old Proclaim headline concealed.
        """
        dated = []
        for r in recs:
            try:
                when = datetime.fromisoformat(str(r.get("when", ""))[:16])
            except ValueError:
                continue
            dated.append((when, r.get("dest")))
        latest = max((when for when, _ in dated), default=datetime.now())
        cutoff = latest - timedelta(days=ROUTE_ACTIVITY_DAYS)
        recent = Counter(dest for when, dest in dated
                         if when >= cutoff and dest)
        firm_counts = validation.get("firmPrecisionCounts", {})
        policy = {}
        for dest in self.all_dests:
            reasons = []
            actual_n = recent.get(dest, 0)
            pred_n, right = firm_counts.get(dest, [0, 0])
            precision = right / pred_n if pred_n else None
            if actual_n < MIN_ROUTE_RECENT_ACTUAL:
                reasons.append(
                    f"{dest} is dormant ({actual_n} actual decisions in the "
                    f"last {ROUTE_ACTIVITY_DAYS} days)")
            if pred_n < MIN_ROUTE_FIRM_PREDICTIONS:
                reasons.append(
                    f"only {pred_n} confident validation predictions exist")
            elif precision is not None and precision < MIN_ROUTE_FIRM_PRECISION:
                reasons.append(
                    f"recent confident precision is {precision:.0%} "
                    f"({right}/{pred_n})")
            lower = wilson_lower(right, pred_n)
            # Precision still needs to reach 95%; additionally require a
            # conservative lower bound of 90% before removing human review.
            if pred_n >= MIN_ROUTE_FIRM_PREDICTIONS and lower < 0.90:
                reasons.append(f"precision lower bound is {lower:.0%}; more evidence needed")
            policy[dest] = {
                "forceReview": bool(reasons),
                "reason": "; ".join(reasons),
                "recentActual": actual_n,
                "firmPredictions": pred_n,
                "firmCorrect": right,
                "firmPrecision": round(precision, 4)
                if precision is not None else None,
                "precisionLowerBound": round(lower, 4),
            }
        return policy

    @staticmethod
    def _tune_drift_warning(today: dict, prior: Optional[dict]) -> str:
        """Flag a genuine swing in tuning outcome from one night to the
        next — never routine small-sample noise (see MIN_DRIFT_SAMPLE).

        Balanced accuracy and confident-band coverage are stable enough at
        this desk's scale to compare directly; a per-class rate is only
        compared when today's holdout bucket for that class is large enough
        that a swing means something. Returns "" when there's nothing
        worth a human's attention.
        """
        if not prior:
            return ""
        lines = []
        for label, key, threshold in (
                ("balanced accuracy",         "balanced", 0.05),
                ("macro F1",                  "macroF1", 0.05),
                ("macro precision",           "macroPrecision", 0.05),
                ("confident-band coverage",   "firmCov",  0.10)):
            now_v  = today.get(key, 0.0)
            then_v = prior.get(key, now_v)
            delta  = now_v - then_v
            if abs(delta) >= threshold:
                verb = "dropped" if delta < 0 else "rose"
                lines.append(f"{label} {verb} {abs(delta):.0%} "
                             f"({then_v:.0%} -> {now_v:.0%})")
        counts = today.get("perCounts", {})
        prior_per = prior.get("per", {})
        for dest, rate in today.get("per", {}).items():
            n = counts.get(dest, [0])[0]
            if n < MIN_DRIFT_SAMPLE or dest not in prior_per:
                continue
            delta = rate - prior_per[dest]
            if abs(delta) >= 0.15:
                verb = "dropped" if delta < 0 else "rose"
                lines.append(f"{dest} accuracy {verb} {abs(delta):.0%} "
                             f"({prior_per[dest]:.0%} -> {rate:.0%}, n={n})")
        return "; ".join(lines)

    def maybe_daily_tune(self) -> None:
        """Once a day, replay the accumulated history under candidate configs,
        adopt the best, and re-fit the model — so the learner measurably sharpens
        itself as more real decisions accrue. Fully self-contained; never writes
        to Jira.

        Any failure is logged and left to the next day — meant literally: the
        date is stamped before the risky work starts, not after it succeeds,
        so a bug in the search itself retries once tomorrow instead of every
        poll cycle for the rest of today. A multi-minute search failing and
        immediately retrying every ~60 seconds is a worse outcome than the
        model simply not sharpening today.
        """
        today = date.today().isoformat()
        if self.state.get("lastTuneDate") == today:
            return
        recs = self._load_history()
        if recs is None:
            return
        self.state["lastTuneDate"] = today
        if len(recs) < 300:                 # too little to tune reliably yet
            self._save_state()
            return
        try:
            # Search over a bounded window, but fit the final model on
            # everything. The search replays the history once per candidate
            # config, so its cost is (history x budget): fine at a few
            # thousand decisions, roughly an hour of CPU a night at twenty-
            # five thousand — on a machine somebody is trying to work on.
            # Recency decay already means the oldest decisions barely move
            # the result, so the tail they would contribute isn't worth the
            # time it costs a busy desk.
            search_recs = recs[-TUNE_WINDOW:]
            if len(search_recs) < len(recs):
                _log_error(f"self-tune searching the most recent "
                           f"{len(search_recs)} of {len(recs)} decisions")
            best = self._self_tune(search_recs)
            if best:
                TUNING.update(best["cfg"])
                self.state["tunedConfig"] = best["cfg"]
                safety = self._derive_route_safety(recs, best)
                self.route_safety.clear()
                self.route_safety.update(safety)
                self.state["routeSafety"] = self.route_safety
                self._rebuild_model(recs)
                hist = self.state.setdefault("tuneHistory", [])
                prior = hist[-1] if hist else None
                drift = self._tune_drift_warning(best, prior)
                self.state["tuneDrift"] = (
                    {"date": today, "detail": drift} if drift else None)
                if drift:
                    _log_error(f"self-tune drift {today}: {drift}")
                hist.append({"date": today, "acc": round(best["acc"], 4),
                             "balanced": round(best["balanced"], 4),
                             "worst": round(best["worst"], 4),
                             "firmAcc": round(best["firmAcc"], 4),
                             "firmBalanced": round(best["firmBalanced"], 4),
                             "firmWorst": round(best["firmWorst"], 4),
                             "firmCov": round(best["firmCov"], 4),
                             "n": best["n"], "trainN": best["trainN"],
                             "holdoutN": best["holdoutN"],
                             "gateN": best["gateN"],
                             "finalHoldoutN": best["finalHoldoutN"],
                             "validationWindows": best["validationWindows"],
                             "challengerRejected": best["challengerRejected"],
                             "gateReason": best.get("gateReason"),
                             "incumbentGateScore": best.get("incumbentGateScore"),
                             "challengerGateScore": best.get("challengerGateScore"),
                             "segments": best.get("segments", {}),
                             "featureOrigins": best.get("featureOrigins", {}),
                             "finalAcc": round(
                                 best["finalHoldout"]["acc"], 4),
                             "finalFirmAcc": round(
                                 best["finalHoldout"]["firmAcc"], 4),
                             "finalBalanced": round(
                                 best["finalHoldout"]["balanced"], 4),
                             "macroF1": round(best["macroF1"], 4),
                             "macroPrecision": round(
                                 best["macroPrecision"], 4),
                             "per": {k: round(v, 4)
                                     for k, v in best["per"].items()},
                             "precision": {k: round(v, 4)
                                           for k, v in best["precision"].items()},
                             "firmPer": {k: round(v, 4)
                                         for k, v in best["firmPer"].items()},
                             "firmPrecision": {k: round(v, 4) for k, v in
                                               best["firmPrecision"].items()},
                             "cfg": best["cfg"]})
                self.state["tuneHistory"] = hist[-60:]
                _log_error(
                    f"daily self-tune {today}: holdoutFirm={best['firmAcc']:.1%} "
                    f"balanced={best['balanced']:.1%} "
                    f"macroF1={best['macroF1']:.1%} "
                    f"worst={best['worst']:.1%} "
                    f"holdoutAcc={best['acc']:.1%} n={best['holdoutN']} "
                    f"finalAcc={best['finalHoldout']['acc']:.1%} "
                    f"challengerRejected={best['challengerRejected']} "
                    f"cfg={best['cfg']}")
        except Exception:                                    # noqa: BLE001
            _log_error(f"daily self-tune {today} failed:\n"
                       + traceback.format_exc())
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
        ask for a human check instead of a confidently-wrong single answer.
        A confident call names the words that drove it, so it's something
        to check rather than just something to trust."""
        best = self._dest_phrase(p["dest"])
        if p.get("review") and p.get("second"):
            second = self._dest_phrase(p["second"])
            return (f"review — likely {best} ({p['conf']}%), "
                    f"or {second} ({p.get('secondConf', 0)}%)")
        label = f"{best} · {p['conf']}%"
        why = p.get("why")
        if why:
            label += " — because: " + ", ".join(why)
        return label

    def _write_suggestions(self, predicted: dict) -> None:
        """Publish current suggestions for the overlay to display.
        Suggestions only — nothing here ever touches Jira."""
        cutoff = f"{self._jira_now() - timedelta(days=7):%Y-%m-%d}"
        out = {}
        for key, p in predicted.items():
            if p.get("dest") and p.get("at", "") >= cutoff:
                # A daily tune can tighten a route's safety policy after this
                # prediction was first made. Apply that policy while publishing
                # too, so tickets already visible in the overlay become review
                # suggestions immediately. Keep the stored prediction intact:
                # its original flag is part of the honest scoring record.
                shown = p
                safety = getattr(self, "route_safety", {}).get(p["dest"], {})
                if safety.get("forceReview") and not p.get("review"):
                    shown = dict(p)
                    shown["review"] = True
                out[key] = {"label": self._short_label(shown),
                            "review": bool(shown.get("review")),
                            "plan": p.get("plan", ""), "at": p["at"]}
        _atomic_json_write(SUGGEST_FILE, out, fallback_in_place=True)

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
            if not correct:
                cases = self.state.setdefault("reviewCases", [])
                cases.append({"key": det["key"], "summary": det["summary"],
                              "predicted": p_dest, "actual": dest,
                              "confidence": pred.get("conf"), "when": tr["when"]})
                self.state["reviewCases"] = cases[-200:]
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
                         "byActual": {}, "byPredicted": {},
                         "confidentByActual": {},
                         "confidentByPredicted": {},
                         "recentEvents": [], "coverageReasons": {}, "routedKeys": {},
                         "bySource": {}, "withRequestType": 0}
        self._metrics_offset = 0
        self._metrics_pending: dict[str, dict] = {}

        try:
            with open(LOG_FILE + ".metrics.json", encoding="utf-8") as f:
                saved = json.load(f)
            self._metrics = saved["metrics"]
            self._metrics_pending = saved.get("pending", {})
            size = saved["prefixSize"]
            with open(LOG_FILE, "rb") as f:
                prefix = f.read(size)
            if len(prefix) == size and hashlib.sha256(prefix).hexdigest() == saved["prefixHash"]:
                self._metrics_offset = size
        except (OSError, ValueError, KeyError, TypeError):
            pass

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
        cutoff = f"{datetime.now() - timedelta(days=30):%Y-%m-%d %H:%M:%S}"
        recent = result.setdefault("recentEvents", [])
        recent[:] = [r for r in recent if r.get("ts", "") >= cutoff]
        routed_keys = result.setdefault("routedKeys", {})
        reasons = result.setdefault("coverageReasons", {})
        try:
            size = os.path.getsize(LOG_FILE)
        except OSError:
            return result
        if size < self._metrics_offset:      # truncated or replaced — restart
            self._reset_metrics()
            result, pending = self._metrics, self._metrics_pending
            recent = result.setdefault("recentEvents", [])
            recent[:] = [r for r in recent if r.get("ts", "") >= cutoff]
            routed_keys = result.setdefault("routedKeys", {})
            reasons = result.setdefault("coverageReasons", {})
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
            if not isinstance(rec, dict):
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
            context = rec if rec.get("review") is not None else pending.get(key, {})
            reason = coverage_reason(rec, pending.get(key), key in routed_keys)
            reasons[reason] = reasons.get(reason, 0) + 1
            stamp = str(rec.get("ts") or "")
            routed_keys[key] = stamp
            if len(routed_keys) > 8000:
                del routed_keys[next(iter(routed_keys))]
            if stamp >= cutoff:
                compact = {k: rec.get(k) for k in
                           ("ts", "predicted", "dest", "predictionCorrect", "requestType")}
                compact["coverageReason"] = reason
                compact["ts"] = stamp
                compact["review"] = context.get("review")
                compact["dest"] = self.canonical_dest(compact.get("dest") or "unknown")
                if compact["predicted"] is not None:
                    compact["predicted"] = self.canonical_dest(compact["predicted"])
                recent.append(compact)
            if rec.get("requestType"):
                result["withRequestType"] += 1
            if rec.get("predicted") is None:
                continue
            result["predicted"] += 1
            hit = int(bool(rec.get("predictionCorrect")))
            result["correct"] += hit
            seg = result.setdefault("segments", {}).setdefault(
                segment_name(rec.get("requestType")), [0, 0])
            seg[0] += 1; seg[1] += hit
            actual = self.canonical_dest(rec.get("dest", "unknown"))
            predicted = self.canonical_dest(rec.get("predicted", "unknown"))
            row = result["byActual"].setdefault(actual, [0, 0])
            row[0] += 1; row[1] += hit
            prow = result["byPredicted"].setdefault(predicted, [0, 0])
            prow[0] += 1; prow[1] += hit
            band = "unknown" if context.get("review") is None else "review" if context["review"] else "confident"
            result.setdefault("unknown", [0, 0])
            result[band][0] += 1; result[band][1] += hit
            if band == "confident":
                crow = result["confidentByActual"].setdefault(actual, [0, 0])
                crow[0] += 1; crow[1] += hit
                cprow = result["confidentByPredicted"].setdefault(
                    predicted, [0, 0])
                cprow[0] += 1; cprow[1] += hit
            source = context.get("source")
            if source:
                srow = result["bySource"].setdefault(source, [0, 0])
                srow[0] += 1; srow[1] += hit
        return result

    def write_report(self) -> None:
        m     = self.learner.m
        stats = self.state.get("stats", {"predictions": 0, "correct": 0})
        # Named distinctly from any loop variable in this function on
        # purpose: a `for pair, n in wrong[:10]` loop further down silently
        # overwrote a plain `n`/`c` here — Python for-loops don't scope —
        # and the status JSON at the end of this method ended up reporting
        # a confusion-pair count as the prediction total.
        pred_n, pred_c = stats["predictions"], stats["correct"]
        acc = (f"{100 * pred_c // pred_n}% ({pred_c}/{pred_n})"
               if pred_n else "no resolved predictions yet")

        seen_counts = m.get("seen", {})
        last_poll = self.state.get("lastSuccessfulPoll")
        lines = [
            "# Triage Shadow Learner — report",
            "",
            f"_Updated {datetime.now():%Y-%m-%d %H:%M:%S} · v{APP_VERSION} · "
            "shadow mode — no tickets are ever actioned._",
            "",
        ]
        # This file is rewritten every cycle regardless of whether Jira
        # actually answered, so "Updated just now" alone can't be trusted
        # to mean "still getting real data" — a broken token would look
        # identical to a healthy quiet desk otherwise. _note_refusal
        # already tracks this per process run; it just never surfaced here.
        refusals = getattr(getattr(self, "jira", None), "_refusals", set())
        if refusals:
            codes = ", ".join(str(c) for c in sorted(refusals))
            lines += [f"## ⚠ Jira has been refusing requests ({codes}) "
                     "on recent requests",
                     "",
                     "Some reads failed. Check access if the warning persists; "
                     "a successful search clears the warning automatically.",
                     ""]
        lines += [
            "## Model provenance",
            "",
            "- **Last completed poll:** " + (
                datetime.fromtimestamp(last_poll).isoformat(timespec="seconds")
                if isinstance(last_poll, (int, float)) else "not recorded yet"),
            "- **Polling:** " + ("incomplete; retrying saved checkpoint"
                                  if self.state.get("pollIncomplete") else "no incomplete scan recorded"),
            f"- **Trained on:** {sum(seen_counts.values())} labelled decisions"
            + (f" since {self.state['processStart']}"
               if self.state.get("processStart") else ""),
            "- **Backfill:** "
            + (f"complete ({self.state.get('backfillCount', 0)} decisions)"
               if self.state.get("backfillDone")
               else "not yet complete — figures below will fill in as it runs"),
            f"- **Last self-tune:** {self.state.get('lastTuneDate') or 'never yet'}"
            f" · tuning schema v{self.state.get('tuningSchemaVersion', 0)}",
            f"- **Outcomes tracked:** {len(self.all_dests)} "
            f"({', '.join(self.all_dests)})",
            "",
            f"## Learned triage map for `{self.project}`",
            "",
            "| Outcome | What the team does | Observed examples |",
            "|---|---|---|",
        ]
        for dest in self.all_dests:
            lines.append(f"| {dest} | {self.learner.plan_for(dest)} | "
                         f"{seen_counts.get(dest, 0)} |")

        live = self._live_metrics()
        recent_windows = {str(days): rolling_metrics(live.get("recentEvents", []), days=days)
                          for days in (7, 30)}
        trend = performance_drift(live.get("recentEvents", []))
        lines += ["", "## Recent live performance", "",
                  "Windows use when a routing was observed; counts are events, not unique tickets.", ""]
        for days, recent in recent_windows.items():
            total, right = recent["predicted"], recent["correct"]
            accuracy = f"{right / total:.1%} ({right}/{total})" if total else "no scored events"
            lines.append(f"- **Last {days} days:** {accuracy}; intake prediction coverage "
                         + (f"{recent['coveragePct']:.1f}% ({total}/{recent['routed']})"
                            if recent["routed"] else "no routing events"))
            for segment, (n, ok) in recent["segments"].items():
                lines.append(f"  - {segment}: {ok}/{n} ({ok / n:.1%})")
        recent = recent_windows["30"]
        lines += ["", "### Gradual performance drift", "",
                  "Compares the last 7 days with the preceding, non-overlapping 21 days. "
                  "Warnings require a material drop and sufficient sample evidence; they are prompts to investigate."]
        old, new = trend["baseline"], trend["recent"]
        if old["predictions"] and new["predictions"]:
            lines.append(f"- Overall: {old['pct']}% ({old['correct']}/{old['predictions']}) "
                         f"baseline versus {new['pct']}% ({new['correct']}/{new['predictions']}) recently.")
        if trend["warnings"]:
            for warning in trend["warnings"]:
                lines.append(f"- Warning — {warning['kind']}: {warning['label']} dropped "
                             f"{warning['dropPoints']:.1f} percentage points.")
        else:
            lines.append("- No deterioration met the warning criteria; small samples and smaller changes can still warrant review.")
        if recent["byActual"] or recent["byPredicted"]:
            lines += ["", "### Last 30 days by route", "",
                      "| Route | Recall | Precision |", "|---|---:|---:|"]
            for dest in self.all_dests:
                actual_n, actual_ok = recent["byActual"].get(dest, [0, 0])
                suggested_n, suggested_ok = recent["byPredicted"].get(dest, [0, 0])
                recall = f"{actual_ok}/{actual_n} ({actual_ok / actual_n:.1%})" if actual_n else "—"
                precision = f"{suggested_ok}/{suggested_n} ({suggested_ok / suggested_n:.1%})" if suggested_n else "—"
                lines.append(f"| {dest} | {recall} | {precision} |")
        missing_reasons = {k: n for k, n in recent["coverageReasons"].items() if k != "predicted"}
        if missing_reasons:
            lines += ["", "### Unpredicted routing events (last 30 days)", "",
                      "Categories describe available log evidence; no intake observation does not establish a polling failure."]
            for reason, n in sorted(missing_reasons.items()):
                lines.append(f"- {reason.replace('_', ' ')}: {n}")
            missing_days = [(day, n - scored, n) for day, (n, scored) in recent["dailyCoverage"].items() if n > scored]
            for day, missing, total in sorted(missing_days, key=lambda r: -r[1])[:3]:
                lines.append(f"- {day}: {missing}/{total} routing events had no prediction")
        lines += ["", f"## Prediction accuracy: {acc}"]
        if live["routed"]:
            coverage = 100 * live["predicted"] // live["routed"]
            lines.append(f"- **Prediction coverage:** {coverage}% "
                         f"({live['predicted']}/{live['routed']} routed tickets "
                         "received an intake suggestion)")
        for key, label in (("confident", "Confident suggestions"),
                           ("review", "Review-flagged suggestions"),
                           ("unknown", "Suggestions with unknown historical review status")):
            total, right = live.get(key, [0, 0])
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
        if live["byActual"] or live["byPredicted"]:
            lines += ["", "### Live accuracy by route", "",
                      "| Route | Recall when actually chosen | Precision when suggested | Confident precision |",
                      "|---|---:|---:|---:|"]
            for dest in self.all_dests:
                actual_n, actual_ok = live["byActual"].get(dest, [0, 0])
                route_pred_n, pred_ok = live["byPredicted"].get(dest, [0, 0])
                firm_n, firm_ok = live["confidentByPredicted"].get(
                    dest, [0, 0])
                recall = (f"{actual_ok}/{actual_n} ({100 * actual_ok // actual_n}%)"
                          if actual_n else "—")
                precision = (f"{pred_ok}/{route_pred_n} ({100 * pred_ok // route_pred_n}%)"
                             if route_pred_n else "—")
                firm_precision = (f"{firm_ok}/{firm_n} ({100 * firm_ok // firm_n}%)"
                                  if firm_n else "—")
                lines.append(f"| {dest} | {recall} | {precision} | "
                             f"{firm_precision} |")

        route_safety = getattr(self, "route_safety", {})
        if live.get("segments"):
            lines += ["", "### Accuracy by observed request type", ""]
            for name, (total, right) in live["segments"].items():
                lines.append(f"- {name}: {right}/{total} ({right / total:.1%})")
        lines += ["", "### Mistake review", "",
                  "Run `python triage_review.py` to view recurring confusions and annotate cases.",
                  "Use `python triage_review.py --export TRIAGE_REVIEW_QUEUE.md` for a prioritised, representative local review queue.",
                  "New training examples preserve intake features; older examples use outcome-time features."]
        guarded = [(dest, policy) for dest, policy in route_safety.items()
                   if policy.get("forceReview")]
        if guarded:
            lines += ["", "### Routes currently forced to human review", ""]
            for dest, policy in guarded:
                lines.append(f"- **{dest}:** {policy.get('reason', 'needs more evidence')}")

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
            drift = self.state.get("tuneDrift")
            if drift and drift.get("date") == (th[-1]["date"] if th else None):
                lines.append(f"- ⚠ **Drift since last night's tune "
                             f"({drift['date']}):** {drift['detail']}")
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
                if t.get("macroF1") is not None:
                    parts.append(f"macro F1 {t['macroF1']*100:.0f}%")
                if t.get("holdoutN"):
                    windows = t.get("validationWindows", 1)
                    parts.append(f"overall {t['acc']*100:.0f}% "
                                 f"({t['holdoutN']} unseen decisions across "
                                 f"{windows} recent window{'s' if windows != 1 else ''}; "
                                 f"selected on {t['trainN']})")
                if t.get("finalHoldoutN"):
                    parts.append(f"latest untouched window "
                                 f"{t.get('finalAcc', 0)*100:.0f}% "
                                 f"({t['finalHoldoutN']} decisions)")
                if t.get("challengerRejected"):
                    parts.append("challenger rejected; incumbent retained")
                else:
                    parts.append(f"overall {t['acc']*100:.0f}% "
                                 f"(replayed {t['n']})")
                lines.append(" · ".join(parts))

        confusion = self.state.get("confusion", {})
        if confusion:
            per: dict[str, list[int]] = {}
            for pair, count in confusion.items():
                p_dest, actual = (self.canonical_dest(x)
                                  for x in pair.split(" → "))
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
            normalised_wrong: dict[str, int] = {}
            for pair, count in confusion.items():
                p_dest, actual = (self.canonical_dest(x)
                                  for x in pair.split(" → "))
                if p_dest != actual:
                    normal = f"{p_dest} → {actual}"
                    normalised_wrong[normal] = (
                        normalised_wrong.get(normal, 0) + count)
            wrong = sorted(normalised_wrong.items(), key=lambda kv: -kv[1])
            if wrong:
                lines += ["", "## Mistakes learned from (said → team chose)",
                          ""]
                for pair, n in wrong[:10]:
                    lines.append(f"- {pair} — {n}×")

        lines += ["", "## Signals per destination", ""]
        for dest in self.all_dests:
            kws = ", ".join(self.learner.top_keywords(dest)) or "—"
            rts = m["reqTypes"].get(dest, {})
            # Counts have been through recency decay, so they're fractional
            # (261.4948...) - real, but meaningless precision to show a
            # reader who just wants "how often, roughly".
            top_rt = ", ".join(f"{k} ({v:.0f})" for k, v in
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
                    actual = self.canonical_dest(rec.get("dest", "unknown"))
                    lines.append(
                        f"- `{rec.get('key', '')}` **{actual}** by "
                        f"{rec.get('by', '?')}"
                        f" (clones: {', '.join(rec.get('clones', []) or []) or 'none'})"
                        f"{pred_txt}  \n  {rec.get('summary', '')[:90]}")
                    shown += 1
                elif ev == "would_have_done" and rec.get("prediction"):
                    # .get throughout: a record from an older schema (or a
                    # hand-truncated line that still parses) can be missing
                    # any of these; a KeyError here escaped the per-line
                    # guard below and killed the whole report — and because
                    # the log is append-only, the bad line stayed in the
                    # tail forever, stopping every future report too.
                    lines.append(
                        f"- `{rec.get('key', '')}` would have done: "
                        f"**{rec.get('plan', '')}**"
                        f" ({rec.get('confidence', 0)}% confident)  \n"
                        f"  {rec.get('summary', '')[:90]}")
                    shown += 1
        except FileNotFoundError:
            lines.append("_No activity logged yet._")

        with open(REPORT_FILE, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")

        # Machine-readable twin of the same health signal, for anything
        # that wants to alert on this without parsing markdown.
        status = {
            "updatedAt": datetime.now().isoformat(timespec="seconds"),
            "version": APP_VERSION,
            "healthy": not refusals and not self.state.get("pollIncomplete", False),
            "healthMessage": ("Polling is incomplete; retrying from the last checkpoint"
                              if self.state.get("pollIncomplete") else
                              "Jira is refusing requests" if refusals else "Healthy"),
            "lastSuccessfulPoll": self.state.get("lastSuccessfulPoll"),
            "refusals": sorted(refusals),
            "provenance": {
                "trainedOnDecisions": sum(seen_counts.values()),
                "trainingSince": self.state.get("processStart"),
                "backfillComplete": bool(self.state.get("backfillDone")),
                "backfillCount": self.state.get("backfillCount", 0),
                "lastTuneDate": self.state.get("lastTuneDate"),
                "tuningSchemaVersion": self.state.get("tuningSchemaVersion", 0),
                "outcomes": self.all_dests,
            },
            "accuracy": accuracy_counts(pred_n, pred_c),
            "routeSafety": route_safety,
            "segments": live.get("segments", {}),
            "recentPerformance": recent_windows,
            "performanceDrift": trend,
            "drift": ({"date": date.today().isoformat(), "detail": "; ".join(
                f"{w['label']} {w['kind']} dropped {w['dropPoints']:.1f} points"
                for w in trend["warnings"][:5])} if trend["warnings"] else self.state.get("tuneDrift")),
        }
        try:
            _atomic_json_write(STATUS_FILE, status, fallback_in_place=True)
        except OSError as exc:
            _log_error(f"could not write {STATUS_FILE}: {exc}")

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
        try:
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
                        _log_error("resolve/learn failed:\n"
                                   + traceback.format_exc())
                    # once a day, self-tune on the accumulated history
                    try:
                        self.maybe_daily_tune()
                    except Exception:
                        _log_error("daily tune failed:\n" + traceback.format_exc())
                self.stop.wait(FAST_POLL_SECONDS)
        finally:
            try:
                self.jira.session.close()   # release keep-alive connections
            except Exception:
                pass


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
    if updates.recover_pending(_DIR):
        os.execv(sys.executable, [sys.executable, os.path.abspath(__file__)])
    # single-instance guard. restype/argtypes set explicitly, as the overlay's
    # acquire_single_instance does: without them ctypes truncates the handle
    # to c_int (harmless here, but wrong) and GetLastError is read through an
    # undeclared call that other intervening ctypes bookkeeping can reset.
    k32 = ctypes.windll.kernel32
    k32.CreateMutexW.restype  = ctypes.c_void_p
    k32.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_bool,
                                 ctypes.c_wchar_p]
    handle = k32.CreateMutexW(None, False, "JiraTriageLearner")
    if k32.GetLastError() == 183:        # ERROR_ALREADY_EXISTS
        if handle:
            k32.CloseHandle(ctypes.c_void_p(handle))
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
    updates.listen_for_update(_DIR, lambda: _quit(icon))
    icon.run()
    # Do not exit while a daemon writer is still learning or saving. The
    # updater waits for this process to end before replacing any code.
    watcher.stop.set()
    worker.join()
    watcher._save_state()


if __name__ == "__main__":
    main()
