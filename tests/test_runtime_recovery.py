"""Offline startup, malformed Jira pages, cancellation and startup ownership."""
from datetime import datetime, timedelta, timezone
from email.utils import format_datetime
import threading
from types import SimpleNamespace

from _harness import Checker, load_learner
import app_startup
from triage_jira import retry_delay

tl = load_learner()
check = Checker("runtime recovery")


class Page:
    ok = True
    status_code = 200
    headers = {}
    request = SimpleNamespace(method="POST", path_url="/rest/api/3/search/jql")
    def __init__(self, value): self.value = value
    def json(self): return self.value


def client():
    result = object.__new__(tl.Jira)
    result.stop = threading.Event()
    result.incomplete_reads = 0
    result._refusals = set()
    return result


for data in ({}, {"issues": None}, {"issues": "bad"}, {"issues": [None]},
             {"issues": [{}]}, {"issues": [], "isLast": False},
             {"issues": [], "isLast": "true"}, {"issues": [], "nextPageToken": []}):
    j = client()
    j._send = lambda *a, **k: Page(data)
    j.search("arbitrary project", [], cap=None)
    check("malformed search cannot be certified complete: " + str(data), j.incomplete_reads > 0)
j = client()
pages = iter([{"issues": [{"key": "ANY-1"}], "nextPageToken": "first"},
              {"issues": [{"key": "ANY-1"}], "nextPageToken": "second"}])
j._send = lambda *a, **k: Page(next(pages))
check("repeated search pages terminate, deduplicate and retain a retry checkpoint",
      j.search("arbitrary", [], cap=None) == [{"key": "ANY-1"}] and j.incomplete_reads > 0)
j = client()
j._send = lambda *a, **k: Page({"issues": [{"key": "SAMPLE-1"}], "nextPageToken": "more"})
check("an explicit bounded sample is not a transport failure",
      len(j.search("sample", [], cap=1)) == 1 and j.incomplete_reads == 0)
for data in ({"values": None}, {"values": [], "isLast": False},
             {"values": [{}], "isLast": True}, {"values": [], "isLast": "true"},
             {"values": [], "startAt": 10}):
    j = client()
    j.get = lambda *a, **k: data
    j.changelog("ARBITRARY-1")
    check("malformed changelog retains retry evidence: " + str(data), j.incomplete_reads > 0)
j = client()
calls = []
def repeating(*args):
    calls.append(args)
    return {"values": [{"id": "stable-id", "items": []}], "isLast": False}
j.get = repeating
check("a server repeating changelog pages cannot hang shutdown or duplicate learning",
      len(j.changelog("OTHER-1")) == 1 and len(calls) == 2 and j.incomplete_reads > 0)
j = client()
pages = iter([{"values": [{"id": "one"}], "total": 2, "startAt": 0},
              {"values": [{"id": "two"}], "total": 2, "startAt": 1}])
j.get = lambda *a: next(pages)
check("total-based changelog pagination works without isLast",
      len(j.changelog("NEW-9")) == 2 and not j.incomplete_reads)
now = datetime.now(timezone.utc).replace(microsecond=0)
check("Retry-After supports HTTP dates and fractional seconds",
      retry_delay(format_datetime(now + timedelta(seconds=120)), 1, now) == 120
      and retry_delay("0.5", 1) == 0.5)
check("invalid or nonfinite retry headers use a safe fallback",
      all(retry_delay(value, 2, now) == 2 for value in (None, "garbage", "nan", "inf")))
j = client()
j._send = lambda *a, **k: Page({"issues": [], "isLast": True, "nextPageToken": ""})
check("an empty terminal token is accepted for compatible Jira responses", j.search("empty desk", [], cap=None) == [] and not j.incomplete_reads)
j = client()
j.domain = "example.invalid"
j.stop.set()
j.session = SimpleNamespace(request=lambda *a, **k: (_ for _ in ()).throw(AssertionError("No request after quit")))
check("shutdown prevents even the initial HTTP request", j._send("GET", "/read-only") is None)

# Failed metadata discovery must remain retryable, not become saved defaults.
catalogue = [None]
discovery = tl.Discovery(SimpleNamespace(get=lambda *a: catalogue[0]), {})
fields = discovery.fields()
w = object.__new__(tl.ShadowWatcher)
w.discovery = discovery
w.state = {"workflowDiscovered": True}
w.resolved, w.dests = {"Completed by this desk"}, []
w.req_field, w.context_fields = fields
saved = []
w._save_cfg = lambda values: saved.append(values)
check("an offline field catalogue does not persist empty discovery defaults",
      not w.ensure_workflow_known() and not saved)
