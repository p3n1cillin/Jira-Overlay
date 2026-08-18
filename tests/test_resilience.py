"""Window placement and network retry.

Neither has misbehaved; both are the sort of thing that breaks quietly on
somebody else's machine - a monitor that isn't there any more, a Jira that
is rate-limiting - so they are pinned down here.
"""

import os
import sys
import threading
import time

import requests

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_learner, load_overlay      # noqa: E402

check = Checker("placement and retry")
ov = load_overlay()

# --- Window placement ------------------------------------------------------

MONITOR = (0, 0, 1920, 1032)
WIDTH, HEIGHT = 260, 300


def place(cfg, monitor):
    """Returns (x, y, config-after) from one _reposition."""
    o = object.__new__(ov.JiraOverlay)
    o.config = dict(cfg)
    o._sw, o._sh = 1920, 1080
    placed = {}

    class FakeRoot:
        def update_idletasks(self):
            pass

        def winfo_width(self):
            return WIDTH

        def winfo_height(self):
            return HEIGHT

        def winfo_x(self):
            return 100

        def winfo_y(self):
            return 100

        def geometry(self, spec):
            placed["spec"] = spec

    o.root = FakeRoot()
    ov.get_monitor_rect = lambda x, y, work=False, m=monitor: m
    ov.JiraOverlay._reposition(o)
    x, y = (int(v) for v in placed["spec"].lstrip("+").split("+"))
    return x, y, o.config


for label, cfg, monitor in [
        ("a saved corner", {"snapCorner": "top-left"}, MONITOR),
        ("a corner name it doesn't know", {"snapCorner": "nowhere"}, MONITOR),
        ("a saved position", {"lastX": 300, "lastY": 400}, MONITOR),
        ("a position far off the right", {"lastX": 99999, "lastY": 99999}, MONITOR),
        ("a negative position", {"lastX": -5000, "lastY": -5000}, MONITOR),
        ("nothing saved at all", {}, MONITOR)]:
    x, y, _cfg = place(cfg, MONITOR)
    on_screen = (MONITOR[0] <= x and MONITOR[1] <= y
                 and x + WIDTH <= MONITOR[2] and y + HEIGHT <= MONITOR[3])
    check(f"the overlay lands on screen given {label}", on_screen, f"({x}, {y})")

# A monitor that has been unplugged since the position was saved.
x, y, cfg_after = place({"lastX": 300, "lastY": 400}, None)
check("an unplugged monitor falls back to a corner",
      cfg_after.get("snapCorner") == "bottom-right"
      and "lastX" not in cfg_after, str(cfg_after))


# --- Network retry ---------------------------------------------------------

tl = load_learner()


class Response:
    def __init__(self, code, retry_after=None):
        self.status_code = code
        self.ok = 200 <= code < 300
        self.headers = {"Retry-After": retry_after} if retry_after else {}
        self.request = type("R", (), {"method": "GET", "path_url": "/x"})()

    def json(self):
        return {}


def client(responses, stop=None, real_backoff=False):
    """A Jira client wired to canned responses.

    The backoff is skipped unless a test is specifically about timing -
    waiting out the real 1+2+4+8 seconds would make this suite slow enough
    that people stop running it.
    """
    jira = object.__new__(tl.Jira)
    jira.domain = "x"
    jira.stop = stop
    jira._refusals = set()
    attempts = []
    it = iter(responses)

    class Session:
        def request(self, *a, **k):
            attempts.append(time.perf_counter())
            nxt = next(it)
            if isinstance(nxt, Exception):
                raise nxt
            return nxt

    jira.session = Session()
    if not real_backoff:
        jira._pause = lambda seconds: (stop.wait(0) if stop else False)
    return jira, attempts


