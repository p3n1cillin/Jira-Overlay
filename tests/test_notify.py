"""Notifications and the tray icon.

The toast is built by interpolating ticket text into a PowerShell command,
inside a single-quoted string, inside an XML document. Ticket summaries are
written by customers, so they contain apostrophes, ampersands and angle
brackets as a matter of course - and anything that escapes that string would
be running arbitrary PowerShell.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_overlay          # noqa: E402

ov = load_overlay()
check = Checker("notifications and tray")

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

sys.exit(check.done())