catalogue[0] = [{"id": "customfield_87654", "name": "Arbitrary portal label",
                 "schema": {"custom": tl.REQUEST_TYPE_SCHEMA}}]
check("field metadata recovers using that desk's own schema and identifiers",
      w.ensure_workflow_known() and w.req_field == "customfield_87654"
      and w.req_field in w._issue_fields and len(saved) == 1)

# The real workflow loop, with all IO replaced. Never uses real Jira/config.
class Stop:
    def __init__(self, cancel=False): self.stopped = False; self.cancel = cancel; self.waits = []
    def is_set(self): return self.stopped
    def set(self): self.stopped = True
    def wait(self, seconds):
        self.waits.append(seconds)
        if self.cancel: self.stopped = True
        return self.stopped


def runtime(cancel=False):
    w = object.__new__(tl.ShadowWatcher)
    w.state = {}
    w.stop = Stop(cancel)
    events = []
    attempts = iter([False, True])
    w.ensure_workflow_known = lambda: next(attempts)
    w.write_report = lambda: events.append("health")
    w.jira = SimpleNamespace(session=SimpleNamespace(close=lambda: events.append("closed")))
    def backfill(): w.state["backfillDone"] = True; events.append("backfill")
    w.backfill = backfill
    w.predict_intake = lambda: (events.append("predict"), w.stop.set())
    w.resolve_and_learn = lambda: events.append("UNEXPECTED poll after quit")
    w.maybe_daily_tune = lambda: events.append("UNEXPECTED tune after quit")
    return w, events
w, events = runtime()
w.run_forever()
check("offline workflow discovery retries and recovers without restarting the process",
      w.stop.waits == [30] and events == ["health", "backfill", "predict", "closed"])
w, events = runtime(cancel=True)
w.run_forever()
check("quit interrupts discovery backoff and always closes the Jira session",
      events == ["health", "closed"] and bool(w.state.get("runtimeProblem")))

# Historical transport failures cannot certify a backfill as complete.
w = object.__new__(tl.ShadowWatcher)
w.state = {"backfillCount": 12}
w.stop = threading.Event()
w.project = "GENERIC"
w.dests, w.move_targets = [], []
w.resolved = {"Finished by service desk"}
w._issue_fields = []
w._jira_now = lambda: datetime.now()
w._save_state = lambda **k: None
w.write_report = lambda: None
class Partial:
    incomplete_reads = 0
    def search(self, *a, **k): self.incomplete_reads += 1; return []
w.jira = Partial()
w.backfill()
check("partial historical reads remain resumable without discarding counts",
      not w.state["backfillDone"] and w.state["backfillCount"] == 12)
w.jira.search = lambda *a, **k: []
w.backfill()
check("a successful historical retry completes and retains existing progress", w.state["backfillDone"])

class Key:
    def __enter__(self): return self
    def __exit__(self, *a): pass
class Registry:
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ = 1, 1, 2, 1
    def __init__(self): self.values = {}; self.exists = False
    def OpenKey(self, *a):
        if not self.exists: raise FileNotFoundError()
        return Key()
    def CreateKeyEx(self, *a): self.exists = True; return Key()
    def QueryValueEx(self, key, name):
        if name not in self.values: raise FileNotFoundError()
        return self.values[name], 1
    def SetValueEx(self, key, name, r, t, value): self.values[name] = value
    def DeleteValue(self, key, name): del self.values[name]
registry = Registry()
root, interpreter = r"C:\Service Desk", r"C:\Runtime\pythonw.exe"
app_startup.set_enabled(root, interpreter, True, registry)
check("a fresh Windows account gets one coordinated startup entry",
      app_startup.enabled(root, interpreter, registry) and list(registry.values) == [app_startup.PRIMARY])
registry.values[app_startup.LEGACY] = '"C:\\Old\\pythonw.exe" "C:\\Old\\triage_learner.pyw"'
app_startup.set_enabled(root, interpreter, True, registry)
check("enabling startup retires the legacy independent learner launch", app_startup.LEGACY not in registry.values)
registry.values[app_startup.PRIMARY] = '"C:\\Elsewhere\\pythonw.exe" "C:\\Elsewhere\\jira_overlay.pyw"'
app_startup.set_enabled(root, interpreter, False, registry)
check("stale/other-install commands are not misreported or removed on disable",
      not app_startup.enabled(root, interpreter, registry) and bool(registry.values))
raise SystemExit(check.done())
