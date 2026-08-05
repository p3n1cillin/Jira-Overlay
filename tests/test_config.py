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

sys.exit(check.done())
