"""Every control in the UI: is it wired to something, and does using it work?

A smoke test rather than a behavioural one. It catches the class of mistake
that is invisible until somebody clicks - a binding pointing at a renamed
method, a handler expecting an argument it no longer gets - which is exactly
what does not show up in a review.
"""

import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, load_overlay          # noqa: E402

check = Checker("UI wiring")
ov = load_overlay()

opened, saved = [], []
ov.save_config = lambda cfg: saved.append(1)
ov.set_startup = lambda enable: None
ov.startup_enabled = lambda: False
ov.webbrowser = type("W", (), {"open": staticmethod(opened.append)})()
ov.messagebox = type("M", (), {
    "askyesno": staticmethod(lambda *a, **k: False),
    "showerror": staticmethod(lambda *a, **k: None),
    "showinfo": staticmethod(lambda *a, **k: None)})()
ov.JiraOverlay._fetch = lambda self: None
ov.JiraOverlay._start_tray = lambda self: None

app = ov.JiraOverlay({
    "domain": "e.atlassian.net", "email": "a@b.c", "token": "t",
    "serviceDeskName": "Desk", "projectKey": "SD", "serviceDeskId": 1,
    "alertQueues": ["Open"], "alertQueuesAsked": True, "overlayWidth": 260,
    "alpha": 1.0, "alwaysVisible": True, "snapCorner": "bottom-right"})
app.queues = [{"name": n, "id": i, "issueCount": i * 3}
              for i, n in enumerate(["Open", "In Progress"], 1)]
app._all_queue_names = [q["name"] for q in app.queues]
app.completed_today = 5
app.sla_compliance = 98
app._update_ui()
app._do_show()
app.root.update()

# Quit really does quit - its command was bound to root.destroy when the
# window was built, so stubbing root.destroy afterwards doesn't stop it.
# Clicking it mid-sweep tears down the app and every later click then fails
# against dead widgets, which reads exactly like a bug and isn't one.
QUIT_GLYPH = ov._icon("close")


def descendants(widget, out):
    out.append(widget)
    for child in widget.winfo_children():
        descendants(child, out)


def sweep(root_widget, label):
    widgets = []
    descendants(root_widget, widgets)
    used = failures = 0
    for w in widgets:
        try:
            text = str(w.cget("text"))
        except Exception:                           # noqa: BLE001
            text = ""
        if text == QUIT_GLYPH:
            continue
        cls = w.winfo_class()
        try:
            if cls in ("Button", "Checkbutton", "Radiobutton"):
                kind = "command"
            elif w.bind("<ButtonRelease-1>"):
                kind = "click"
            else:
                continue
        except Exception:                           # noqa: BLE001
            continue
        used += 1
        try:
            if kind == "command":
                w.invoke()
            else:
                w.event_generate("<ButtonRelease-1>", x=3, y=3)
            app.root.update()
        except Exception as exc:                    # noqa: BLE001
            failures += 1
            check(f"{label}: {text[:22]!r} works", False,
                  f"{type(exc).__name__}: {exc}")
        if not app.root.winfo_exists():
            check(f"{label}: {text[:22]!r} did not tear down the app", False)
            return failures, used
    check(f"every control in the {label} works", failures == 0,
          f"{used} exercised")
    return failures, used


total, count = sweep(app.frame, "compact overlay")
check("the header opens the service desk in a browser", bool(opened))

app._toggle_settings()
app.root.update()
time.sleep(0.6)
app.root.update()
failures, used = sweep(app.settings_panel, "settings panel")
total += failures
count += used

check("a useful number of controls was covered", count >= 30, f"{count}")
check("the app is still running afterwards", bool(app.root.winfo_exists()))
app.root.destroy()

sys.exit(check.done())
