"""Configuration writing.

This file holds an API token and a service desk id that nobody can
reconstruct from memory, and two separate processes write to it. Every case
below is one that has actually gone wrong.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_learner, load_overlay   # noqa: E402

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
check("a partial save keeps the token",
      "token" not in after and
      ov.unprotect_secret(after.get("tokenProtected", "")) == "SECRET",
      after.get("tokenProtected"))
check("a partial save keeps the desk id", after.get("serviceDeskId") == 11)

# Replacing them deliberately still works.
write(dict(FULL))
ov.save_config(dict(FULL, domain="new.atlassian.net", token="NEW"))
after = read()
check("credentials can still be replaced when supplied",
      after["domain"] == "new.atlassian.net" and "token" not in after
      and ov.unprotect_secret(after.get("tokenProtected", "")) == "NEW")

# Explicitly clearing it (as opposed to just not mentioning it) must clear
# it, not leave a stale encrypted value behind claiming to still be current.
write(dict(FULL))
ov.save_config(dict(FULL, token=""))
after = read()
check("explicitly clearing the token removes it, not just the plain-text copy",
      "token" not in after and "tokenProtected" not in after, after)

# --- load_config: decrypt on the way in, migrate a legacy file on sight ----
write(dict(FULL))                                  # a legacy plain-text file
loaded = ov.load_config()
check("load_config decrypts a legacy plain-text token straight through",
      loaded.get("token") == "SECRET")
check("  and migrates it on disk immediately, not on the next unrelated save",
      "token" not in read() and
      ov.unprotect_secret(read().get("tokenProtected", "")) == "SECRET")

# Loading it again must round-trip through the now-encrypted form too.
loaded2 = ov.load_config()
check("a config already migrated still decrypts correctly on the next load",
      loaded2.get("token") == "SECRET" and "tokenProtected" not in loaded2)

# A blob that can't be decrypted here - moved from another machine or user,
# or simply corrupted - must fail closed to an empty token, not raise
# load_config out to main() with the app unable to start at all.
tampered = dict(FULL)
del tampered["token"]
tampered["tokenProtected"] = "not-a-real-dpapi-blob"
write(tampered)
loaded3 = ov.load_config()
check("an undecryptable token yields an empty one, not a crash",
      loaded3 is not None and loaded3.get("token") == "")

# The learner reads the same file and must decrypt what the overlay wrote.
write(dict(FULL))
ov.save_config(dict(FULL))          # migrates SECRET to an encrypted blob
tl = load_learner(isolate_files=False)
tl_cfg = json.load(open(ov.CONFIG_FILE))
check("the learner can decrypt a token the overlay encrypted",
      tl.unprotect_secret(tl_cfg.get("tokenProtected", "")) == "SECRET")

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
# The token's on-disk shape changes on its first save after migration (plain
# text -> encrypted) even though the value itself hasn't; that's the point
# of it, not drift, so it's checked separately from everything else.
drift = sorted(k for k in set(ROUND_TRIP) | set(after)
               if k not in ("token", "tokenProtected")
               and ROUND_TRIP.get(k, "<absent>") != after.get(k, "<absent>"))
check("saving Settings unchanged rewrites nothing", not drift, str(drift))
check("  and the token itself still round-trips to the same value",
      ov.unprotect_secret(after.get("tokenProtected", "")) == ROUND_TRIP["token"])

# --- Learner health/drift renders in Settings without crashing -------------
# Built from a file a different, optional process writes, so nothing about
# its shape can be trusted - this actually builds the panel rather than
# just calling the loader, to catch a widget-construction bug the loader
# tests alone wouldn't.
app._load_learner_status = lambda: {
    "healthy": False, "refusals": [401, 403],
    "provenance": {"trainedOnDecisions": 500, "lastTuneDate": "2026-08-01"},
    "accuracy": {"pct": 91.2},
    "drift": {"date": "2026-08-01",
             "detail": "balanced accuracy dropped 20% (90% -> 70%)"},
}
app._toggle_settings()          # closed after Save above; reopen with new data
app.root.update()
time.sleep(0.3)
app.root.update()

widgets = []
walk(app.settings_panel)
texts = [str(w.cget("text")) for w in widgets
        if "text" in w.keys() and w.winfo_class() == "Label"]
check("an unhealthy/drifting learner renders without raising",
      any("refusing requests" in t for t in texts), texts[:3])
check("  and the drift detail is shown",
      any("balanced accuracy dropped 20%" in t for t in texts))

# A status.json shaped in ways a hand-edit, a corrupted write, or a future
# schema change could produce - provenance/accuracy/refusals/drift present
# but the wrong TYPE, not just missing. `x or {}` only guards against a
# falsy value, not a wrong-type truthy one (a non-empty string is truthy),
# and provenance.get(...) on a string blew up the whole panel.
MALFORMED_LEARNER_STATUS = [
    ("missing keys entirely", {}),
    ("healthy not a bool", {"healthy": "yes"}),
    ("provenance is a string", {"healthy": True, "provenance": "nope"}),
    ("provenance is a list", {"healthy": True, "provenance": [1, 2, 3]}),
    ("accuracy is a string", {"healthy": True, "accuracy": "nope"}),
    ("accuracy.pct is a string", {"healthy": True, "accuracy": {"pct": "high"}}),
    ("refusals is a string, not a list", {"healthy": False, "refusals": "oops"}),
    ("drift is a string, not a dict", {"healthy": True, "drift": "weird"}),
]
for label, data in MALFORMED_LEARNER_STATUS:
    app._load_learner_status = lambda d=data: d
    try:
        app._build_settings_panel()
        ok = True
    except Exception as exc:                            # noqa: BLE001
        ok = False
        err = f"{type(exc).__name__}: {exc}"
    check(f"a status file with {label} doesn't crash Settings",
          ok, "" if ok else err)

app.root.destroy()


# --- A hand-edited config must not stop the app starting -------------------
# This file is meant to be editable by hand and is also rewritten by the
# learner, so a value can be null or the wrong type without the JSON being
# malformed enough for json.load to object. dict.get(key, default) only
# substitutes the default when the key is MISSING, so a present null reached
# arithmetic and crashed startup.
import copy                                            # noqa: E402

BROKEN = {
    "alertQueues null":      {"alertQueues": None},
    "alertQueues a string":  {"alertQueues": "Newly Opened"},
    "hiddenQueues null":     {"hiddenQueues": None},
    "skipKeywords a string": {"skipKeywords": "test"},
    "maxQueueSize null":     {"maxQueueSize": None},
    "maxQueueSize a string": {"maxQueueSize": "500"},
    "alpha null":            {"alpha": None},
    "alpha out of range":    {"alpha": 5.0},
    "overlayWidth null":     {"overlayWidth": None},
    "overlayWidth a string": {"overlayWidth": "260"},
    "overlayWidth zero":     {"overlayWidth": 0},
    "refreshSeconds null":   {"refreshSeconds": None},
    "slaField null":         {"slaField": None},
    "trayBadgeQueue null":   {"trayBadgeQueue": None},
    "reportUsers null":      {"reportUsers": None},
}

for label, extra in BROKEN.items():
    cfg = ov.normalise_config(dict(FULL, **extra))
    key = next(iter(extra))
    value = cfg.get(key)
    if key in ("alertQueues", "hiddenQueues", "skipKeywords", "reportUsers"):
        ok = isinstance(value, list)
    elif key in ("slaField", "trayBadgeQueue"):
        ok = isinstance(value, str)
    elif key == "maxQueueSize":
        # 0 is meaningful here — it's how "no limit" is expressed.
        ok = isinstance(value, int) and value >= 0
    else:
        ok = isinstance(value, (int, float)) and value > 0
    check(f"{label} is made usable", ok, f"{key}={value!r}")

# Crucially, a perfectly good config must come back untouched — normalising
# is there to stop crashes, not to overrule choices. Bounds are the limits of
# what the app can work with, NOT the values the Settings dropdown offers: an
# earlier version clamped refreshSeconds to the dropdown's 30s floor and
# would have quietly halved the poll rate of an install deliberately set to
# 15.
VALID = {
    "domain": "acme.atlassian.net", "email": "a@b.c", "token": "SECRET",
    "serviceDeskId": 11, "serviceDeskName": "Desk", "projectKey": "SD",
    "alertQueues": ["Open"], "trayBadgeQueue": "Open",
    "hiddenQueues": ["Archive"], "skipKeywords": ["test"],
    "maxQueueSize": 500, "refreshSeconds": 15,          # below the dropdown
    "alpha": 0.95, "overlayWidth": 300,
    "slaField": "Time to resolution",
    "completedTodayFilter": 'assignee in ("Sam")',
    "reportUsers": [{"name": "Sam", "accountId": "a1"}],
    "triageStatuses": ["With DEV"],                      # another tool's key
}
kept = ov.normalise_config(copy.deepcopy(VALID))
changed = {k: (VALID.get(k), kept.get(k)) for k in set(VALID) | set(kept)
           if VALID.get(k) != kept.get(k)}
check("a valid config is returned completely unchanged", not changed,
      str(changed))
check("  including a deliberate refreshSeconds below the dropdown's range",
      kept["refreshSeconds"] == 15, repr(kept["refreshSeconds"]))
check("  and another tool's keys", kept["triageStatuses"] == ["With DEV"])

# --- projectKey is a bare JQL identifier, not a quotable value -------------
# Every query this app builds interpolates it as `project = {key}`, and
# there's no quoting syntax for "this is a field/identifier" the way
# jql_quote() quotes a literal value - the only defence is restricting it
# to characters a real Jira project key can ever contain. Config is
# hand-editable text, so this needs validating the same way an unvalidated
# table name reaching SQL would.
for label, key in (("a clause tacked on", 'SD" OR project = "SECRET'),
                   ("a space", "SD OR 1=1"),
                   ("starting with a digit", "1SD")):
    bad = ov.normalise_config({"projectKey": key})
    check(f"a projectKey with {label} is blanked, not trusted",
          bad["projectKey"] == "", repr(bad))
good = ov.normalise_config({"projectKey": "SD"})
check("a genuine projectKey passes through untouched",
      good["projectKey"] == "SD")

sys.exit(check.done())
