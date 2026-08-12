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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_learner          # noqa: E402

tl = load_learner()
check = Checker("triage learner")


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
    CHANGELOG[key] = [{"id": f"c{i}", "created": "2026-07-01T10:00:00.000+0000",
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


def _procs_named(name: str) -> set:
    import subprocess
    out = subprocess.check_output(
        ["tasklist", "/fi", f"imagename eq {name}", "/fo", "csv"], text=True)
    return {ln for ln in out.splitlines() if name.lower() in ln.lower()}


import tempfile as _tempfile
fake_ext = ".fake9182nope"
target = os.path.join(_tempfile.mkdtemp(), f"report{fake_ext}")
open(target, "w", encoding="utf-8").write("test report content\n")
before = _procs_named("notepad.exe")
tl.open_in_default_or_notepad(target)
import time as _time
_time.sleep(1.5)
after = _procs_named("notepad.exe")
check("an unassociated extension falls back to Notepad, not silence",
      len(after) > len(before), f"before={len(before)} after={len(after)}")
import subprocess as _sp
_sp.run(["taskkill", "/f", "/im", "notepad.exe"], capture_output=True)


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

sys.exit(check.done())