for label, responses, want_attempts, want_result in [
        ("a first-time success", [Response(200)], 1, 200),
        ("a 500 then success", [Response(500), Response(200)], 2, 200),
        ("a 429 then success", [Response(429, "1"), Response(200)], 2, 200),
        ("a dropped connection",
         [requests.ConnectionError(), Response(200)], 2, 200),
        ("a persistent 500", [Response(500)] * 6, 5, None)]:
    jira, attempts = client(responses)
    result = jira._send("GET", "/x")
    got = None if result is None else result.status_code
    check(f"{label} takes {want_attempts} attempt(s)",
          len(attempts) == want_attempts, f"{len(attempts)}")
    check(f"  ...and returns {want_result}", got == want_result, str(got))

# Rejections are permanent, so retrying them only delays the report.
for code in (401, 403, 404):
    jira, attempts = client([Response(code)])
    jira._send("GET", "/x")
    check(f"an HTTP {code} is not retried", len(attempts) == 1, str(len(attempts)))

# Quitting must not wait out a backoff.
stop = threading.Event()
jira, attempts = client([Response(500)] * 6, stop=stop, real_backoff=True)
threading.Timer(0.3, stop.set).start()
started = time.perf_counter()
jira._send("GET", "/x")
elapsed = time.perf_counter() - started
check("Quit aborts a backoff instead of waiting it out", elapsed < 2.0,
      f"{elapsed:.2f}s")

# --- A malformed queue count must not stop the clock ----------------------
# dict.get(key, 0) returns None when the key is present but null, which Jira
# does send. That None reached a `>` comparison and raised TypeError out of
# _update_ui — and because _fetch_finished repaints BEFORE scheduling the
# next poll, the scheduling was skipped and the overlay went quiet for good
# with nothing on screen to say so.
O = ov.JiraOverlay


class _QR:
    ok = True
    status_code = 200

    def __init__(self, payload):
        self._p = payload

    def json(self):
        return self._p

    def raise_for_status(self):
        pass


def _counts_for(raw_count):
    """Run a real fetch with `raw_count` as Jira's issueCount."""
    queues = [{"name": "Q", "id": 1, "issueCount": raw_count}] \
        if raw_count is not ... else [{"name": "Q", "id": 1}]
    o = object.__new__(O)
    o.config = {"projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "D",
                "alertQueues": ["Q"], "alertQueuesAsked": True}
    o._jira_url = lambda p="": "https://x" + p

    def fake_get(url, **kw):
        return _QR({"values": [{"key": "SD-1", "fields": {"summary": "t"}}]}
                   if "/queue/1/issue" in url else {"values": queues})

    o._session = type("S", (), {"get": staticmethod(fake_get)})()
    o.queues = []
    o._all_queue_names = []
    o.new_issues = []
    o._reconciled_counts = {}
    o._alert_count = 0
    o._last_full_fetch = 9e18
    o._alert_queues_missing = []
    o._post = lambda fn: None
    o._fetch_completion_stats = lambda c: None
    O._fetch_data(o)
    return o.queues[0].get("issueCount") if o.queues else None


for label, raw in (("null", None), ("a string", "12"), ("a float", 3.7),
                   ("negative", -5), ("garbage text", "many"),
                   ("absent", ...), ("a bool", True)):
    got = _counts_for(raw)
    check(f"an issueCount of {label} becomes a usable integer",
          isinstance(got, int) and got >= 0, repr(got))

# A queue LIST of "values": null (as opposed to one queue's issueCount being
# null, covered above) fed straight into a `for` loop two lines below where
# it's read.
o = object.__new__(O)
o.config = {"projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "D",
           "alertQueues": ["Q"], "alertQueuesAsked": True}
o._jira_url = lambda p="": "https://x" + p
o._session = type("S", (), {"get": staticmethod(
    lambda url, **kw: _QR({"values": None}))})()
o.queues = []
o._all_queue_names = []
o.new_issues = []
o._reconciled_counts = {}
o._alert_count = 0
o._last_full_fetch = 9e18
o._alert_queues_missing = []
o._post = lambda fn: None
o._fetch_completion_stats = lambda c: None
try:
    O._fetch_data(o)
    ok = True
except Exception as exc:                                  # noqa: BLE001
    ok = False
    err = f"{type(exc).__name__}: {exc}"
