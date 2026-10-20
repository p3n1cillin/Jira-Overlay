"""CI-only live UI upgrade; run against the native smoke test's temporary app.

Jira fetches and GitHub downloads are replaced with local fixtures. The actual
UI, default install policy, external supervisor, shutdown signal, installer and
runtime replacement execute unchanged. No real learner or Jira session runs.
"""
import importlib.util
from pathlib import Path
import sys

root, installer, version, digest = sys.argv[1:]
root = Path(root)
spec = importlib.util.spec_from_file_location("fixture_overlay", root / "jira_overlay.pyw")
ov = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = ov
spec.loader.exec_module(ov)
ov.JiraOverlay._fetch = lambda self: None
ov.JiraOverlay._start_tray = lambda self: None
assert ov.acquire_single_instance()
app = ov.JiraOverlay({"domain": "example.invalid", "email": "test@example.invalid", "token": "FAKE",
    "serviceDeskId": 1, "serviceDeskName": "Test fixture", "projectKey": "SD", "notificationsEnabled": False})


def fail(*args, **kwargs):
    print("UPDATE ERROR:", args, file=sys.stderr, flush=True)
    app.root.destroy()


ov.messagebox.showerror = fail
ov.updates.listen_for_update(root, lambda: app._post(app.root.destroy))
release = {"version": version, "installer": {"sha256": digest}}
ov.updates.latest_release = lambda: release
ov.updates.stage_installer = lambda release: Path(installer)
external_launch = ov.native_update.launch


def launch(*args, **kwargs):
    # Do not launch a real app/learner with the fake migrated Jira credentials
    # after setup. Production defaults to relaunch=True; tested separately.
    process = external_launch(*args, **kwargs, relaunch=False)
    print(process.pid, flush=True)
    return process


ov.native_update.launch = launch
app._check_updates(False)
app.run()
