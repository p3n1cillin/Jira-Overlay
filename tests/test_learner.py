"""The triage learner must work on anyone's service desk.

The headline case builds a fictional Jira with nothing in common with the one
this was written against - different project key, different statuses, custom
field ids in a different range - and checks it configures itself from that
alone.
"""

import json
import os
import sys
import threading
from datetime import datetime, timedelta

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_learner          # noqa: E402

tl = load_learner()
check = Checker("triage learner")
FIXTURE_CREATED = f"{datetime.now() - timedelta(days=1):%Y-%m-%dT10:00:00.000+0000}"


# --- A completely foreign service desk -------------------------------------

STATUSES = [{"statuses": [
    {"name": "Triage",             "statusCategory": {"key": "new"}},
    {"name": "Investigating",      "statusCategory": {"key": "indeterminate"}},
    {"name": "Awaiting user",      "statusCategory": {"key": "indeterminate"}},
    {"name": "Closed",             "statusCategory": {"key": "done"}},
    {"name": "Passed to Networks", "statusCategory": {"key": "done"}},
    {"name": "Passed to Security", "statusCategory": {"key": "done"}},
]}]
FIELDS = [
    {"id": "customfield_99001", "name": "Request Type",
     "schema": {"custom": "com.atlassian.servicedesk:vp-origin"}},
    {"id": "customfield_99002", "name": "Affected services", "schema": {}},
    {"id": "customfield_99003", "name": "Organizations", "schema": {}},
    {"id": "customfield_99999", "name": "Story Points", "schema": {}},
]
ISSUES, CHANGELOG = [], {}


def add(key, final, path):
    ISSUES.append({"key": key, "fields": {"status": {"name": final},
                                          "summary": f"ticket {key}"}})
    CHANGELOG[key] = [{"id": f"c{i}", "created": FIXTURE_CREATED,
                       "author": {"displayName": "Agent"},
                       "items": [{"field": "status", "toString": st}]}
                      for i, st in enumerate(path)]


for i in range(30):
    add(f"HD-{i}", "Closed", ["Investigating", "Closed"])
for i in range(30, 45):
    add(f"HD-{i}", "Passed to Networks", ["Investigating", "Passed to Networks"])
for i in range(45, 55):
    add(f"HD-{i}", "Passed to Security", ["Passed to Security"])
for i in range(55, 60):
    add(f"HD-{i}", "Awaiting user", ["Investigating", "Awaiting user"])


class FakeJira:
    """Answers the handful of endpoints discovery uses, and records JQL."""

    def __init__(self, statuses=True):
        self.queries = []
        self.gets = []
        self.stop = None
        self._refusals = set()
        self._statuses = statuses

    def get(self, path, params=None):
        self.gets.append(path)
        if path == "/rest/api/3/field":
            return FIELDS
        if path.endswith("/statuses") and self._statuses:
            return STATUSES
        return None

    def search(self, jql, fields, cap=100):
        self.queries.append(jql)
        return ISSUES if "updated >=" in jql else []

    def changelog(self, key):
        return CHANGELOG.get(key, [])


def watcher(cfg_extra=None, state=None, jira=None):
    cfg = {"domain": "x", "email": "e", "token": "t", "projectKey": "HD"}
    cfg.update(cfg_extra or {})
    json.dump(cfg, open(tl.CONFIG_FILE, "w"))
    w = object.__new__(tl.ShadowWatcher)
    w.cfg = cfg
    w.project = cfg["projectKey"]
    w.jira = jira or FakeJira()
    w.state = state or {}
    w.discovery = tl.Discovery(w.jira, cfg)
    w.req_field, w.context_fields = w.discovery.fields()
    w.dests = list(cfg.get("triageStatuses") or [])
    w.resolved = set(cfg.get("resolvedStatuses") or [])
    w.move_targets = list(cfg.get("moveTargetProjects") or [])
    w.resolved_label = (cfg.get("resolvedLabel")
                        or tl.RESOLVED_LABEL_FMT.format(project=w.project))
    w.moved_label_fmt = cfg.get("movedLabel") or tl.MOVED_LABEL_FMT
    w.moved_label_overrides = dict(cfg.get("movedLabels") or {})
    w.plans = {w.resolved_label: f"resolve in {w.project}"}
    w.learner = tl.Learner(w.state, plans=w.plans)
    w.stop = threading.Event()
    w._issue_fields = ["summary"]
    w._intake_keys = set()
    w._metrics = None
    w._metrics_offset = 0
    w._metrics_pending = {}
    return w


w = watcher()
check("finds the Request Type field by its JSM schema, not a fixed id",
      w.req_field == "customfield_99001", w.req_field)
check("matches context fields by name and ignores unrelated ones",
      set(w.context_fields) == {"customfield_99002", "customfield_99003"})

w.ensure_workflow_known()
check("picks the busiest settled status as 'resolved'", w.resolved == {"Closed"})
check("finds every handoff status",
      set(w.dests) == {"Passed to Networks", "Passed to Security"},
      str(w.dests))
check("does not mistake in-progress or waiting states for outcomes",
      not ({"Investigating", "Awaiting user", "Triage"} & set(w.dests)))
check("labels are generated from that site's own project key",
      w.resolved_label == "Resolved in HD")
check("move labels are generic", w.moved_label("BUGS") == "Moved to BUGS")
check("discovery records that it ran, so it isn't repeated every startup",
      w.state.get("workflowDiscovered") is True)

# A hand-edited resolvedLabel that happens to collide with a real status
# name (or a move label colliding with one) must not show the same outcome
# twice in report tables that index by all_dests.
collider = watcher({"triageStatuses": ["Passed to Networks"],
                    "resolvedLabel": "Passed to Networks"})
check("a colliding resolved label doesn't duplicate the outcome list",
      collider.all_dests == ["Passed to Networks"], collider.all_dests)

# ensure_workflow_known's own changelog scan used h.get("items", []),
# which only falls back when the key is MISSING - not when a history entry
# carries an explicit "items": null, the same present-but-null shape Jira
# sends everywhere else. _transitions already guards this correctly with
# json_objects(); this inline scan had drifted out of step with it, and
# would have refused to even start the learner on any startup where one
# of the 250 sampled issues had a redacted/null changelog entry.
class NullItemsJira(FakeJira):
    def changelog(self, key):
        real = CHANGELOG.get(key, [])
        if key == "HD-0":
            return [{"id": "cnull", "items": None}] + real
        return real


null_watcher = watcher(jira=NullItemsJira())
try:
    result = null_watcher.ensure_workflow_known()
    ok = True
except Exception as exc:                                  # noqa: BLE001
    ok = False
    err = f"{type(exc).__name__}: {exc}"
