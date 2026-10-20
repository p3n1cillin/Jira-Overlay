"""Confirmed update/rollback helper. No Jira account or GitHub token needed."""
from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import tkinter as tk
from tkinter import filedialog, messagebox
from source_layout import activate
activate(Path(__file__).resolve().parent)

import app_updates as updates
from app_release import VERSION


def restart_apps(root):
    executable = Path(sys.executable)
    pythonw = executable.with_name("pythonw.exe")
    if not pythonw.exists():
        pythonw = executable
    for name in ("jira_overlay.pyw", "triage_learner.pyw"):
        subprocess.Popen([str(pythonw), str(root / name)], cwd=root, creationflags=0x08000000)


def prepare_dependencies(payload, progress):
    result = []
    interpreter = Path(sys.executable).with_name("python.exe")
    if not interpreter.exists():
        interpreter = Path(sys.executable)

    def run():
        try:
            result.append(subprocess.run([str(interpreter), str(payload/"setup.py"), "--dependencies-only"],
                cwd=payload, creationflags=0x08000000, capture_output=True, text=True, timeout=300))
        except Exception as exc:
            result.append(exc)
    worker = threading.Thread(target=run, daemon=True)
    worker.start()
    while worker.is_alive():
        progress()
        time.sleep(0.1)
    if isinstance(result[0], Exception) or result[0].returncode != 0:
        raise updates.UpdateError("Dependency preparation failed before stopping the apps. Run setup.bat from the new release and try again.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target")
    parser.add_argument("--payload")
    parser.add_argument("--choose-target", action="store_true")
    parser.add_argument("--rollback", action="store_true")
    args = parser.parse_args()
    window = tk.Tk()
    window.withdraw()
    changed = False
    stopped = False
    target = None
    version = None
    try:
        target_text = args.target
        if args.choose_target or not target_text:
            target_text = filedialog.askdirectory(title="Select your existing Jira Overlay application folder", mustexist=True)
        if not target_text:
            return 0
        target = updates._target(target_text)
        if updates.native_install(target):
            raise updates.UpdateError("This installation has a private runtime. Run the latest Windows Setup EXE to upgrade or repair it; do not apply a code-only update/rollback.")
        if args.rollback:
            _, backups = updates._storage(target)
            choices = sorted((p for p in backups.iterdir() if p.is_dir() and (p/"backup.json").is_file()), reverse=True)
            if not choices:
                raise updates.UpdateError("No code backup is available for this installation")
            action = f"Restore application code from {choices[0].name}?"
            payload = None
        elif args.payload:
            payload = Path(args.payload).resolve()
            version = updates.verify_payload(payload)["version"]
            action = None  # the in-app update button already obtained consent
        else:
            # One-time migration for older GitHub ZIP installations: use the
            # clean extracted release itself, not the older client's updater.
            with tempfile.TemporaryDirectory(prefix="jira-migrate-check-") as folder:
                from release_tools import build_release
                archive = build_release(Path(__file__).resolve().parent, folder)
                payload = updates.unpack_package(archive, Path(folder)/"payload", VERSION)
                # Keep a verified staged copy after the build scratch directory closes.
                saved = Path(tempfile.mkdtemp(prefix="jira-migrate-"))/"payload"
                shutil.copytree(payload, saved)
                payload = saved
            action = f"Update the application in {target} to {VERSION}?"
        if action and not messagebox.askyesno("Jira Overlay update", action + "\n\nCredentials, settings and learner data are preserved.\nClose both tray icons first if this is an older installation.", parent=window):
            return 0
        window.deiconify()
        window.title("Jira Overlay update")
        window.geometry("430x120")
        window.protocol("WM_DELETE_WINDOW", lambda: None)
        label = tk.Label(window, text="Preparing the update…", padx=15, pady=20)
        label.pack()
        window.update()
        if not args.rollback:
            prepare_dependencies(payload, window.update)
        label.config(text="Waiting for both applications to save and close…\n\nThis can take a few minutes during learner tuning.")
        with updates.update_lock(target):
            with updates.stop_apps(target, progress=window.update):
                stopped = True
                if args.rollback:
                    updates.restore_backup(target, choices[0])
                else:
                    updates.install_payload(target, payload)
                changed = True
        if version:
            updates.record_update(target, version, "completed", "Application update completed.")
        restart_apps(target)
        return 0
    except Exception as exc:
        detail = f"{type(exc).__name__}: {exc}"
        # Public update diagnostics contain no config, tokens or ticket data.
        if target is not None:
            try:
                (target / "app_update_error.log").write_text(detail + "\n", encoding="utf-8")
                if version:
                    updates.record_update(target, version, "failed", detail, notified=True)
            except OSError:
                pass
        messagebox.showerror("Jira Overlay update", detail + "\n\nIf an interrupted update needs recovery, run update.bat --rollback. Your personal data has not been replaced.", parent=window)
        if stopped and not changed and target is not None and not (target/".app-updates"/"active.json").exists():
            restart_apps(target)
        return 1
    finally:
        window.destroy()


if __name__ == "__main__":
    raise SystemExit(main())
