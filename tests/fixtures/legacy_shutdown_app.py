"""Standalone old-style tray/Tk loop for installer tests; never imports the app.

Copy to jira_overlay.pyw / triage_learner.pyw ONLY inside a temporary test tree.
The unique mutex in unit tests prevents touching a developer's real companions.
"""
import ctypes
import os
from pathlib import Path
import sys
import threading

k = ctypes.windll.kernel32
k.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_bool, ctypes.c_wchar_p]
k.CreateMutexW.restype = ctypes.c_void_p
k.CloseHandle.argtypes = [ctypes.c_void_p]
handle = k.CreateMutexW(None, False, os.environ["JIRA_TEST_MUTEX"])
assert handle
root = Path(__file__).parent

try:
    if "learner" in Path(__file__).name:
        import pystray
        from PIL import Image
        icon = pystray.Icon("TriageLearner", Image.new("RGB", (16, 16)), "Test fixture")

        def ready(icon):
            print("READY", flush=True)

        # Failsafe cooperative cleanup if a test fails before sending quit.
        def cleanup():
            sys.stdin.readline()
            icon.stop()

        threading.Thread(target=cleanup, daemon=True).start()
        icon.run(setup=ready)
    else:
        import tkinter as tk
        app = tk.Tk()
        app.withdraw()

        def cleanup():
            sys.stdin.readline()
            app.after(0, app.destroy)

        threading.Thread(target=cleanup, daemon=True).start()
        app.after(100, lambda: print("READY", flush=True))
        app.mainloop()
    (root / "closed.txt").write_text("normal loop exit")
finally:
    k.CloseHandle(handle)