check("a changelog entry with items: null doesn't stop discovery starting",
      ok, "" if ok else err)
if ok:
    check("  and discovery still completes correctly from the other issues",
          result is True and null_watcher.resolved == {"Closed"},
          (result, null_watcher.resolved))


# --- JQL injection --------------------------------------------------------
# Every query this file builds interpolates a project key as a bare
# identifier (project = {key}) and status names as quoted literals
# ("{status}"). Both can originate from a hand-edited config file, so both
# need to survive a value crafted to break out of the query - the same
# threat model as an unescaped column name or string value reaching SQL.
check('jql_quote escapes a literal double quote',
      tl.jql_quote('O"Brien') == '"O\\"Brien"', tl.jql_quote('O"Brien'))
check('jql_quote escapes a backslash before the quote escape can be forged',
      tl.jql_quote('a\\" OR "1"="1') == '"a\\\\\\" OR \\"1\\"=\\"1"',
      tl.jql_quote('a\\" OR "1"="1'))

for label, key in (('a project key with a JQL clause tacked on',
                    'HD" OR project = "SECRET'),
                   ('a project key with a space', 'HD OR 1=1'),
                   ('an empty project key', ''),
                   ('a project key starting with a digit', '1HD')):
    check(f"{label} is rejected as a project-key identifier",
          not tl._SAFE_PROJECT_KEY.match(key), repr(key))

# A status name containing a quote must not be able to break out of the
# CHANGED TO (...) clause built from self.dests/self.resolved.
evil = watcher({"triageStatuses": ['Passed" OR project = "SECRET'],
               "resolvedStatuses": []})
evil.backfill()
queries = "".join(evil.jira.queries)
check("a malicious status name is quoted, not able to inject a clause",
      'OR project = "SECRET' not in queries
      and '\\"' in queries, evil.jira.queries)

# _remember_move_target persists straight to config, so an invalid value
# reaching it (a tampered response, a bad hand-edit of moveTargetProjects)
# must never be adopted.
mt = watcher()
mt._save_cfg = lambda d: None
mt._remember_move_target('BUGS" OR project = "SECRET')
check("an invalid move-target project key is never adopted",
      mt.move_targets == [], mt.move_targets)
mt._remember_move_target('BUGS')
check("  but a genuine one still is", mt.move_targets == ['BUGS'],
      mt.move_targets)


# --- A desk where everything is resolved in place --------------------------

w2 = watcher({"resolvedStatuses": ["Done"], "triageStatuses": []})
w2.resolve_and_learn()
check("no 'CHANGED TO ()' query when a site has no handoff statuses",
      not [q for q in w2.jira.queries if "CHANGED TO ()" in q])
check("  and the resolved-in-place pass still runs",
      any('CHANGED TO ("Done")' in q for q in w2.jira.queries))

w3 = watcher({"resolvedStatuses": ["Done"], "triageStatuses": []})
w3.state["backfillDone"] = False
w3.backfill()
check("backfill avoids the same malformed query",
      not [q for q in w3.jira.queries if "CHANGED TO ()" in q])

# --- ADF description nodes can carry null content, not just omit it -------
# .get("content", []) only falls back when the key is MISSING; a leaf node
# that instead carries an explicit "content": null passed the null straight
# through and crashed on the following `for child in ...`. flatten_adf runs
# on every issue's description, with no per-issue guard around it anywhere
# it's called - a single ticket shaped like this would have permanently
# stalled backfill on every retry, and silently truncated every
# predict_intake cycle at that same ticket.
for label, node in (
        ("a leaf with content: null",
         {"type": "paragraph", "text": "hi", "content": None}),
        ("a nested child with content: null",
         {"type": "doc", "content": [
             {"type": "paragraph", "content": None},
             {"type": "text", "text": "still here"}]}),
        ("content missing entirely (the normal leaf shape)",
         {"type": "text", "text": "fine"})):
    try:
        result = tl.flatten_adf(node)
        ok = isinstance(result, str)
    except Exception as exc:                            # noqa: BLE001
        ok = False
        result = f"{type(exc).__name__}: {exc}"
    check(f"flatten_adf survives {label}", ok, repr(result))
check("text from other nodes is kept when one sibling has null content",
      "still here" in tl.flatten_adf(
          {"type": "doc", "content": [
              {"type": "paragraph", "content": None},
              {"type": "text", "text": "still here"}]}))

# --- Nightly tune drift must be flagged, but not on small-sample noise -----
# Measured directly on this desk: a 12-ticket holdout bucket for the
# smallest destination swings 30+ points on a single ticket flipping right
# or wrong. Any drift check that alerted on that would be ignored within a
# week. MIN_DRIFT_SAMPLE exists specifically to keep it quiet on exactly
# that case while still catching a real swing.
PRIOR_TUNE = {"date": "2026-08-01", "balanced": 0.90, "firmCov": 0.85,
             "per": {"With BI": 0.80, "With DEV": 0.90}}

noisy_today = dict(PRIOR_TUNE, date="2026-08-02", balanced=0.895,
                   firmCov=0.86,
                   per={"With BI": 0.30, "With DEV": 0.905},
                   perCounts={"With BI": [12, 4], "With DEV": [300, 271]})
check("a wild swing in a tiny holdout bucket is not flagged as drift",
      tl.ShadowWatcher._tune_drift_warning(noisy_today, PRIOR_TUNE) == "",
      tl.ShadowWatcher._tune_drift_warning(noisy_today, PRIOR_TUNE))

real_drift = dict(PRIOR_TUNE, date="2026-08-02", balanced=0.70, firmCov=0.85,
                  per={"With BI": 0.80, "With DEV": 0.50},
                  perCounts={"With BI": [12, 9], "With DEV": [300, 150]})
warning = tl.ShadowWatcher._tune_drift_warning(real_drift, PRIOR_TUNE)
check("a real swing backed by a large enough bucket is flagged",
      "balanced accuracy" in warning and "With DEV" in warning
      and "With BI" not in warning, warning)

check("the first-ever tune has nothing prior to drift from",
      tl.ShadowWatcher._tune_drift_warning(real_drift, None) == "")

# --- A suggestion should name the words that drove it ----------------------
# A confident number alone gives a reviewer nothing to check it against.
# predict_ex now also returns which words in THIS ticket most favoured the
# winning destination over the runner-up, and _short_label folds that into
# the one line the overlay shows.
why_learner = tl.Learner({})
for _ in range(20):
    why_learner.learn("With DEV", tl.tokenize("automation script deploy"),
                      "", [], "Sam", exemplar=["automation"])
    why_learner.learn("With DATA", tl.tokenize("report totals export"),
                      "", [], "Sam", exemplar=["report"])

