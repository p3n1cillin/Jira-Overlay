"""First run on a machine that hasn't got the dependencies.

This is the path every new user takes and the one least likely to be
exercised during development. It has broken twice: once because the
bootstrap referenced a helper defined further down the file, and once
because a failed install fell through to an ImportError that pythonw had
nowhere to print.
"""

import builtins
import importlib.util
import os
import subprocess
import sys
import tkinter.messagebox as mb

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import Checker, PROJECT               # noqa: E402

check = Checker("first run")
real_find_spec = importlib.util.find_spec
real_call = subprocess.call
real_import = builtins.__import__
real_showerror = mb.showerror


def fresh_machine():
    """Pretend none of the third-party packages are installed."""
    importlib.util.find_spec = (
        lambda n, *a, **k: None if n in ("requests", "PIL", "pystray")
        else real_find_spec(n, *a, **k))


def restore():
    importlib.util.find_spec = real_find_spec
    subprocess.call = real_call
    builtins.__import__ = real_import
    mb.showerror = real_showerror
    for name in ("ov", "tl"):
        sys.modules.pop(name, None)


def load_overlay_module():
    spec = importlib.util.spec_from_file_location(
        "ov", os.path.join(PROJECT, "jira_overlay.pyw"))
    mod = importlib.util.module_from_spec(spec)
    sys.modules["ov"] = mod
    spec.loader.exec_module(mod)
    return mod


# --- pip succeeds: the app must import cleanly ------------------------------
fresh_machine()
subprocess.call = lambda *a, **k: 0
try:
    load_overlay_module()
    check("the overlay starts on a machine missing every dependency", True)
except NameError as exc:
    check("the overlay starts on a machine missing every dependency", False,
          f"NameError: {exc}")
except Exception as exc:
    check("the overlay starts on a machine missing every dependency", False,
          f"{type(exc).__name__}: {exc}")
finally:
    restore()

# --- pip fails: say so, don't die silently ---------------------------------
fresh_machine()
subprocess.call = lambda *a, **k: 1
shown = {}
mb.showerror = lambda title, msg: shown.update(title=title, msg=msg)
try:
    load_overlay_module()
    check("a failed install stops rather than crashing later", False,
          "module imported anyway")
except SystemExit:
    check("a failed install stops rather than crashing later", True)
    check("  and tells the user what to run",
          "pip install" in shown.get("msg", ""), shown.get("title", ""))
except ImportError as exc:
    check("a failed install stops rather than crashing later", False,
          f"fell through to ImportError: {exc}")
finally:
    restore()

# --- the learner started first, before anything is installed ---------------
shown = {}
mb.showerror = lambda title, msg: shown.update(title=title, msg=msg)


def blocked_import(name, *a, **k):
    if name in ("requests", "PIL", "pystray") or name.startswith("PIL."):
        raise ImportError(f"No module named '{name}'")
    return real_import(name, *a, **k)


builtins.__import__ = blocked_import
try:
    spec = importlib.util.spec_from_file_location(
        "tl", os.path.join(PROJECT, "triage_learner.pyw"))
    mod = importlib.util.module_from_spec(spec)
    sys.modules["tl"] = mod
    spec.loader.exec_module(mod)
    check("the learner explains itself if started before setup", False,
          "imported despite missing dependencies")
except SystemExit:
    builtins.__import__ = real_import
    check("the learner explains itself if started before setup",
          "pip install" in shown.get("msg", ""))
except ImportError as exc:
    builtins.__import__ = real_import
    check("the learner explains itself if started before setup", False,
          f"raw ImportError escaped: {exc}")
finally:
    restore()

# --- startup registration must heal a moved install, not just remember ----
# The registry only stores a name -> command line. Checking just the name's
# presence means a folder move (this one lives under Temp) or a Python
# upgrade leaves a stale command line forever "already configured" and
# silently dead. A first-time install should still ask before touching
# anything; once the user has said yes once, later drift should self-heal
# without asking again.


class _FakeKey:
    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class FakeWinReg:
    HKEY_CURRENT_USER = object()
    KEY_READ = 1
    KEY_SET_VALUE = 2
    REG_SZ = 1

    def __init__(self, initial=None):
        self.values = dict(initial or {})

    def OpenKey(self, hive, path, res, access):
        return _FakeKey()

    def CreateKeyEx(self, hive, path, res, access):
        return _FakeKey()

    def DeleteValue(self, key, name):
        if name not in self.values:
            raise FileNotFoundError(name)
        del self.values[name]

    def QueryValueEx(self, key, name):
        if name not in self.values:
            raise FileNotFoundError(name)
        return (self.values[name], 1)

    def SetValueEx(self, key, name, res, typ, value):
        self.values[name] = value


spec = importlib.util.spec_from_file_location(
    "su", os.path.join(PROJECT, "setup.py"))
su = importlib.util.module_from_spec(spec)
spec.loader.exec_module(su)
real_winreg = sys.modules.get("winreg")
wanted = {name: f'"{su.pythonw()}" "{script}"'
          for name, script in su.STARTUP_NAMES.items()}


def run_configure(initial: dict, answer: bool):
    fake = FakeWinReg(initial)
    sys.modules["winreg"] = fake
    asked = {"n": 0}
    su.ask_yes_no = lambda *a, **k: (asked.__setitem__("n", asked["n"] + 1)
                                     or answer)
    try:
        su.configure_startup()
    finally:
        if real_winreg is not None:
            sys.modules["winreg"] = real_winreg
        else:
            sys.modules.pop("winreg", None)
    return fake.values, asked["n"]


values, asked = run_configure({}, True)
check("a fresh machine asks before registering startup", asked == 1)
check("  and registers the coordinated app once the user agrees", values == wanted,
      values)

values, asked = run_configure({}, False)
check("declining leaves nothing registered", values == {} and asked == 1)

stale = {name: '"C:\\Old\\Python\\pythonw.exe" "C:\\Old\\Path\\x.pyw"'
         for name in su.STARTUP_NAMES}
values, asked = run_configure(stale, True)
check("a moved install or upgraded Python is corrected without asking",
      values == wanted and asked == 0, f"asked={asked} values={values}")

values, asked = run_configure(dict(wanted), True)
check("an already-correct registration is left alone and not re-asked",
      values == wanted and asked == 0, f"asked={asked}")

partial = {"JiraTriageLearner": next(iter(stale.values()))}
values, asked = run_configure(partial, True)
check("a legacy learner-only startup is migrated without re-asking",
      values == wanted and asked == 0, f"asked={asked} values={values}")

# --- setup scripts must be readable by Windows PowerShell 5.1 --------------
# 5.1 reads a script without a byte-order mark as ANSI, so a single non-ASCII
# character can break parsing before the installer prints anything.
for name in ("setup.ps1", "setup.py", "setup.bat", "launch.bat"):
    path = os.path.join(PROJECT, name)
    if not os.path.exists(path):
        check(f"{name} exists", False)
        continue
    text = open(path, encoding="utf-8").read()
    bad = sorted({c for c in text if ord(c) > 127})
    check(f"{name} is pure ASCII", not bad, "".join(bad))

sys.exit(check.done())
