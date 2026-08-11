"""Overlay behaviour: SLA, the alert-queue picker, startup registration and
the event pump. All of these were reported by a user rather than caught here,
which is why they now have tests.
"""

import os
import queue
import sys
import tkinter as tk
import winreg

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_overlay          # noqa: E402

ov = load_overlay()
O = ov.JiraOverlay
check = Checker("overlay behaviour")


# --- SLA: teams that have SLAs but don't track them -------------------------

def stats(cfg_extra, counts):
    """Run one completion-stats pass; returns (sla, queries issued, config)."""
    o = object.__new__(O)
    o._jira_url = lambda p="": "https://x" + p
    o.config = {"projectKey": "SD", "completedTodayFilter": ""}
    o.config.update(cfg_extra)
    o._sla_unavailable = bool(o.config.get("slaUnavailable", False))
    o._approx_count_ok = True
    seen = []
    it = iter(counts)

    def fake(jql):
        seen.append(jql)
        return next(it)

    o._count_issues = fake
    O._fetch_completion_stats(o, o.config)
    return o.sla_compliance, seen, o.config


sla, q, _ = stats({}, [(10, False), (2, False)])
check("SLA is measured by default", sla == 80 and len(q) == 2)

sla, q, _ = stats({"slaEnabled": False}, [(10, False)])
check("switched off, Jira is not asked about SLA at all",
      sla is None and len(q) == 1)

sla, q, _ = stats({"slaField": ""}, [(10, False)])
check("clearing the field name also turns it off", sla is None and len(q) == 1)

sla, q, cfg = stats({}, [(10, False), (None, True)])
check("a missing SLA field is detected", sla is None and len(q) == 2)
check("  and is NOT written to the config, where a wrong verdict would stick",
      "slaUnavailable" not in cfg)

sla, q, cfg = stats({}, [(10, False), (None, False)])
check("a transient failure does not latch SLA off",
      "slaUnavailable" not in cfg)

_sla, q, _ = stats({"slaEnabled": False}, [(37, False)])
check("completed-today still counts with SLA switched off", len(q) == 1)


# --- Alert queue picker: it must be possible to decline ---------------------

QUEUES = [{"name": "Open", "id": 1, "issueCount": 4},
          {"name": "Closed", "id": 2, "issueCount": 9}]


class Resp:
    ok = True
    status_code = 200

    def json(self):
        return {"values": QUEUES}

    def raise_for_status(self):
        pass


def fetch_cycle(cfg):
    o = object.__new__(O)
    o.config = cfg
    o._jira_url = lambda p="": "https://x" + p
    o._session = type("S", (), {"get": lambda *a, **k: Resp()})()
    o.queues = []
    o._all_queue_names = []
    o.new_issues = []
    o._reconciled_counts = {}
    o._alert_count = 0
    o._last_full_fetch = 9e18
    o._alert_queues_missing = []
    posted = []
    o._post = lambda fn: posted.append(fn)
    o._fetch_completion_stats = lambda c: None
    O._fetch_data(o)
    shown = any(getattr(f, "__name__", "") == "_show_alert_queue_picker"
                for f in posted)
    return shown, len(o.queues)


base = {"projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "D"}
shown, _n = fetch_cycle(dict(base))
check("the queue picker is offered on first run", shown)

shown, n = fetch_cycle(dict(base, alertQueuesAsked=True))
check("closing it without choosing is remembered, not re-asked", not shown)
check("  and queue counts still populate", n == 2)

shown, n = fetch_cycle(dict(base, alertQueues=[], alertQueuesAsked=True))
check("running with no alert queue is allowed", not shown and n == 2)


# --- Display filters must never switch off an alert ------------------------
# hiddenQueues, skipKeywords and maxQueueSize all decide what is worth
# looking at. None of them should be able to silence a notification the user
# asked for - and maxQueueSize only bites once a queue is busy, which is
# exactly when the alert matters.

BUSY = [{"name": "Newly Opened", "id": 1, "issueCount": 150},
        {"name": "Archive Escalations", "id": 2, "issueCount": 5},
        {"name": "Open tasks", "id": 3, "issueCount": 12}]


class BusyResp:
    ok = True
    status_code = 200

    def json(self):
        return {"values": BUSY}

    def raise_for_status(self):
        pass


def alerting(cfg):
    o = object.__new__(O)
    o.config = cfg
    o._jira_url = lambda p="": "https://x" + p
    o._session = type("S", (), {"get": lambda *a, **k: BusyResp()})()
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
    return o._alert_count, o._alert_queues_missing, len(o.queues)


alert_base = {"projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "D",
              "alertQueues": ["Newly Opened"], "alertQueuesAsked": True}

for label, extra, rows in (
        ("with no filters set", {}, 3),
        ("when capped by maxQueueSize", {"maxQueueSize": 100}, 2),
        ("when excluded by skipKeywords", {"skipKeywords": ["NEWLY"]}, 2),
        ("when hidden from display", {"hiddenQueues": ["Newly Opened"]}, 2)):
    count, missing, shown_rows = alerting(dict(alert_base, **extra))
    check(f"a busy alert queue still alerts {label}",
          count == 150 and not missing, f"count={count} missing={missing}")
    check(f"  ...while the display is still filtered {label}",
          shown_rows == rows, f"{shown_rows} rows")

count, missing, _rows = alerting(dict(alert_base, alertQueues=["Nope"]))
check("a queue Jira really doesn't have is still reported",
      missing == ["Nope"], str(missing))


# --- Startup registration covers both apps ---------------------------------

TEST_KEY = r"Software\JiraOverlayTest\Run"
ov.STARTUP_REG = TEST_KEY
winreg.CreateKey(winreg.HKEY_CURRENT_USER, TEST_KEY).Close()


def values():
    out, i = [], 0
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, TEST_KEY) as k:
        while True:
            try:
                out.append(winreg.EnumValue(k, i)[0])
                i += 1
            except OSError:
                return out