p = why_learner.predict_ex(tl.tokenize("automation script needs deploying"),
                           "", "Sam", exemplar=["automation"])
check("a confident prediction explains itself with words from the ticket",
      p["dest"] == "With DEV" and bool(p["why"]), p)
check("  and the explanation is actually made of this ticket's own words",
      all(w in "automation script needs deploying" for w in p["why"]),
      p["why"])

label_maker = object.__new__(tl.ShadowWatcher)
label_maker.resolved_label = "Resolved"
label_maker.project = "SD"
label = tl.ShadowWatcher._short_label(label_maker, p)
check("  which shows up in the one-line suggestion the overlay displays",
      "because:" in label, label)

# Cold start (fewer than two destinations with enough examples) returns
# early without ever reaching the why/second-choice logic - predict_intake
# reads p["why"] unconditionally, so a missing key here is a live KeyError
# on a fresh install or a newly-discovered destination, not just a test gap.
cold = tl.Learner({})
p_cold = cold.predict_ex(tl.tokenize("anything at all"), "", "")
check("a cold-start prediction still carries a why key",
      p_cold["dest"] is None and p_cold["why"] == [], p_cold)
check("  so _short_label doesn't need special-casing it",
      isinstance(tl.ShadowWatcher._short_label(
          label_maker, dict(p_cold, dest="Resolved")), str))

# --- Explanations should credit who raised it, not just the wording -------
# Reporter alone is a strong signal - measured at 82.6% discriminative
# power on this desk's real history - so a call driven mostly by who raised
# it, with otherwise-identical wording, would look unexplained without this.
who_learner = tl.Learner({})
for _ in range(20):
    who_learner.learn("With DEV", tl.tokenize("please help with this"),
                      "", [], "Sam", exemplar=["please"])
    who_learner.learn("With DATA", tl.tokenize("please help with this"),
                      "", [], "Alex", exemplar=["please"])

p_match = who_learner.predict_ex(tl.tokenize("please help with this"), "",
                                 "Alex", exemplar=["please"])
check("a reporter whose history matches the winning destination is credited",
      p_match["dest"] == "With DATA"
      and "usually raised by Alex" in p_match["why"], p_match)

p_mismatch = who_learner.predict_ex(tl.tokenize("please help with this"), "",
                                    "Sam", exemplar=["please"])
check("  but not when their history actually points the other way",
      not any("usually raised by Sam" in w for w in p_mismatch["why"]),
      p_mismatch["why"])

p_new = who_learner.predict_ex(tl.tokenize("please help with this"), "",
                               "Someone New", exemplar=["please"])
check("  and a reporter with no track record yet isn't credited either",
      not any("usually raised by" in w for w in p_new["why"]), p_new["why"])

# --- An interrupted backfill must not discard what it already learned -----
# backfill() is a one-time pass that can mean thousands of Jira calls on a
# busy desk - one changelog fetch per issue. It used to write history and
# seenTransitions only once, at the very end; a crash or forced restart
# partway through discarded everything already learned even though
# backfillDone correctly stays False and the next launch tries again.
class _StoppingJira:
    """Every changelog() call counts down; past the limit it also sets the
    stop event, simulating the process being told to quit mid-backfill."""

    def __init__(self, issues, changelog_by_key, stop_after):
        self._issues = issues
        self._changelog = changelog_by_key
        self._stop_after = stop_after
        self._calls = 0
        self.stop = None
        self._refusals = set()

    def get(self, path, params=None):
        return None

    def search(self, jql, fields, cap=100):
        return self._issues

    def changelog(self, key):
        self._calls += 1
        if self._calls > self._stop_after and self.stop is not None:
            self.stop.set()
        return self._changelog.get(key, [])


_N = 20
_bf_issues = [{"key": f"BF-{i}", "fields": {"status": {"name": "Done"}}}
             for i in range(_N)]
_bf_changelog = {
    f"BF-{i}": [{"id": f"bf{i}", "created": FIXTURE_CREATED,
                "author": {"displayName": "Agent"},
                "items": [{"field": "status",
                          "toString": "Passed to Networks"}]}]
    for i in range(_N)}