check("a queue list of \"values\": null doesn't crash the fetch",
      ok, "" if ok else err)
if ok:
    check("  and yields an empty queue list rather than stale data",
          o.queues == [], o.queues)

# The same "values": null shape, found by inspection in two more places
# that read a Jira JSON response with a bare .get(key, []) rather than
# `or []`: service-desk discovery, and the per-queue issue reconciliation
# fetch inside _fetch_data.
d = object.__new__(O)
d._session = type("S", (), {"get": staticmethod(
    lambda url, **kw: _QR({"values": None}))})()
d._jira_url = lambda p="": "https://x" + p
try:
    result = O._discover_service_desk(d)
    ok = True
except Exception as exc:                                  # noqa: BLE001
    ok = False
    err = f"{type(exc).__name__}: {exc}"
check('service-desk discovery survives a "values": null response',
      ok and result is None, "" if ok else err)

o2 = object.__new__(O)
o2.config = {"projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "D",
            "alertQueues": ["Q"], "alertQueuesAsked": True}
o2._jira_url = lambda p="": "https://x" + p


def _null_issue_page(url, **kw):
    return _QR({"values": None} if "/queue/1/issue" in url
               else {"values": [{"name": "Q", "id": 1, "issueCount": 5}]})


o2._session = type("S", (), {"get": staticmethod(_null_issue_page)})()
o2.queues = []
o2._all_queue_names = []
o2.new_issues = []
o2._reconciled_counts = {}
o2._alert_count = 0
o2._last_full_fetch = 9e18
o2._alert_queues_missing = []
o2._post = lambda fn: None
o2._fetch_completion_stats = lambda c: None
try:
    O._fetch_data(o2)
    ok = True
except Exception as exc:                                  # noqa: BLE001
    ok = False
    err = f"{type(exc).__name__}: {exc}"
check('a per-queue issue page of "values": null doesn\'t crash reconciliation',
      ok, "" if ok else err)

# _count_issues (completed-today / SLA) already had a try/except around
# this line, but only for ValueError/AttributeError - len(None) raises
# TypeError, which slipped straight through it.
class _QR404:
    ok = False
    status_code = 404

    def json(self):
        return {}


def _count_post(url, **kw):
    return _QR404() if "approximate-count" in url else _QR({"issues": None})


count_holder = object.__new__(O)
count_holder._session = type("S", (),
                             {"post": staticmethod(_count_post)})()
count_holder._jira_url = lambda p="": "https://x" + p
count_holder._approx_count_ok = True
try:
    n, rejected = O._count_issues(count_holder, "project = SD")
    ok = True
except Exception as exc:                                  # noqa: BLE001
    ok = False
    err = f"{type(exc).__name__}: {exc}"
check('_count_issues survives an "issues": null search response',
      ok and n == 0, "" if ok else err)

# And the scheduling itself must survive any repaint failure at all.
import queue as _queue                                 # noqa: E402
import tkinter as _tk                                  # noqa: E402

_root = _tk.Tk()
_root.withdraw()
sched = object.__new__(O)
sched.root = _root
sched._events = _queue.Queue()
sched._PUMP_MS = 20
sched._settings_changed = False
sched._retry_after_ms = None
sched._conn_error = False
sched._error_count = 0
sched._refresh_job = None
sched._last_full_fetch = 0.0
sched._fetch = lambda: None
sched._update_ui = lambda: (_ for _ in ()).throw(RuntimeError("repaint blew up"))
# Keep the deliberate failure out of the real app's log, where it
# reads like a live fault to anyone checking on the running overlay.
sched._log_error = lambda detail: None
O._post(sched, sched._fetch_finished)
O._pump(sched)
_root.update()
check("a repaint that raises still leaves the next poll scheduled",
      sched._refresh_job is not None, repr(sched._refresh_job))
try:
    _root.after_cancel(sched._refresh_job)
except Exception:
    pass
_root.destroy()

sys.exit(check.done())
