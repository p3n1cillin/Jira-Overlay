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

try:
    release = {"version": "2099.01.01", "page": "https://github.com/owner/repo/releases/tag/v2099.01.01"}
    with patch.object(ov.updates, "latest_release", return_value=release):
        app._check_updates()
        drain()
    app._build_settings_panel()
    check("background results populate the update label and enable the install control",
          "2099.01.01" in app._update_status_var.get() and str(app._update_install_button["state"]) == "normal")
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
             patch.object(ov.subprocess, "Popen", side_effect=lambda args, **kwargs: launched.append(args)):
            app._install_update()
            until = time.monotonic()+5
            while not launched and time.monotonic() < until:
                app.root.update()
                time.sleep(0.01)
        check("confirmed ZIP updates launch a helper but do not quit before its shutdown request",
              bool(launched) and "--payload" in launched[0] and bool(app.root.winfo_exists()))
        launched.clear()
        with patch.object(ov.subprocess, "Popen", side_effect=lambda args, **kwargs: launched.append(args)):
            app._launch_native_installer(folder, Path(folder)/"Setup.exe")
        check("native updates run the same installer without quitting the learner early",
              bool(launched) and "/SILENT" in launched[0] and "/DIR=" + folder in launched[0]
              and bool(app.root.winfo_exists()))
finally:
    app.root.destroy()
    app._session.close()

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
