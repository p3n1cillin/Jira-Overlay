"""Update controls stay on Tk's main thread and never replace a Git checkout."""
from pathlib import Path
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
from unittest.mock import patch

from _harness import Checker, load_overlay, load_learner

check = Checker("update UI and writer shutdown")
ov = load_overlay()
ov.JiraOverlay._fetch = lambda self: None
ov.JiraOverlay._start_tray = lambda self: None
ov.startup_enabled = lambda: False
messages, launched = [], []
ov.messagebox = SimpleNamespace(showinfo=lambda *a, **k: messages.append(a),
    showerror=lambda *a, **k: messages.append(a), askyesno=lambda *a, **k: True)
app = ov.JiraOverlay({"domain": "example.atlassian.net", "email": "a@example.invalid", "token": "fake",
    "projectKey": "SD", "serviceDeskId": 1, "serviceDeskName": "Arbitrary desk", "notificationsEnabled": False})

def drain():
    deadline = time.monotonic()+5
    while app._update_busy and time.monotonic() < deadline:
        app.root.update()
        time.sleep(0.01)
    app.root.update()


def fake_popen(args, **kwargs):
    launched.append(args)
    return SimpleNamespace(wait=lambda: 0)

try:
    release = {"version": "2099.01.01", "page": "https://github.com/owner/repo/releases/tag/v2099.01.01"}
    with patch.object(ov.updates, "latest_release", return_value=release):
        app._check_updates()
        drain()
    app._build_settings_panel()
    check("background results populate the update label and enable the install control",
          "2099.01.01" in app._update_status_var.get() and str(app._update_install_button["state"]) == "normal")
    with tempfile.TemporaryDirectory() as folder:
        root = Path(folder)
        opened = []
        def widget_text(widget):
            texts = [str(widget.cget("text"))] if "text" in widget.keys() else []
            return texts + [text for child in widget.winfo_children() for text in widget_text(child)]
        with patch.object(ov, "__file__", str(root / "jira_overlay.pyw")), \
             patch.object(ov.updates, "native_install", return_value=True), \
             patch.object(ov.lifecycle, "recovery_status", return_value={"healthy": False, "message": "Automatic recovery unavailable [JIRA-R101]"}), \
             patch.object(ov.os, "startfile", side_effect=opened.append):
            app._build_settings_panel()
            check("Settings surfaces recovery availability and the local diagnostics control",
                any("JIRA-R101" in text for text in widget_text(app.root)) and "Open local diagnostic logs" in widget_text(app.root))
            app._open_diagnostics()
            check("opening absent diagnostics does not create files", not opened and not list(root.iterdir()))
            logs = ov.lifecycle.folder(root)
            app._open_diagnostics()
            check("diagnostics opens only the validated local install folder", opened == [str(logs)])
        with patch.object(ov, "__file__", str(root / "jira_overlay.pyw")), \
             patch.object(ov.os, "startfile", side_effect=OSError("isolated Windows failure")):
            before = len(messages)
            app._open_diagnostics()
            check("Explorer failures produce an actionable message rather than crashing Settings",
                len(messages) == before + 1 and "could not open" in str(messages[-1]))
    with tempfile.TemporaryDirectory() as folder:
        (Path(folder)/".git").mkdir()
        with patch.object(ov, "__file__", str(Path(folder)/"jira_overlay.pyw")), \
             patch.object(ov.updates, "stage_release", side_effect=AssertionError("Must not replace Git files")):
            app._install_update()
    check("the update button gives Git checkout instructions without downloading",
          any("git pull --ff-only" in str(message) for message in messages))
    app.config["checkForUpdates"] = False
    with patch.object(ov.updates, "latest_release", side_effect=AssertionError("Opted out")):
        app._check_updates(False)
    check("automatic checks honour the user's opt-out", not app._update_busy)
    app.config["checkForUpdates"] = True
    with patch.object(ov.updates, "latest_release", side_effect=OSError("offline")):
        app._check_updates()
        drain()
    check("network failure is shown without changing Jira connection health",
          "unavailable" in app._update_status and not app._conn_error)
    with tempfile.TemporaryDirectory() as folder:
        app._update_release = release
        with patch.object(ov, "__file__", str(Path(folder)/"jira_overlay.pyw")), \
             patch.object(ov.updates, "stage_release", return_value=Path(folder)/"payload"), \
             patch.object(ov.subprocess, "Popen", side_effect=fake_popen):
            app._install_update()
            until = time.monotonic()+5
            while not launched and time.monotonic() < until:
                app.root.update()
                time.sleep(0.01)
        check("confirmed ZIP updates launch a helper but do not quit before its shutdown request",
              bool(launched) and "--payload" in launched[0] and bool(app.root.winfo_exists()))
        launched.clear()
        native_calls = []
        with patch.object(ov.native_update, "launch", side_effect=lambda *args: native_calls.append(args) or SimpleNamespace(wait=lambda: 0)):
            app._launch_native_installer(folder, Path(folder)/"Setup.exe")
        check("native updates use an external monitor without quitting the learner early",
              len(native_calls) == 1 and native_calls[0][0] == folder
              and bool(app.root.winfo_exists()))
    with tempfile.TemporaryDirectory() as folder:
        root = Path(folder)
        automatic = dict(release, installer={"sha256": "1" * 64})
        app._update_busy = False
        app.config.pop("autoInstallUpdates", None)
        native_calls = []
        with patch.object(ov, "__file__", str(root / "jira_overlay.pyw")), \
             patch.object(ov.updates, "native_install", return_value=True), \
             patch.object(ov.updates, "stage_installer", return_value=root / "Setup.exe"), \
             patch.object(ov.native_update, "launch", side_effect=lambda *a: native_calls.append(a) or SimpleNamespace(wait=lambda: 0)), \
             patch.object(ov.messagebox, "askyesno", side_effect=AssertionError("Automatic updates must not ask")):
            app._update_checked(automatic, None, False)
            until = time.monotonic() + 5
            while not native_calls and time.monotonic() < until:
                app.root.update()
                time.sleep(.01)
            check("automatic installation is the default and needs no confirmation", len(native_calls) == 1)
            app._update_busy = False
            app._update_checked(automatic, None, False)
            check("the same automatic attempt is not relaunched in a restart loop", len(native_calls) == 1 and "delayed" in app._update_status)
            app.config["autoInstallUpdates"] = False
            updates_before = len(native_calls)
            app._update_checked(dict(automatic, version="2099.01.02"), None, False)
            check("opt-out retains update notices without installing", len(native_calls) == updates_before and not app._update_busy)
        app.config["autoInstallUpdates"] = True
        app._update_release = automatic
        with patch.object(ov, "__file__", str(root / "jira_overlay.pyw")):
            app.config["autoInstallUpdates"] = False
            cancelled = app._automatic_update_cancelled(folder, True)
            check("opting out during download prevents execution", cancelled and ov.updates.update_state(root).get("phase") == "cancelled")
            ov.updates.record_update(root, automatic["version"], "failed", "[JIRA-U202] Example failure")
            before = len(messages)
            app._report_previous_update()
            app._report_previous_update()
            check("a post-shutdown failure is displayed once after relaunch", len(messages) == before + 1 and "JIRA-U202" in app._update_status)
        app._update_busy = False
