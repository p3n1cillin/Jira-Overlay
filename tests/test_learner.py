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

sys.exit(check.done())
