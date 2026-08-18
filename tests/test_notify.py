"""Notifications and the tray icon.

The toast is built by interpolating ticket text into a PowerShell command,
inside a single-quoted string, inside an XML document. Ticket summaries are
written by customers, so they contain apostrophes, ampersands and angle
brackets as a matter of course - and anything that escapes that string would
be running arbitrary PowerShell.
"""

import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_overlay          # noqa: E402

ov = load_overlay()
check = Checker("notifications and tray")

# A locked-down or PowerShell-Core-only machine can have no "powershell.exe"
# on PATH at all. send_toast runs inline inside _update_ui's repaint, which
# only catches exceptions at the caller — a failure here used to be able to
# skip the tray badge, SLA label and status line for that whole cycle,
# worst of all on the exact cycles a new ticket actually arrived.
real_popen = subprocess.Popen
subprocess.Popen = lambda *a, **k: (_ for _ in ()).throw(
    FileNotFoundError("powershell.exe not found"))
try:
    ov.send_toast("Jira — SD-1", "New ticket in alert queue")
    check("a missing powershell.exe doesn't raise out of send_toast", True)
except Exception as exc:                             # noqa: BLE001
    check("a missing powershell.exe doesn't raise out of send_toast", False,
          f"{type(exc).__name__}: {exc}")
finally:
    subprocess.Popen = real_popen

AWKWARD = [
    ("an apostrophe",        "Bob's laptop won't boot"),
    ("a quote break-out",    "'; Write-Host PWNED; '"),
    ("angle brackets",       "Error <script>alert(1)</script>"),
    ("an ampersand",         "Sales & Marketing share"),
    ("double quotes",        'User said "it is broken"'),
    ("backtick and dollar",  "Cost `$500 and $env:PATH"),
    ("a newline",            "line one\nline two"),
    ("control characters",   "bell\x07 and null\x00 here"),
    ("an emoji",             "Printer jammed \U0001F5A8"),
]

for name, text in AWKWARD:
    escaped = ov._xml_escape(text)
    # A bare ' would close the PowerShell string early; a bare < > or &
    # would break the XML the toast is parsed from.
    leftovers = (
        "'" in escaped
        or "<" in escaped
        or ">" in escaped
        or "&" in (escaped.replace("&amp;", "").replace("&lt;", "")
                          .replace("&gt;", "").replace("&quot;", "")
                          .replace("&apos;", ""))
        or any(ord(c) < 32 and c not in "\t\n\r" for c in escaped)
    )
    check(f"a summary with {name} can't break out", not leftovers, escaped[:48])

# The badge is drawn, not templated, so the risk is a count that doesn't fit.
for count in (0, 1, 9, 10, 99, 100, 999, 1000, 99999):
    try:
        image = ov._make_tray_image(count, count > 0)
        ok = image.size == (128, 128)
    except Exception as exc:                        # noqa: BLE001
        ok = False
        image = f"{type(exc).__name__}: {exc}"
    check(f"tray badge renders for a count of {count}", ok, str(image))

# Being dragged off-screen, or onto a monitor that has since been unplugged,
# must still resolve to somewhere real.
for x, y in ((0, 0), (-5000, -5000), (99999, 99999)):
    rect = ov.get_monitor_rect(x, y, work=True)
    sane = (rect is not None and rect[2] > rect[0] and rect[3] > rect[1])
    check(f"a monitor is found for coordinates ({x}, {y})", sane, str(rect))


# --- Suggestions file, written by the learner ------------------------------
# A separate process writes this, so nothing in it can be taken on trust.
import json                                          # noqa: E402
import tempfile                                      # noqa: E402
import time                                          # noqa: E402

_dir = tempfile.mkdtemp()
_path = os.path.join(_dir, "suggestions.json")


def _reader():
    o = object.__new__(ov.JiraOverlay)
    o.config = {"suggestionsFile": _path}
    o._suggestions_mtime = 0.0
    o._suggestions = {}
    return o