_bf_jira = _StoppingJira(_bf_issues, _bf_changelog, stop_after=8)
w5 = watcher({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": []},
            jira=_bf_jira)
_bf_jira.stop = w5.stop
w5.state["backfillDone"] = False
w5.backfill()

with open(tl.HISTORY_FILE, encoding="utf-8") as f:
    saved = [json.loads(line) for line in f if line.strip()]
check("an interrupted backfill still writes what it already learned",
      0 < len(saved) < _N, f"{len(saved)} of {_N}")
check("  and remembers which transitions it already scored",
      0 < len(w5.state.get("seenTransitions", [])) < _N,
      str(len(w5.state.get("seenTransitions", []))))
check("  without marking the backfill as finished",
      not w5.state.get("backfillDone"))
check("  and preserves the historical learning count at a shutdown checkpoint",
      w5.state.get("backfillCount") == len(saved))

# --- One unusable issue must not cost the whole pass ------------------------
# Unlike the stop above, this is a genuine failure reaching changelog() - a
# malformed response, a transient error _send's own retries didn't absorb -
# raised from inside the per-issue loop with no per-issue guard around it.
# That aborted the entire pass before any of the checkpointing above could
# run, discarding every other issue in the same batch and, for
# resolve_and_learn specifically, re-fetching and re-failing on the same
# issue every cycle since seenTransitions only saves once, at the end.
class _PoisonJira:
    """changelog() raises for one specific key, as if that one issue's data
    were malformed in a way nothing upstream caught."""

    def __init__(self, issues, changelog_by_key, poison_key):
        self._issues = issues
        self._changelog = changelog_by_key
        self._poison_key = poison_key
        self.stop = None
        self._refusals = set()

    def get(self, path, params=None):
        return None

    def search(self, jql, fields, cap=100):
        return self._issues

    def changelog(self, key):
        if key == self._poison_key:
            raise ValueError("simulated malformed changelog")
        return self._changelog.get(key, [])


_pz_issues = [{"key": f"PZ-{i}", "fields": {"status": {"name": "Done"}}}
             for i in range(10)]
_pz_changelog = {
    f"PZ-{i}": [{"id": f"pz{i}", "created": FIXTURE_CREATED,
                "author": {"displayName": "Agent"},
                "items": [{"field": "status",
                          "toString": "Passed to Networks"}]}]
    for i in range(10)}

w6 = watcher({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": []},
            jira=_PoisonJira(_pz_issues, _pz_changelog, poison_key="PZ-4"))
w6.state["backfillDone"] = False
backfill_ok = True
try:
    w6.backfill()
except Exception as exc:                                  # noqa: BLE001
    backfill_ok = False
    backfill_exc = repr(exc)
check("one unusable issue among many doesn't abort the whole backfill",
      backfill_ok, "" if backfill_ok else backfill_exc)
if backfill_ok:
    check("  and every other issue is still learned",
          len(w6.state.get("seenTransitions", [])) == 9,
          str(len(w6.state.get("seenTransitions", []))))
    check("  with the backfill still marked complete",
          w6.state.get("backfillDone") is True)

w7 = watcher({"triageStatuses": ["Passed to Networks"], "resolvedStatuses": []},
            jira=_PoisonJira(_pz_issues, _pz_changelog, poison_key="PZ-7"))
resolve_ok = True
try:
    w7.resolve_and_learn()
except Exception as exc:                                  # noqa: BLE001
    resolve_ok = False
    resolve_exc = repr(exc)
check("one unusable issue doesn't abort a live resolve_and_learn cycle",
      resolve_ok, "" if resolve_ok else resolve_exc)
if resolve_ok:
    check("  and the other issues in the same cycle are still learned",
          len(w7.state.get("seenTransitions", [])) == 9,
          str(len(w7.state.get("seenTransitions", []))))

# predict_intake is the FAST loop (~12s) and, unlike backfill/resolve_and_
# learn, its queue can hold customer-submitted requests directly - a real
# source of malformed data, not just a hand-edited config. Fault-injected
# via _issue_details itself, since _issue_details' own hardening makes it
# genuinely hard to construct a "naturally" malformed issue that still
# reaches an exception - this represents any failure reaching the per-
# issue body, whatever its source.
_pi_issues = [{"key": f"PI-{i}", "fields": {"status": {"name": "New"},
                                            "summary": f"ticket {i}"}}
             for i in range(10)]


class _IntakeJira(FakeJira):
    def search(self, jql, fields, cap=100):
        return _pi_issues


w8 = watcher(jira=_IntakeJira())
_real_issue_details = w8._issue_details


def _poison_issue_details(issue):
    if issue.get("key") == "PI-4":
        raise ValueError("simulated malformed issue")
    return _real_issue_details(issue)


w8._issue_details = _poison_issue_details
predict_ok = True
try:
    w8.predict_intake()
except Exception as exc:                                  # noqa: BLE001
    predict_ok = False
    predict_exc = repr(exc)
check("one unusable ticket doesn't abort a live predict_intake cycle",
      predict_ok, "" if predict_ok else predict_exc)
if predict_ok:
    check("  and the other tickets in the same cycle still get a suggestion",
          len(w8.state.get("predicted", {})) == 9,
          str(len(w8.state.get("predicted", {}))))

w4 = watcher({"resolvedStatuses": ["Done"], "triageStatuses": []},
             state={"workflowDiscovered": True})
before = len(w4.jira.queries) + len(w4.jira.gets)
w4.ensure_workflow_known()
check("discovery is not repeated once recorded",
      len(w4.jira.queries) + len(w4.jira.gets) == before)


# --- Outcome wording survives the nightly re-fit ---------------------------

w5 = watcher({"resolvedStatuses": ["Done"], "triageStatuses": ["Passed to Networks"]})
label = w5.resolved_label
w5._rebuild_model([{"summary": "a", "desc": "b", "labels": [], "components": [],
                    "structured": [], "reporter": "r", "dest": label,
                    "cloneProjects": [], "requestType": ""}])
check("outcome wording survives the nightly model rebuild",
      w5.learner.plan_for(label) == "resolve in HD",
      w5.learner.plan_for(label))


# --- A rejected token has to leave a trace ---------------------------------

class Req:
    method = "GET"
    path_url = "/rest/api/3/search/jql"


class Resp:
    def __init__(self, code):
        self.status_code = code
        self.ok = 200 <= code < 300
        self.request = Req()


open(tl.ERROR_LOG, "w").close()      # discovery above already logged here
j = object.__new__(tl.Jira)
j._refusals = set()
for _ in range(4):
    j._note_refusal(Resp(401))
j._note_refusal(Resp(403))
j._note_refusal(Resp(200))
lines = (open(tl.ERROR_LOG, encoding="utf-8").read().strip().splitlines()
         if os.path.exists(tl.ERROR_LOG) else [])
check("a rejected token is reported once, not silently swallowed",
      len(lines) == 2 and "401" in lines[0] and "API token" in lines[0],
      f"{len(lines)} line(s)")


# --- Opening the report: os.startfile succeeds silently when there is no --
# file association, so a machine with nothing markdown-aware installed sees
# "Open report" do nothing, forever, with no error anywhere to explain why.
# Confirmed on a real machine: .md had no association, os.startfile raised
# nothing on either the main thread or a background thread (pystray's own
# menu callbacks run on one), and no new process appeared either time.
check("an extension that plainly doesn't exist has no association",
      tl._has_association(".fake9182nope") is False)
check("a real, universally-associated extension is detected as having one",
      tl._has_association(".txt") is True)


# A real notepad.exe used to be launched here to prove the fallback works,
# cleaned up with `taskkill /f /im notepad.exe` - which isn't scoped to the
# one process this test started, so it force-closes anything ELSE named
# notepad.exe too, including a real window with someone's own unsaved
# work in it. Mock the Popen call instead: same proof (the right command
# gets run), no window ever actually opens.
import subprocess as _sp
import tempfile as _tempfile
_real_popen = _sp.Popen
_popen_calls = []
_sp.Popen = lambda args, **kw: _popen_calls.append(args)
fake_ext = ".fake9182nope"
target = os.path.join(_tempfile.mkdtemp(), f"report{fake_ext}")
open(target, "w", encoding="utf-8").write("test report content\n")
try:
    tl.open_in_default_or_notepad(target)
finally:
    _sp.Popen = _real_popen
check("an unassociated extension falls back to Notepad, not silence",
      len(_popen_calls) == 1 and _popen_calls[0][1] == target
      and _popen_calls[0][0].lower().endswith("notepad.exe"), _popen_calls)
check("  launched by its fully-qualified path, not a bare name a malicious "
      "same-named binary earlier in PATH could intercept",
      os.path.isabs(_popen_calls[0][0]), _popen_calls)


# --- Jira sends null, not just absent ---------------------------------------
# .get(key, default) only substitutes the default when the key is MISSING; a
# present null passes straight through. Jira returns an explicit null for a
# field the requesting account cannot see, or one that doesn't apply to the
# issue type — so which fields arrive as null depends on the account and the
# site. A null that never appears here can be routine on someone else's desk,
# and the TypeError it caused would fail every cycle while the learner still
# appeared to be running.
parser = object.__new__(tl.ShadowWatcher)
parser.project = "SD"
parser.req_field = "customfield_10010"
parser.context_fields = ["customfield_10332"]

MALFORMED = {
    "fields missing":          {"key": "SD-1"},
    "fields null":             {"key": "SD-1", "fields": None},
    "summary null":            {"key": "SD-1", "fields": {"summary": None}},
    "summary not a string":    {"key": "SD-1", "fields": {"summary": 42}},
    "description null":        {"key": "SD-1", "fields": {"summary": "x",
                                                          "description": None}},
    "labels null":             {"key": "SD-1", "fields": {"summary": "x",
                                                          "labels": None}},
    "components null":         {"key": "SD-1", "fields": {"summary": "x",
                                                          "components": None}},
    "reporter null":           {"key": "SD-1", "fields": {"summary": "x",
                                                          "reporter": None}},
    "issuelinks null":         {"key": "SD-1", "fields": {"summary": "x",
                                                          "issuelinks": None}},
    "component without name":  {"key": "SD-1", "fields": {"summary": "x",
                                                          "components": [{}]}},
    "link without an issue":   {"key": "SD-1", "fields": {"summary": "x",
                                                          "issuelinks": [{}]}},
    "link type null":          {"key": "SD-1", "fields": {"summary": "x",
                                "issuelinks": [{"type": None,
                                                "outwardIssue": {"key": "X-1"}}]}},
    "key null":                {"key": None, "fields": {"summary": "x"}},
}

for label, payload in MALFORMED.items():
    try:
        det = tl.ShadowWatcher._issue_details(parser, payload)
        feats = tl.features_for(det, det.get("requestType", ""))
        lr = tl.Learner({})
        lr.learn("Somewhere", feats, det.get("requestType", ""), [],
                 det.get("reporter", ""), exemplar=tl.tokenize(det["summary"]))
        ok = isinstance(det["summary"], str) and isinstance(det["labels"], list)
    except Exception as exc:                            # noqa: BLE001
        ok = False
        feats = f"{type(exc).__name__}: {exc}"
    check(f"an issue with {label} is parsed and learned from",
          ok, str(feats)[:60])

# The hardening must not change what a well-formed ticket looks like.
WELL_FORMED = {
    "key": "SD-9",
    "fields": {
        "summary": "Printer offline in Legal",
        "description": {"content": [{"content": [{"text": "stopped working"}]}]},
        "labels": ["hardware", "urgent"],
        "components": [{"name": "Printers"}],
        "reporter": {"displayName": "Sam Carter"},
        "issuelinks": [{"type": {"name": "Escalation"},
                        "outwardIssue": {"key": "DEV-9"}}],
        "customfield_10010": {"requestType": {"name": "Get IT help"}},
        "customfield_10332": [{"value": "Printing"}],
    },
}
good = tl.ShadowWatcher._issue_details(parser, WELL_FORMED)
check("a normal ticket still yields its summary", good["summary"]
      == "Printer offline in Legal")
check("  its description is still flattened", "stopped working" in good["desc"])
check("  its labels and components survive",
      good["labels"] == ["hardware", "urgent"]
      and good["components"] == ["Printers"])
check("  its reporter survives", good["reporter"] == "Sam Carter")
check("  its request type survives", good["requestType"] == "Get IT help")
check("  a link to another project is still a clone",
      [c["key"] for c in good["clones"]] == ["DEV-9"], str(good["clones"]))


# --- Hand-edited settings: a bare string is worse than a null ---------------
# list("With DEV") is eight single characters, not one status. That turned a
# plausible hand-edit into eight bogus statuses, every query built from them
# was nonsense, and nothing said so — silent corruption rather than a crash.
check("a bare string becomes one name, not one per character",
      tl.as_name_list("With DEV") == ["With DEV"],
      str(tl.as_name_list("With DEV")))
check("a null becomes an empty list", tl.as_name_list(None) == [])
check("a proper list is unchanged",
      tl.as_name_list(["With BI", "With DEV"]) == ["With BI", "With DEV"])
check("nulls inside a list are dropped",
      tl.as_name_list(["With DEV", None, ""]) == ["With DEV"])
check("a number becomes its text rather than exploding",
      tl.as_name_list([42]) == ["42"])
check("something not iterable at all is survivable",
      tl.as_name_list(object()) == [])
check("a blank string yields nothing", tl.as_name_list("   ") == [])

check("a label map survives", tl.as_label_map({"PPS": "Proclaim bug"})
      == {"PPS": "Proclaim bug"})
check("a list where a map belongs doesn't crash",
      tl.as_label_map(["PPS"]) == {})
check("a null label map is empty", tl.as_label_map(None) == {})


# --- Everything read off the wire or off disk can be the wrong shape ------
# Jira responses, the changelog, the field catalogue and the history file
# are all arrays-of-objects in the happy case, and every level of that can
# be null or something else on a different site, an older API, or a partial
# response. json_objects filters once so callers can iterate plainly.
check("a null array yields nothing", tl.json_objects(None) == [])
check("a string is not iterated character by character",
      tl.json_objects("junk") == [])
check("non-object entries are dropped",
      tl.json_objects([{"a": 1}, "junk", None, 5]) == [{"a": 1}])
check("a proper array of objects is unchanged",
      tl.json_objects([{"a": 1}, {"b": 2}]) == [{"a": 1}, {"b": 2}])

# The model file carries every decision learned so far, so an unusable
# section is rebuilt empty rather than taking the learner down.
for label, broken in (("model null", {"model": None}),
                      ("model not a dict", {"model": "corrupt"}),
                      ("priors null", {"model": {"priors": None}}),
                      ("tokens null", {"model": {"tokens": None}}),
                      ("exemplars null", {"model": {"exemplars": None}}),
                      ("scale null", {"model": {"scale": None}}),
                      ("scale zero", {"model": {"scale": 0}}),
                      ("scale negative", {"model": {"scale": -1}})):
    try:
        lr = tl.Learner(dict(broken))
        lr.learn("D", ["a", "b", "c"], "rt", [], "Sam", exemplar=["a"])
        lr.predict_ex(["a"], "rt", "Sam", exemplar=["a"])
        ok = lr.m["scale"] > 0
    except Exception:                                   # noqa: BLE001
        ok = False
    check(f"a state file with {label} still loads and learns", ok)

# One unusable history record must not cost the whole nightly re-fit.
rebuild = object.__new__(tl.ShadowWatcher)
rebuild.plans = {}
rebuild.state = {}
rebuild.learner = tl.Learner({})
GOOD_REC = {"summary": "printer broken", "desc": "it broke",
            "labels": ["hw"], "components": ["Printers"],
            "structured": [], "reporter": "Sam", "dest": "With DEV",
            "cloneProjects": [], "requestType": "Get IT help"}
mixed = [GOOD_REC, {}, dict(GOOD_REC, summary=None),
         dict(GOOD_REC, labels=None), dict(GOOD_REC, dest=None), GOOD_REC]
tl.ShadowWatcher._rebuild_model(rebuild, mixed)
check("a re-fit survives unusable records and keeps the good ones",
      rebuild.state["model"]["priors"].get("With DEV", 0) > 0,
      str(rebuild.state["model"]["priors"]))

# The nightly search replays the history once per candidate config, so its
# cost is (history x budget) — about an hour of CPU a night at twenty-five
# thousand decisions, on a machine somebody is trying to work on. The search
# is windowed to bound that; the model itself is still fitted on everything.
check("the tuning window is smaller than a busy desk's history",
      isinstance(tl.TUNE_WINDOW, int) and 0 < tl.TUNE_WINDOW <= 20000,
      str(tl.TUNE_WINDOW))

searched = {}
tuner = object.__new__(tl.ShadowWatcher)
tuner.state = {"lastTuneDate": ""}
tuner.plans = {}
tuner.learner = tl.Learner({})
tuner._save_state = lambda: None
tuner.write_report = lambda: None
tuner._load_history = lambda: [
    {"summary": f"t{i}", "desc": "", "labels": [], "components": [],
     "structured": [], "reporter": "r", "dest": "D",
     "cloneProjects": [], "requestType": ""}
    for i in range(tl.TUNE_WINDOW + 2500)]
tuner._self_tune = lambda recs: searched.update(n=len(recs)) or None
tuner.maybe_daily_tune()
check("the search is capped at the window, not the whole history",
      searched.get("n") == tl.TUNE_WINDOW,
      f"searched {searched.get('n')} of {tl.TUNE_WINDOW + 2500}")

# --- A failed nightly tune must be left to the next day, not retried every
# poll cycle for the rest of today. The date used to be stamped only after
# a successful search; a bug in the search itself meant lastTuneDate never
# advanced, so the very next poll cycle (~60s later, not tomorrow) would
# attempt the same multi-minute, doomed-to-fail search again — and again
# every cycle after that, all day.
attempts = {"n": 0}
broken = object.__new__(tl.ShadowWatcher)
broken.state = {"lastTuneDate": ""}
broken.plans = {}
broken.learner = tl.Learner({})
broken._save_state = lambda: None
broken.write_report = lambda: None
broken._load_history = lambda: [
    {"summary": f"t{i}", "desc": "", "labels": [], "components": [],
     "structured": [], "reporter": "r", "dest": "D",
     "cloneProjects": [], "requestType": ""}
    for i in range(400)]


def _boom(recs):
    attempts["n"] += 1
    raise RuntimeError("the search itself is broken")


broken._self_tune = _boom
broken.maybe_daily_tune()
check("a failed tune doesn't raise out and still stamps today's date",
      broken.state.get("lastTuneDate") == tl.date.today().isoformat(),
      broken.state.get("lastTuneDate"))
broken.maybe_daily_tune()          # simulates the next poll cycle, same day
check("  so a second attempt the same day is skipped, not retried",
      attempts["n"] == 1, f"attempted {attempts['n']} times")

# --- The nightly search must not stay stuck in one basin -------------------
# _self_tune used to warm-start every night from whatever TUNING already
# was. Proven against real history: with decayFactor stuck at 0.999 the
# smallest destination class scored 54.5%; restarting the search from the
# offline BALANCED_TUNED_CONFIG baseline as well found decayFactor=1.0 and
# lifted it to 81.8%. Pin the mechanism without the cost of a real replay:
# make the live-config seed score as a local optimum (no single-axis move
# beats it) while the baseline seed scores higher outright everywhere, and
# check the search still ends up at the baseline.
def _m(balanced: float) -> dict:
    return {"n": 1, "balanced": balanced, "worst": balanced,
            "firmBalanced": balanced, "firmWorst": balanced,
            "firmAcc": balanced, "firmCov": balanced, "acc": balanced,
            "per": {}, "firmPer": {}, "perCounts": {}, "firmPerCounts": {},
            "sources": {}}


def _matches(cfg: dict, seed: dict) -> bool:
    return all(cfg.get(k) == v for k, v in seed.items()
               if k in tl.ShadowWatcher._TUNE_KEEP)


def _fake_replay(self, recs, cfg, score_start=None, score_end=None):
    if _matches(cfg, tl.BALANCED_TUNED_CONFIG):
        return _m(0.95)
    if _matches(cfg, TUNING_SEED):
        return _m(0.80)
    return _m(0.10)          # any single-axis move away from either seed


TUNING_SEED = dict(tl.TUNING)
trap = object.__new__(tl.ShadowWatcher)
trap.state = {}
trap.plans = {}
trap.learner = tl.Learner({})
trap._replay_history = _fake_replay.__get__(trap)
picked = tl.ShadowWatcher._self_tune(trap, [{} for _ in range(10)])
check("two starting points are tried and the better one wins",
      picked is not None and picked["score"][0] == 0.95,
      repr(picked["score"] if picked else None))

# A descent that never plateaus (every trial looks like an improvement)
# used to be able to spend the WHOLE shared budget before the second start
# ever got to run — confirmed on real history, where the first (warm)
# descent alone consumed all 120 evaluations, leaving the offline baseline
# unexamined. Each start now gets its own guaranteed half; check the
# baseline's own seed config still gets at least one real evaluation even
# against a first descent designed to be insatiable.
baseline_seen = {"n": 0}
call_n = {"n": 0}


def _fake_replay_hungry(self, recs, cfg, score_start=None, score_end=None):
    call_n["n"] += 1
    if _matches(cfg, tl.BALANCED_TUNED_CONFIG):
        baseline_seen["n"] += 1
    return _m(0.001 * call_n["n"])   # strictly increasing: never plateaus


hungry = object.__new__(tl.ShadowWatcher)
hungry.state = {}
hungry.plans = {}
hungry.learner = tl.Learner({})
hungry._replay_history = _fake_replay_hungry.__get__(hungry)
tl.ShadowWatcher._self_tune(hungry, [{} for _ in range(10)])
check("an insatiable first descent doesn't starve the second of its budget",
      baseline_seen["n"] >= 1, repr(baseline_seen))

# --- Champion/challenger gate: a config that only looks good on the exact
# window it was searched against must not be deployed on that basis alone --
# The search picks whichever config scores best on tuning_recs; the gate is
# what's supposed to catch that config overfitting to that specific window
# rather than genuinely being better. Real risk here is an off-by-one or a
# swapped comparison that would silently deploy the search's pick regardless
# of what the gate says - wrong, but nothing a normal test run would notice,
# since it wouldn't crash and wouldn't touch Jira either way.
incumbent_cfg = dict(tl.TUNING)


def _fake_gate_replay(self, recs, cfg, score_start=None, score_end=None):
    on_gate_or_holdout = score_start is not None
    is_challenger = _matches(cfg, tl.BALANCED_TUNED_CONFIG)
    is_incumbent = _matches(cfg, incumbent_cfg)
    if not on_gate_or_holdout:
        # Coordinate descent: the eventual challenger looks best everywhere
        # it's tried during the search itself.
        return _m(0.95) if is_challenger else _m(0.10)
    # Gate/holdout: the challenger's apparent edge doesn't hold up on data
    # it wasn't searched against - exactly the overfit case the gate exists
    # to catch.
    if is_challenger:
        return _m(0.20)
    if is_incumbent:
        return _m(0.90)
    return _m(0.50)


rejector = object.__new__(tl.ShadowWatcher)
rejector.state = {}
rejector.plans = {}
rejector.learner = tl.Learner({})
rejector._replay_history = _fake_gate_replay.__get__(rejector)
gated = tl.ShadowWatcher._self_tune(rejector, [{} for _ in range(3000)])
check("a challenger that only wins on the window it was searched against "
      "is rejected by the gate",
      gated is not None and gated["challengerRejected"] is True,
      gated)
check("  and the incumbent config is what actually gets kept",
      gated is not None and _matches(gated["cfg"], incumbent_cfg),
      gated["cfg"] if gated else None)


def _fake_gate_replay_genuine(self, recs, cfg, score_start=None, score_end=None):
    # Same shape, but this time the challenger is genuinely better
    # everywhere, gate window included.
    return _m(0.95) if _matches(cfg, tl.BALANCED_TUNED_CONFIG) else _m(0.10)


accepter = object.__new__(tl.ShadowWatcher)
accepter.state = {}
accepter.plans = {}
accepter.learner = tl.Learner({})
accepter._replay_history = _fake_gate_replay_genuine.__get__(accepter)
accepted = tl.ShadowWatcher._self_tune(accepter, [{} for _ in range(3000)])
check("a challenger that is genuinely better on the gate too is deployed",
      accepted is not None and accepted["challengerRejected"] is False
      and _matches(accepted["cfg"], tl.BALANCED_TUNED_CONFIG),
      accepted["cfg"] if accepted else None)

# --- A truncated log line must not blank the whole report ------------------
# _log() is a plain append, not atomic, so a kill mid-write (a forced
# restart, a crash, Windows updating) can leave a half-written last line.
# _load_history and _live_metrics already skip a line like that; write_report
# read the same file's tail without the same guard, so one bad line would
# raise out of resolve_and_learn every cycle from then on — freezing
# TRIAGE_REPORT.md and, since the exception fires before last_slow is
# updated, pulling the "slow" Jira queries up to the 12s fast-poll cadence.
reporter = object.__new__(tl.ShadowWatcher)
reporter.state = {}
reporter.plans = {}
reporter.project = "SD"
reporter.dests = []
reporter.resolved_label = "Resolved"
reporter.move_targets = []
reporter.learner = tl.Learner({})
reporter._metrics = None
with open(tl.LOG_FILE, "w", encoding="utf-8") as f:
    f.write(json.dumps({"event": "routed", "key": "SD-1", "dest": "D",
                        "by": "Sam", "when": "2026-01-01 00:00",
                        "clones": [], "predicted": None}) + "\n")
    f.write('{"event": "routed", "key": "SD-2", "dest": "D", "by": "Sam"')
    # no closing brace/newline — an interrupted write

report_ok = True
try:
    tl.ShadowWatcher.write_report(reporter)
except Exception as exc:                                  # noqa: BLE001
    report_ok = False
    report_exc = repr(exc)
check("a truncated final log line doesn't crash the report",
      report_ok, "" if report_ok else report_exc)
if report_ok:
    with open(tl.REPORT_FILE, encoding="utf-8") as f:
        report_text = f.read()
    check("the entry before the truncated line still made it into the report",
          "SD-1" in report_text, report_text[-300:])

# --- A persistent auth failure must not hide behind a fresh-looking report -
# The report is rewritten every cycle whether or not Jira actually answered,
# so "Updated just now" alone can't be trusted to mean "still getting real
# data" - a dead token looks identical to a healthy quiet desk otherwise.
# _note_refusal already tracks this per process run; it just never surfaced
# in the one place a person actually looks.
class _RefusingJira:
    _refusals = {401}


reporter.jira = _RefusingJira()
tl.ShadowWatcher.write_report(reporter)
with open(tl.REPORT_FILE, encoding="utf-8") as f:
    report_text = f.read()
check("a tracked auth refusal is surfaced at the top of the report",
      "401" in report_text and "refusing requests" in report_text,
      report_text[:300])

# The same signal should be scrapeable without parsing markdown.
status = json.load(open(tl.STATUS_FILE, encoding="utf-8"))
check("the status file marks itself unhealthy during a refusal",
      status["healthy"] is False and status["refusals"] == [401], status)
check("  and still carries the provenance fields a monitor would want",
      set(status["provenance"]) >= {"trainedOnDecisions", "backfillComplete",
                                    "lastTuneDate", "outcomes"},
      status["provenance"])

reporter.jira = object.__new__(tl.Jira)
reporter.jira._refusals = set()
tl.ShadowWatcher.write_report(reporter)
with open(tl.REPORT_FILE, encoding="utf-8") as f:
    report_text = f.read()
check("no refusals means no banner",
      "refusing requests" not in report_text, report_text[:200])
check("  and the status file agrees it's healthy",
      json.load(open(tl.STATUS_FILE, encoding="utf-8"))["healthy"] is True)

# The "Mistakes learned from" section loops `for pair, n in wrong[:10]` -
# and a plain `n`/`c` earlier in this same function, meant to hold
# stats["predictions"]/["correct"], is not a different variable from that
# loop's `n` in Python. It silently got overwritten with a confusion-pair
# count, and the status file above reported that count as the total number
# of predictions ever made instead. Caught live, against the real running
# model, not by any test - this reproduces it with a populated confusion
# dict, which the fixtures above never had.
reporter.state["stats"] = {"predictions": 1313, "correct": 1178}
reporter.state["confusion"] = {"With DEV → With BI": 3, "With BI → With DEV": 5}
tl.ShadowWatcher.write_report(reporter)
status = json.load(open(tl.STATUS_FILE, encoding="utf-8"))
check("a populated confusion table doesn't clobber the prediction total",
      status["accuracy"] == {"predictions": 1313, "correct": 1178,
                             "pct": round(100 * 1178 / 1313, 1)},
      status["accuracy"])

del reporter.jira            # matches how the earlier fixtures never set it
try:
    tl.ShadowWatcher.write_report(reporter)
    no_jira_ok = True
except Exception as exc:                                  # noqa: BLE001
    no_jira_ok = False
    no_jira_exc = repr(exc)
check("write_report survives not even having a jira attribute",
      no_jira_ok, "" if no_jira_ok else no_jira_exc)


# --- Don't rewrite megabytes when nothing was learned ----------------------
# The state file holds the whole model, and lastPollEpoch is stamped every
# cycle — so the state looked changed every time and the file was rewritten
# at 1.2 times a minute, roughly 4GB of disk a day, on a tool that is
# usually idle. Learning is still written immediately.
import time as _t                                       # noqa: E402

saver = object.__new__(tl.ShadowWatcher)
saver.state = {"model": {}}
saver.plans = {}
saver.learner = tl.Learner(saver.state, plans={})
saver._saved_mutations = 0
saver._last_state_save = 0.0
_written = []
_real_write = tl._atomic_json_write
tl._atomic_json_write = lambda p, d: _written.append(p)
try:
    saver._save_state(force=False)          # never saved before
    first = len(_written)
    for _ in range(5):
        saver._save_state(force=False)      # idle
    check("idle cycles don't rewrite the state",
          len(_written) == first, f"{len(_written) - first} extra writes")

    saver.learner.learn("D", ["a", "b", "c"], "rt", [], "Sam", exemplar=["a"])
    before = len(_written)
    saver._save_state(force=False)
    check("anything learned is persisted straight away",
          len(_written) == before + 1)

    before = len(_written)
    saver._save_state()                     # shutdown / tune / discovery
    check("a forced save always writes", len(_written) == before + 1)

    saver._last_state_save = _t.time() - tl.ShadowWatcher._IDLE_SAVE_SECS - 1
    before = len(_written)
    saver._save_state(force=False)
    check("bookkeeping is still flushed once the idle interval passes",
          len(_written) == before + 1)
finally:
    tl._atomic_json_write = _real_write

# --- Atomic writes must not write through a pre-existing file at the temp
# name --------------------------------------------------------------------
# The old temp name was f"{path}.{pid}.{tid}.tmp" - a PID and thread id are
# both guessable - and plain open(tmp, "w") follows whatever already sits
# there rather than refusing. NTFS lets an unprivileged local process
# create a hard link at a predictable path; this app would then write the
# real content straight through it into wherever that link actually
# points. Proven here with a real file standing in for that link: it must
# come out completely untouched, and the real target must still update.
import os as _os                                                     # noqa: E402

target = _os.path.join(_os.path.dirname(tl.STATE_FILE), "atomic_target.json")
planted = f"{target}.{_os.getpid()}.{threading.get_ident()}.tmp"
_os.makedirs(_os.path.dirname(target), exist_ok=True)
sentinel = "do not touch — stands in for a pre-planted hard link"
with open(planted, "w", encoding="utf-8") as f:
    f.write(sentinel)
try:
    tl._atomic_json_write(target, {"real": "content"})
    still_intact = (_os.path.exists(planted)
                    and open(planted, encoding="utf-8").read() == sentinel)
    check("a file already at the old predictable temp name is left untouched",
          still_intact)
    check("  and the real target is still written correctly",
          json.load(open(target, encoding="utf-8")) == {"real": "content"})
finally:
    for p in (planted, target):
        try:
            _os.unlink(p)
        except OSError:
            pass

# --- Route safety: dormant/low-precision/underevidenced routes must be
# forced to review even when the model itself is confident ------------------
# _derive_route_safety is the mechanism that stands between a plausible-
# looking suggestion and a route with no real evidence behind it - the
# thing the whole shadow-only design exists to protect against getting
# wrong, so it gets exercised directly rather than just trusted because
# the rest of the suite still passes.
rs = watcher({"triageStatuses": ["With DEV", "With BI", "With DATA"]})
now = datetime.now()
recent = lambda days_ago: f"{now - timedelta(days=days_ago):%Y-%m-%d %H:%M}"

route_recs = (
    # With DEV: active recently, precision comfortably clears the bar
    [{"when": recent(1), "dest": "With DEV"} for _ in range(5)]
    # With BI: precision is fine, but nothing routed there recently
    + [{"when": recent(200), "dest": "With BI"} for _ in range(5)]
    # With DATA: active recently, but its confident calls are unreliable
    + [{"when": recent(1), "dest": "With DATA"} for _ in range(5)]
)
route_validation = {"firmPrecisionCounts": {
    "With DEV":  [100, 100], # enough evidence for the precision lower bound too
    "With BI":   [20, 20],   # would also clear it, if it weren't dormant
    "With DATA": [20, 10],   # 50% - half of what it claims is wrong
}}
policy = rs._derive_route_safety(route_recs, route_validation)

check("an active route with strong confident precision is not forced to review",
      policy["With DEV"]["forceReview"] is False, policy["With DEV"])
check("a dormant route is forced to review even with perfect past precision",
      policy["With BI"]["forceReview"] is True
      and "dormant" in policy["With BI"]["reason"], policy["With BI"])
check("a route with weak confident precision is forced to review despite "
      "recent activity",
      policy["With DATA"]["forceReview"] is True
      and "precision" in policy["With DATA"]["reason"], policy["With DATA"])

# A route with no confident predictions at all in validation (e.g. a
# brand-new outcome) must not be trusted by default - "no evidence of being
# wrong" is not the same as "evidence of being right".
check("a route with no confident validation history is forced to review",
      policy.get("Resolved in HD", {}).get("forceReview") is True,
      policy.get("Resolved in HD"))

# --- A tightened safety policy must apply to suggestions already on screen -
# The prediction that was already shown kept its own honest review flag,
# but write_suggestions must still publish it as needing review if the
# nightly tune has since decided that route can't be trusted.
rs.route_safety = {"With DATA": policy["With DATA"]}
already_shown = {"K-1": {"dest": "With DATA", "conf": 91, "second": None,
                        "secondConf": 0, "review": False, "source": "model",
                        "note": "move to With DATA", "at": recent(0)[:16]}}
rs._write_suggestions(already_shown)
published = json.load(open(tl.SUGGEST_FILE, encoding="utf-8"))
check("a suggestion is republished as review once its route is flagged unsafe",
      published["K-1"]["review"] is True, published)
check("  without mutating the original stored prediction's own review flag",
      already_shown["K-1"]["review"] is False, already_shown["K-1"])

sys.exit(check.done())
