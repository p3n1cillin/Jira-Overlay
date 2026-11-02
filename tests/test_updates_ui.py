"""Notice-only updates, approved browser navigation and graceful writer exit.

All networking, browser launches, child processes and tray callbacks are fake.
No application task or live learner is started by these tests.
"""
from pathlib import Path
import tempfile
import threading
import time
from types import SimpleNamespace
from unittest.mock import patch
from _harness import Checker, load_overlay, load_learner

check = Checker("reviewed update UI")
ov = load_overlay()
ov.JiraOverlay._fetch = lambda self: None
ov.JiraOverlay._start_tray = lambda self: None
ov.startup_enabled = lambda: False
messages, pages = [], []
ov.messagebox = SimpleNamespace(showinfo=lambda *a, **k: messages.append(a),
    showerror=lambda *a, **k: messages.append(a), askyesno=lambda *a, **k: True)
app = ov.JiraOverlay({"domain":"example.invalid","email":"fake","token":"fake", "projectKey":"ANY",
    "serviceDeskId":1,"serviceDeskName":"Arbitrary desk","notificationsEnabled":False})

def drain():
    until = time.monotonic() + 5
    while app._update_busy and time.monotonic() < until:
        app.root.update()
        time.sleep(.01)
    app.root.update()

def texts(widget):
    values = [str(widget.cget("text"))] if "text" in widget.keys() else []
    return values + [text for child in widget.winfo_children() for text in texts(child)]

try:
    release = {"version":"2099.01.01"}
    with patch.object(ov.updates,"latest_release",return_value=release):
        app._check_updates()
        drain()
    app._build_settings_panel()
    check("manual checks show availability and an explicit download control",
        "2099.01.01" in app._update_status_var.get() and "Open update download" in texts(app.root))
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        with patch.object(ov,"__file__",str(root/"jira_overlay.pyw")), \
             patch.object(ov.subprocess,"Popen",side_effect=AssertionError("No child process")), \
             patch.object(ov.updates,"stage_installer",side_effect=AssertionError("No download")), \
             patch.object(ov.updates,"stage_release",side_effect=AssertionError("No download")), \
             patch.object(ov.webbrowser,"open",side_effect=lambda page: pages.append(page) or True):
            for legacy in (None,True,False):
                app.config["autoInstallUpdates"] = legacy
                app._update_checked(release,None,False)
                app._install_update(automatic=True)
            check("background checks never download, execute or open a browser even with an old opt-in", not pages and not app._update_busy)
            app._install_update()
            check("confirmed manual updates open only the repository's version-bound HTTPS release page",
                pages == [f"https://github.com/{ov.updates.REPOSITORY}/releases/tag/v2099.01.01"])
            with patch.object(ov.messagebox,"askyesno",return_value=False):
                app._install_update()
            check("declining update navigation causes no action", len(pages)==1)
            app._update_release = {"version":"2099.01.02"}
            app._install_update()
            check("navigation is derived from the configured publisher, not an arbitrary response URL", pages[-1].endswith("/v2099.01.02"))
        opened = []
        with patch.object(ov,"__file__",str(root/"jira_overlay.pyw")), \
             patch.object(ov.updates,"native_install",return_value=True), \
             patch.object(ov.lifecycle,"recovery_status",return_value={"healthy":False,"message":"User-session recovery is not running [JIRA-R101]"}), \
             patch.object(ov.os,"startfile",side_effect=opened.append):
            app._build_settings_panel()
            check("Settings shows recovery availability and local diagnostics", any("JIRA-R101" in s for s in texts(app.root)) and "Open local diagnostic logs" in texts(app.root))
            app._open_diagnostics()
            check("absent diagnostic logs are not created just by viewing",not opened and not list(root.iterdir()))
            folder=ov.lifecycle.folder(root)
            app._open_diagnostics()
            check("only the validated local diagnostic directory is opened",opened==[str(folder)])
        with patch.object(ov,"__file__",str(root/"jira_overlay.pyw")), \
             patch.object(ov.os,"startfile",side_effect=OSError("isolated failure")):
            count=len(messages)
            app._open_diagnostics()
            check("Explorer errors are visible instead of crashing Settings",len(messages)==count+1)
        (root/".git").mkdir()
        with patch.object(ov,"__file__",str(root/"jira_overlay.pyw")):
            app._install_update()
        check("source checkouts receive Git instructions without execution",any("git pull --ff-only" in str(s) for s in messages))
    with patch.object(ov.updates,"latest_release",side_effect=OSError("offline")):
        app._check_updates()
        drain()
    check("failed release checks do not change Jira connection health","unavailable" in app._update_status and not app._conn_error)
finally:
    app.root.destroy()
    app._session.close()

# Exercise real main() ordering without real clients, tasks or IPC listeners.
sequence = []
class FreshOverlay:
    def __init__(self, config): sequence.append("overlay-ready")
    def run(self): sequence.append("running")
class FreshSetup:
    def __init__(self):
        sequence.append("credentials-ready")
        self.result={"domain":"example.invalid","token":"FAKE"}
with patch.object(ov.updates,"recover_pending",return_value=False), \
     patch.object(ov,"acquire_single_instance",return_value=True), \
     patch.object(ov,"_create_show_event"), patch.object(ov,"_start_show_waiter"), \
     patch.object(ov.updates,"listen_for_update"), patch.object(ov,"load_config",return_value=None), \
     patch.object(ov,"SetupWindow",FreshSetup), patch.object(ov,"JiraOverlay",FreshOverlay), \
     patch.object(ov.subprocess,"Popen",side_effect=lambda *a, **k: sequence.append("learner-started")):
    ov.main()
check("fresh setup precedes learner launch",sequence==["credentials-ready","overlay-ready","learner-started","running"])

tl=load_learner()
sequence, callbacks=[],[]
class Watcher:
    def __init__(self): self.stop=threading.Event()
    def run_forever(self):
        self.stop.wait(5)
        time.sleep(.03)
        sequence.append("writer-finished")
    def _save_state(self): sequence.append("saved")
class Icon:
    def __init__(self,*a,**k): pass
    def stop(self): sequence.append("tray-stop")
    def run(self): callbacks[0]()
kernel=SimpleNamespace(CreateMutexW=lambda *a:1,GetLastError=lambda:0,CloseHandle=lambda *a:None)
with patch.object(tl,"ShadowWatcher",Watcher), patch.object(tl.pystray,"Icon",Icon), \
     patch.object(tl.updates,"listen_for_update",side_effect=lambda root,callback:callbacks.append(callback)), \
     patch.object(tl.updates,"recover_pending",return_value=False), patch.object(tl.ctypes.windll,"kernel32",kernel):
    tl.main()
check("shutdown joins and saves the learner writer",sequence==["tray-stop","writer-finished","saved"])
with tempfile.TemporaryDirectory() as directory:
    hold=Path(directory)/".jira-overlay-security-hold"
    hold.write_text("isolated approval hold")
    for module in (ov,tl):
        with patch.object(module,"__file__",str(Path(directory)/"app.pyw")), \
             patch.object(module.updates,"recover_pending",side_effect=AssertionError("Hold must be checked first")):
            module.main()
    check("security holds prevent either companion from entering startup",hold.exists())
raise SystemExit(check.done())
