"""CI-only companion with real IPC, but no Jira/model code. Temporary trees only."""
import ctypes
import os
from pathlib import Path
import threading

import app_lifecycle as life
import app_updates as updates

root = Path(__file__).resolve().parent
role = "learner" if "learner" in Path(__file__).name else "overlay"


def main():
    kernel = updates._kernel()
    handle = kernel.CreateMutexW(None, False, updates.MUTEXES[0 if role == "overlay" else 1])
    assert handle and kernel.GetLastError() != 183
    stop = threading.Event()

    def quit_update():
        life.reason("update")
        stop.set()

    updates.listen_for_update(root, quit_update)
    life.ready()
    try:
        while not stop.wait(.1):
            crash = root / ("crash-" + role)
            if crash.exists():
                crash.unlink()
                os._exit(7)  # ONLY this data-free fixture, never a real writer.
            if (root / ("quit-" + role)).exists():
                life.reason("user-quit", quit=True)
                stop.set()
    finally:
        kernel.CloseHandle(ctypes.c_void_p(handle))


life.run(root, role, main)
