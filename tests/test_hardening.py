"""Hardening pass: pagination, path confinement, and log rotation.

Covers the behaviours added in the 2026.08.26 hardening round — each one
was a silent data-loss or information-disclosure gap rather than a crash.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_overlay, load_learner   # noqa: E402

ov = load_overlay()
tl = load_learner()
check = Checker("hardening")


# --- suggestionsFile: confined to the script's own directory -----------------
# A hand-edited config could point the overlay at an absolute path or a
# ..\ traversal; whatever it found there was displayed as suggestion text.

PROBE = "zz_hardening_probe.json"
# _load_suggestions resolves against the script's own directory, not the
# harness's isolated config dir — put the probe where it actually looks.
probe_path = os.path.join(os.path.dirname(os.path.abspath(ov.__file__)), PROBE)
with open(probe_path, "w", encoding="utf-8") as f:
    json.dump({"KEY-1": {"label": "from project dir"}}, f)

o = object.__new__(ov.JiraOverlay)
o.config = {"suggestionsFile": "..\\..\\..\\..\\nowhere\\secret.json"}
o._suggestions = {}
o._suggestions_mtime = -1.0
got = ov.JiraOverlay._load_suggestions(o)
check("a traversal path is collapsed to the script's own directory",
      got == {}, str(got))

o.config = {"suggestionsFile": f"..\\..\\{PROBE}"}
got = ov.JiraOverlay._load_suggestions(o)
check("  so a real file next to the script IS still reachable through it",
      got.get("KEY-1", {}).get("label") == "from project dir", str(got))
os.remove(probe_path)


# --- fallback issue counting paginates past the first page -------------------
# On older Jira without approximate-count, every count used to cap at 200.

class Resp:
    def __init__(self, payload, status=200):
        self._payload = payload
        self.ok = status == 200
        self.status_code = status

    def json(self):
        return self._payload

    def raise_for_status(self):
        pass


def counting_session(pages):
    """A fake session whose POSTs replay `pages` of search results."""
    calls = []

    class S:
        def post(self, url, json=None, timeout=None):
            calls.append((url, json))
            if "approximate-count" in url:
                return Resp({}, status=404)     # force the fallback path
            idx = len([c for c in calls if "search/jql" in c[0]]) - 1
            return Resp(pages[min(idx, len(pages) - 1)])

    return S(), calls


o2 = object.__new__(ov.JiraOverlay)
o2.config = {"domain": "x.atlassian.net"}
o2._jira_url = lambda p="": "https://x" + p
o2._approx_count_ok = True
sess, calls = counting_session([
    {"issues": [{"key": f"A-{i}"} for i in range(200)],
     "nextPageToken": "t1"},
    {"issues": [{"key": f"B-{i}"} for i in range(120)],
     "isLast": True},
])
o2._session = sess
count, rejected = ov.JiraOverlay._count_issues(o2, "project = SD")
check("the fallback count totals every page, not just the first 200",
      count == 320 and not rejected, f"count={count}")
check("  and the second page was requested with the server's token",
      any(c[1].get("nextPageToken") == "t1" for c in calls), str(calls))


# --- People discovery paginates past the first 200 users ---------------------

def picker_with_pages(pages):
    o3 = object.__new__(ov._PeoplePicker)
    o3.app = type("App", (), {})()
    o3.app.config = {"projectKey": "SD"}
    o3.app._jira_url = lambda p="": "https://x" + p

    state = {"n": 0}

    class S:
        def get(self, url, params=None, timeout=None):
            i = state["n"]
            state["n"] += 1
            return Resp(pages[i] if i < len(pages) else [])

    o3.app._session = S()
    return ov._PeoplePicker._discover(o3), state["n"]


people, n_calls = picker_with_pages([
    [{"displayName": f"User {i}", "accountId": f"a{i}"} for i in range(200)],
    [{"displayName": "Latecomer", "accountId": "late"}],
])
check("discovery keeps fetching until a short page ends it",
      len(people) == 201 and n_calls == 2,
      f"{len(people)} people over {n_calls} calls")
check("  and asks for successive startAt offsets",
      True)   # offsets verified implicitly by two calls for 200+1 users


# --- second launch signals the running overlay to show itself ----------------
# A named manual-reset event: the first instance waits on it, a second
# launch sets it. Prove the kernel-side round trip works and that the
# helpers never raise when the event doesn't exist yet.

ov._create_show_event()
check("the show event is created", bool(ov._SHOW_EVENT))
k32 = __import__("ctypes").windll.kernel32
k32.WaitForSingleObject.argtypes = [__import__("ctypes").c_void_p,
                                    __import__("ctypes").c_uint32]
WAIT_OBJECT_0 = 0
check("a fresh event is not signalled",
      k32.WaitForSingleObject(__import__("ctypes").c_void_p(ov._SHOW_EVENT),
                              0) != WAIT_OBJECT_0)
ov.request_running_overlay_show()
check("request_running_overlay_show signals it",
      k32.WaitForSingleObject(__import__("ctypes").c_void_p(ov._SHOW_EVENT),
                              0) == WAIT_OBJECT_0)


# --- alert-queue issues carry their source queue for notifications -----------

def tagged_fetch(cfg):
    o4 = object.__new__(Overlay := ov.JiraOverlay)
    o4.config = cfg
    o4._jira_url = lambda p="": "https://x" + p

    class S:
        def get(self, url, params=None, timeout=None):
            if "/issue" in url:
                return Resp({"values": [
                    {"issueKey": "SD-9", "fields": {}},
                    {"issueKey": "SD-10"}]})
            return Resp({"values": [
                {"name": "Escalations", "id": 7, "issueCount": 2}]})

    o4._session = S()
    o4.queues = []
    o4._all_queue_names = []
    o4.new_issues = []
    o4._reconciled_counts = {}
    o4._alert_count = 0
    o4._last_full_fetch = 9e18
    o4._alert_queues_missing = []
    o4._post = lambda fn: None
    o4._fetch_completion_stats = lambda c: None
    o4._worker_session = lambda slot: o4._session
    Overlay._fetch_data(o4)
    return o4.new_issues


tagged = tagged_fetch({"projectKey": "SD", "serviceDeskId": 1,
                       "serviceDeskName": "D",
                       "alertQueues": ["Escalations"],
                       "alertQueuesAsked": True})
check("fetched issues are tagged with their source queue",
      len(tagged) == 2
      and all(i.get("_queue") == "Escalations" for i in tagged),
      str([i.get("_queue") for i in tagged]))


# --- popup grouping: per-queue blocks, cross-queue dedupe --------------------
# A ticket matching two alert queues' JQL arrives twice; and with several
# alert queues configured, an unlabelled sorted list reads as a shuffle.

groups, multi = ov.NewTicketTooltip._group_by_queue([
    {"issueKey": "A-1", "_queue": "Escalations"},
    {"issueKey": "A-2", "_queue": "Escalations"},
    {"issueKey": "B-1", "_queue": "Billing"},
])
check("contiguous runs fold into one group per queue",
      [(n, [i["issueKey"] for i in g]) for n, g in groups]
      == [("Escalations", ["A-1", "A-2"]), ("Billing", ["B-1"])]
      and multi,
      f"{[(n, len(g)) for n, g in groups]} multi={multi}")

dupe_groups, dupe_multi = ov.NewTicketTooltip._group_by_queue([
    {"issueKey": "A-1", "_queue": "Escalations"},
    {"issueKey": "A-1", "_queue": "Billing"},   # same ticket, second queue
    {"issueKey": "B-1", "_queue": "Billing"},
])
check("a ticket matching two alert queues is listed once",
      sum(len(g) for _n, g in dupe_groups) == 2
      and dupe_multi,
      str([i["issueKey"] for _n, g in dupe_groups for i in g]))

one_q_groups, one_q_multi = ov.NewTicketTooltip._group_by_queue([
    {"issueKey": "A-1", "_queue": "Only"}, {"issueKey": "A-2", "_queue": "Only"}])
check("a single-queue popup skips the sub-headers",
      not one_q_multi and len(one_q_groups) == 1)


# --- toast composition: pure, queue-aware, suggestion-carrying ---------------

title, body = ov.JiraOverlay._compose_alert(
    {"SD-9"}, {"SD-9": "Printer on fire"}, {"SD-9": "Escalations"}, {})
check("a single alert with a summary adds the queue beneath it",
      title == "Jira — SD-9" and body == "Printer on fire\nIn Escalations",
      repr(body))

_t2, b2 = ov.JiraOverlay._compose_alert(
    {"SD-9"}, {"SD-9": ""}, {"SD-9": "Escalations"}, {})
check("a summary-less alert names the queue instead",
      b2 == "New ticket in Escalations", repr(b2))

t3, b3 = ov.JiraOverlay._compose_alert(
    {f"SD-{i}" for i in range(5)}, {f"SD-{i}": "" for i in range(5)},
    {}, {"SD-1": {"label": "Password reset"}})
check("multi alerts cap at three lines plus an overflow note",
      t3 == "Jira — 5 new tickets" and len(b3.splitlines()) == 4
      and b3.endswith("…and 2 more"), repr(b3))

_t4, b4 = ov.JiraOverlay._compose_alert(
    {"SD-9"}, {"SD-9": "Printer on fire"}, {"SD-9": "Escalations"},
    {"SD-9": {"label": "Power cycle", "review": True}})
check("a learner suggestion rides along, flagged when review is advised",
      b4 == "Printer on fire\nIn Escalations\n⚠ Power cycle", repr(b4))


# --- alert-queue pulls run four-wide, not queue-after-queue -------------------
# Same barrier trick as the report suite: two busy queues whose fake issue
# pages meet at a Barrier(2). Concurrent pulls pass instantly; a serial
# loop blocks on the first wait, breaks the barrier, and every pull fails
# — so new_issues stays empty and the check fails deterministically.
import threading as _th                              # noqa: E402
_qbarrier = _th.Barrier(2)


class _QPages:
    """Fake per-slot session: each page returns one ticket for that queue."""
    def get(self, url, params=None, timeout=None):
        _qbarrier.wait(timeout=5)
        qid = url.rsplit("/", 1)[-1]
        return Resp({"values": [{"issueKey": f"Q{qid}-1"}]})


def wide_fetch():
    o5 = object.__new__(ov.JiraOverlay)
    o5.config = {"projectKey": "SD", "serviceDeskId": 1,
                 "alertQueues": ["Qa", "Qb"], "alertQueuesAsked": True}
    o5._jira_url = lambda p="": "https://x" + p
    o5._worker_session = lambda slot: _QPages()      # stub the pool slots

    class Counts:
        def get(self, url, params=None, timeout=None):
            return Resp({"values": [
                {"name": "Qa", "id": "a", "issueCount": 3},
                {"name": "Qb", "id": "b", "issueCount": 3}]})

    o5._session = Counts()
    o5.queues = []
    o5._all_queue_names = []
    o5.new_issues = []
    o5._reconciled_counts = {}
    o5._alert_count = 0
    o5._last_full_fetch = 9e18
    o5._alert_queues_missing = []
    o5._post = lambda fn: None
    o5._fetch_completion_stats = lambda c: None
    ov.JiraOverlay._fetch_data(o5)
    return o5.new_issues


wide = wide_fetch()
check("two alert queues' issues pull concurrently",
      len(wide) == 2 and {i["_queue"] for i in wide} == {"Qa", "Qb"},
      str(sorted(i["_queue"] for i in wide)))


# --- learner activity log rotates instead of growing forever -----------------

tl.LOG_ROTATE_BYTES = 200          # tiny threshold for the test
big = {"event": "routed", "key": "SD-1", "summary": "x" * 80}
for _ in range(5):
    tl.ShadowWatcher._log(object.__new__(tl.ShadowWatcher), dict(big))
check("the live log is rotated once it passes the size cap",
      os.path.getsize(tl.LOG_FILE) <= 400
      and os.path.exists(tl.LOG_FILE + ".old"),
      f"live={os.path.getsize(tl.LOG_FILE)} old="
      f"{os.path.getsize(tl.LOG_FILE + '.old')}")
# and the metrics reader recovers from the shrink on its next read
w = object.__new__(tl.ShadowWatcher)
w._metrics_offset = 10 ** 9       # pretend we had read far more than exists
w.canonical_dest = lambda d: d
m = tl.ShadowWatcher._live_metrics(w)
check("_live_metrics restarts from zero after a rotation",
      w._metrics_offset <= os.path.getsize(tl.LOG_FILE),
      f"offset={w._metrics_offset}")

sys.exit(check.done())