finally:
    app.root.destroy()
    app._session.close()

# Fresh users must complete credential setup BEFORE the learner is launched.
startup_order = []
class FreshOverlay:
    def __init__(self, config): startup_order.append("overlay-ready")
    def run(self): startup_order.append("running")
class FreshSetup:
    def __init__(self):
        startup_order.append("credentials-ready")
        self.result = {"domain": "example.invalid", "token": "FAKE"}
with patch.object(ov.updates, "recover_pending", return_value=False), \
     patch.object(ov, "acquire_single_instance", return_value=True), \
     patch.object(ov, "_create_show_event"), \
     patch.object(ov, "_start_show_waiter"), \
     patch.object(ov.updates, "listen_for_update"), \
     patch.object(ov, "load_config", return_value=None), \
     patch.object(ov, "SetupWindow", FreshSetup), \
     patch.object(ov, "JiraOverlay", FreshOverlay), \
     patch.object(ov.subprocess, "Popen", side_effect=lambda *a, **k: startup_order.append("learner-started")):
    ov.main()
check("fresh-user configuration is saved before the learner is started",
      startup_order == ["credentials-ready", "overlay-ready", "learner-started", "running"])

# Exercise the learner's real main() lifecycle with a fake data source/tray.
# No actual global mutex, Jira session, model or configuration is touched.
tl = load_learner()
sequence, callbacks = [], []
class Watcher:
    def __init__(self): self.stop = threading.Event()
    def run_forever(self):
        self.stop.wait(5)
        time.sleep(0.03)
        sequence.append("writer-finished")
    def _save_state(self): sequence.append("saved")
class Icon:
    def __init__(self, *a, **k): pass
    def stop(self): sequence.append("tray-stop")
    def run(self): callbacks[0]()
kernel = SimpleNamespace(CreateMutexW=lambda *a: 1, GetLastError=lambda: 0, CloseHandle=lambda *a: None)
with patch.object(tl, "ShadowWatcher", Watcher), patch.object(tl.pystray, "Icon", Icon), \
     patch.object(tl.updates, "listen_for_update", side_effect=lambda root, callback: callbacks.append(callback)), \
     patch.object(tl.updates, "recover_pending", return_value=False), \
     patch.object(tl.ctypes.windll, "kernel32", kernel):
    tl.main()
check("the learner joins its writer and saves before the update can replace code",
      sequence == ["tray-stop", "writer-finished", "saved"])
sys.exit(check.done())
