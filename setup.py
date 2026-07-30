#!/usr/bin/env python3
"""
Jira Overlay - first-time setup
===============================
One command to get a new machine running:

    python setup.py          (or double-click setup.bat)

It checks your Python, installs the three packages the app needs, offers to
start it with Windows, and launches it. Credentials are collected by the
overlay's own setup window the first time it runs.

This is a console script on purpose. The app itself runs under pythonw, which
has nowhere to print a traceback - so anything that can go wrong during
installation is made to go wrong *here*, where you can read it.
"""

from __future__ import annotations

import os
import subprocess
import sys

DIR = os.path.dirname(os.path.abspath(__file__))

OVERLAY = os.path.join(DIR, "jira_overlay.pyw")
LEARNER = os.path.join(DIR, "triage_learner.pyw")
CONFIG  = os.path.join(DIR, "jira_config.json")

# (import name, pip name)
DEPENDENCIES = [("requests", "requests"), ("PIL", "Pillow"),
                ("pystray", "pystray")]

MIN_PYTHON = (3, 9)
STARTUP_REG   = r"Software\Microsoft\Windows\CurrentVersion\Run"
STARTUP_NAMES = {"JiraOverlay": OVERLAY, "JiraTriageLearner": LEARNER}


# ---------------------------------------------------------------------------
# Small console helpers
# ---------------------------------------------------------------------------

def say(text: str = "") -> None:
    print(text, flush=True)


def step(n: int, total: int, text: str) -> None:
    say(f"\n[{n}/{total}] {text}")


def ask_yes_no(question: str, default: bool = True) -> bool:
    suffix = "[Y/n]" if default else "[y/N]"
    while True:
        try:
            answer = input(f"{question} {suffix} ").strip().lower()
        except EOFError:
            return default
        if not answer:
            return default
        if answer in ("y", "yes"):
            return True
        if answer in ("n", "no"):
            return False


def pythonw() -> str:
    """pythonw.exe next to this interpreter, so the apps run without a console."""
    head, tail = os.path.split(sys.executable)
    if tail.lower() == "python.exe":
        candidate = os.path.join(head, "pythonw.exe")
        if os.path.exists(candidate):
            return candidate
    return sys.executable


# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------

def check_python() -> bool:
    say(f"    Python {sys.version.split()[0]} at {sys.executable}")
    if sys.version_info < MIN_PYTHON:
        say(f"    This needs Python {MIN_PYTHON[0]}.{MIN_PYTHON[1]} or newer.")
        say("    Get it from https://python.org and run setup again.")
        return False
    if os.name != "nt":
        say("    This is a Windows app - it uses the Windows tray, toast "
            "notifications and registry.")
        return False
    return True


def check_files() -> bool:
    missing = [os.path.basename(p) for p in (OVERLAY, LEARNER)
               if not os.path.exists(p)]
    if missing:
        say(f"    Missing from this folder: {', '.join(missing)}")
        say("    Re-download the project so all files sit together.")
        return False
    say("    All application files present.")
    return True


def install_dependencies() -> bool:
    import importlib.util
    missing = [(mod, pkg) for mod, pkg in DEPENDENCIES
               if importlib.util.find_spec(mod) is None]
    if not missing:
        say("    Already installed: " + ", ".join(p for _, p in DEPENDENCIES))
        return True

    failed = []
    for _mod, pkg in missing:
        say(f"    Installing {pkg} ...")
        base = [sys.executable, "-m", "pip", "install", "--quiet"]
        # Retry into the user site-packages: on a managed machine, or Python
        # from the Microsoft Store, a plain install often cannot write to the
        # interpreter's own directory.
        if subprocess.call(base + [pkg]) != 0 and \
                subprocess.call(base + ["--user", pkg]) != 0:
            failed.append(pkg)

    if failed:
        say(f"    Could not install: {', '.join(failed)}")
        say(f'    Try by hand:  "{sys.executable}" -m pip install '
            + " ".join(failed))
        return False
    say("    Done.")
    return True


def configure_startup() -> None:
    import winreg
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, STARTUP_REG, 0,
                            winreg.KEY_READ) as key:
            already = all(_has_value(key, name) for name in STARTUP_NAMES)
    except OSError:
        already = False

    if already:
        say("    Already set to start with Windows.")
        return
    if not ask_yes_no("    Start automatically when you log in?"):
        say("    Skipped - you can turn this on later in Settings.")
        return
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, STARTUP_REG, 0,
                            winreg.KEY_SET_VALUE) as key:
            for name, script in STARTUP_NAMES.items():
                winreg.SetValueEx(key, name, 0, winreg.REG_SZ,
                                  f'"{pythonw()}" "{script}"')
        say("    Enabled.")
    except OSError as exc:
        say(f"    Could not write the registry key: {exc}")


def _has_value(key, name: str) -> bool:
    import winreg
    try:
        winreg.QueryValueEx(key, name)
        return True
    except FileNotFoundError:
        return False


def launch() -> None:
    flags = 0x08000000          # CREATE_NO_WINDOW
    subprocess.Popen([pythonw(), OVERLAY], cwd=DIR, creationflags=flags)
    subprocess.Popen([pythonw(), LEARNER], cwd=DIR, creationflags=flags)


# ---------------------------------------------------------------------------

def main() -> int:
    say("=" * 62)
    say("  Jira Overlay - setup")
    say("=" * 62)

    total = 4
    step(1, total, "Checking Python")
    if not check_python():
        return 1

    step(2, total, "Checking application files")
    if not check_files():
        return 1

    step(3, total, "Installing required packages")
    if not install_dependencies():
        return 1

    step(4, total, "Windows startup")
    configure_startup()

    first_run = not os.path.exists(CONFIG)
    say("\n" + "=" * 62)
    if first_run:
        say("  Setup complete. The overlay will now ask for your Jira details:")
        say("")
        say("    * Jira domain     e.g. yourcompany.atlassian.net")
        say("    * Email           your Atlassian account email")
        say("    * API token       the setup window links straight to the")
        say("                      page that creates one")
        say("")
        say("  It then finds your service desk and asks which queue should")
        say("  raise alerts. After that it lives in the system tray.")
    else:
        say("  Setup complete - existing configuration kept.")
    say("=" * 62)

    if ask_yes_no("\nStart it now?"):
        launch()
        say("\nStarted. Look for the icons in your system tray:")
        say("  * blue circle   the overlay (ticket count)")
        say("  * purple circle the triage learner - read-only, it only ever")
        say("                  suggests where a ticket is heading")
    else:
        say("\nStart it any time with launch.bat")
    return 0


if __name__ == "__main__":
    try:
        code = main()
    except KeyboardInterrupt:
        say("\nCancelled.")
        code = 1
    if sys.stdout.isatty():
        try:
            input("\nPress Enter to close...")
        except EOFError:
            pass
    sys.exit(code)