for label, content in (("a valid object", {"SD-1": {"label": "With DEV"}}),
                       ("a list", ["nope"]),
                       ("a bare null", "null"),
                       ("truncated json", '{"SD-1": {"label": "With'),
                       ("an empty object", {})):
    if isinstance(content, str):
        open(_path, "w", encoding="utf-8").write(content)
    else:
        json.dump(content, open(_path, "w"))
    loaded = ov.JiraOverlay._load_suggestions(_reader())
    check(f"{label} yields a dict the callers can use",
          isinstance(loaded, dict), type(loaded).__name__)

# The mtime shortcut must return what was parsed, not whatever the field
# happened to hold - it previously relied on the one caller assigning back.
json.dump({"SD-1": {"label": "With DEV"}}, open(_path, "w"))
reader = _reader()
first = ov.JiraOverlay._load_suggestions(reader)
second = ov.JiraOverlay._load_suggestions(reader)
check("a second read returns the same suggestions", first == second,
      f"{first} then {second}")

# Suggestions from a learner that has since stopped must not be shown.
old = time.time() - 3600
os.utime(_path, (old, old))
check("suggestions older than the staleness window are dropped",
      ov.JiraOverlay._load_suggestions(_reader()) == {})


# --- One empty reading must not re-notify the whole queue ------------------
# JSM's cached issueCount decides whether a queue is fetched at all, so a
# single stale zero for a queue that really has tickets emptied new_issues
# with no fetch failure involved. The seen-set was then replaced outright,
# and every ticket re-notified on the next poll. Keys now survive one absent
# poll before being forgotten.
O = ov.JiraOverlay
TICKETS = [{"key": f"SD-{i}", "fields": {"summary": f"t{i}"}} for i in range(1, 6)]


class _R:
    ok = True
    status_code = 200

    def __init__(self, payload):
        self._p = payload

    def json(self):
        return self._p

    def raise_for_status(self):
        pass


def _cycle(o, cached_count):
    queues = [{"name": "Newly Opened", "id": 1, "issueCount": cached_count}]

    def fake_get(url, **kw):
        return _R({"values": TICKETS if "/queue/1/issue" in url else queues})

    o._session = type("S", (), {"get": staticmethod(fake_get)})()
    O._fetch_data(o)
    # the dedup step _update_ui performs, isolated from Tk
    cur = {k for k in (i.get("issueKey") or i.get("key", "")
                       for i in o.new_issues) if k}
    fresh = cur - o._seen_newly_ids
    vanished = o._seen_newly_ids - cur
    o._seen_newly_ids = cur | (vanished - o._missing_once)
    o._missing_once = vanished
    return fresh


watcher = object.__new__(O)
watcher.config = {"projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "D",
                  "alertQueues": ["Newly Opened"], "alertQueuesAsked": True}
watcher._jira_url = lambda p="": "https://x" + p
watcher.queues = []
watcher._all_queue_names = []
watcher.new_issues = []
watcher._reconciled_counts = {}
watcher._alert_count = 0
watcher._last_full_fetch = 9e18
watcher._alert_queues_missing = []
watcher._post = lambda fn: None
watcher._fetch_completion_stats = lambda c: None
watcher._seen_newly_ids = set()
watcher._missing_once = set()

first = _cycle(watcher, 5)
check("all five tickets are new on the first poll", len(first) == 5, str(len(first)))
blip = _cycle(watcher, 0)          # stale zero: queue not fetched at all
recovered = _cycle(watcher, 5)     # cache catches up
check("a single stale-zero poll does not re-notify the queue",
      not recovered, f"re-notified {sorted(recovered)}")

# A ticket genuinely gone for two polls is forgotten, so a real reopen still
# notifies rather than being suppressed for ever.
_cycle(watcher, 0)
_cycle(watcher, 0)
reopened = _cycle(watcher, 5)
check("a ticket absent for two polls notifies again if it returns",
      len(reopened) == 5, f"{len(reopened)} notified")

sys.exit(check.done())
