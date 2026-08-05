"""Configuration writing.

This file holds an API token and a service desk id that nobody can
reconstruct from memory, and two separate processes write to it. Every case
below is one that has actually gone wrong.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_overlay          # noqa: E402

ov = load_overlay()
check = Checker("config writing")

FULL = {"domain": "acme.atlassian.net", "email": "a@b.c", "token": "SECRET",
        "serviceDeskId": 11, "serviceDeskName": "Desk", "projectKey": "SD",
        "alertQueues": ["Open"], "lastX": 10, "lastY": 20,
        "completedTodayFilter": 'assignee in ("Sam")',
        "triageStatuses": ["With DEV"]}


def write(d):
    json.dump(d, open(ov.CONFIG_FILE, "w"))


def read():
    return json.load(open(ov.CONFIG_FILE))


# The learner writes settings it discovers into the same file. The overlay
# holds its own copy from startup, so a blind dump deletes them - dragging
# the window was once enough to wipe the learner's whole workflow.
write(dict(FULL))
ov.save_config({k: v for k, v in FULL.items() if k != "triageStatuses"})
check("another tool's settings survive an overlay save",
      read().get("triageStatuses") == ["With DEV"])

# ...but the overlay must still be able to drop its own keys, which is how
# lastX/lastY disappear when the window snaps to a corner.
write(dict(FULL))
snapped = {k: v for k, v in FULL.items() if k not in ("lastX", "lastY")}
snapped["snapCorner"] = "top-left"
ov.save_config(snapped)
after = read()
check("a full save still removes keys left out of it",
      "lastX" not in after and after.get("snapCorner") == "top-left")

# A caller passing an incomplete dict must not be able to destroy the
# install. This happened: a test stub of two keys wiped a live config.
write(dict(FULL))
ov.save_config({"projectKey": "SD", "completedTodayFilter": ""})
after = read()
check("a partial save keeps the domain", after.get("domain") == "acme.atlassian.net")
check("a partial save keeps the token",  after.get("token") == "SECRET")
check("a partial save keeps the desk id", after.get("serviceDeskId") == 11)

# Replacing them deliberately still works.
write(dict(FULL))
ov.save_config(dict(FULL, domain="new.atlassian.net", token="NEW"))
after = read()
check("credentials can still be replaced when supplied",
      after["domain"] == "new.atlassian.net" and after["token"] == "NEW")

# The previous version is kept, because a bad write is only ever noticed
# afterwards.
write(dict(FULL))
ov.save_config(dict(FULL, completedTodayFilter=""))
check("the previous config is recoverable from .bak",
      json.load(open(ov.CONFIG_FILE + ".bak"))["completedTodayFilter"]
      == 'assignee in ("Sam")')

os.remove(ov.CONFIG_FILE)
os.remove(ov.CONFIG_FILE + ".bak")
ov.save_config(dict(FULL))
check("first save with no existing file works", os.path.exists(ov.CONFIG_FILE))
check("  and writes no spurious backup",
      not os.path.exists(ov.CONFIG_FILE + ".bak"))

# A corrupt file must not stop the app saving.
open(ov.CONFIG_FILE, "w").write("{not json")
ov.save_config(dict(FULL))
check("a corrupt config is replaced rather than fatal",
      read().get("domain") == "acme.atlassian.net")

# Opening Settings and saving without touching anything must be a no-op.
# Every setting passes through a Tk variable and back, so a list read as a
# string, or a number read as text, would quietly rewrite the file.
import time                                          # noqa: E402

ROUND_TRIP = {
    "domain": "acme.atlassian.net", "email": "a@b.c", "token": "SECRET",
    "serviceDeskId": 11, "serviceDeskName": "Desk", "projectKey": "SD",
    "alertQueues": ["Open", "Escalated"], "alertQueue": "Open",
    "trayBadgeQueue": "Open", "hiddenQueues": ["Archive"],
    "skipKeywords": ["test", "spam"], "maxQueueSize": 500,
    "refreshSeconds": 15, "notificationsEnabled": True, "soundEnabled": False,
    "alpha": 0.95, "overlayWidth": 300, "alwaysVisible": False,
    "slaField": "Time to resolution", "slaEnabled": True,
    "completedTodayFilter": 'assignee in ("Sam Carter")',
    "lastX": 100, "lastY": 200,
    "reportUsers": [{"name": "Sam Carter", "accountId": "a1"}],
    "reportRange": "7 days", "reportBasis": "updated",
    # keys belonging to the learner, which the overlay must not disturb
    "triageStatuses": ["With DEV"], "movedLabels": {"BUGS": "Product bug"},
}
write(dict(ROUND_TRIP))
ov.set_startup = lambda enable: None
ov.startup_enabled = lambda: False
ov.JiraOverlay._fetch = lambda self: None
ov.JiraOverlay._start_tray = lambda self: None
app = ov.JiraOverlay(dict(ROUND_TRIP))
app.queues = [{"name": n, "id": i, "issueCount": 1}
              for i, n in enumerate(["Open", "Escalated", "Archive"], 1)]
app._all_queue_names = [q["name"] for q in app.queues]
app.completed_today = 1
app.sla_compliance = 100
app._update_ui()
app._do_show()
app._toggle_settings()
app.root.update()
time.sleep(0.6)
app.root.update()

widgets = []


def walk(w):
    widgets.append(w)
    for child in w.winfo_children():
        walk(child)


walk(app.settings_panel)
[b for b in widgets
 if b.winfo_class() == "Button" and "Save" in str(b.cget("text"))][0].invoke()
app.root.update()

after = read()
drift = sorted(k for k in set(ROUND_TRIP) | set(after)
               if ROUND_TRIP.get(k, "<absent>") != after.get(k, "<absent>"))
check("saving Settings unchanged rewrites nothing", not drift, str(drift))
app.root.destroy()

sys.exit(check.done())