ov.set_startup(True)
on, names = ov.startup_enabled(), values()
ov.set_startup(False)
off, left = ov.startup_enabled(), values()
check("the startup switch registers overlay and learner together",
      on and len(names) == 2, str(sorted(names)))
check("  and removes both again", (not off) and not left)
winreg.DeleteKey(winreg.HKEY_CURRENT_USER, TEST_KEY)
winreg.DeleteKey(winreg.HKEY_CURRENT_USER, r"Software\JiraOverlayTest")


# --- Event pump and poll scheduling ----------------------------------------

root = tk.Tk()
root.withdraw()
o = object.__new__(O)
o._events = queue.Queue()
o.root = root
o._PUMP_MS = 40
ran = []
O._post(o, lambda: (_ for _ in ()).throw(tk.TclError("transient")))
O._post(o, lambda: ran.append("kept going"))
O._pump(o)
check("one transient TclError doesn't stop the pump for good",
      ran == ["kept going"])

scheduled, cancelled = [], []
real_after, real_cancel = root.after, root.after_cancel


class Spy:
    def __getattr__(self, n):
        return getattr(root, n)

    def after(self, ms, fn=None, *a):
        jid = real_after(ms, fn, *a)
        scheduled.append(jid)
        return jid

    def after_cancel(self, jid):
        cancelled.append(jid)
        return real_cancel(jid)


o2 = object.__new__(O)
o2.root = Spy()
o2._events = queue.Queue()
o2._settings_changed = False
o2._retry_after_ms = None
o2._conn_error = False
o2._error_count = 0
o2._refresh_job = None
o2._last_full_fetch = 0.0
o2._update_ui = lambda: None
o2._fetch = lambda: None
for _ in range(3):
    O._fetch_finished(o2)
live = [j for j in scheduled if j not in cancelled]
check("finishing a fetch replaces the poll timer instead of duplicating it",
      len(live) == 1, f"{len(live)} timers live")
for j in live:
    try:
        real_cancel(j)
    except Exception:
        pass
# --- Opening an exported file: os.startfile succeeds silently when there is
# no association, so "Export CSV" on a machine with no spreadsheet app would
# save the file and then appear to do nothing when it tries to open it.
check("an extension that plainly doesn't exist has no association",
      ov._has_association(".fake9182nope") is False)
check("a real, universally-associated extension is detected as having one",
      ov._has_association(".txt") is True)


def _procs_named(name: str) -> set:
    import subprocess as sp
    out = sp.check_output(["tasklist", "/fi", f"imagename eq {name}",
                           "/fo", "csv"], text=True)
    return {ln for ln in out.splitlines() if name.lower() in ln.lower()}


import tempfile
target = os.path.join(tempfile.mkdtemp(), "export.fake9182nope")
open(target, "w", encoding="utf-8").write("test export content\n")
before = _procs_named("notepad.exe")
ov.open_in_default_or_notepad(target)
import time
time.sleep(1.5)
after = _procs_named("notepad.exe")
check("an unassociated extension falls back to Notepad, not silence",
      len(after) > len(before), f"before={len(before)} after={len(after)}")
import subprocess
subprocess.run(["taskkill", "/f", "/im", "notepad.exe"], capture_output=True)

root.destroy()

sys.exit(check.done())
