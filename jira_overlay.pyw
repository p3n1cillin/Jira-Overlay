#!/usr/bin/env python3
"""
Jira JSM Queue Overlay
======================
A lightweight Windows desktop overlay that monitors Jira Service Management
queues in real time.

Requirements:  Python 3.9+ (Windows)
               pip install requests pystray Pillow  (auto-installed on first run)

Quick start:   double-click setup.bat once, then launch.bat

Configuration: jira_config.json  (created on first run, excluded from git)
"""

from __future__ import annotations

import base64
import ctypes
import ctypes.wintypes
import functools
import importlib.util
import json
import os
import csv
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.parse
import webbrowser
import winreg
import winsound
from datetime import date, datetime, timedelta
from typing import Callable, Optional
import tkinter as tk
from tkinter import messagebox

# ---------------------------------------------------------------------------
# Dependency bootstrap — must run before any third-party import
# ---------------------------------------------------------------------------

_BOOTSTRAP_DEPS = [("requests", "requests"), ("PIL", "Pillow"), ("pystray", "pystray")]


def _bootstrap() -> None:
    """Install missing third-party packages with a progress UI on first run.

    This runs before the rest of the module exists, so it deliberately uses
    nothing defined further down — not the colour palette, not the font
    helper. Referencing them here raises NameError before the app has a
    window to report it in, and under pythonw that is a completely silent
    failure on exactly the machines that need the bootstrap: new ones.
    """
    missing = [(mod, pkg) for mod, pkg in _BOOTSTRAP_DEPS
               if importlib.util.find_spec(mod) is None]
    if not missing:
        return

    # Same palette and typeface as the rest of the app (_C / _FONT_STACK,
    # both defined further down — this runs before either exists, so the
    # values are repeated here rather than shared). This is the very first
    # thing a brand-new install ever shows; it looked like a different,
    # unrelated program bolted onto the front of this one.
    win = tk.Tk()
    win.title("Jira Overlay — First Run Setup")
    win.configure(bg="#11151d")
    win.resizable(False, False)
    win.update_idletasks()
    try:
        # Same call _round_corners makes further down (also not yet
        # defined here) — DWMWA_WINDOW_CORNER_PREFERENCE=33,
        # DWMWCP_ROUNDSMALL=3. Every other window in this app gets
        # Windows 11's native rounded corners; this one is the first
        # thing anyone sees and was the one place still square.
        hwnd = ctypes.windll.user32.GetAncestor(win.winfo_id(), 2)
        if hwnd:
            pref = ctypes.c_int(3)
            ctypes.windll.dwmapi.DwmSetWindowAttribute(
                ctypes.wintypes.HWND(hwnd), ctypes.c_uint(33),
                ctypes.byref(pref), ctypes.sizeof(pref))
    except Exception:
        pass          # cosmetic only
    sw, sh = win.winfo_screenwidth(), win.winfo_screenheight()
    win.geometry(f"360x130+{sw // 2 - 180}+{sh // 2 - 65}")

    tk.Label(win, text="Installing required packages…",
             font=("Segoe UI Variable Display", 11, "bold"),
             fg="#f2f5fa", bg="#11151d").pack(pady=(22, 4))
    lbl = tk.Label(win, text="", font=("Segoe UI Variable Text", 9),
                   fg="#98a4b6", bg="#11151d")
    lbl.pack()
    track = tk.Frame(win, bg="#1a212b", height=5)
    track.pack(fill="x", padx=24, pady=(12, 0))
    bar = tk.Frame(track, bg="#4d9dff", height=5, width=0)
    bar.place(x=0, y=0, height=5)
    win.update()

    def _pip(pkg: str, user: bool) -> int:
        cmd = [sys.executable, "-m", "pip", "install", "--quiet"]
        if user:
            cmd.append("--user")
        return subprocess.call(cmd + [pkg], creationflags=0x08000000)

    errors: list[str] = []
    for i, (_mod, pkg) in enumerate(missing):
        lbl.config(text=f"pip install {pkg}")
        win.update()
        # Retry into the user site-packages: on a managed machine, or Python
        # from the Microsoft Store, a plain install often can't write to the
        # interpreter's own directory.
        if _pip(pkg, user=False) != 0 and _pip(pkg, user=True) != 0:
            errors.append(pkg)
        track.update_idletasks()
        bar.place(width=int(track.winfo_width() * (i + 1) / len(missing)))
        win.update()

    if not errors:
        lbl.config(text="All done — starting overlay…")
        win.after(900, win.destroy)
        win.mainloop()
        return

    # Falling through would hit `import requests` and die with an ImportError
    # nobody can see. Say what failed and how to fix it, then stop.
    win.destroy()
    messagebox.showerror(
        "Jira Overlay — setup failed",
        "Could not install: " + ", ".join(errors) + "\n\n"
        "Install them by hand, then start the overlay again:\n\n"
        f'    "{sys.executable}" -m pip install ' + " ".join(errors))
    sys.exit(1)


_bootstrap()

# Third-party imports — guaranteed present after bootstrap
import requests
from requests.auth import HTTPBasicAuth
from PIL import Image, ImageDraw, ImageFont
import pystray

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

# Bump on a meaningful change. There's no build pipeline here to stamp this
# automatically — it's a single-file script someone may have just copied,
# not necessarily a git checkout — so this is the plain, low-maintenance
# answer to "what version is this" for a support conversation.
APP_VERSION  = "2026.08.21"
CONFIG_FILE  = os.path.join(os.path.dirname(os.path.abspath(__file__)), "jira_config.json")
STARTUP_NAME = "JiraOverlay"
LEARNER_STARTUP_NAME = "JiraTriageLearner"
STARTUP_REG  = r"Software\Microsoft\Windows\CurrentVersion\Run"
_SNAP_MARGIN  = 60   # px from a corner to snap
_LIVE_POLL_MS = 5_000   # queue-count poll interval — always 5 s regardless of settings

# ── Design tokens ──────────────────────────────────────────────────────────
# A deep, low-chroma neutral base lets one accent carry the whole interface.
# Surfaces step up in luminance (bg -> input -> chip) so depth reads without
# borders, and text sits on a three-level hierarchy (bright / body / dim).
_C = {
    "bg":        "#11151d",   # base surface
    "bg_dark":   "#0c0f15",   # deeper (setup dialogs, popups)
    "border":    "#232c39",   # 1-px window edge
    "divider":   "#1b222c",   # hairline separators
    "input":     "#1a212b",   # input / elevated surfaces
    "fg":        "#98a4b6",   # body text
    "fg_bright": "#f2f5fa",   # emphasis text
    "fg_dim":    "#5c6675",   # muted / inactive
    "accent":    "#4d9dff",   # primary accent
    "accent_hi": "#74b4ff",   # accent hover
    "alert":     "#ff6363",   # alert / breach
    "warn":      "#f7c065",   # snooze / stale
    "ok":        "#41d392",   # completed / healthy
    "info":      "#5fc8e8",   # alert-queue accent
    "hover":     "#1e2733",   # row hover background
    "chip":      "#26313f",   # button hover chip
    "badge":     "#212a36",   # count badge background
    "flash":      "#28405e",  # row-change highlight, fades back to bg
    "done_hover": "#16281f",  # completed-row hover tint (green-leaning)
}

# Typography — Segoe UI Variable is the Windows 11 system face and reads
# noticeably cleaner at small sizes; fall back on older Windows.
_FONT_STACK = ("Segoe UI Variable Text", "Segoe UI")
_FONT_DISPLAY_STACK = ("Segoe UI Variable Display", "Segoe UI Semibold",
                       "Segoe UI")
_font_cache: dict[tuple, tuple] = {}


def _font(size: int, weight: str = "normal", display: bool = False) -> tuple:
    """A font tuple using the best face available on this machine.

    Resolved once per (size, weight, display) and cached — font family
    probing needs a live Tk interpreter and is far too slow to repeat on
    every widget.
    """
    key = (size, weight, display)
    hit = _font_cache.get(key)
    if hit:
        return hit
    stack = _FONT_DISPLAY_STACK if display else _FONT_STACK
    family = stack[-1]
    try:
        import tkinter.font as tkfont
        available = set(tkfont.families())
        for candidate in stack:
            if candidate in available:
                family = candidate
                break
    except Exception:
        pass
    _font_cache[key] = (family, size, weight) if weight != "normal" \
        else (family, size)
    return _font_cache[key]


# ── Windows 11 window chrome ───────────────────────────────────────────────

_DWMWA_WINDOW_CORNER_PREFERENCE = 33
_DWMWCP_ROUND       = 2      # full radius
_DWMWCP_ROUNDSMALL  = 3      # subtle radius, right for compact surfaces


def _bind_wheel(widget: tk.Misc, command) -> None:
    """Bind mouse-wheel scrolling to a widget and everything inside it.

    Tk delivers <MouseWheel> to whatever sits under the pointer, so binding
    only the canvas means the wheel does nothing while hovering a checkbox.
    In a panel that is mostly checkboxes, that reads as the wheel being
    broken rather than as a subtlety of event delivery.
    """
    widget.bind("<MouseWheel>", command, add=True)
    for child in widget.winfo_children():
        _bind_wheel(child, command)


def _track(text: str, spacing: str = " ") -> str:
    """Letter-space a short label.

    Tk has no letter-spacing option, so a thin space between characters is
    the only way to get the airy small-caps look that makes an all-caps
    label read as typography rather than shouting.
    """
    return spacing.join(text)

# Windows' own icon font. Real system glyphs instead of a mix of Unicode
# symbols and emoji is much of the difference between "script" and "product":
# they share a stroke weight, sit on one baseline, and take the text colour
# rather than rendering as a colour emoji bitmap.
_ICON_STACK = ("Segoe Fluent Icons",   # Windows 11
               "Segoe MDL2 Assets")    # Windows 10
_ICON_GLYPH = {          # codepoints are shared by Fluent Icons and MDL2
    "refresh":  "",
    "snooze":   "",   # quiet hours (moon)
    "hide":     "",   # minimise
    "close":    "",
    "settings": "",
}
# Plain-text stand-ins so the UI still reads if no icon font is installed.
_ICON_TEXT = {"refresh": "↻", "snooze": "☾", "hide": "—",
              "close": "✕", "settings": "⚙"}
_icon_family: Optional[str] = None


def _icon_font(size: int = 10) -> tuple:
    """Font tuple for icon glyphs; the family is resolved once."""
    global _icon_family
    if _icon_family is None:
        try:
            import tkinter.font as tkfont
            available = set(tkfont.families())
            _icon_family = next((f for f in _ICON_STACK if f in available), "")
        except Exception:
            _icon_family = ""
    return (_icon_family or _FONT_STACK[-1], size)


def _icon(name: str) -> str:
    """Glyph for `name`, falling back to a text symbol without an icon font."""
    _icon_font()                      # ensure the family has been probed
    return _ICON_GLYPH[name] if _icon_family else _ICON_TEXT[name]


def _round_corners(window, small: bool = False) -> None:
    """Give a borderless Tk window real Windows 11 rounded corners.

    An overrideredirect window is a bare rectangle; DWM will round and shadow
    it for us if asked. Silently does nothing on Windows 10 and earlier, where
    the attribute is simply unsupported.
    """
    try:
        window.update_idletasks()
        hwnd = ctypes.windll.user32.GetAncestor(window.winfo_id(), 2)  # GA_ROOT
        if not hwnd:
            return
        pref = ctypes.c_int(_DWMWCP_ROUNDSMALL if small else _DWMWCP_ROUND)
        ctypes.windll.dwmapi.DwmSetWindowAttribute(
            ctypes.wintypes.HWND(hwnd),
            ctypes.c_uint(_DWMWA_WINDOW_CORNER_PREFERENCE),
            ctypes.byref(pref), ctypes.sizeof(pref))
    except Exception:
        pass          # cosmetic only — never break the app over chrome

def _lerp_hex(a: str, b: str, t: float) -> str:
    """Linear-interpolate between two #rrggbb colours (t in 0..1)."""
    t = max(0.0, min(1.0, t))
    ar, ag, ab = int(a[1:3], 16), int(a[3:5], 16), int(a[5:7], 16)
    br, bg, bb = int(b[1:3], 16), int(b[3:5], 16), int(b[5:7], 16)
    return (f"#{int(ar + (br - ar) * t):02x}"
            f"{int(ag + (bg - ag) * t):02x}"
            f"{int(ab + (bb - ab) * t):02x}")


# Tray icon font — loaded once, reused on every badge update
_tray_font: Optional[ImageFont.FreeTypeFont] = None


def _get_tray_font() -> ImageFont.ImageFont:
    """Return a cached 64-pt bold font for tray badge rendering."""
    global _tray_font
    if _tray_font is not None:
        return _tray_font
    for path in ("C:/Windows/Fonts/arialbd.ttf",
                 "C:/Windows/Fonts/arial.ttf",
                 "C:/Windows/Fonts/calibrib.ttf"):
        try:
            _tray_font = ImageFont.truetype(path, 64)
            return _tray_font
        except Exception:
            pass
    # This runs during __init__, outside any Tk callback, so anything raised
    # here takes the whole app down before it has a window to say so in.
    for attempt in (lambda: ImageFont.load_default(size=56),
                    ImageFont.load_default):
        try:
            _tray_font = attempt()
            return _tray_font
        except Exception:
            continue
    _tray_font = None       # caller falls back to Pillow's own default
    return _tray_font


# ---------------------------------------------------------------------------
# Config helpers
# ---------------------------------------------------------------------------

# Settings whose *type* the rest of the app relies on, with the value used
# when what's on disk can't be made sense of. This file is meant to be
# hand-editable, and is also written by a second process, so a value can be
# null, the wrong type, or nonsense without anything being corrupt enough
# for json.load to complain. Coercing once here beats guarding at every
# read: the same "present but null" mistake has already cost this project
# four separate bugs, because dict.get(key, default) only substitutes the
# default when the key is MISSING.
# Bounds here are the limits of what the app can physically work with, NOT
# the choices the Settings panel offers. Someone who hand-edits a value the
# dropdown doesn't list has chosen it deliberately — clamping refreshSeconds
# to the dropdown's 30s floor would have quietly halved the poll rate of an
# install running a deliberate 15.
# A real Jira project key: a letter, then letters/digits/underscore. Used to
# validate projectKey before it's trusted as a JQL identifier — see
# normalise_config for why a value here needs a character allowlist rather
# than jql_quote()'s literal-string escaping.
_SAFE_PROJECT_KEY = re.compile(r"^[A-Za-z][A-Za-z0-9_]*$")

_NUMERIC_SETTINGS = {           # key: (caster, default, low, high)
    "overlayWidth":   (int,   260,  120, 4000),   # below ~120 nothing fits
    "alpha":          (float, 0.93, 0.1, 1.0),    # Tk requires 0..1
    "refreshSeconds": (int,   60,   5,   86400),  # don't hammer the API
    "maxQueueSize":   (int,   0,    0,   10 ** 7),
}
_LIST_SETTINGS   = ("alertQueues", "hiddenQueues", "skipKeywords",
                    "reportUsers")
_STRING_SETTINGS = ("slaField", "completedTodayFilter", "trayBadgeQueue",
                    "alertQueue", "serviceDeskName", "projectKey",
                    "snapCorner", "reportRange", "reportBasis")


# ── Credential protection (DPAPI) ────────────────────────────────────────
# The API token used to sit in jira_config.json as plain text — readable by
# anything else running as this Windows user, or by copying the file off the
# machine entirely. DPAPI encrypts it to a blob only this Windows login can
# decrypt again; CryptProtectData/CryptUnprotectData ship in every Windows
# install's own crypt32.dll, so this needs no key to manage and no new
# dependency. domain/email stay plain text — neither grants access on its
# own, and email doubles as a visible identity elsewhere in the app.
class _DPAPI_BLOB(ctypes.Structure):
    _fields_ = [("cbData", ctypes.c_uint32),
                ("pbData", ctypes.POINTER(ctypes.c_char))]


def _dpapi_call(fn, data: bytes) -> bytes:
    buf = ctypes.create_string_buffer(data, len(data))
    inblob  = _DPAPI_BLOB(len(data),
                          ctypes.cast(buf, ctypes.POINTER(ctypes.c_char)))
    outblob = _DPAPI_BLOB()
    if not fn(ctypes.byref(inblob), None, None, None, None, 0,
             ctypes.byref(outblob)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return ctypes.string_at(outblob.pbData, outblob.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(outblob.pbData)


def protect_secret(plaintext: str) -> str:
    """Encrypt for storage, tied to this Windows login. Base64, JSON-safe."""
    encrypted = _dpapi_call(ctypes.windll.crypt32.CryptProtectData,
                            plaintext.encode("utf-8"))
    return base64.b64encode(encrypted).decode("ascii")


def unprotect_secret(encoded: str) -> Optional[str]:
    """Reverse of protect_secret. None on any failure — a blob encrypted
    under a different Windows profile (a moved or restored config file),
    or simply corrupted — rather than raising into a caller with nowhere
    good to show a decryption error. The caller treats that the same as
    "no token", which the existing sign-in failure handling already
    reports clearly instead of silently.
    """
    try:
        decrypted = _dpapi_call(ctypes.windll.crypt32.CryptUnprotectData,
                                base64.b64decode(encoded))
        return decrypted.decode("utf-8")
    except Exception:
        return None


def normalise_config(cfg: dict) -> dict:
    """Make a config safe to use, whatever is actually in the file."""
    for key, (cast, default, low, high) in _NUMERIC_SETTINGS.items():
        if key not in cfg:
            continue
        try:
            cfg[key] = min(high, max(low, cast(cfg[key])))
        except (TypeError, ValueError):
            cfg[key] = default
    for key in _LIST_SETTINGS:
        if key in cfg and not isinstance(cfg[key], list):
            # A bare string is the likely hand-edit; keep it as one entry
            # rather than silently discarding what the user meant.
            cfg[key] = [cfg[key]] if isinstance(cfg[key], str) and cfg[key] \
                else []
    for key in _STRING_SETTINGS:
        if key in cfg and not isinstance(cfg[key], str):
            cfg[key] = "" if cfg[key] is None else str(cfg[key])
    # projectKey is interpolated straight into JQL as an identifier
    # (`project = {pk}`), not a quoted value jql_quote() could protect —
    # there's no quoting syntax for "this is a field/identifier", so the
    # only defence is restricting it to what a real Jira project key can
    # ever contain. Config is meant to be hand-editable; an unvalidated
    # identifier reaching a query is the same class of hole as an
    # unvalidated column or table name in SQL. Blanked rather than
    # rejected outright: absent is already a state the rest of the app
    # recovers from on its own (it re-discovers the project from the
    # service desk), so this degrades the same way a missing value always
    # has, instead of opening a new failure mode.
    if "projectKey" in cfg and not _SAFE_PROJECT_KEY.match(cfg["projectKey"]):
        cfg["projectKey"] = ""
    return cfg


def load_config() -> Optional[dict]:
    try:
        with open(CONFIG_FILE, encoding="utf-8") as f:
            loaded = json.load(f)
        if not isinstance(loaded, dict):
            return None
        needs_migration = "token" in loaded and "tokenProtected" not in loaded
        if "tokenProtected" in loaded:
            # None on failure (a blob from a different Windows profile, or a
            # corrupted value) becomes an empty token rather than a crash —
            # sign-in then fails with the app's normal, visible bad-token
            # handling instead of a silent wrong value.
            loaded["token"] = unprotect_secret(loaded.pop("tokenProtected")) or ""
        loaded = normalise_config(loaded)
        if needs_migration and loaded.get("token"):
            # Get the plain-text token off disk as soon as it's seen, not
            # whenever the user next happens to change a setting.
            save_config(loaded)
        return loaded
    except FileNotFoundError:
        return None
    except (json.JSONDecodeError, OSError):
        # Corrupt config — move it aside so setup can run again instead of crashing
        try:
            os.replace(CONFIG_FILE, CONFIG_FILE + ".bad")
        except OSError:
            pass
        return None


# Settings this app owns. Anything else in the config file belongs to another
# tool (the triage learner writes what it discovers there) and is preserved on
# save. Listing them explicitly — rather than merging whatever is on disk —
# means keys the overlay deliberately *removes*, such as lastX/lastY when the
# window is snapped to a corner, still get removed.
_OVERLAY_KEYS = {
    "domain", "email", "token",
    "serviceDeskId", "serviceDeskName", "projectKey",
    "alertQueue", "alertQueues", "trayBadgeQueue", "hiddenQueues",
    "skipKeywords", "maxQueueSize", "refreshSeconds", "alertQueuesAsked",
    "notificationsEnabled", "soundEnabled",
    "alpha", "overlayWidth", "alwaysVisible",
    "slaField", "slaEnabled",
    "reportUsers", "reportRange", "reportBasis",
    "completedTodayFilter", "suggestionsFile",
    "lastX", "lastY", "snapCorner",
}

# Settings that identify the install. A save may *replace* these, but must
# never drop them: they are the ones a user cannot reconstruct, and losing
# them turns a working install into "No accessible service desk found" with
# no way back except typing the credentials in again. Everything else is
# either rediscovered or a preference worth re-setting.
_IDENTITY_KEYS = {"domain", "email", "token",
                  "serviceDeskId", "serviceDeskName", "projectKey"}


def normalise_domain(raw: str) -> str:
    """Reduce whatever the user pasted to a bare Jira host.

    People copy the address out of their browser, so the field regularly
    arrives as a full URL - often a link to a specific ticket. Keeping the
    path turned every later request into a 404 with nothing to explain it,
    so take the host and discard the rest. Returns "" if there is no host
    to be found, which the caller reports rather than saving.
    """
    text = (raw or "").strip().strip("/")
    if not text:
        return ""
    if "//" not in text:
        text = "//" + text          # urlsplit needs a scheme to find a host
    host = urllib.parse.urlsplit(text).netloc or ""
    host = host.split("@")[-1]      # drop any user:pass@
    if host.endswith(":443"):
        host = host[:-4]
    host = host.strip().lower()
    # A host has at least one dot and no path, space or wildcard in it.
    if "." not in host or any(c in host for c in ' /\\?#'):
        return ""
    return host


def save_config(cfg: dict) -> None:
    """Atomic write — a crash mid-save can't corrupt the existing config.

    Only this app's own keys are rewritten. The overlay holds its config in
    memory from startup, so a blind dump would silently delete anything
    another process had added since — dragging the window was enough to wipe
    the learner's discovered workflow settings.

    Keys absent from `cfg` are treated as deliberately removed, which is how
    lastX/lastY disappear when the window snaps to a corner. The identity
    settings are exempt: a caller passing an incomplete dict must not be able
    to destroy the credentials, because nothing can recover them.
    """
    try:
        with open(CONFIG_FILE, encoding="utf-8") as f:
            merged = json.load(f)
        if not isinstance(merged, dict):
            merged = {}
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        merged = {}
    for key in _OVERLAY_KEYS - _IDENTITY_KEYS:
        merged.pop(key, None)
    merged.update(cfg)

    # The token is never written as plain text. A caller that explicitly
    # supplied one (including "" to clear it) sets tokenProtected fresh; a
    # caller that didn't must not be treated as clearing it — that's the
    # same "absent means preserved" identity-key contract as everything
    # else here — so a not-yet-migrated plain-text token already on disk is
    # encrypted in place instead of just being dropped.
    if "token" in cfg:
        token = cfg.get("token") or ""
        merged.pop("token", None)
        if token:
            merged["tokenProtected"] = protect_secret(token)
        else:
            merged.pop("tokenProtected", None)
    elif "token" in merged:
        legacy = merged.pop("token")
        if legacy:
            merged["tokenProtected"] = protect_secret(legacy)

    # Keep the last good copy. This file holds an API token and a service
    # desk id that nobody can reconstruct from memory, and a bad write is
    # only ever noticed after the fact.
    try:
        if os.path.exists(CONFIG_FILE):
            shutil.copy2(CONFIG_FILE, CONFIG_FILE + ".bak")
    except OSError:
        pass

    # mkstemp, not open(path + ".tmp", "w"): the latter follows whatever
    # already sits at that name - a symlink or a hard link, which NTFS
    # lets an unprivileged local process create - and would silently write
    # this file's contents (including the encrypted token) through it into
    # somewhere else entirely. A fixed, predictable name made that easy to
    # pre-plant; mkstemp's O_CREAT|O_EXCL fails outright if anything is
    # already there instead of writing through it.
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(CONFIG_FILE) or ".",
                               prefix=os.path.basename(CONFIG_FILE) + ".",
                               suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(merged, f, indent=2)
        os.replace(tmp, CONFIG_FILE)
    finally:
        try:
            if os.path.exists(tmp):
                os.unlink(tmp)
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Windows helpers
# ---------------------------------------------------------------------------

def _xml_escape(text: str) -> str:
    """Make text safe to drop into the toast's XML.

    As well as the five markup characters, control characters have to go:
    XML 1.0 permits only tab, newline and carriage return below U+0020, and
    ticket summaries do pick the others up from pasted console output or
    email. One of them makes the whole document malformed, and because the
    toast script runs with SilentlyContinue the only symptom is a
    notification that never arrives.
    """
    escaped = (text
               .replace("&", "&amp;")
               .replace("<", "&lt;")
               .replace(">", "&gt;")
               .replace('"', "&quot;")
               .replace("'", "&apos;"))
    return "".join(ch for ch in escaped if _XML_OK(ord(ch)))


def _XML_OK(cp: int) -> bool:
    """True for a code point XML 1.0 allows in character data."""
    return (cp in (0x09, 0x0A, 0x0D)
            or 0x20 <= cp <= 0xD7FF
            or 0xE000 <= cp <= 0xFFFD
            or 0x10000 <= cp <= 0x10FFFF)


def send_toast(title: str, body: str) -> None:
    """Fire a Windows 10/11 toast notification via PowerShell WinRT."""
    t, b = _xml_escape(title), _xml_escape(body)
    ps = f"""
$ErrorActionPreference = 'SilentlyContinue'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
$id = '{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe'
$doc = New-Object Windows.Data.Xml.Dom.XmlDocument
$doc.LoadXml('<toast duration="short"><visual><binding template="ToastGeneric"><text hint-maxLines="1">{t}</text><text>{b}</text></binding></visual></toast>')
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($id).Show((New-Object Windows.UI.Notifications.ToastNotification($doc)))
"""
    try:
        subprocess.Popen(
            [_system_binary(r"WindowsPowerShell\v1.0\powershell.exe"),
             "-WindowStyle", "Hidden", "-NonInteractive", "-Command", ps],
            creationflags=0x08000000,
        )
    except OSError:
        # powershell.exe can be missing or blocked on a locked-down machine.
        # This runs inline inside _update_ui, so a failure here must not cost
        # the rest of that repaint — same reasoning as play_alert() below.
        pass


def play_alert() -> None:
    try:
        winsound.MessageBeep(winsound.MB_ICONEXCLAMATION)
    except Exception:
        pass


_INSTANCE_MUTEX: Optional[int] = None
_INSTANCE_NAME  = "JiraOverlaySingleInstance"


def acquire_single_instance() -> bool:
    """Claim the single-instance mutex; False if an overlay already runs.

    Without this it was easy to end up with two overlays (a stale one plus a
    freshly launched one), each polling Jira and each firing its own toast
    for every new ticket.
    """
    global _INSTANCE_MUTEX
    try:
        k32 = ctypes.windll.kernel32
        k32.CreateMutexW.restype  = ctypes.c_void_p
        k32.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_bool,
                                     ctypes.c_wchar_p]
        handle = k32.CreateMutexW(None, False, _INSTANCE_NAME)
        if k32.GetLastError() == 183:          # ERROR_ALREADY_EXISTS
            if handle:
                k32.CloseHandle(ctypes.c_void_p(handle))
            return False
        _INSTANCE_MUTEX = handle
        return True
    except Exception:
        return True      # never let the guard itself block startup


def release_single_instance() -> None:
    """Drop the mutex so a deliberate relaunch isn't locked out by itself."""
    global _INSTANCE_MUTEX
    if _INSTANCE_MUTEX:
        try:
            ctypes.windll.kernel32.CloseHandle(ctypes.c_void_p(_INSTANCE_MUTEX))
        except Exception:
            pass
        _INSTANCE_MUTEX = None


def _startup_entries() -> dict[str, str]:
    """Registry value name -> script, for every part of the app.

    The overlay and the triage learner are one product, so the single
    "Run on Windows startup" switch has to cover both — registering only the
    overlay left the learner silently not starting, and its suggestions
    quietly absent.
    """
    here = os.path.dirname(os.path.abspath(__file__))
    entries = {STARTUP_NAME: os.path.abspath(__file__)}
    learner = os.path.join(here, "triage_learner.pyw")
    if os.path.exists(learner):
        entries[LEARNER_STARTUP_NAME] = learner
    return entries


def startup_enabled() -> bool:
    """True when everything present is registered to start with Windows."""
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, STARTUP_REG, 0,
                            winreg.KEY_READ) as k:
            for name in _startup_entries():
                try:
                    winreg.QueryValueEx(k, name)
                except FileNotFoundError:
                    return False
        return True
    except OSError:
        return False


def _pythonw_exe() -> str:
    """Absolute path to pythonw.exe (falls back to the current interpreter)."""
    exe = sys.executable
    head, tail = os.path.split(exe)
    if tail.lower() == "python.exe":
        cand = os.path.join(head, "pythonw.exe")
        if os.path.exists(cand):
            return cand
    return exe


def set_startup(enable: bool) -> None:
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, STARTUP_REG, 0,
                            winreg.KEY_SET_VALUE) as k:
            for name, script in _startup_entries().items():
                if enable:
                    winreg.SetValueEx(k, name, 0, winreg.REG_SZ,
                                      f'"{_pythonw_exe()}" "{script}"')
                else:
                    try:
                        winreg.DeleteValue(k, name)
                    except FileNotFoundError:
                        pass
    except OSError:
        pass


class _MONITORINFO(ctypes.Structure):
    """Maps to the Windows MONITORINFO struct used by GetMonitorInfoW."""
    _fields_ = [
        ("cbSize",    ctypes.c_uint32),
        ("rcMonitor", ctypes.c_int32 * 4),
        ("rcWork",    ctypes.c_int32 * 4),
        ("dwFlags",   ctypes.c_uint32),
    ]


def get_monitor_rect(x: int, y: int,
                     work: bool = False) -> Optional[tuple[int, int, int, int]]:
    """Return (left, top, right, bottom) of the monitor nearest to (x, y).

    With work=True the work area is returned (excludes taskbar).
    """
    try:
        pt  = ctypes.wintypes.POINT(x, y)
        mon = ctypes.windll.user32.MonitorFromPoint(pt, 2)  # MONITOR_DEFAULTTONEAREST
        info = _MONITORINFO()
        info.cbSize = ctypes.sizeof(_MONITORINFO)
        if ctypes.windll.user32.GetMonitorInfoW(mon, ctypes.byref(info)):
            left, top, right, bottom = (info.rcWork if work
                                        else info.rcMonitor)
            return left, top, right, bottom
    except Exception:
        pass
    return None


# ---------------------------------------------------------------------------
# Tray icon image builder
# ---------------------------------------------------------------------------

@functools.lru_cache(maxsize=8)
def _make_tray_image(count: int, alert: bool) -> Image.Image:
    """Render a circular badge icon; result cached by (count, alert) key."""
    size  = 128
    img   = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw  = ImageDraw.Draw(img)
    color = (220, 80, 60) if alert else (0, 82, 204)
    draw.ellipse([2, 2, size - 2, size - 2], fill=color)
    text  = str(count) if count < 100 else "99+"
    font  = _get_tray_font()
    bbox  = draw.textbbox((0, 0), text, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    draw.text(
        ((size - tw) // 2 - bbox[0], (size - th) // 2 - bbox[1]),
        text, fill=(255, 255, 255), font=font,
    )
    return img


# ---------------------------------------------------------------------------
# Dark-themed tooltip (always appears above the target widget)
# ---------------------------------------------------------------------------

class _Tooltip:
    """Styled tooltip that appears above its widget after a short hover delay."""

    def __init__(self, widget: tk.Widget, text: str, delay: int = 600) -> None:
        self._widget = widget
        self._text   = text
        self._delay  = delay
        self._job:  Optional[str]         = None
        self._tip:  Optional[tk.Toplevel] = None
        widget.bind("<Enter>",        self._schedule, add=True)
        widget.bind("<Leave>",        self._cancel,   add=True)
        widget.bind("<ButtonPress-1>", self._cancel,  add=True)

    def _schedule(self, _e=None) -> None:
        self._cancel()
        self._job = self._widget.after(self._delay, self._show)

    def _cancel(self, _e=None) -> None:
        if self._job:
            self._widget.after_cancel(self._job)
            self._job = None
        if self._tip:
            self._tip.destroy()
            self._tip = None

    def _show(self) -> None:
        if self._tip:
            return
        tip = tk.Toplevel(self._widget)
        tip.overrideredirect(True)
        tip.attributes("-topmost", True)
        tip.configure(bg=_C["border"])          # 1-px accent-colour border

        tk.Label(tip, text=self._text,
                 font=_font(8),
                 fg=_C["fg_bright"], bg=_C["input"],
                 padx=8, pady=5).pack(padx=1, pady=1)

        tip.update_idletasks()
        tw = tip.winfo_reqwidth()
        th = tip.winfo_reqheight()
        wx = self._widget.winfo_rootx()
        wy = self._widget.winfo_rooty()
        ww = self._widget.winfo_width()
        sw = tip.winfo_screenwidth()

        # Centre horizontally over the widget; always place ABOVE it
        x = max(4, min(wx + (ww - tw) // 2, sw - tw - 4))
        y = max(4, wy - th - 6)

        tip.geometry(f"+{x}+{y}")
        _round_corners(tip, small=True)
        self._tip = tip


# ---------------------------------------------------------------------------
# Setup window
# ---------------------------------------------------------------------------

class SetupWindow:
    def __init__(self) -> None:
        self.root   = tk.Tk()
        self.result: Optional[dict] = None
        self.root.title("Jira Overlay — Setup")
        self.root.configure(bg=_C["bg_dark"])
        self.root.resizable(False, False)
        self._build()
        self.root.mainloop()

    def _build(self) -> None:
        self.root.update_idletasks()
        sw, sh = self.root.winfo_screenwidth(), self.root.winfo_screenheight()
        self.root.geometry(f"430x320+{sw // 2 - 215}+{sh // 2 - 160}")
        _round_corners(self.root)

        tk.Label(self.root, text="Jira Overlay Setup",
                 font=_font(14, "bold"),
                 fg=_C["fg_bright"], bg=_C["bg_dark"]
                 ).grid(row=0, column=0, columnspan=2, pady=(20, 16))

        fields = [
            ("Jira Domain", "yourcompany.atlassian.net", False),
            ("Email",       "you@company.com",           False),
            ("API Token",   "paste your token here",     True),
        ]
        self._vars: dict[str, tuple[tk.StringVar, str]] = {}
        for i, (label, hint, secret) in enumerate(fields, 1):
            tk.Label(self.root, text=f"{label}:", font=_font(9),
                     fg=_C["fg"], bg=_C["bg_dark"], anchor="e"
                     ).grid(row=i, column=0, sticky="e", padx=(20, 8), pady=6)
            var = tk.StringVar()
            entry = tk.Entry(self.root, textvariable=var, width=36,
                             show="*" if secret else "",
                             bg=_C["input"], fg=_C["fg_dim"],
                             insertbackground="white", relief="flat",
                             font=_font(9))
            entry.grid(row=i, column=1, sticky="ew", padx=(0, 20), pady=6, ipady=5)
            entry.insert(0, hint)
            entry.bind("<FocusIn>",
                       lambda ev, w=entry, h=hint:
                           (w.get() == h) and (w.delete(0, "end") or w.config(fg=_C["fg_bright"])))
            entry.bind("<FocusOut>",
                       lambda ev, w=entry, h=hint:
                           (not w.get()) and (w.insert(0, h) or w.config(fg=_C["fg_dim"])))
            self._vars[label] = (var, hint)

        token_url = "https://id.atlassian.com/manage-profile/security/api-tokens"
        # No leading icon: the underline + accent colour already say "link"
        # on their own, and a fixed-colour emoji glyph here couldn't follow
        # the hover colour change the text itself gets below.
        link = tk.Label(self.root,
                        text="Generate an API token at id.atlassian.com",
                        font=_font(8, "underline"),
                        fg=_C["accent"], bg=_C["bg_dark"], cursor="hand2")
        link.grid(row=4, column=0, columnspan=2, pady=(0, 8))
        link.bind("<ButtonRelease-1>", lambda _e: webbrowser.open(token_url))
        link.bind("<Enter>", lambda _e: link.config(fg=_C["accent_hi"]))
        link.bind("<Leave>", lambda _e: link.config(fg=_C["accent"]))

        tk.Button(self.root, text="Save & Launch", command=self._save,
                  bg=_C["accent"], fg="white", font=_font(10, "bold"),
                  relief="flat", padx=16, pady=8, cursor="hand2",
                  activebackground=_C["accent_hi"], activeforeground="white"
                  ).grid(row=5, column=0, columnspan=2, pady=(4, 4))
        self._status = tk.Label(self.root, text="", font=_font(8),
                                fg=_C["fg_dim"], bg=_C["bg_dark"])
        self._status.grid(row=6, column=0, columnspan=2, pady=(0, 14))
        self.root.columnconfigure(1, weight=1)

    def _save(self) -> None:
        values: dict[str, str] = {}
        for label, (var, hint) in self._vars.items():
            v = var.get().strip()
            if not v or v == hint:
                messagebox.showerror("Missing", f"{label} is required.", parent=self.root)
                return
            values[label] = v
        domain = normalise_domain(values["Jira Domain"])
        if not domain:
            messagebox.showerror(
                "Jira Domain",
                "That doesn't look like a Jira address.\n\n"
                "It should be just the host, like:\n"
                "    yourcompany.atlassian.net",
                parent=self.root)
            return
        candidate = {"domain": domain, "email": values["Email"],
                     "token": values["API Token"]}
        problem = self._check_credentials(candidate)
        if problem:
            messagebox.showerror("Could not sign in", problem, parent=self.root)
            return
        self.result = candidate
        save_config(self.result)
        self.root.destroy()

    def _check_credentials(self, cfg: dict) -> str:
        """Try the credentials now; return a problem description, or "".

        Without this the first sign a token is wrong is a two-word status
        line some minutes later, by which point the user has moved on and
        has no idea which of the three fields is at fault.
        """
        self._busy("Checking your details...")
        try:
            r = requests.get(
                f"https://{cfg['domain']}/rest/api/3/myself",
                auth=HTTPBasicAuth(cfg["email"], cfg["token"]),
                headers={"Accept": "application/json"}, timeout=15)
        except requests.RequestException as exc:
            return (f"Could not reach {cfg['domain']}.\n\n"
                    "Check the domain and your internet connection.\n\n"
                    f"({type(exc).__name__})")
        finally:
            self._busy("")

        if r.status_code in (401, 403):
            return ("Jira rejected the email or API token.\n\n"
                    "The token must belong to the same account as the email, "
                    "and tokens expire - create a fresh one if in doubt.")
        if r.status_code == 404:
            return (f"{cfg['domain']} answered, but doesn't look like a Jira "
                    "site.\n\nCheck the domain.")
        if not r.ok:
            return f"Jira returned HTTP {r.status_code} when signing in."
        try:
            who = r.json().get("displayName") or cfg["email"]
        except ValueError:
            who = cfg["email"]
        messagebox.showinfo("Signed in", f"Connected to Jira as {who}.",
                            parent=self.root)
        return ""

    def _busy(self, text: str) -> None:
        """Show progress while the sign-in check runs, since it blocks."""
        try:
            self._status.config(text=text)
            self.root.update()
        except tk.TclError:
            pass


# ---------------------------------------------------------------------------
# Service desk picker
# ---------------------------------------------------------------------------

class PickDeskWindow:
    """Service-desk chooser, shown when an account can see more than one.

    Takes a parent when the overlay is already running. Creating a second
    tk.Tk() would spin up an entirely separate Tcl interpreter and nest its
    mainloop inside the overlay's — this dialog appears during first-run
    discovery, which is exactly when that is true.
    """

    def __init__(self, desks: list[dict],
                 parent: Optional[tk.Misc] = None) -> None:
        self._owns_root = parent is None
        self.root = tk.Tk() if self._owns_root else tk.Toplevel(parent)
        self.result: Optional[dict] = None
        self.root.title("Jira Overlay — Pick Desk")
        self.root.configure(bg=_C["bg_dark"])
        self.root.resizable(False, False)
        self._build(desks)
        if self._owns_root:
            self.root.mainloop()
        else:
            self.root.transient(parent)          # type: ignore[arg-type]
            self.root.grab_set()
            self.root.focus_force()
            parent.wait_window(self.root)        # type: ignore[union-attr]

    def _build(self, desks: list[dict]) -> None:
        sw, sh = self.root.winfo_screenwidth(), self.root.winfo_screenheight()
        self.root.geometry(f"360x{90 + len(desks) * 44}+{sw // 2 - 180}+{sh // 2 - 100}")
        _round_corners(self.root)
        tk.Label(self.root, text="Which service desk to monitor?",
                 font=_font(11, "bold"),
                 fg=_C["fg_bright"], bg=_C["bg_dark"]).pack(pady=(18, 10))
        for desk in desks:
            tk.Button(self.root,
                      text=f"{desk['projectName']}  ({desk['projectKey']})",
                      command=lambda d=desk: self._pick(d),
                      bg=_C["input"], fg=_C["fg_bright"], font=_font(10),
                      relief="flat", padx=12, pady=8, cursor="hand2",
                      activebackground=_C["accent"], activeforeground="white"
                      ).pack(fill="x", padx=24, pady=4)

    def _pick(self, desk: dict) -> None:
        self.result = desk
        self.root.destroy()


# ---------------------------------------------------------------------------
# Team report — who completed the most, over a period you choose
# ---------------------------------------------------------------------------

# label -> how many days back from today; None means "pick the dates yourself"
REPORT_RANGES = [("Today", 0), ("7 days", 6), ("30 days", 29),
                 ("90 days", 89), ("Custom", None)]

# Which Jira date a ticket is counted against.
#   resolved — when it was actually finished. Accurate, but only populated if
#              the workflow sets a resolution.
#   updated  — last touched. Always present, and what the overlay's
#              "Completed today" figure already uses, so the two reconcile.
REPORT_BASES = [("Resolved date", "resolved"), ("Last updated", "updated")]


def csv_safe(value: str) -> str:
    """Neutralise a value that could be read as a spreadsheet formula.

    Display names are set by the people they belong to, and a name starting
    with =, +, -, @ or a tab is a valid Jira display name that Excel will
    still try to evaluate as a formula when the exported CSV is opened —
    "CSV injection", OWASP-documented and not merely theoretical: some Excel
    configurations still honour the old DDE syntax inside such a formula.
    Prefixing a single quote is the standard mitigation; Excel shows the
    text as-is and drops the quote rather than evaluating it.
    """
    text = str(value)
    return "'" + text if text and text[0] in "=+-@\t\r" else text


def _has_association(ext: str) -> bool:
    """Whether Windows has anything registered to open `ext`.

    os.startfile() (ShellExecute under the hood) doesn't raise when there is
    no association — on a machine with no spreadsheet app, exporting the
    report just silently does nothing: no new window, no exception, nothing
    a background process could log. Checking first is the only way to catch
    that before the button click does nothing.
    """
    try:
        with winreg.OpenKey(
                winreg.HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts"
                rf"\{ext}\UserChoice") as k:
            if winreg.QueryValueEx(k, "ProgId")[0]:
                return True
    except OSError:
        pass
    try:
        with winreg.OpenKey(winreg.HKEY_CLASSES_ROOT, ext) as k:
            return bool(winreg.QueryValueEx(k, "")[0])
    except OSError:
        return False


def _system_binary(relative_path: str) -> str:
    """Fully-qualified path to a well-known Windows system binary.

    Launching by bare name ("notepad.exe", "powershell") lets Windows'
    normal search order run whatever answers to that name first — a
    directory earlier in PATH than System32, planted deliberately or by
    another install, runs instead of the real thing. Falls back to the
    bare name only if System32 genuinely doesn't have it there, so an
    unusual Windows install still works rather than silently doing nothing.
    """
    candidate = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"),
                             "System32", relative_path)
    return candidate if os.path.exists(candidate) else relative_path


def open_in_default_or_notepad(path: str) -> None:
    """Open `path` with whatever Windows has registered for it, falling back
    to Notepad — present on every Windows install by definition — rather
    than risk this silently doing nothing on a machine with no spreadsheet
    app associated with .csv.
    """
    _, ext = os.path.splitext(path)
    if _has_association(ext):
        try:
            os.startfile(path)
            return
        except OSError:
            pass
    try:
        subprocess.Popen([_system_binary("notepad.exe"), path])
    except OSError:
        pass


def jql_quote(value: str) -> str:
    """Quote a value for use as a JQL string literal.

    Display names are set by the people they belong to, so they really do
    contain quotes and backslashes. Interpolated raw, a name like O"Brien
    breaks the query outright, and a crafted one changes what it matches -
    the count is read-only, but it would be counting something other than
    what was asked for.
    """
    escaped = (value or "").replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def report_users(cfg: dict) -> list[dict]:
    """People to report on, as [{"name":…, "accountId":…}].

    Accepts a plain list of names too, so a hand-edited config keeps working.
    Falls back to parsing the completed-today filter, which is where an
    existing install already lists its team.
    """
    def dedupe(people: list[dict]) -> list[dict]:
        """One entry per human.

        The same person can legitimately end up listed twice — seeded from
        the completed-today filter and then also ticked from Jira, or simply
        hand-edited. Counted twice they get a double-length bar, inflate the
        total, and push the coverage figure above 100%, which is nonsense on
        its face. Matched on accountId where there is one, since that is the
        only identifier two entries for one person are guaranteed to share.
        """
        # A name that appears anywhere with an accountId is that person, so a
        # bare-name entry for the same name is the same human listed twice —
        # which is exactly what happens when a list seeded from the filter is
        # later topped up from Jira.
        identified = {p["name"].casefold() for p in people if p["accountId"]}
        seen: set = set()
        unique = []
        for person in people:
            name = person["name"].casefold()
            if not person["accountId"] and name in identified:
                continue
            key = person["accountId"] or f"name:{name}"
            if key in seen:
                continue
            seen.add(key)
            unique.append(person)
        return unique

    raw = cfg.get("reportUsers")
    if isinstance(raw, list) and raw:
        out = []
        for item in raw:
            if isinstance(item, str) and item.strip():
                out.append({"name": item, "accountId": ""})
            elif isinstance(item, dict) and item.get("name"):
                out.append({"name": str(item["name"]),
                            "accountId": str(item.get("accountId") or "")})
        return dedupe(out)
    # Nothing configured — seed from `assignee in ("A", "B")` if it's there.
    found = re.findall(r'"([^"]+)"',
                       cfg.get("completedTodayFilter", "") or "")
    return dedupe([{"name": n, "accountId": ""} for n in found])


class TeamReportWindow:
    """Ranked count of completed tickets per person over a chosen period.

    Counts are one cheap approximate-count per person rather than fetching
    issues, and they run on a worker thread so the window stays responsive.
    """

    def __init__(self, app: "JiraOverlay") -> None:
        self.app  = app
        self.rows: dict[str, dict] = {}
        self.results: list[tuple[str, Optional[int]]] = []
        self._seq = 0            # ignore results from a superseded request
        self._closed = False

        cfg = app.config
        self.win = tk.Toplevel(app.root)
        self.win.title("Jira Overlay — Team Report")
        self.win.configure(bg=_C["bg"])
        self.win.minsize(460, 380)
        sw, sh = self.win.winfo_screenwidth(), self.win.winfo_screenheight()
        self.win.geometry(f"520x560+{sw // 2 - 260}+{max(20, sh // 2 - 300)}")
        self.win.protocol("WM_DELETE_WINDOW", self._close)
        _round_corners(self.win)

        self.v_range = tk.StringVar(value=cfg.get("reportRange", "30 days"))
        self.v_basis = tk.StringVar(value=cfg.get("reportBasis", "resolved"))
        today = date.today()
        self.v_from  = tk.StringVar(value=str(today - timedelta(days=29)))
        self.v_to    = tk.StringVar(value=str(today))
        self.people  = report_users(cfg)

        self._build()
        self.refresh()

    # -- construction ------------------------------------------------------

    def _build(self) -> None:
        BG = _C["bg"]
        outer = tk.Frame(self.win, bg=BG, padx=18, pady=16)
        outer.pack(fill="both", expand=True)

        tk.Label(outer, text=_track("TEAM REPORT"),
                 font=_font(9, "bold", display=True),
                 fg=_C["accent"], bg=BG).pack(anchor="w")
        self.lbl_sub = tk.Label(outer, text="", font=_font(9),
                                fg=_C["fg_dim"], bg=BG, anchor="w")
        self.lbl_sub.pack(anchor="w", pady=(2, 10))

        # Period
        rng = self._range_row = tk.Frame(outer, bg=BG)
        rng.pack(fill="x")
        for label, _days in REPORT_RANGES:
            # activebackground matching bg made hover a no-op here too -
            # same fix as the Settings checkboxes/radios.
            rb = tk.Radiobutton(rng, text=label, value=label, variable=self.v_range,
                               command=self._range_changed,
                               font=_font(9), fg=_C["fg"], bg=BG,
                               selectcolor=_C["input"], activebackground=_C["chip"],
                               activeforeground=_C["fg_bright"],
                               highlightthickness=0, bd=0)
            rb.pack(side="left", padx=(0, 8))
            rb.bind("<Enter>", lambda _e, w=rb: w.config(bg=_C["chip"]))
            rb.bind("<Leave>", lambda _e, w=rb: w.config(bg=BG))

        self.custom = tk.Frame(outer, bg=BG)
        for text, var in (("From", self.v_from), ("To", self.v_to)):
            tk.Label(self.custom, text=text, font=_font(8), fg=_C["fg_dim"],
                     bg=BG).pack(side="left", padx=(0, 4))
            e = tk.Entry(self.custom, textvariable=var, width=11,
                         bg=_C["input"], fg=_C["fg_bright"], relief="flat",
                         insertbackground="white", font=_font(9))
            e.pack(side="left", padx=(0, 10), ipady=2)
            e.bind("<Return>", lambda _e: self.refresh())
        tk.Label(self.custom, text="yyyy-mm-dd", font=_font(8),
                 fg=_C["fg_dim"], bg=BG).pack(side="left")

        # Basis
        basis = tk.Frame(outer, bg=BG)
        basis.pack(fill="x", pady=(10, 0))
        tk.Label(basis, text="Count by", font=_font(8), fg=_C["fg_dim"],
                 bg=BG).pack(side="left", padx=(0, 8))
        for label, value in REPORT_BASES:
            rb = tk.Radiobutton(basis, text=label, value=value,
                               variable=self.v_basis, command=self.refresh,
                               font=_font(9), fg=_C["fg"], bg=BG,
                               selectcolor=_C["input"], activebackground=_C["chip"],
                               activeforeground=_C["fg_bright"],
                               highlightthickness=0, bd=0)
            rb.pack(side="left", padx=(0, 8))
            rb.bind("<Enter>", lambda _e, w=rb: w.config(bg=_C["chip"]))
            rb.bind("<Leave>", lambda _e, w=rb: w.config(bg=BG))

        tk.Frame(outer, bg=_C["divider"], height=1).pack(fill="x", pady=12)

        # Pack the footer against the bottom *before* the results area claims
        # the remaining space, so a long list can never push the buttons off
        # the edge of the window.
        foot = tk.Frame(outer, bg=BG)
        foot.pack(side="bottom", fill="x", pady=(8, 0))
        tk.Frame(outer, bg=_C["divider"], height=1).pack(side="bottom",
                                                         fill="x", pady=(12, 0))

        self.results_frame = tk.Frame(outer, bg=BG)
        self.results_frame.pack(fill="both", expand=True)
        for text, cmd in (("People…", self._edit_people),
                          ("Export CSV", self._export),
                          ("Refresh", self.refresh)):
            b = tk.Label(foot, text=text, font=_font(9), fg=_C["fg"],
                         bg=_C["input"], cursor="hand2", padx=12, pady=5)
            b.pack(side="left", padx=(0, 8))
            b.bind("<ButtonRelease-1>", lambda _e, c=cmd: c())
            b.bind("<Enter>", lambda _e, w=b: w.config(bg=_C["chip"],
                                                       fg=_C["fg_bright"]))
            b.bind("<Leave>", lambda _e, w=b: w.config(bg=_C["input"],
                                                       fg=_C["fg"]))
        self.lbl_status = tk.Label(foot, text="", font=_font(8),
                                   fg=_C["fg_dim"], bg=BG)
        self.lbl_status.pack(side="right")
        self._range_changed(refresh=False)

    def _range_changed(self, refresh: bool = True) -> None:
        if self.v_range.get() == "Custom":
            # Anchored directly beneath the range buttons: packed without
            # `after`, it lands wherever the packer has got to, which by this
            # point is below the results list.
            self.custom.pack(fill="x", pady=(8, 0), after=self._range_row)
        else:
            self.custom.pack_forget()
        if refresh:
            self.refresh()

    # -- period ------------------------------------------------------------

    def _period(self) -> Optional[tuple[date, date]]:
        """(start, end) inclusive, or None if the typed dates make no sense."""
        label = self.v_range.get()
        days = dict(REPORT_RANGES).get(label)
        if days is not None:
            end = date.today()
            return end - timedelta(days=days), end
        try:
            start = date.fromisoformat(self.v_from.get().strip())
            end   = date.fromisoformat(self.v_to.get().strip())
        except ValueError:
            return None
        return (start, end) if start <= end else (end, start)

    # -- counting ----------------------------------------------------------

    def refresh(self) -> None:
        if self._closed:
            return
        period = self._period()
        if period is None:
            self._set_status("Dates must be yyyy-mm-dd", warn=True)
            return
        start, end = period
        self.lbl_sub.config(
            text=f"{self.app.config.get('serviceDeskName', 'Service desk')}"
                 f"  ·  {start} to {end}")
        if not self.people:
            self._render([], 0)
            self._set_status("No people chosen yet — use People…")
            return

        self._seq += 1
        seq = self._seq
        self._set_status("Counting…")
        threading.Thread(target=self._work, args=(seq, start, end),
                         daemon=True).start()

    def _work(self, seq: int, start: date, end: date) -> None:
        # basis names a JQL FIELD, not a value, so it can't be quoted the
        # way jql_quote() quotes a string literal — the only defence for a
        # bare identifier is restricting it to the fixed set the UI itself
        # ever offers. The UI can't produce anything else, but the config
        # file backing it can: "reportBasis" is user-editable text, and an
        # unvalidated field name interpolated straight into JQL is an
        # injection point exactly like an unvalidated column name in SQL.
        allowed_bases = {v for _label, v in REPORT_BASES}
        basis = self.v_basis.get()
        if basis not in allowed_bases:
            basis = "resolved"
        pk = self.app.config.get("projectKey", "")
        # `end` is inclusive, so compare against the following midnight.
        window = (f'{basis} >= "{start}" AND {basis} < "{end + timedelta(days=1)}"')
        results: list[tuple[str, Optional[int]]] = []
        for person in self.people:
            if self._closed or seq != self._seq:
                return
            if not (person.get("accountId") or person.get("name")):
                results.append((person.get("name", ""), None))
                continue
            # accountId is opaque and unambiguous; the display name is only a
            # fallback, and must be quoted like any other user-supplied text.
            who = jql_quote(person["accountId"] or person["name"])
            jql = (f'project = {pk} AND statusCategory = Done '
                   f'AND {window} AND assignee = {who}')
            try:
                count, _rejected = self.app._count_issues(jql)
            except Exception:
                count = None
            results.append((person["name"], count))
        total = sum(c for _n, c in results if c is not None)

        # How much of the completed work this ranking actually accounts for.
        # A desk that resolves tickets without assigning them can leave most
        # of its output invisible here, and a ranking presented as though it
        # were the whole picture would be quietly misleading.
        everything = f'project = {pk} AND statusCategory = Done AND {window}'
        try:
            completed, _r = self.app._count_issues(everything)
            unassigned, _r = self.app._count_issues(
                everything + " AND assignee IS EMPTY")
        except Exception:
            completed = unassigned = None
        self.app._post(
            lambda: self._done(seq, results, total, completed, unassigned))

    def _done(self, seq: int, results: list, total: int,
              completed: Optional[int] = None,
              unassigned: Optional[int] = None) -> None:
        if self._closed or seq != self._seq:
            return
        self.results = results
        self._render(results, total)
        failed = [n for n, c in results if c is None]
        if failed:
            self._set_status(f"Could not count: {', '.join(failed)}", warn=True)
            return
        if completed:
            # The per-person counts and the project total are separate
            # queries taken moments apart, so a ticket resolved in between
            # can make the parts add to more than the whole. Report at most
            # 100%: a coverage figure above it is visibly nonsense and
            # discredits the number next to it.
            share = min(100, 100 * total // completed)
            text = f"{total} of {completed} completed ({share}%)"
            if unassigned:
                text += f" · {unassigned} had no assignee"
            # A low share isn't necessarily neglect: on a desk that clones a
            # ticket out to another team's project (see the triage learner's
            # "moved to" outcomes), the master here can go unassigned while
            # the real work — and its assignee — lives on the clone in a
            # project this report was never asked about. The number is
            # shown either way; only the colour hints that it's worth a look.
            self._set_status(text, warn=share < 50)
        else:
            self._set_status(f"{total} completed in total")

    # -- rendering ---------------------------------------------------------

    def _render(self, results: list, total: int) -> None:
        for child in self.results_frame.winfo_children():
            child.destroy()
        if not results:
            tk.Label(self.results_frame,
                     text="Nothing to show yet.", font=_font(9),
                     fg=_C["fg_dim"], bg=_C["bg"]).pack(anchor="w")
            return

        ranked = sorted(results, key=lambda r: (r[1] is None, -(r[1] or 0)))
        best = max((c or 0) for _n, c in ranked) or 1
        for place, (name, count) in enumerate(ranked, 1):
            row = tk.Frame(self.results_frame, bg=_C["bg"])
            row.pack(fill="x", pady=3)
            tk.Label(row, text=f"{place}.", font=_font(9),
                     fg=_C["fg_dim"], bg=_C["bg"], width=3,
                     anchor="w").pack(side="left")
            tk.Label(row, text=name, font=_font(9),
                     fg=_C["fg_bright"] if place == 1 else _C["fg"],
                     bg=_C["bg"], width=20, anchor="w").pack(side="left")
            shown = "—" if count is None else str(count)
            tk.Label(row, text=shown, font=_font(9, "bold"),
                     fg=_C["accent"] if place == 1 else _C["fg"],
                     bg=_C["badge"], padx=8, width=5).pack(side="right")
            share = tk.Label(row,
                             text=f"{100 * (count or 0) // total}%" if total
                                  else "",
                             font=_font(8), fg=_C["fg_dim"], bg=_C["bg"],
                             width=5, anchor="e")
            share.pack(side="right", padx=(0, 8))
            # Proportional bar — the quickest way to read a ranking.
            track = tk.Frame(row, bg=_C["input"], height=6)
            track.pack(side="left", fill="x", expand=True, padx=(8, 8))
            track.pack_propagate(False)
            if count:
                bar = tk.Frame(track,
                               bg=_C["accent"] if place == 1 else _C["info"],
                               height=6)
                bar.place(relwidth=(count / best), relheight=1)

    def _set_status(self, text: str, warn: bool = False) -> None:
        if not self._closed:
            self.lbl_status.config(text=text,
                                   fg=_C["warn"] if warn else _C["fg_dim"])

    # -- actions -----------------------------------------------------------

    def _edit_people(self) -> None:
        picked = _PeoplePicker(self.win, self.app, self.people).result
        if picked is None:
            return
        self.people = picked
        self.app.config["reportUsers"] = picked
        save_config(self.app.config)
        self.refresh()

    def _export(self) -> None:
        if not self.results:
            self._set_status("Nothing to export yet", warn=True)
            return
        period = self._period()
        start, end = period if period else (date.today(), date.today())
        path = os.path.join(
            os.path.dirname(os.path.abspath(__file__)),
            f"team-report-{start}-to-{end}.csv")
        try:
            with open(path, "w", encoding="utf-8", newline="") as f:
                w = csv.writer(f)
                w.writerow(["Person", "Completed", "From", "To", "Counted by"])
                for name, count in sorted(self.results,
                                          key=lambda r: -(r[1] or 0)):
                    w.writerow([csv_safe(name), "" if count is None else count,
                                start, end, self.v_basis.get()])
            self._set_status(f"Saved {os.path.basename(path)}")
        except OSError as exc:
            self._set_status(f"Could not save: {exc}", warn=True)
            return
        open_in_default_or_notepad(path)

    def _close(self) -> None:
        self._closed = True
        cfg = self.app.config
        cfg["reportRange"] = self.v_range.get()
        cfg["reportBasis"] = self.v_basis.get()
        save_config(cfg)
        self.win.destroy()


class _PeoplePicker:
    """Choose which people the report covers, from the project's own users."""

    def __init__(self, parent: tk.Misc, app: "JiraOverlay",
                 current: list[dict]) -> None:
        self.app = app
        self.result: Optional[list[dict]] = None
        self.win = tk.Toplevel(parent)
        self.win.title("Jira Overlay — Report People")
        self.win.configure(bg=_C["bg"])
        self.win.geometry("380x460")
        self.win.transient(parent)
        _round_corners(self.win)

        BG = _C["bg"]
        tk.Label(self.win, text="Tick everyone the report should cover",
                 font=_font(9), fg=_C["fg"], bg=BG).pack(anchor="w",
                                                         padx=16, pady=(14, 8))
        wrap = tk.Frame(self.win, bg=BG)
        # padx=16, matching the label above (and its checkboxes carry no
        # padx of their own) - this used to be 12, leaving every checkbox
        # sitting 4px left of the heading that introduces them.
        wrap.pack(fill="both", expand=True, padx=16)
        canvas = tk.Canvas(wrap, bg=BG, highlightthickness=0)
        sb = tk.Scrollbar(wrap, orient="vertical", command=canvas.yview)
        body = tk.Frame(canvas, bg=BG)
        body.bind("<Configure>",
                  lambda _e: canvas.configure(scrollregion=canvas.bbox("all")))
        cw = canvas.create_window((0, 0), window=body, anchor="nw")
        canvas.bind("<Configure>",
                    lambda e: canvas.itemconfig(cw, width=e.width))
        canvas.configure(yscrollcommand=sb.set)
        canvas.pack(side="left", fill="both", expand=True)
        sb.pack(side="right", fill="y")

        self.vars: list[tuple[tk.BooleanVar, dict]] = []
        people, already_chosen = self._merge(current, self._discover())

        for person in people:
            var = tk.BooleanVar(value=already_chosen(person))
            cb = tk.Checkbutton(body, text=person["name"], variable=var,
                                font=_font(9), fg=_C["fg"], bg=BG,
                                selectcolor=_C["input"], activebackground=_C["chip"],
                                activeforeground=_C["fg_bright"],
                                highlightthickness=0, bd=0, anchor="w")
            cb.pack(fill="x", pady=1)
            cb.bind("<Enter>", lambda _e, w=cb: w.config(bg=_C["chip"]))
            cb.bind("<Leave>", lambda _e, w=cb: w.config(bg=BG))
            self.vars.append((var, person))
        scroll = lambda e: canvas.yview_scroll(-1 * (e.delta // 120), "units")
        _bind_wheel(wrap, scroll)

        if not self.vars:
            tk.Label(body, text="No assignable users came back from Jira.",
                     font=_font(9), fg=_C["fg_dim"], bg=BG).pack(anchor="w")

        foot = tk.Frame(self.win, bg=BG)
        foot.pack(fill="x", padx=16, pady=12)
        tk.Button(foot, text="Save", command=self._save, relief="flat",
                  bg=_C["accent"], fg="white", font=_font(9, "bold"),
                  padx=14, pady=5, cursor="hand2").pack(side="left")
        tk.Button(foot, text="Cancel", command=self.win.destroy,
                  relief="flat", bg=_C["input"], fg=_C["fg"], font=_font(9),
                  padx=14, pady=5, cursor="hand2").pack(side="left", padx=8)

        self.win.grab_set()
        parent.wait_window(self.win)

    @staticmethod
    def _merge(current: list[dict], discovered: list[dict]):
        """Combine the people already chosen with the ones Jira knows about.

        Returns (people to list, predicate saying whether one is chosen).

        The two sources identify people differently: anyone seeded from a
        completed-today filter has a name and no accountId, while Jira always
        supplies an accountId. Keying both as "accountId or name" meant they
        never matched, so every person already chosen was listed twice, once
        ticked and once not. Match on accountId when there is one to match,
        and on name otherwise.
        """
        chosen_ids = {p["accountId"] for p in current if p.get("accountId")}
        chosen_names = {p["name"].strip().lower() for p in current
                        if p.get("name")}

        def already_chosen(person: dict) -> bool:
            return (person.get("accountId") in chosen_ids
                    or person.get("name", "").strip().lower() in chosen_names)

        people = list(discovered)
        known_ids = {p["accountId"] for p in people if p.get("accountId")}
        known_names = {p["name"].strip().lower() for p in people}
        # Keep anyone chosen whom the lookup didn't return: someone who has
        # left, or a name that was typed in by hand.
        for person in current:
            if person.get("accountId") and person["accountId"] in known_ids:
                continue
            if person.get("name", "").strip().lower() in known_names:
                continue
            people.append(person)
        people.sort(key=lambda p: p["name"].lower())
        return people, already_chosen

    def _discover(self) -> list[dict]:
        """Users Jira says can be assigned work in this project."""
        pk = self.app.config.get("projectKey", "")
        try:
            r = self.app._session.get(
                self.app._jira_url("/rest/api/3/user/assignable/search"),
                params={"project": pk, "maxResults": 200}, timeout=15)
            if not r.ok:
                return []
            return [{"name": u.get("displayName", ""),
                     "accountId": u.get("accountId", "")}
                    for u in r.json() if u.get("displayName")]
        except Exception:
            return []

    def _save(self) -> None:
        self.result = [person for var, person in self.vars if var.get()]
        self.win.destroy()


# ---------------------------------------------------------------------------
# Newly-opened ticket tooltip
# ---------------------------------------------------------------------------

class NewTicketTooltip:
    """Frameless popup listing alert-queue tickets; each row opens in browser."""

    def __init__(self, parent: tk.Tk, issues: list[dict],
                 domain: str, on_mark_seen=None,
                 suggestions: Optional[dict] = None) -> None:
        self.win = tk.Toplevel(parent)
        self.win.overrideredirect(True)
        self.win.attributes("-topmost", True)
        self.win.configure(bg=_C["input"])

        if not issues:
            tk.Label(self.win, text="  No tickets  ",
                     font=_font(9), fg=_C["fg"],
                     bg=_C["input"], pady=8).pack()
            return

        tk.Label(self.win, text="  ALERT QUEUE — click to open  ",
                 font=_font(7, "bold"), fg=_C["fg_dim"],
                 bg=_C["input"], pady=4).pack(fill="x")
        tk.Frame(self.win, bg=_C["fg_dim"], height=1).pack(fill="x")

        for issue in issues[:15]:
            key     = issue.get("issueKey") or issue.get("key", "")
            summary = (issue.get("fields") or {}).get("summary", "(no summary)")
            short   = summary[:55] + "…" if len(summary) > 55 else summary
            url     = f"https://{domain}/browse/{key}"
            sugg    = (suggestions or {}).get(key)
            self._make_issue_row(url, key, short, sugg)

        if on_mark_seen:
            tk.Frame(self.win, bg=_C["fg_dim"], height=1).pack(fill="x", pady=(4, 0))
            tk.Button(self.win, text="✓  Dismiss — hide until new tickets",
                      command=lambda: (on_mark_seen(), self.destroy()),
                      bg=_C["input"], fg=_C["ok"], font=_font(8),
                      relief="flat", pady=4, cursor="hand2",
                      activebackground=_C["chip"], activeforeground=_C["ok"]
                      ).pack(fill="x", padx=8, pady=(0, 4))

    def _make_issue_row(self, url: str, key: str, summary: str,
                        sugg: Optional[dict] = None) -> None:
        row = tk.Frame(self.win, bg=_C["input"], cursor="hand2")
        row.pack(fill="x", padx=8, pady=2)
        top = tk.Frame(row, bg=_C["input"])
        top.pack(fill="x")
        lbl_key = tk.Label(top, text=key, font=_font(8, "bold"),
                           fg=_C["accent"], bg=_C["input"], width=10, anchor="w")
        lbl_key.pack(side="left")
        lbl_txt = tk.Label(top, text=summary, font=_font(8),
                           fg=_C["fg"], bg=_C["input"], anchor="w")
        lbl_txt.pack(side="left")
        widgets = [row, top, lbl_key, lbl_txt]
        if sugg and sugg.get("label"):
            review = sugg.get("review")
            # The trailing U+FE0E on the warning triangle forces its plain
            # text glyph instead of a colour emoji, which would carry its
            # own fixed colours and ignore `colour` below entirely. A
            # lightbulb has no such text variant at all, so the confident
            # case reuses the same dot already used for status elsewhere.
            icon   = "⚠︎" if review else "●"
            colour = _C["warn"] if review else _C["ok"]
            lbl_sugg = tk.Label(row, text=f"        {icon} {sugg['label']}",
                                font=_font(8), fg=colour,
                                bg=_C["input"], anchor="w")
            lbl_sugg.pack(fill="x")
            widgets.append(lbl_sugg)

        def _hover_in(_e: tk.Event) -> None:
            for w in widgets:
                try:
                    w.configure(bg=_C["chip"])
                except tk.TclError:
                    pass

        def _hover_out(_e: tk.Event) -> None:
            for w in widgets:
                try:
                    w.configure(bg=_C["input"])
                except tk.TclError:
                    pass

        for widget in widgets:
            widget.bind("<ButtonRelease-1>", lambda _e, u=url: webbrowser.open(u))
            widget.bind("<Enter>", _hover_in)
            widget.bind("<Leave>", _hover_out)

    def place(self, x: int, y: int, screen_w: int, screen_h: int) -> None:
        _round_corners(self.win)
        self.win.update_idletasks()
        w = self.win.winfo_reqwidth()
        h = self.win.winfo_reqheight()
        tx = max(0, min(x, screen_w - w - 10))
        ty = max(0, y - h - 6)
        self.win.geometry(f"+{tx}+{ty}")

    def destroy(self) -> None:
        try:
            self.win.destroy()
        except tk.TclError:
            pass   # already destroyed (e.g. via the dismiss button)


# ---------------------------------------------------------------------------
# Main overlay
# ---------------------------------------------------------------------------

class JiraOverlay:
    """
    Main application window.

    Instance attributes
    -------------------
    config            Current config dict (mutated in place, flushed via save_config)
    queues            Visible queue dicts after filtering
    _all_queue_names  All queue names before hidden-queue filter (for Settings)
    new_issues        Issues currently in the alert queue
    completed_today   Count of tickets closed today
    sla_compliance    Compliance % or None if SLA unavailable
    _sla_unavailable  True once confirmed the SLA JQL field doesn't exist
    _settings_changed Re-fetch flag set when settings saved during an in-flight fetch
    _in_settings      True while the inline settings panel is showing
    _queue_geom       (w,h,x,y) saved before settings panel expands
    loading           True while a background fetch is running
    error_msg         Last fetch error string or None
    _error_count      Consecutive error count (drives exponential back-off)
    _retry_after_ms   Server-requested retry delay from a 429, or None
    _alert_count      Total tickets across ALL alert queues (unfiltered list)
    _prev_newly       Alert-queue count from the previous cycle (None = first)
    _seen_newly_ids   Set of ticket keys already notified (deduplication)
    _missing_once     Keys gone from the last poll, kept one more cycle so a
                      single empty reading can't re-notify everything
    _dismissed        User hid the overlay; stay hidden until a NEW ticket lands
    _manual_show      User explicitly showed via tray; keep visible until hidden
    _snooze_until     Datetime when snooze expires, or None
    _prev_counts      {queue_name: last_count} for flash-on-change detection
    _last_refresh     Datetime of the last successful fetch
    _visible          True when the overlay window is shown (not withdrawn)
    _fade_job         Pending root.after() id for the alpha-fade animation
    _geom_job         Pending root.after() id for the settings expand/collapse
    _show_job         Pending root.after() id for a delayed _do_show call
    _picker_open      Guard so the alert-queue picker can't stack duplicates
    _tooltip          Active NewTicketTooltip, or None
    _refresh_job      Pending root.after() id for the next auto-refresh
    _tray             pystray.Icon instance
    _tray_last        (count, alert, name) from last tray update (skip dup redraws)
    _session          Persistent requests.Session (connection reuse, auth preset)
    _last_full_fetch  Epoch when SLA/completed stats were last fetched
    _approx_count_ok  False once the server is known to lack /approximate-count
    _events           Queue of callables the fetch thread hands to the main one
    _flash_jobs       {queue_name: pending after() id} for the row-change fade
    _sw / _sh         Primary screen width / height (pixels)
    """

    _SETTINGS_W = 420   # overlay width when inline settings are open

    def __init__(self, config: dict) -> None:
        # Normalise here too, not just in load_config: the config also
        # arrives straight from the setup window, and the learner rewrites
        # the file underneath us, so this is the one place every path meets.
        self.config            = normalise_config(config)
        self.queues: list[dict]      = []
        self._all_queue_names: list[str] = []
        self.new_issues: list[dict]  = []
        self.completed_today         = 0
        self.sla_compliance: Optional[int] = None
        # Session-only: see _fetch_completion_stats. An older build
        # persisted this; drop the value rather than honouring it.
        self._sla_unavailable  = False
        config.pop("slaUnavailable", None)
        self._settings_changed = False
        self._in_settings      = False
        self._queue_geom: Optional[tuple[int, int, int, int]] = None
        self.loading           = False
        self.error_msg: Optional[str] = None   # hard error requiring user action
        self._conn_error: bool = False          # transient network error (keep last data)
        self._error_count      = 0
        self._retry_after_ms: Optional[int] = None
        self._alert_count      = 0
        self._reconciled_counts: dict[str, int] = {}   # alert queue → live count
        self._prev_newly: Optional[int] = None
        self._seen_newly_ids: set[str]  = set()
        # Keys absent from the last poll but not yet forgotten — see the
        # two-poll grace in _update_ui.
        self._missing_once: set[str]    = set()
        self._dismissed        = False
        self._manual_show      = False
        self._snooze_until: Optional[datetime] = None
        self._prev_counts: dict[str, int] = {}
        self._last_refresh: Optional[datetime] = None
        self._visible          = False
        self._fade_job: Optional[str] = None
        self._geom_job: Optional[str] = None   # settings expand/collapse motion
        self._show_job: Optional[str] = None
        self._picker_open      = False
        self._tooltip: Optional[NewTicketTooltip] = None
        self._suggestions: dict = {}   # key -> {label, plan} from the (optional)
                                       # local triage learner; display only
        self._suggestions_mtime: float = -1.0   # cache guard for the above
        self._refresh_job: Optional[str] = None
        self._tray: Optional[pystray.Icon] = None
        self._tray_last: tuple[int, bool, str] = (-1, False, "")
        self._session = requests.Session()   # persistent connection for live polling
        self._session.auth = HTTPBasicAuth(config.get("email", ""),
                                           config.get("token", ""))
        self._session.headers["Accept"] = "application/json"
        self._last_full_fetch: float = 0.0   # epoch when stats were last fetched
        self._approx_count_ok = True   # server supports /search/approximate-count
        self._alert_queues_missing: list[str] = []   # configured but gone
        # Work handed back from the fetch thread; drained by _pump on the
        # main thread (see _post).
        self._events: queue.Queue = queue.Queue()
        self._flash_jobs: dict[str, str] = {}   # queue name -> pending after id
        # Drag state
        self._drag_x0 = self._drag_y0 = 0
        self._drag_win_x = self._drag_win_y = 0
        self._did_drag = False

        self.root = tk.Tk()
        # Tk's default handler for an exception raised inside any callback
        # (a button command, a bound event, an after() job) prints to
        # stderr - which pythonw has none of, so it's simply discarded.
        # Every other error path in this app already logs instead of
        # going silent for exactly this reason; the Tk callback dispatch
        # itself was the one place still falling through that gap.
        self.root.report_callback_exception = self._on_tk_callback_error
        self.root.overrideredirect(True)
        self.root.attributes("-topmost", True)
        self.root.attributes("-alpha", 0)
        self.root.configure(bg=_C["border"])   # 1-px accent border visible between root and frame
        self.root.geometry("260x60+100+100")
        self._sw = self.root.winfo_screenwidth()
        self._sh = self.root.winfo_screenheight()

        self._build_ui()
        _round_corners(self.root)
        self.root.withdraw()
        self._start_tray()
        self.root.after(self._PUMP_MS, self._pump)
        self._fetch()
        self.root.after(30_000, self._update_relative_time)

    # ── Main-thread hand-off ──────────────────────────────────────────────────

    _PUMP_MS = 40   # how often the main thread drains the worker's queue

    def _post(self, fn: Callable[[], None]) -> None:
        """Queue `fn` to run on the main thread.

        Tk is not thread-safe: every widget call — including root.after() —
        must happen on the thread running mainloop. The fetch thread therefore
        hands work over through a queue instead of touching Tk directly.
        """
        self._events.put(fn)

    def _pump(self) -> None:
        while True:
            try:
                fn = self._events.get_nowait()
            except queue.Empty:
                break
            try:
                fn()
            except tk.TclError:
                # Only stop if the window really has gone. Treating any
                # TclError as shutdown meant one transient widget error
                # silently killed the pump, and with it every future
                # refresh — the overlay just quietly stopped updating.
                if not self.root.winfo_exists():
                    return
            except Exception:
                pass            # a failed callback must not kill the loop
        try:
            self.root.after(self._PUMP_MS, self._pump)
        except tk.TclError:
            pass                # window destroyed between drain and reschedule

    # ── Properties ────────────────────────────────────────────────────────────

    @property
    def _alert_queues(self) -> set[str]:
        """Set of queue names that trigger overlay + notifications."""
        cfg = self.config
        names = cfg.get("alertQueues") or ([cfg["alertQueue"]] if cfg.get("alertQueue") else [])
        return {n for n in names if n}

    def _jira_url(self, path: str = "") -> str:
        """Build a base Jira URL for this instance."""
        dom = self.config.get("domain", "")
        return f"https://{dom}{path}"

    # ── Tray ─────────────────────────────────────────────────────────────────

    def _start_tray(self) -> None:
        menu = pystray.Menu(
            pystray.MenuItem("Show / Hide", self._tray_toggle, default=True),
            pystray.MenuItem("Team report", lambda: self._post(self._open_report)),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem("Quit",        lambda: self._post(self.root.destroy)),
        )
        self._tray = pystray.Icon("JiraOverlay", _make_tray_image(0, False),
                                  "Jira Overlay", menu)
        threading.Thread(target=self._tray.run, daemon=True).start()

    def _tray_toggle(self) -> None:
        if self._visible:
            self._post(self._user_hide)
        else:
            self._post(self._user_show)

    def _update_tray(self, count: int, alert: bool,
                     queue_name: str = "Newly Opened") -> None:
        """Refresh tray icon and tooltip; skips if state is unchanged."""
        if not self._tray or (count, alert, queue_name) == self._tray_last:
            return
        self._tray_last = (count, alert, queue_name)
        self._tray.icon  = _make_tray_image(count, alert)
        noun = "ticket" if count == 1 else "tickets"
        self._tray.title = (f"Jira — {queue_name}: {count} {noun}"
                            if count else f"Jira — {queue_name}: 0")

    # ── UI construction ───────────────────────────────────────────────────────

    _GEAR_W = 30    # settings button footprint reserved in the header

    def _title_text(self, desk: str) -> str:
        """Header label sized to the space actually available.

        Letter-spacing looks deliberate but very nearly doubles the width, so
        it is only used when it fits. Failing that we fall back to plain caps
        and finally to an ellipsis — a long service-desk name must never
        crowd out the settings button or run off the edge.
        """
        name = (desk or "JIRA").upper()
        try:
            import tkinter.font as tkfont
            metric = tkfont.Font(font=_font(8, "bold", display=True))
            # window border + frame padding, then the settings button
            avail = (self.config.get("overlayWidth", 260)
                     - 2 - 28 - self._GEAR_W)
            for candidate in (f"● {_track(name)}", f"● {name}"):
                if metric.measure(candidate) <= avail:
                    return candidate
            while name and metric.measure(f"● {name}…") > avail:
                name = name[:-1]
            return f"● {name}…"
        except Exception:
            return f"● {name}"

    def _build_ui(self) -> None:
        BG = _C["bg"]
        # Generous, even padding is most of what separates a "tool" from a
        # finished product; the 14/10 inset gives content room to breathe
        # inside the rounded window.
        self.frame = tk.Frame(self.root, bg=BG, padx=14, pady=10)
        self.frame.pack(fill="both", expand=True, padx=1, pady=1)

        # Header
        hdr = tk.Frame(self.frame, bg=BG)
        hdr.pack(fill="x")
        desk = self.config.get("serviceDeskName", "JIRA")
        # Pack the settings button FIRST so it always reserves its slot; a
        # long desk name then has to fit around it instead of shoving it off
        # the edge of the window.
        self.btn_gear = tk.Label(hdr, text=_icon("settings"),
                                 font=_icon_font(11),
                                 fg=_C["fg_dim"], bg=BG, cursor="hand2",
                                 padx=6, pady=2)
        self.btn_gear.pack(side="right")
        self.lbl_title = tk.Label(hdr, text=self._title_text(desk),
                                  font=_font(8, "bold", display=True),
                                  fg=_C["accent"], bg=BG, cursor="hand2",
                                  anchor="w")
        self.lbl_title.pack(side="left", fill="x", expand=True)
        self.lbl_title.bind("<Enter>", lambda _e: self.lbl_title.config(fg=_C["accent_hi"]))
        self.lbl_title.bind("<Leave>", lambda _e: self.lbl_title.config(fg=_C["accent"]))
        self.lbl_title.bind("<ButtonRelease-1>", lambda _e: self._open_service_desk())
        _Tooltip(self.lbl_title, "Open service desk in Jira")
        self.btn_gear.bind("<ButtonRelease-1>", lambda _e: self._toggle_settings())
        self.btn_gear.bind("<Enter>", lambda _e: self.btn_gear.config(
            fg=_C["fg_bright"], bg=_C["chip"]))
        self.btn_gear.bind("<Leave>", lambda _e: self.btn_gear.config(
            fg=_C["fg_dim"], bg=BG))
        _Tooltip(self.btn_gear, "Settings")

        tk.Frame(self.frame, bg=_C["divider"], height=1).pack(fill="x",
                                                              pady=(8, 6))

        # Content area below the header. Both panels live here, which gives
        # the settings transition a coordinate space of its own to animate in
        # (see _animate_panel_swap).
        self.body = tk.Frame(self.frame, bg=BG)
        self.body.pack(fill="both", expand=True)

        # Main (queue) panel
        self.main_panel = tk.Frame(self.body, bg=BG)
        self.main_panel.pack(fill="both", expand=True)

        self.rows_frame = tk.Frame(self.main_panel, bg=BG)
        self.rows_frame.pack(fill="both", expand=True)
        self.row_widgets: dict[str, dict] = {}

        tk.Frame(self.main_panel, bg=_C["divider"], height=1).pack(fill="x",
                                                                   pady=(8, 4))
        done_row = tk.Frame(self.main_panel, bg=BG, cursor="hand2")
        done_row.pack(fill="x", pady=2)
        self.lbl_done = tk.Label(done_row, text="✓  Completed today — —",
                                 font=_font(9), fg=_C["ok"], bg=BG,
                                 cursor="hand2", padx=2)
        self.lbl_done.pack(side="left")
        self.lbl_sla = tk.Label(done_row, text="",
                                font=_font(8, "bold"), fg=_C["ok"], bg=BG,
                                cursor="hand2", padx=2)
        self.lbl_sla.pack(side="right")

        def _done_hover(bg: str) -> None:
            for w in (done_row, self.lbl_done, self.lbl_sla):
                w.configure(bg=bg)

        for w in (done_row, self.lbl_done, self.lbl_sla):
            w.bind("<ButtonPress-1>",   self._drag_press)
            w.bind("<B1-Motion>",       self._drag_move)
            w.bind("<ButtonRelease-1>", lambda _e: self._open_completed_today())
            w.bind("<Enter>", lambda _e: _done_hover(_C["done_hover"]))
            w.bind("<Leave>", lambda _e: _done_hover(BG))
        _Tooltip(self.lbl_done, "Open completed tickets in Jira")

        tk.Frame(self.main_panel, bg=_C["divider"], height=1).pack(fill="x",
                                                                   pady=(6, 4))

        # Footer: action buttons + status
        footer = tk.Frame(self.main_panel, bg=BG)
        footer.pack(fill="x")
        self.btn_snooze_icon = self._footer_btn(
            footer, _icon("snooze"), self._snooze_cycle, tip="Snooze alerts")
        self._footer_btn(footer, _icon("refresh"), self._fetch,
                         tip="Refresh now").pack(side="left", padx=(0, 2))
        self.btn_snooze_icon.pack(side="left", padx=(0, 2))
        self._footer_btn(footer, _icon("hide"), self._user_hide,
                         tip="Hide until new tickets arrive"
                         ).pack(side="left", padx=(0, 2))
        # Quit sits quietly until hovered — a destructive action shouldn't be
        # the loudest thing on screen.
        self._footer_btn(footer, _icon("close"), self.root.destroy,
                         hover_color=_C["alert"], tip="Quit").pack(side="left")
        self.lbl_status = tk.Label(footer, text="Connecting…",
                                   font=_font(7),
                                   fg=_C["fg_dim"], bg=BG, anchor="e")
        self.lbl_status.pack(side="right")

        # Settings panel (hidden until the gear is clicked)
        self.settings_panel = tk.Frame(self.body, bg=BG)

        # Bind drag to structural chrome widgets
        for w in (self.root, self.frame, hdr):
            self._bind_drag(w)
        # Title participates in drag press/move but release opens the service desk
        self.lbl_title.bind("<ButtonPress-1>", self._drag_press)
        self.lbl_title.bind("<B1-Motion>",     self._drag_move)
        self.lbl_title.bind("<ButtonRelease-1>",
                            lambda _e: self._open_service_desk()
                            if not self._did_drag else self._drag_release(_e))
        hdr.bind("<Double-Button-1>",            lambda _e: self._snap_to_nearest_corner())
        self.lbl_title.bind("<Double-Button-1>", lambda _e: self._snap_to_nearest_corner())

    @staticmethod
    def _footer_btn(parent: tk.Frame, text: str, cmd,
                    color: str = _C["fg_dim"],
                    tip: str = "",
                    hover_color: str = "") -> tk.Label:
        btn = tk.Label(parent, text=text, font=_icon_font(11),
                       fg=color, bg=_C["bg"], cursor="hand2",
                       padx=7, pady=3)

        def _enter(_e: tk.Event) -> None:
            btn.config(fg=hover_color or _C["fg_bright"], bg=_C["chip"])

        def _leave(_e: tk.Event) -> None:
            btn.config(fg=color, bg=_C["bg"])

        btn.bind("<ButtonRelease-1>", lambda _e: cmd())
        btn.bind("<Enter>", _enter)
        btn.bind("<Leave>", _leave)
        if tip:
            _Tooltip(btn, tip)
        return btn

    def _bind_drag(self, w: tk.Widget) -> None:
        w.bind("<ButtonPress-1>",   self._drag_press)
        w.bind("<B1-Motion>",       self._drag_move)
        w.bind("<ButtonRelease-1>", self._drag_release)

    def _make_row(self, queue: dict) -> dict:
        name   = queue["name"]
        qid    = queue["id"]
        is_new = name in self._alert_queues
        w_cfg  = self.config.get("overlayWidth", 260)
        BG     = _C["bg"]

        nc = _C["info"]  if is_new else _C["fg"]
        cc = _C["info"]  if is_new else _C["fg_bright"]
        max_chars = max(12, (w_cfg - 100) // 7)
        label = name if len(name) <= max_chars else name[:max_chars - 1] + "…"

        # Taller rows with inner padding give a comfortable click target and
        # let the hover tint read as a band rather than a thin stripe.
        row = tk.Frame(self.rows_frame, bg=BG, cursor="hand2")
        row.pack(fill="x", pady=1)
        lbl_name  = tk.Label(row, text=label, font=_font(9),
                             fg=nc, bg=BG, anchor="w", pady=3)
        lbl_name.pack(side="left", padx=(4, 0))
        # count shown as a small chip so it reads as a discrete badge
        lbl_count = tk.Label(row, text="—", font=_font(8, "bold"),
                             fg=cc, bg=_C["badge"], padx=8, pady=1)
        lbl_count.pack(side="right", padx=(0, 4))
        dot = tk.Label(row, text="", font=_font(8),
                       fg=_C["info"], bg=BG)
        dot.pack(side="right", padx=(0, 5))

        pk  = self.config.get("projectKey", "")
        url = (self._jira_url(f"/jira/servicedesk/projects/{pk}/queues/custom/{qid}")
               if pk else self._jira_url("/jira/servicedesk"))

        for w in (row, lbl_name, lbl_count, dot):
            w.bind("<ButtonPress-1>",   self._drag_press)
            w.bind("<B1-Motion>",       self._drag_move)
            w.bind("<ButtonRelease-1>", lambda e, u=url: self._row_click(e, u))

        # subtle row hover; the count chip keeps its own background
        hover_targets = (row, lbl_name, dot)

        def _hover_in(_e: tk.Event) -> None:
            for w in hover_targets:
                try: w.configure(bg=_C["hover"])
                except tk.TclError: pass

        def _hover_out(_e: tk.Event) -> None:
            for w in hover_targets:
                try: w.configure(bg=BG)
                except tk.TclError: pass

        if is_new:
            def _enter(e: tk.Event) -> None:
                _hover_in(e); self._tooltip_show(e)
            def _leave(e: tk.Event) -> None:
                _hover_out(e); self._tooltip_hide(e)
            for w in (row, lbl_name, lbl_count, dot):
                w.bind("<Enter>", _enter)
                w.bind("<Leave>", _leave)
        else:
            for w in (row, lbl_name, lbl_count, dot):
                w.bind("<Enter>", _hover_in)
                w.bind("<Leave>", _hover_out)

        return {"row": row, "lbl_name": lbl_name,
                "lbl_count": lbl_count, "dot": dot}

    # ── Drag ─────────────────────────────────────────────────────────────────

    def _drag_press(self, e: tk.Event) -> None:
        self._drag_x0    = e.x_root
        self._drag_y0    = e.y_root
        self._drag_win_x = self.root.winfo_x()
        self._drag_win_y = self.root.winfo_y()
        self._did_drag   = False

    def _drag_move(self, e: tk.Event) -> None:
        dx = e.x_root - self._drag_x0
        dy = e.y_root - self._drag_y0
        if abs(dx) > 4 or abs(dy) > 4:
            self._did_drag = True
        self.root.geometry(f"+{self._drag_win_x + dx}+{self._drag_win_y + dy}")

    def _corners(self, w: int, h: int) -> dict[str, tuple[int, int]]:
        """Corner coordinates on the monitor the overlay currently occupies.

        Uses the work area, so 'bottom' corners sit above the taskbar and
        snapping works on any monitor, not just the primary.
        """
        x, y = self.root.winfo_x(), self.root.winfo_y()
        rect = get_monitor_rect(x + w // 2, y + h // 2, work=True)
        left, top, right, bottom = (rect if rect else
                                    (0, 0, self._sw, self._sh - 50))
        m = 10
        return {
            "top-left":     (left + m,         top + m),
            "top-right":    (right - w - m,    top + m),
            "bottom-left":  (left + m,         bottom - h - m),
            "bottom-right": (right - w - m,    bottom - h - m),
        }

    def _drag_release(self, e: tk.Event) -> None:
        if not self._did_drag:
            self._did_drag = False
            return
        x, y = self.root.winfo_x(), self.root.winfo_y()
        w, h = self.root.winfo_width(), self.root.winfo_height()
        corners = self._corners(w, h)
        snapped = next(
            (name for name, (cx, cy) in corners.items()
             if abs(x - cx) < _SNAP_MARGIN and abs(y - cy) < _SNAP_MARGIN),
            None,
        )
        self.config.pop("lastX", None)
        self.config.pop("lastY", None)
        self.config.pop("snapCorner", None)
        if snapped:
            cx, cy = corners[snapped]
            self.root.geometry(f"+{cx}+{cy}")
            self.config["snapCorner"] = snapped
        else:
            self.config["lastX"], self.config["lastY"] = x, y
        save_config(self.config)
        self._did_drag = False

    def _row_click(self, _e: tk.Event, url: str) -> None:
        if not self._did_drag:
            webbrowser.open(url)
        self._did_drag = False

    def _snooze_cycle(self) -> None:
        """If snoozed, cancel immediately; otherwise show duration picker."""
        if self._is_snoozed():
            self._cancel_snooze()
        else:
            self._show_snooze_popup()

    def _show_snooze_popup(self) -> None:
        """Small popup with snooze duration choices, positioned above the 💤 button."""
        popup = tk.Toplevel(self.root)
        popup.overrideredirect(True)
        popup.attributes("-topmost", True)
        popup.configure(bg=_C["divider"])   # 1-px border effect

        inner = tk.Frame(popup, bg=_C["input"])
        inner.pack(padx=1, pady=1)

        for label, minutes in [("Snooze 15 min", 15),
                                ("Snooze 30 min", 30),
                                ("Snooze 1 hour", 60)]:
            tk.Button(inner, text=label,
                      command=lambda m=minutes, p=popup: (p.destroy(), self._snooze(m)),
                      bg=_C["input"], fg=_C["fg_bright"],
                      font=_font(9), relief="flat",
                      padx=16, pady=5, cursor="hand2",
                      activebackground=_C["accent"],
                      activeforeground="white").pack(fill="x")

        popup.update_idletasks()
        bx = self.btn_snooze_icon.winfo_rootx()
        by = self.btn_snooze_icon.winfo_rooty()
        ph = popup.winfo_reqheight()
        popup.geometry(f"+{bx}+{max(0, by - ph - 4)}")
        _round_corners(popup, small=True)

        popup.focus_force()
        popup.bind("<FocusOut>",
                   lambda _e: popup.destroy() if popup.winfo_exists() else None)
        popup.bind("<Escape>",
                   lambda _e: popup.destroy() if popup.winfo_exists() else None)

    # ── Corner snapping ───────────────────────────────────────────────────────

    def _snap_to_nearest_corner(self) -> None:
        self.root.update_idletasks()
        x, y = self.root.winfo_x(), self.root.winfo_y()
        w, h = self.root.winfo_width(), self.root.winfo_height()
        corners = self._corners(w, h)
        name, (cx, cy) = min(
            corners.items(),
            key=lambda kv: (x - kv[1][0]) ** 2 + (y - kv[1][1]) ** 2,
        )
        self.root.geometry(f"+{cx}+{cy}")
        self.config.pop("lastX", None)
        self.config.pop("lastY", None)
        self.config["snapCorner"] = name
        save_config(self.config)

    # ── Tooltip ───────────────────────────────────────────────────────────────

    def _tooltip_show(self, _e: tk.Event) -> None:
        if self._tooltip or not self.new_issues:
            return
        self._tooltip = NewTicketTooltip(
            self.root, self.new_issues, self.config["domain"],
            suggestions=self._suggestions,
            on_mark_seen=self._mark_all_seen)
        self._tooltip.place(self.root.winfo_x(), self.root.winfo_y(),
                            self._sw, self._sh)

    def _tooltip_hide(self, e: tk.Event) -> None:
        rx = self.root.winfo_rootx()
        ry = self.root.winfo_rooty()
        rw = self.root.winfo_width()
        rh = self.root.winfo_height()
        if not (rx <= e.x_root <= rx + rw and ry <= e.y_root <= ry + rh):
            if self._tooltip:
                self._tooltip.destroy()
                self._tooltip = None

    def _mark_all_seen(self) -> None:
        """Dismiss the current alert: hide the overlay until a NEW ticket lands.

        Displayed tickets are already in the seen-set, so nothing to add —
        clearing it (the old behaviour) re-notified every ticket, which was
        the opposite of the button's intent.
        """
        self._tooltip = None   # popup destroys itself after calling us
        self._user_hide()

    @staticmethod
    def _log_error(detail: str) -> None:
        """Record a non-fatal failure. pythonw has no console, so without
        this the user just sees a feature quietly do nothing."""
        try:
            with open(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                   "jira_overlay_error.log"),
                      "a", encoding="utf-8") as f:
                f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S}\n{detail}\n\n")
        except OSError:
            pass

    def _on_tk_callback_error(self, exc, val, tb) -> None:
        """Tk's report_callback_exception hook - see the comment where it's
        assigned in __init__. Logs and moves on; re-raising here would just
        hand the exception back to Tk's own dispatcher."""
        self._log_error("Tk callback failed:\n"
                        + "".join(traceback.format_exception(exc, val, tb)))

    def _open_report(self) -> None:
        """Open the team report, or focus it if it is already up."""
        existing = getattr(self, "_report_win", None)
        if existing is not None and not existing._closed:
            try:
                existing.win.deiconify()
                existing.win.lift()
                existing.win.focus_force()
                return
            except tk.TclError:
                pass          # window was destroyed behind our back
        try:
            self._report_win = TeamReportWindow(self)
        except Exception:
            self._log_error("team report failed to open:\n"
                            + traceback.format_exc())

    def _open_service_desk(self) -> None:
        if self._did_drag:
            return
        pk  = self.config.get("projectKey", "")
        url = (self._jira_url(f"/jira/servicedesk/projects/{pk}/queues")
               if pk else self._jira_url("/jira/servicedesk"))
        webbrowser.open(url)

    def _completed_today_jql(self) -> str:
        """JQL behind the 'Completed today' figure and its Jira link."""
        pk    = self.config.get("projectKey", "")
        jql   = (f"project = {pk} AND statusCategory = Done "
                 "AND updated >= startOfDay()")
        extra = self.config.get("completedTodayFilter", "").strip()
        return f"{jql} AND {extra}" if extra else jql

    def _open_completed_today(self) -> None:
        if self._did_drag:
            return
        # Percent-encode: a custom filter can contain =, ", & and other
        # characters that a bare space→+ swap would corrupt.
        jql = urllib.parse.quote(self._completed_today_jql(), safe="")
        webbrowser.open(self._jira_url(f"/issues/?jql={jql}"))

    # ── Service-desk discovery ────────────────────────────────────────────────

    def _discover_service_desk(self) -> Optional[dict]:
        """Probe accessible service desks; shows picker if multiple found."""
        try:
            r = self._session.get(
                self._jira_url("/rest/servicedeskapi/servicedesk"),
                timeout=10)
            r.raise_for_status()
            desks = r.json().get("values") or []
        except Exception:
            return None

        # Probe each desk independently — one unreachable desk must not abort
        # discovery for the rest.
        accessible = []
        for d in desks:
            try:
                probe = self._session.get(
                    self._jira_url(
                        f"/rest/servicedeskapi/servicedesk/{d['id']}/queue"),
                    params={"limit": 1}, timeout=5)
            except requests.RequestException:
                continue
            if probe.ok:
                accessible.append(d)

        if not accessible:
            return None
        if len(accessible) == 1:
            return accessible[0]

        # Multiple desks — ask the user (must run on main thread)
        self._desk_pick: Optional[dict] = None
        picked = threading.Event()

        def _show_picker() -> None:
            try:
                p = PickDeskWindow(accessible, parent=self.root)
                self._desk_pick = p.result or accessible[0]
            except Exception:
                self._desk_pick = accessible[0]   # never strand the fetch
            finally:
                picked.set()

        self._post(_show_picker)
        picked.wait(timeout=120)
        return self._desk_pick

    def _show_alert_queue_picker(self) -> None:
        """Modal queue picker — runs on main thread; re-fetches on selection.

        Guarded so the 5-second poll loop can't stack duplicate pickers while
        one is already waiting for the user.
        """
        if self._picker_open:
            return
        self._picker_open = True
        try:
            self._show_alert_queue_picker_inner()
        finally:
            self._picker_open = False

    def _show_alert_queue_picker_inner(self) -> None:
        win = tk.Toplevel(self.root)
        win.title("Jira Overlay — Choose Alert Queue")
        win.configure(bg=_C["bg_dark"])
        win.resizable(False, False)
        win.attributes("-topmost", True)
        _round_corners(win)

        sw, sh = win.winfo_screenwidth(), win.winfo_screenheight()
        tk.Label(win, text="Which queue should trigger alerts?",
                 font=_font(11, "bold"),
                 fg=_C["fg_bright"], bg=_C["bg_dark"]).pack(pady=(18, 4))
        tk.Label(win,
                 text="The overlay will appear and notifications will fire\n"
                      "when new tickets arrive in this queue.",
                 font=_font(8), fg=_C["fg_dim"],
                 bg=_C["bg_dark"], justify="center").pack(pady=(0, 10))

        container = tk.Frame(win, bg=_C["bg_dark"])
        container.pack(fill="both", expand=True, padx=20, pady=(0, 16))
        canvas = tk.Canvas(container, bg=_C["bg_dark"], highlightthickness=0, width=320)
        sb     = tk.Scrollbar(container, orient="vertical", command=canvas.yview)
        sf     = tk.Frame(canvas, bg=_C["bg_dark"])
        sf.bind("<Configure>", lambda _e: canvas.configure(scrollregion=canvas.bbox("all")))
        canvas.create_window((0, 0), window=sf, anchor="nw", width=320)
        canvas.configure(yscrollcommand=sb.set)
        pick_scroll = lambda e: canvas.yview_scroll(-1 * (e.delta // 120), "units")
        canvas.bind("<MouseWheel>", pick_scroll)
        sb.pack(side="right", fill="y")
        canvas.pack(side="left", fill="both", expand=True)

        def pick(name: str) -> None:
            self.config["alertQueue"]  = name
            self.config["alertQueues"] = [name]
            save_config(self.config)
            win.destroy()

        for q in self.queues:
            tk.Button(sf, text=q["name"], command=lambda n=q["name"]: pick(n),
                      bg=_C["input"], fg=_C["fg_bright"], font=_font(10),
                      relief="flat", padx=12, pady=7, cursor="hand2",
                      activebackground=_C["accent"], activeforeground="white"
                      ).pack(fill="x", pady=3, padx=4)
        tk.Button(sf, text="Don't alert me — just show the counts",
                  command=win.destroy,
                  bg=_C["bg_dark"], fg=_C["fg_dim"], font=_font(9),
                  relief="flat", padx=12, pady=6, cursor="hand2",
                  activebackground=_C["chip"], activeforeground=_C["fg_bright"]
                  ).pack(fill="x", pady=(8, 3), padx=4)

        _bind_wheel(sf, pick_scroll)      # the buttons, not just the canvas
        win.update_idletasks()
        btn_h   = min(sf.winfo_reqheight(), sh - 250)
        total_h = btn_h + 130
        win.geometry(f"380x{total_h}+{sw // 2 - 190}+{sh // 2 - total_h // 2}")
        canvas.configure(height=btn_h)
        win.focus_force()
        win.wait_window(win)

        # Asked and answered - including "answered by closing the window".
        self.config["alertQueuesAsked"] = True
        save_config(self.config)
        if self.config.get("alertQueue"):
            self._fetch()

    # ── Fetch ─────────────────────────────────────────────────────────────────

    def _fetch(self) -> None:
        """Kick off a background data refresh (no-op if one is already running)."""
        if self.loading:
            return
        if self._refresh_job:
            self.root.after_cancel(self._refresh_job)
            self._refresh_job = None
        self.loading = True
        threading.Thread(target=self._fetch_worker, daemon=True).start()

    def _fetch_worker(self) -> None:
        self._retry_after_ms = None
        try:
            self._fetch_data()
        except requests.HTTPError as exc:
            resp   = exc.response
            status = resp.status_code if resp is not None else 0
            if status == 429 or status >= 500:
                # Rate-limited or server hiccup — transient, keep last data
                self._conn_error = True
                ra = resp.headers.get("Retry-After", "") if resp is not None else ""
                if ra.isdigit():
                    self._retry_after_ms = max(int(ra) * 1_000, _LIVE_POLL_MS)
            else:
                self._error_count += 1
                hint = ("check credentials" if status in (401, 403)
                        else "request failed")
                self.error_msg   = f"HTTP {status} — {hint}"
                self._conn_error = False
                self.queues      = []
        except (requests.ConnectionError, requests.Timeout):
            # Transient (network drop, timeout) — keep last data, retry at normal rate
            self._conn_error = True
        except Exception as exc:
            self._error_count += 1
            self.error_msg   = f"{type(exc).__name__}: {str(exc)[:40]}"
            self._conn_error  = False
            self.queues       = []
        finally:
            self.loading = False
            # Refresh the UI and schedule the next poll on the main thread —
            # this method runs on the fetch thread, which must not touch Tk.
            self._post(self._fetch_finished)

    def _fetch_finished(self) -> None:
        """Main-thread tail of a fetch: repaint, then queue the next poll."""
        # Repainting must never be able to stop the clock. The pump swallows
        # a failed callback so the app survives, but this method is that
        # callback — an exception out of _update_ui used to skip the
        # scheduling below entirely, and the overlay went quiet for good with
        # nothing on screen to say so. Scheduling the next poll is the one
        # thing here that has to happen either way.
        try:
            self._update_ui()
        except Exception:
            self._log_error("repaint failed:\n" + traceback.format_exc())
        # Replace any poll already queued rather than just overwriting the
        # handle. The first-run queue picker starts a second fetch of its own,
        # so two completions can land back to back — and each orphaned timer
        # would go on firing, quietly multiplying the polling rate.
        if self._refresh_job:
            try:
                self.root.after_cancel(self._refresh_job)
            except tk.TclError:
                pass
            self._refresh_job = None
        if self._settings_changed:
            self._settings_changed = False
            self._last_full_fetch = 0.0   # force stats refresh on next cycle
            delay = 0                     # immediate
        elif self._retry_after_ms:
            delay = self._retry_after_ms  # honour the server's Retry-After
        elif self._conn_error:
            delay = _LIVE_POLL_MS         # retry at live rate during reconnect
        elif self._error_count > 0:
            delay = min(_LIVE_POLL_MS * (2 ** min(self._error_count, 6)),
                        5 * 60 * 1_000)
        else:
            delay = _LIVE_POLL_MS         # always poll counts every 5 s
        self._refresh_job = self.root.after(delay, self._fetch)

    def _fetch_data(self) -> None:
        """Core data-fetch logic (called from worker thread)."""
        cfg = self.config

        # Discover service desk on first run
        if not cfg.get("serviceDeskId"):
            desk = self._discover_service_desk()
            if not desk:
                self.error_msg = "No accessible service desk found"
                return
            cfg.update({
                "serviceDeskId":   desk["id"],
                "serviceDeskName": desk["projectName"],
                "projectKey":      desk.get("projectKey", ""),
            })
            save_config(cfg)
            self._post(lambda n=desk["projectName"]:
                self.lbl_title.config(text=self._title_text(n)))

        sd_id = cfg["serviceDeskId"]

        # Backfill projectKey if absent (e.g. settings save stripped it)
        if not cfg.get("projectKey"):
            try:
                ri = self._session.get(
                    self._jira_url(f"/rest/servicedeskapi/servicedesk/{sd_id}"),
                    timeout=10)
                if ri.ok:
                    cfg["projectKey"] = ri.json().get("projectKey", "")
                    save_config(cfg)
            except Exception:
                pass

        # --- Fast path: queue counts (runs every 5 s) ---
        r = self._session.get(
            self._jira_url(f"/rest/servicedeskapi/servicedesk/{sd_id}/queue"),
            params={"includeCount": "true"}, timeout=10)
        r.raise_for_status()

        skip_kw   = {k.strip().upper() for k in cfg.get("skipKeywords", []) if k.strip()}
        max_count = cfg.get("maxQueueSize", 0)
        hidden    = set(cfg.get("hiddenQueues", []))

        # Everything the service desk has, before any of the user's filters.
        # Alerting reads from this. The three filters below are all about
        # what is worth *looking* at, and none of them should be able to
        # switch off a notification the user asked for - maxQueueSize in
        # particular only bites once a queue gets busy, which is exactly when
        # the alert matters most.
        # `or []`, not `.get("values", [])` alone: the latter only falls
        # back when the key is missing, and the very next line's whole
        # reason for existing is that Jira does send present-but-null
        # fields — "values" itself is no exception, and every_queue feeds
        # a `for` loop two lines down.
        every_queue = r.json().get("values") or []
        # Normalise the count once, here, rather than defending at each of
        # the five places that read it. dict.get(key, 0) returns None when
        # the key is present but null — which Jira does send — and the
        # resulting None flowed into a `>` comparison, raised TypeError out
        # of _update_ui, and (before the guard in _fetch_finished) stopped
        # the poll loop for good.
        for q in every_queue:
            try:
                q["issueCount"] = max(0, int(q.get("issueCount") or 0))
            except (TypeError, ValueError):
                q["issueCount"] = 0
        all_queues = [
            q for q in every_queue
            if not any(kw in q["name"].upper() for kw in skip_kw)
            and (max_count == 0 or q.get("issueCount", 0) <= max_count)
        ]
        self._all_queue_names = [q["name"] for q in all_queues]
        self.queues = [q for q in all_queues if q["name"] not in hidden]

        # Offer the alert-queue picker once, on first run. Only once: a user
        # who closes it without choosing was previously re-prompted by the
        # very next poll, every five seconds, for ever. Running with no alert
        # queue at all is a legitimate choice - a passive dashboard - so the
        # fetch carries on either way and the queue counts stay live.
        alert_qs = self._alert_queues
        if not alert_qs and self.queues and not cfg.get("alertQueuesAsked"):
            self._post(self._show_alert_queue_picker)

        # Alert data comes from the genuinely unfiltered list, so that hiding
        # a queue, skipping it by keyword, or capping it by size only ever
        # affects what is shown.
        alert_objs = [q for q in every_queue if q["name"] in alert_qs]
        # Only report a queue as missing when Jira really doesn't have it —
        # measuring against the filtered list blamed Jira for the user's own
        # display settings.
        self._alert_queues_missing = (
            sorted(alert_qs - {q["name"] for q in every_queue})
            if every_queue else [])

        # Fetch issues in every alert queue (tooltips + notifications).
        # includeCount is cached by JSM and can report a stale non-zero for an
        # empty queue, so we treat the live issue list as authoritative and
        # reconcile each queue's displayed count against what we actually pull.
        _ISSUE_LIMIT = 50
        issues: list[dict] = []
        complete = True
        reconciled: dict[str, int] = {}
        for q in alert_objs:
            cached = q.get("issueCount", 0)
            if cached <= 0:
                reconciled[q["name"]] = 0
                continue
            ir = self._session.get(
                self._jira_url(
                    f"/rest/servicedeskapi/servicedesk/{sd_id}/queue/{q['id']}/issue"),
                params={"start": 0, "limit": _ISSUE_LIMIT}, timeout=10)
            if ir.ok:
                vals = ir.json().get("values") or []
                issues.extend(vals)
                # Exact when the whole queue fits in one page; else trust cache
                reconciled[q["name"]] = len(vals) if cached <= _ISSUE_LIMIT else cached
            else:
                complete = False
                reconciled[q["name"]] = cached   # fall back to cached on failure
        if complete:
            self.new_issues = issues
        # On partial failure keep the previous snapshot — replacing it would
        # wipe the seen-set and re-notify every ticket once the fetch recovers
        self._reconciled_counts = reconciled
        self._alert_count = sum(reconciled.get(q["name"], q.get("issueCount", 0))
                                for q in alert_objs)

        # --- Slow path: SLA & completed-today (runs every refreshSeconds) ---
        stats_interval = cfg.get("refreshSeconds", 60)
        if time.time() - self._last_full_fetch >= stats_interval:
            self._fetch_completion_stats(cfg)
            self._last_full_fetch = time.time()

        self.error_msg    = None
        self._conn_error  = False
        self._error_count = 0

    def _count_issues(self, jql: str) -> tuple[Optional[int], bool]:
        """Count issues matching `jql`.

        Returns (count, jql_rejected). `count` is None when the query didn't
        succeed; `jql_rejected` is True only when Jira actively rejected the
        query itself (HTTP 400 — e.g. an SLA field that doesn't exist), so a
        transient network or 5xx blip is never mistaken for a permanent
        configuration problem.

        Prefers /search/approximate-count, which returns just a number; the
        old path pulled up to 200 whole issues purely to call len() on them,
        which also silently capped the figure at '200+'.
        """
        if self._approx_count_ok:
            r = self._session.post(
                self._jira_url("/rest/api/3/search/approximate-count"),
                json={"jql": jql}, timeout=10)
            if r.ok:
                try:
                    return int(r.json()["count"]), False
                except (ValueError, TypeError, KeyError, AttributeError):
                    return None, False
            if r.status_code == 400:
                return None, True
            if r.status_code != 404:
                return None, False
            # 404 — older Jira without the endpoint; use the search API
            # from here on.
            self._approx_count_ok = False

        r = self._session.post(
            self._jira_url("/rest/api/3/search/jql"),
            json={"jql": jql, "maxResults": 200, "fields": ["status"]},
            timeout=10)
        if not r.ok:
            return None, r.status_code == 400
        try:
            return len(r.json().get("issues") or []), False
        except (ValueError, AttributeError):
            return None, False

    def _fetch_completion_stats(self, cfg: dict) -> None:
        """Fetch completed-today count and optional SLA compliance."""
        if not cfg.get("projectKey", ""):
            self.completed_today = 0
            self.sla_compliance  = None
            return

        base_jql = self._completed_today_jql()
        try:
            total_n, _ = self._count_issues(base_jql)
            if total_n is None:
                self.completed_today = 0
                self.sla_compliance  = None
                return
            self.completed_today = total_n

            # SLAs are a Jira Service Management Premium feature, so plenty of
            # desks simply haven't got them. An empty field name means the
            # user has turned this off, and we ask Jira nothing at all.
            # Absent means "never configured", so fall back to the usual JSM
            # field name. Present but empty means the user cleared it on
            # purpose, which is one of the ways to turn this off.
            sla_field = (cfg.get("slaField", "Time to resolution") or "").strip()
            if (not cfg.get("slaEnabled", True) or not sla_field
                    or self._sla_unavailable or total_n == 0):
                self.sla_compliance = None
                return

            breached, rejected = self._count_issues(
                base_jql + f' AND "{sla_field}" = slaBreached()')
            if breached is None:
                # Only stop asking when Jira rejected the query outright —
                # that means the field is missing or renamed, which on a desk
                # without SLAs it always will be. Remember it, or every launch
                # repeats a request that cannot succeed. A transient failure
                # just leaves the figure blank for this cycle.
                # Held for this session only. Persisting it looked like a
                # cheap saving - one skipped request per launch - but it wrote
                # a conclusion to disk that the user then had no obvious way
                # to undo, and a wrong one sticks until they think to toggle
                # an SLA setting. One extra failing request per launch is a
                # far better trade than a sticky wrong state.
                self._sla_unavailable = self._sla_unavailable or rejected
                self.sla_compliance = None
            else:
                self.sla_compliance = round(
                    (total_n - breached) / total_n * 100)

        except Exception:
            self.completed_today = 0
            self.sla_compliance  = None

    def _load_learner_status(self, path: Optional[str] = None) -> Optional[dict]:
        """Health/provenance written by the (optional) local triage learner.

        Settings-panel-only, read fresh each time it's opened rather than
        cached against the poll loop — this isn't on any hot path. None
        means either the learner isn't installed on this machine, or it
        hasn't produced a first report yet; both are normal, not errors.
        """
        path = path or os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "triage_status.json")
        try:
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
            return data if isinstance(data, dict) else None
        except (OSError, ValueError):
            return None

    # ── Update UI ─────────────────────────────────────────────────────────────

    def _load_suggestions(self) -> dict:
        """Optional routing suggestions written by an external process
        (e.g. a local triage learner). Display-only — the overlay never
        acts on them. Ignored if the file is missing or stale. Re-parsed
        only when the file actually changes (cached by mtime)."""
        path = os.path.join(
            os.path.dirname(os.path.abspath(__file__)),
            self.config.get("suggestionsFile", "triage_suggestions.json"))
        try:
            mtime = os.path.getmtime(path)
            if time.time() - mtime > 900:
                return {}
            if mtime == self._suggestions_mtime:
                return self._suggestions          # unchanged since last read
            with open(path, encoding="utf-8") as f:
                data = json.load(f)
            # Another process writes this file, so take nothing on trust: a
            # list or a bare null would otherwise reach the callers that do
            # .get() on it. Caching the parse here rather than relying on the
            # caller to assign the result back also keeps the mtime shortcut
            # honest for anyone who reads it in future.
            self._suggestions = data if isinstance(data, dict) else {}
            self._suggestions_mtime = mtime
            return self._suggestions
        except (OSError, ValueError):
            return {}

    def _update_ui(self) -> None:
        if self._conn_error:
            # Transient network loss — keep existing rows, show subtle status
            self.lbl_status.config(text="Reconnecting…", fg=_C["warn"])
            return
        self._suggestions = self._load_suggestions()

        if self.error_msg:
            # Hard error — wipe rows_frame completely then show single error label
            for child in self.rows_frame.winfo_children():
                child.destroy()
            self.row_widgets.clear()
            err = tk.Label(self.rows_frame, text=self.error_msg,
                           font=_font(8), fg=_C["alert"], bg=_C["bg"])
            err.pack()
            self._bind_drag(err)
            self.lbl_status.config(text="Tap ↻ to retry", fg=_C["alert"])
            self.root.after(10, self._resize)
            return

        # Remove any raw error labels left over from a previous hard-error state
        tracked = {rw["row"] for rw in self.row_widgets.values()}
        for child in self.rows_frame.winfo_children():
            if child not in tracked:
                child.destroy()

        # Sync row widgets with current queue list
        current_names = {q["name"] for q in self.queues}
        for name in list(self.row_widgets):
            if name not in current_names:
                self._cancel_flash(name)
                self.row_widgets[name]["row"].destroy()
                del self.row_widgets[name]

        # Nothing to show is ambiguous on its own: a brand-new service desk
        # with no queues looks exactly like one where every queue has been
        # filtered out. Say which, rather than leaving an empty box that
        # claims to be live.
        if not self.queues and not self._conn_error:
            note = ("All queues are hidden — see Settings"
                    if self._all_queue_names else
                    "No queues found in this service desk")
            hint = tk.Label(self.rows_frame, text=note, font=_font(8),
                            fg=_C["fg_dim"], bg=_C["bg"], pady=6)
            hint.pack()
            self._bind_drag(hint)

        alert_qs = self._alert_queues

        for q in self.queues:
            name   = q["name"]
            is_new = name in alert_qs
            # alert queues use the live reconciled count (cached count can be
            # a stale non-zero for an empty queue); others use includeCount
            count  = (self._reconciled_counts.get(name, q.get("issueCount", 0))
                      if is_new else q.get("issueCount", 0))

            if name not in self.row_widgets:
                self.row_widgets[name] = self._make_row(q)

            rw = self.row_widgets[name]
            rw["lbl_count"].config(text=str(count))

            if is_new:
                if count > 0:
                    # active alert queue — solid accent chip + live dot
                    rw["lbl_count"].config(fg="white", bg=_C["accent"])
                    rw["dot"].config(text="●", fg=_C["info"])
                else:
                    rw["lbl_count"].config(fg=_C["fg_dim"], bg=_C["badge"])
                    rw["dot"].config(text="")
            else:
                rw["lbl_count"].config(
                    fg=_C["fg_bright"] if count > 0 else _C["fg_dim"],
                    bg=_C["badge"])

            prev = self._prev_counts.get(name)
            if prev is not None and count != prev:
                self._flash_row(name, rw)
            self._prev_counts[name] = count

        # Total across ALL alert queues (computed in the fetch from the
        # unfiltered list, so hidden alert queues still count)
        newly_count = self._alert_count

        # Deduplication — notify only for genuinely new ticket IDs
        # Drop blanks: an issue without a key would otherwise be tracked as ""
        # and fire a notification with no ticket reference in it.
        current_ids = {k for k in (i.get("issueKey") or i.get("key", "")
                                   for i in self.new_issues) if k}
        truly_new   = current_ids - self._seen_newly_ids
        # Forget a ticket only once it has been absent from TWO consecutive
        # polls. Replacing the set outright meant any single empty reading
        # re-notified everything on the next poll — and an empty reading does
        # not require a fetch failure: JSM's cached issueCount is what decides
        # whether a queue is even fetched, so one stale zero for a queue that
        # actually has tickets was enough to trigger the whole burst.
        vanished = self._seen_newly_ids - current_ids
        # Keep a key that has just gone missing; drop it only if it was
        # already missing last time too.
        self._seen_newly_ids = current_ids | (vanished - self._missing_once)
        self._missing_once = vanished

        # A genuinely new ticket cancels any manual dismissal
        if truly_new:
            self._dismissed = False

        snoozed       = self._is_snoozed()
        has_new_alert = bool(truly_new) and self._prev_newly is not None

        # Sync snooze button colour
        self.btn_snooze_icon.config(fg=_C["warn"] if snoozed else _C["fg_dim"])

        if has_new_alert and not snoozed:
            # Build a lookup of key → summary from the already-fetched issues
            summaries = {
                (i.get("issueKey") or i.get("key", "")): (
                    (i.get("fields") or {}).get("summary", "")
                )
                for i in self.new_issues
            }
            if len(truly_new) == 1:
                key     = next(iter(truly_new))
                summary = summaries.get(key, "")
                title   = f"Jira — {key}"
                body    = summary[:120] if summary else "New ticket in alert queue"
                s       = self._suggestions.get(key, {})
                if s.get("label"):
                    body += f"\n{'⚠' if s.get('review') else '💡'} {s['label']}"
            else:
                title   = f"Jira — {len(truly_new)} new tickets"
                lines   = []
                for key in list(truly_new)[:3]:
                    s    = summaries.get(key, "")
                    line = f"{key}: {s[:60]}" if s else key
                    sg   = self._suggestions.get(key, {})
                    if sg.get("label"):
                        line += f" {'⚠' if sg.get('review') else '💡'} {sg['label']}"
                    lines.append(line)
                if len(truly_new) > 3:
                    lines.append(f"…and {len(truly_new) - 3} more")
                body = "\n".join(lines)
            if self.config.get("notificationsEnabled", True):
                send_toast(title, body)
            if self.config.get("soundEnabled", True):
                play_alert()

        self._prev_newly   = newly_count
        self._last_refresh = datetime.now()

        self.lbl_done.config(text=f"✓  Completed today — {self.completed_today}")
        if self.sla_compliance is not None:
            pct   = self.sla_compliance
            color = (_C["ok"] if pct >= 95 else
                     _C["warn"] if pct >= 80 else _C["alert"])
            self.lbl_sla.config(text=f"SLA {pct}%", fg=color)
        else:
            self.lbl_sla.config(text="")

        # Match what the rows show: alert queues use the live reconciled count,
        # since JSM's cached includeCount can report a stale non-zero.
        tray_queue = self.config.get("trayBadgeQueue", "Newly Opened")
        tray_count = next(
            (self._reconciled_counts.get(q["name"], q.get("issueCount", 0))
             for q in self.queues
             if tray_queue.lower() in q["name"].lower()),
            newly_count,
        )
        self._update_tray(tray_count, newly_count > 0, tray_queue)

        if snoozed:
            until = self._snooze_until.strftime("%H:%M")  # type: ignore[union-attr]
            self.lbl_status.config(text=f"Snoozed until {until}", fg=_C["warn"])
        elif self._alert_queues_missing:
            # Renaming an alert queue in Jira would otherwise just stop the
            # alerts, with nothing on screen to explain why.
            missing = ", ".join(self._alert_queues_missing)
            self.lbl_status.config(text=f"Alert queue not found: {missing}",
                                   fg=_C["warn"])
        else:
            # Every other branch here colours its whole line to match its
            # meaning (warn for snoozed/missing) — this one left the dot
            # sitting in the same dim grey as everything else, which reads
            # as a status indicator stuck off rather than "all good".
            self.lbl_status.config(text=f"● Live · {datetime.now():%H:%M:%S}",
                                   fg=_C["ok"])

        # Leave geometry alone while settings are open or a transition is
        # still running — both manage the window rect themselves.
        if not self._in_settings and not self._geom_job:
            show = self._manual_show or (
                ((newly_count > 0 and not self._dismissed)
                 or self.config.get("alwaysVisible", False))
                and not snoozed)
            if show:
                self.root.after(10, self._resize)
                if not self._visible and not self._show_job:
                    delay = 2_500 if has_new_alert else 0
                    self._show_job = self.root.after(delay, self._do_show)
            else:
                if self._show_job:
                    self.root.after_cancel(self._show_job)
                    self._show_job = None
                if self._visible:
                    self._do_hide()

    # ── Row flash ─────────────────────────────────────────────────────────────

    def _cancel_flash(self, name: str) -> None:
        """Drop a pending fade so it can't fire against a destroyed widget."""
        job = self._flash_jobs.pop(name, None)
        if job:
            try:
                self.root.after_cancel(job)
            except tk.TclError:
                pass

    def _flash_row(self, name: str, rw: dict) -> None:
        """Smoothly fade a row's background from an accent tint back to base
        when its count changes — a gentle highlight rather than a hard blink.
        The count chip keeps its own background.

        A row that changes again mid-fade restarts cleanly instead of leaving
        two fades fighting over the same background.
        """
        targets = [rw["row"], rw["lbl_name"], rw["dot"]]
        highlight, base = _C["flash"], _C["bg"]
        steps = 14

        job = self._flash_jobs.pop(name, None)
        if job:
            self.root.after_cancel(job)

        def tick(i: int) -> None:
            self._flash_jobs.pop(name, None)
            t   = i / steps
            col = _lerp_hex(highlight, base, t)
            try:
                for w in targets:
                    w.configure(bg=col)
            except tk.TclError:
                return          # row was destroyed (queue list changed)
            if i < steps:
                self._flash_jobs[name] = self.root.after(
                    28, lambda: tick(i + 1))

        tick(0)

    # ── Relative time ──────────────────────────────────────────────────────────

    def _update_relative_time(self) -> None:
        """Flag stale data — with 5 s live polling the status is normally
        rewritten every cycle, so only override it when updates have stopped."""
        if self._last_refresh and not self.error_msg and not self._conn_error and not self._is_snoozed():
            secs = (datetime.now() - self._last_refresh).total_seconds()
            if secs >= 90:
                rel = (f"{int(secs // 60)}m" if secs < 3_600
                       else f"{int(secs // 3_600)}h")
                self.lbl_status.config(text=f"Updated {rel} ago", fg=_C["warn"])
        self.root.after(30_000, self._update_relative_time)

    # ── Snooze ────────────────────────────────────────────────────────────────

    def _snooze(self, minutes: int) -> None:
        self._snooze_until = datetime.now() + timedelta(minutes=minutes)
        self._manual_show  = False   # snoozing means "get out of my way"
        self._update_ui()   # immediate: amber button, status text, hide if needed

    def _cancel_snooze(self) -> None:
        self._snooze_until = None
        self._update_ui()

    def _is_snoozed(self) -> bool:
        if self._snooze_until and datetime.now() < self._snooze_until:
            return True
        self._snooze_until = None
        return False

    # ── Show / hide with alpha fade ───────────────────────────────────────────

    def _user_show(self) -> None:
        """Explicit show (tray click) — stays visible until manually hidden."""
        self._manual_show = True
        self._dismissed   = False
        self._do_show()

    def _user_hide(self) -> None:
        """Explicit hide (⊟ / tray / dismiss) — stays hidden until a NEW ticket."""
        self._manual_show = False
        self._dismissed   = True
        self._do_hide()

    def _do_show(self) -> None:
        self._show_job = None
        if self._in_settings or self._geom_job:
            return   # settings panel manages its own visibility and geometry
        if self._visible and not self._fade_job:
            return
        self._visible = True
        self.root.deiconify()
        self._resize()
        self._animate_alpha(self.config.get("alpha", 0.93))

    def _do_hide(self) -> None:
        if self._show_job:
            self.root.after_cancel(self._show_job)
            self._show_job = None
        if not self._visible and not self._fade_job:
            return
        if self._tooltip:
            self._tooltip.destroy()
            self._tooltip = None
        self._visible = False
        self._animate_alpha(0.0, on_done=self.root.withdraw)

    def _animate_alpha(self, target: float, on_done=None,
                       duration_ms: int = 190) -> None:
        """Ease the window alpha to `target`, cancelling any fade in flight.

        Single animator for show, hide and the settings transition — two
        independent fades can no longer fight over the alpha channel. Uses a
        cubic ease-out so fades start briskly and settle gently (~60 fps).
        """
        if self._fade_job:
            self.root.after_cancel(self._fade_job)
            self._fade_job = None
        start = float(self.root.attributes("-alpha"))
        delta = target - start
        if abs(delta) < 0.001:
            self.root.attributes("-alpha", target)
            if on_done:
                on_done()
            return

        steps = max(1, duration_ms // 16)

        def ease_out(t: float) -> float:
            return 1.0 - (1.0 - t) ** 3

        def tick(i: int) -> None:
            if i >= steps:
                self.root.attributes("-alpha", target)
                self._fade_job = None
                if on_done:
                    on_done()
                return
            val = start + delta * ease_out(i / steps)
            self.root.attributes("-alpha", max(0.0, min(1.0, val)))
            self._fade_job = self.root.after(16, lambda: tick(i + 1))

        tick(1)

    # ── Resize / reposition ───────────────────────────────────────────────────

    def _resize(self) -> None:
        if self._in_settings or self._geom_job:
            return   # settings geometry is managed by the settings transition
        self.root.update_idletasks()
        w = self.config.get("overlayWidth", 260)
        self.root.geometry(f"{w}x{self.frame.winfo_reqheight() + 4}")
        self._reposition()

    def _reposition(self) -> None:
        self.root.update_idletasks()
        w = self.root.winfo_width()
        h = self.root.winfo_height()
        corners = self._corners(w, h)

        corner = self.config.get("snapCorner")
        if corner and corner in corners:
            x, y = corners[corner]
        elif "lastX" in self.config and "lastY" in self.config:
            x, y = self.config["lastX"], self.config["lastY"]
        else:
            x, y = corners["bottom-right"]

        mr = get_monitor_rect(x + w // 2, y + h // 2, work=True)
        if mr:
            ml, mt, mr2, mb = mr
            x = max(ml, min(x, mr2 - w))
            y = max(mt, min(y, mb - h))
        else:
            x, y = corners["bottom-right"]
            self.config.pop("lastX", None)
            self.config.pop("lastY", None)
            self.config["snapCorner"] = "bottom-right"

        self.root.geometry(f"+{x}+{y}")

    # ── Inline settings ───────────────────────────────────────────────────────

    _SETTINGS_MS = 260     # duration of the expand / collapse motion

    def _current_rect(self) -> tuple[int, int, int, int]:
        return (self.root.winfo_width(), self.root.winfo_height(),
                self.root.winfo_x(), self.root.winfo_y())

    def _animate_geometry(self, start: tuple[int, int, int, int],
                          target: tuple[int, int, int, int],
                          on_frame=None, on_done=None,
                          duration_ms: int = 0) -> None:
        """Ease the window rect from `start` to `target`.

        Cancels any motion already in flight, so a rapid second click takes
        over smoothly from wherever the window currently is rather than
        snapping back.
        """
        if self._geom_job:
            self.root.after_cancel(self._geom_job)
            self._geom_job = None
        duration = max(1, duration_ms or self._SETTINGS_MS) / 1000.0
        sw, sh, sx, sy = start
        tw, th, tx, ty = target
        begun = time.perf_counter()

        def ease(t: float) -> float:
            """Cubic ease-in-out.

            A plain ease-out covers a sixth of the distance in its first
            frame, which on a window this size reads as a snap followed by a
            drift. Easing in as well as out keeps the motion continuous at
            both ends, so it looks like the panel has weight.
            """
            return 4 * t ** 3 if t < 0.5 else 1 - ((-2 * t + 2) ** 3) / 2

        def tick() -> None:
            # Progress comes from the clock, not a frame counter: Tk's
            # after(16) really lands nearer 20 ms, which stretched a 260 ms
            # animation to 324 ms. Timing it means the duration is honest on
            # any machine, and a slow frame is skipped over rather than
            # dragging the whole transition out.
            t = (time.perf_counter() - begun) / duration
            if t >= 1.0:
                self._geom_job = None
                self.root.geometry("{}x{}+{}+{}".format(*target))
                if on_frame:
                    on_frame(target)
                if on_done:
                    on_done()
                return
            k = ease(t)
            cur = (round(sw + (tw - sw) * k), round(sh + (th - sh) * k),
                   round(sx + (tx - sx) * k), round(sy + (ty - sy) * k))
            self.root.geometry("{}x{}+{}+{}".format(*cur))
            if on_frame:
                on_frame(cur)
            # Deliberately no update_idletasks() here: forcing a synchronous
            # repaint of a panel this size costs ~25 ms, which halved the
            # frame rate and started dropping frames. Letting Tk repaint on
            # its own schedule is measurably smoother.
            self._geom_job = self.root.after(8, tick)

        tick()

    def _animate_panel_swap(self, incoming: tk.Frame, outgoing: tk.Frame,
                            target: tuple[int, int, int, int],
                            on_done=None) -> None:
        """Grow (or shrink) the window to `target` while swapping the panel.

        The incoming panel is packed normally and re-laid-out at each
        intermediate size. That is deliberate: the obvious optimisation —
        placing it at its final size and revealing it as the window grows —
        renders as an empty box, because the scroll canvas' contents then sit
        outside the window and Tk never draws them. Laying out per frame
        costs little here and is what actually looks right, with widgets
        stretching smoothly as the panel opens.
        """
        outgoing.pack_forget()
        incoming.pack(fill="both", expand=True)
        self.root.update_idletasks()
        self._animate_geometry(self._current_rect(), target, on_done=on_done)

    def _toggle_settings(self) -> None:
        if self._in_settings:
            self._close_settings()
            return
        self._in_settings = True
        if self._show_job:
            self.root.after_cancel(self._show_job)
            self._show_job = None
        if self._tooltip:
            self._tooltip.destroy()
            self._tooltip = None

        was_hidden = not self._visible
        if was_hidden:
            self._visible = True
            self.root.attributes("-alpha", 0.0)
            self.root.deiconify()
            self.root.update_idletasks()

        # Don't re-read the compact size while a transition is still running —
        # that would record an intermediate frame as "the compact overlay".
        if self._queue_geom is None or not self._geom_job:
            self._queue_geom = self._current_rect()
        self.btn_gear.config(text=_icon("close"))
        self._build_settings_panel()

        target = self._settings_target()
        if was_hidden:
            # Nothing on screen to expand from — snap to size and fade in.
            self.main_panel.pack_forget()
            self.settings_panel.pack(fill="both", expand=True)
            self.root.geometry("{}x{}+{}+{}".format(*target))
            self._animate_alpha(self.config.get("alpha", 0.93))
        else:
            self._animate_panel_swap(self.settings_panel, self.main_panel,
                                     target)

    _SETTINGS_MIN_H = 320   # never open a panel too short to be usable

    def _settings_target(self) -> tuple[int, int, int, int]:
        """Target (w, h, x, y) for the settings panel, anchored to the
        overlay's own monitor with right edges aligned to the compact view.

        The height is chosen first and the panel then placed, rather than
        derived from wherever the overlay happens to sit. Deriving it meant an
        overlay dragged above the top of the screen produced a *negative*
        height, which is not a geometry Tk accepts.
        """
        cw, ch, cx, cy = self._queue_geom  # type: ignore[misc]
        tw   = self._SETTINGS_W
        rect = get_monitor_rect(cx + cw // 2, cy + ch // 2, work=True)
        left, top, right, bottom = (rect if rect else
                                    (0, 0, self._sw, self._sh - 50))
        top_limit, bot_limit = top + 20, bottom - 10

        th = min(int((bottom - top) * 0.9), max(1, bot_limit - top_limit))
        th = max(self._SETTINGS_MIN_H, th)
        # Share the compact overlay's bottom edge where possible, then clamp
        # so the whole panel stays on the work area.
        ty = min(cy + ch, bot_limit) - th
        ty = max(top_limit, min(ty, bot_limit - th))
        tx = max(left + 10, min(cx + cw - tw, right - tw - 10))
        return tw, th, tx, ty

    def _close_settings(self) -> None:
        """Collapse back to the compact overlay, reversing the expand."""
        # Clear the flag now rather than when the motion ends: it records
        # intent, not animation state. Leaving it set until the collapse
        # finished meant a click during those 260 ms read as another close,
        # so rapid toggling ended up in the wrong state.
        self._in_settings = False
        if not self._queue_geom:
            return
        self.btn_gear.config(text=_icon("settings"))
        self._animate_panel_swap(self.main_panel, self.settings_panel,
                                 self._queue_geom,
                                 on_done=self._settings_closed)

    def _settings_closed(self) -> None:
        # A queue update may have arrived while settings were open.
        self._resize()

    def _build_settings_panel(self) -> None:
        """Rebuild inline settings content from current config."""
        for child in self.settings_panel.winfo_children():
            child.destroy()

        cfg         = self.config
        queue_names = self._all_queue_names or [q["name"] for q in self.queues]
        BG, FG, SEL = _C["bg"], _C["fg"], _C["input"]

        # Scrollable body
        wrap   = tk.Frame(self.settings_panel, bg=BG)
        wrap.pack(fill="both", expand=True)
        canvas = tk.Canvas(wrap, bg=BG, highlightthickness=0)
        sb     = tk.Scrollbar(wrap, orient="vertical", command=canvas.yview)
        body   = tk.Frame(canvas, bg=BG)
        body.bind("<Configure>",
                  lambda _e: canvas.configure(scrollregion=canvas.bbox("all")))
        cw_id = canvas.create_window((0, 0), window=body, anchor="nw")
        canvas.configure(yscrollcommand=sb.set)
        canvas.bind("<Configure>",
                    lambda e: canvas.itemconfig(cw_id, width=e.width))
        scroll_cmd = lambda e: canvas.yview_scroll(-1 * (e.delta // 120), "units")
        canvas.bind("<MouseWheel>", scroll_cmd)
        body.bind(  "<MouseWheel>", scroll_cmd)
        sb.pack(side="right", fill="y")
        canvas.pack(side="left", fill="both", expand=True)

        # ── Section helpers (defined after body so they target body) ──────────
        def section(title: str) -> None:
            # padx=16, matching every row of content a section introduces
            # (check(), and every direct label/entry below) - this used to
            # be 12, leaving every section title and its divider sitting
            # 4px left of everything under it, on every section in the
            # panel.
            tk.Label(body, text=title.upper(), font=_font(7, "bold"),
                     fg=_C["accent"], bg=BG).pack(anchor="w", padx=16, pady=(12, 0))
            tk.Frame(body, bg=_C["divider"], height=1).pack(fill="x", padx=16, pady=(3, 2))

        def check(parent: tk.Widget, text: str, var: tk.BooleanVar) -> None:
            # activebackground matching the resting bg made Tk's own hover
            # state a no-op - every row looked identically inert whether the
            # mouse was over it or not, unlike every button elsewhere in the
            # app, which all darken or lighten on hover. Bound explicitly,
            # like every other hover-responsive widget here, rather than
            # trusting Tk's native-themed active state to honour the color.
            cb = tk.Checkbutton(parent, text=text, variable=var,
                                fg=FG, bg=BG, selectcolor=SEL,
                                activeforeground=_C["fg_bright"],
                                activebackground=_C["chip"],
                                font=_font(9))
            cb.pack(anchor="w", padx=16, pady=1)
            cb.bind("<Enter>", lambda _e, w=cb: w.config(bg=_C["chip"]))
            cb.bind("<Leave>", lambda _e, w=cb: w.config(bg=BG))

        # Stats refresh interval
        section("Stats refresh interval")
        tk.Label(body, text="Queue counts update live every 5 s.  This controls how often\nSLA % and Completed Today refresh (heavier queries).",
                 font=_font(8), fg=_C["fg_dim"], bg=BG,
                 justify="left").pack(anchor="w", padx=16, pady=(0, 4))
        rf = tk.Frame(body, bg=BG)
        rf.pack(fill="x", padx=16, pady=(0, 4))
        s_refresh = tk.IntVar(value=cfg.get("refreshSeconds", 60))
        for secs, lbl in [(30, "30 s"), (60, "1 min"), (120, "2 min"), (300, "5 min")]:
            rb = tk.Radiobutton(rf, text=lbl, variable=s_refresh, value=secs,
                                fg=FG, bg=BG, selectcolor=SEL,
                                activeforeground=_C["fg_bright"],
                                activebackground=_C["chip"],
                                font=_font(9))
            rb.pack(side="left", padx=(0, 8))
            rb.bind("<Enter>", lambda _e, w=rb: w.config(bg=_C["chip"]))
            rb.bind("<Leave>", lambda _e, w=rb: w.config(bg=BG))

        # Alerts
        section("Alerts")
        s_notif = tk.BooleanVar(value=cfg.get("notificationsEnabled", True))
        s_sound = tk.BooleanVar(value=cfg.get("soundEnabled", True))
        check(body, "Desktop notification", s_notif)
        check(body, "Sound alert",          s_sound)

        # Appearance
        section("Appearance")
        af = tk.Frame(body, bg=BG)
        af.pack(fill="x", padx=16, pady=(0, 4))
        tk.Label(af, text="Transparency", font=_font(9),
                 fg=FG, bg=BG).grid(row=0, column=0, sticky="w")
        s_alpha = tk.DoubleVar(value=cfg.get("alpha", 0.93))
        tk.Scale(af, from_=0.3, to=1.0, resolution=0.05, orient="horizontal",
                 variable=s_alpha, length=140, bg=BG, fg=FG,
                 troughcolor=SEL, highlightthickness=0, sliderrelief="flat"
                 ).grid(row=0, column=1, padx=(8, 0))
        tk.Label(af, text="Width", font=_font(9),
                 fg=FG, bg=BG).grid(row=1, column=0, sticky="w", pady=(6, 0))
        s_width = tk.IntVar(value=cfg.get("overlayWidth", 260))
        tk.Scale(af, from_=200, to=420, resolution=10, orient="horizontal",
                 variable=s_width, length=140, bg=BG, fg=FG,
                 troughcolor=SEL, highlightthickness=0, sliderrelief="flat"
                 ).grid(row=1, column=1, padx=(8, 0), pady=(6, 0))
        s_always = tk.BooleanVar(value=cfg.get("alwaysVisible", False))
        check(body, "Always show (dashboard mode)", s_always)

        # Queue filtering
        section("Queue filtering")
        kf = tk.Frame(body, bg=BG)
        kf.pack(fill="x", padx=16, pady=(0, 4))
        tk.Label(kf, text="Hide queues whose name contains:",
                 font=_font(8), fg=FG, bg=BG).pack(anchor="w")
        s_skip_kw = tk.StringVar(value=", ".join(cfg.get("skipKeywords", [])))
        tk.Entry(kf, textvariable=s_skip_kw, bg=SEL, fg=_C["fg_bright"],
                 insertbackground="white", relief="flat", font=_font(9)
                 ).pack(fill="x", ipady=3, pady=(2, 6))
        tk.Label(kf, text="Hide queues with more than N tickets (0 = no limit):",
                 font=_font(8), fg=FG, bg=BG).pack(anchor="w")
        s_max_q = tk.IntVar(value=cfg.get("maxQueueSize", 0))
        tk.Spinbox(kf, from_=0, to=999_999, increment=1_000,
                   textvariable=s_max_q, width=10, bg=SEL, fg=_C["fg_bright"],
                   insertbackground="white", relief="flat", font=_font(9),
                   buttonbackground=SEL).pack(anchor="w", ipady=3, pady=(2, 0))

        # Queues to show
        hidden_set = set(cfg.get("hiddenQueues", []))
        s_queue_vars: dict[str, tk.BooleanVar] = {}
        if queue_names:
            section("Queues to show")
            for name in queue_names:
                var = tk.BooleanVar(value=name not in hidden_set)
                s_queue_vars[name] = var
                check(body, name, var)

        # Tray badge
        section("Tray badge shows")
        s_tray_badge = tk.StringVar(value=cfg.get("trayBadgeQueue", ""))
        for name in queue_names:
            rb = tk.Radiobutton(body, text=name, variable=s_tray_badge, value=name,
                                fg=FG, bg=BG, selectcolor=SEL,
                                activeforeground=_C["fg_bright"],
                                activebackground=_C["chip"],
                                font=_font(9))
            rb.pack(anchor="w", padx=16)
            rb.bind("<Enter>", lambda _e, w=rb: w.config(bg=_C["chip"]))
            rb.bind("<Leave>", lambda _e, w=rb: w.config(bg=BG))

        # Alert queues
        section("Alert queues (trigger overlay + notifications)")
        current_alerts = set(cfg.get("alertQueues",
                             [cfg["alertQueue"]] if cfg.get("alertQueue") else []))
        s_alert_vars: dict[str, tk.BooleanVar] = {}
        for name in queue_names:
            var = tk.BooleanVar(value=name in current_alerts)
            s_alert_vars[name] = var
            check(body, name, var)

        # SLA & completed today
        section("SLA & completed today")
        s_sla_on = tk.BooleanVar(value=cfg.get("slaEnabled", True))
        check(body, "Show SLA compliance", s_sla_on)
        sf = tk.Frame(body, bg=BG)
        sf.pack(fill="x", padx=16, pady=(0, 4))
        tk.Label(sf, text="SLA field name:",
                 font=_font(9), fg=FG, bg=BG).pack(anchor="w")
        s_sla = tk.StringVar(value=cfg.get("slaField", "Time to resolution"))
        sla_entry = tk.Entry(sf, textvariable=s_sla, bg=SEL,
                             fg=_C["fg_bright"], insertbackground="white",
                             relief="flat", font=_font(9))
        sla_entry.pack(fill="x", ipady=3, pady=(2, 2))
        tk.Label(sf, text="Plenty of teams have SLAs and don't track them.\n"
                          "Turn this off and the overlay stops asking Jira "
                          "about them entirely.",
                 font=_font(8), fg=_C["fg_dim"], bg=BG,
                 justify="left").pack(anchor="w", pady=(0, 6))

        def _sync_sla_entry(*_a: object) -> None:
            sla_entry.config(state="normal" if s_sla_on.get() else "disabled")
        s_sla_on.trace_add("write", _sync_sla_entry)
        _sync_sla_entry()
        tk.Label(sf, text='Extra JQL filter (e.g. "assignee is not EMPTY"):',
                 font=_font(8), fg=FG, bg=BG).pack(anchor="w")
        s_completed_filter = tk.StringVar(value=cfg.get("completedTodayFilter", ""))
        tk.Entry(sf, textvariable=s_completed_filter, bg=SEL, fg=_C["fg_bright"],
                 insertbackground="white", relief="flat", font=_font(9)
                 ).pack(fill="x", ipady=3, pady=(2, 0))

        # System
        section("System")
        s_startup = tk.BooleanVar(value=startup_enabled())
        check(body, "Run on Windows startup", s_startup)
        tk.Label(body, text=f"Jira Overlay {APP_VERSION}", font=_font(8),
                 fg=_C["fg_dim"], bg=BG).pack(anchor="w", padx=16, pady=(4, 0))
        tk.Frame(body, bg=BG, height=4).pack()
        # No leading icon: the key emoji here couldn't take the app's text
        # colour (it's a fixed-colour glyph, not a font-drawn one), so it
        # just sat there in its own Windows-emoji colours - the one glyph
        # anywhere in the app that didn't match anything around it.
        reconf_btn = tk.Button(body, text="Reconfigure credentials…",
                               command=self._reconfigure, bg=SEL, fg=FG,
                               font=_font(9), relief="flat", padx=8, pady=4,
                               cursor="hand2")
        reconf_btn.pack(anchor="w", padx=16, pady=(0, 8))
        reconf_btn.bind("<Enter>", lambda _e: reconf_btn.config(bg=_C["chip"]))
        reconf_btn.bind("<Leave>", lambda _e: reconf_btn.config(bg=SEL))

        # Triage learner health — a separate, optional process on this same
        # machine. Its own report lives in a file nobody opens day to day;
        # this is the thing people actually have open, so a problem there
        # (a dead token, a broken tune) should be visible here too.
        learner = self._load_learner_status()
        if learner is not None:
            section("Triage learner")
            # A different process writes this file, so nothing about its
            # shape can be trusted — a dict-shaped value can arrive as a
            # string or a list from a hand-edit or an older/newer version
            # writing a different schema, and `x or {}` only guards against
            # falsy values, not wrong types.
            healthy = bool(learner.get("healthy", True))
            prov_raw = learner.get("provenance")
            prov     = prov_raw if isinstance(prov_raw, dict) else {}
            acc_raw  = learner.get("accuracy")
            acc      = acc_raw if isinstance(acc_raw, dict) else {}
            row = tk.Frame(body, bg=BG)
            row.pack(fill="x", padx=16, pady=(0, 1))
            tk.Label(row, text="●", font=_font(10),
                     fg=_C["ok"] if healthy else _C["alert"],
                     bg=BG).pack(side="left")
            if healthy:
                headline = "Healthy"
            else:
                refusals = learner.get("refusals")
                codes = (", ".join(str(c) for c in refusals)
                        if isinstance(refusals, list) else "")
                headline = f"Jira is refusing requests ({codes or '?'})"
            tk.Label(row, text=headline, font=_font(9, "bold"),
                     fg=_C["fg_bright"] if healthy else _C["alert"],
                     bg=BG).pack(side="left", padx=(5, 0))
            details = []
            trained = prov.get("trainedOnDecisions")
            if trained:
                details.append(f"{trained} decisions learned")
            if prov.get("lastTuneDate"):
                details.append(f"last tuned {prov['lastTuneDate']}")
            if isinstance(acc.get("pct"), (int, float)):
                details.append(f"{acc['pct']:.0f}% accurate")
            if learner.get("version"):
                details.append(f"v{learner['version']}")
            if details:
                tk.Label(body, text=" · ".join(details), font=_font(8),
                        fg=_C["fg_dim"], bg=BG, justify="left"
                        ).pack(anchor="w", padx=16, pady=(0, 2))
            drift = learner.get("drift")
            if isinstance(drift, dict) and drift.get("detail"):
                # U+FE0E: plain triangle, not a colour emoji that would
                # ignore fg=_C["warn"] below.
                tk.Label(body,
                        text=f"⚠︎ Drift since {drift.get('date', 'last tune')}: "
                             f"{drift['detail']}",
                        font=_font(8), fg=_C["warn"], bg=BG,
                        justify="left", wraplength=280
                        ).pack(anchor="w", padx=16, pady=(0, 8))
            else:
                tk.Frame(body, bg=BG, height=6).pack()

        # Save / Cancel
        was_sla_on    = bool(cfg.get("slaEnabled", True))
        was_sla_field = (cfg.get("slaField") or "").strip()

        def _save() -> None:
            set_startup(s_startup.get())
            alert_list = [n for n, v in s_alert_vars.items() if v.get()]
            try:
                max_q = max(0, int(s_max_q.get()))
            except (tk.TclError, ValueError):
                max_q = 0   # junk typed into the spinbox — treat as no limit
            self.config.update({
                "refreshSeconds":       s_refresh.get(),
                "notificationsEnabled": s_notif.get(),
                "soundEnabled":         s_sound.get(),
                "hiddenQueues":         [n for n, v in s_queue_vars.items() if not v.get()],
                "trayBadgeQueue":       s_tray_badge.get(),
                "alpha":                round(s_alpha.get(), 2),
                "overlayWidth":         s_width.get(),
                "alwaysVisible":        s_always.get(),
                "alertQueues":          alert_list,
                "alertQueue":           alert_list[0] if alert_list else "",
                "slaEnabled":           s_sla_on.get(),
                # No fallback to the default here: forcing it back meant the
                # setting could never actually be turned off.
                "slaField":             s_sla.get().strip(),
                "completedTodayFilter": s_completed_filter.get().strip(),
                "skipKeywords":         [k.strip() for k in s_skip_kw.get().split(",")
                                         if k.strip()],
                "maxQueueSize":         max_q,
            })
            # Changing either SLA setting is the user telling us to try
            # again, so drop any "this desk has no such field" latch.
            if (s_sla_on.get() != was_sla_on) or (s_sla.get().strip() != was_sla_field):
                self._sla_unavailable = False
            save_config(self.config)
            # Width may have changed — refit the header to the new space.
            self.lbl_title.config(text=self._title_text(
                self.config.get("serviceDeskName", "JIRA")))
            for row_name, rw in self.row_widgets.items():
                self._cancel_flash(row_name)
                rw["row"].destroy()
            self.row_widgets.clear()
            if self.loading:
                self._settings_changed = True
            else:
                self._fetch()
            self._close_settings()

        bf = tk.Frame(self.settings_panel, bg=BG)
        bf.pack(fill="x", pady=(6, 2), padx=2)
        save_btn = tk.Button(bf, text="✓  Save", command=_save,
                  bg=_C["accent"], fg="white", font=_font(9, "bold"),
                  relief="flat", padx=10, pady=6, cursor="hand2", bd=0,
                  activebackground=_C["accent_hi"], activeforeground="white")
        save_btn.pack(side="left", fill="x", expand=True, padx=(0, 4))
        cancel_btn = tk.Button(bf, text="✕  Cancel", command=self._close_settings,
                  bg=SEL, fg=FG, font=_font(9),
                  relief="flat", padx=10, pady=6, cursor="hand2", bd=0,
                  activebackground=_C["chip"], activeforeground=_C["fg_bright"])
        cancel_btn.pack(side="left", fill="x", expand=True)
        save_btn.bind("<Enter>",   lambda _e: save_btn.config(bg=_C["accent_hi"]))
        save_btn.bind("<Leave>",   lambda _e: save_btn.config(bg=_C["accent"]))
        cancel_btn.bind("<Enter>", lambda _e: cancel_btn.config(bg=_C["chip"]))
        cancel_btn.bind("<Leave>", lambda _e: cancel_btn.config(bg=SEL))

        # Every control in here is something you might be hovering when you
        # reach for the wheel, so the whole subtree scrolls the panel.
        _bind_wheel(self.settings_panel, scroll_cmd)

    def _reconfigure(self) -> None:
        if messagebox.askyesno("Reconfigure",
                               "Clear saved credentials and restart setup?",
                               parent=self.root):
            if self._tray:
                self._tray.stop()
            try:
                os.remove(CONFIG_FILE)
            except OSError:
                pass
            # Hand the single-instance mutex over before spawning the
            # replacement, or it would see us still holding it and exit.
            release_single_instance()
            # Relaunch so the setup window actually appears (previously this
            # just quit and the user had to start the app again by hand)
            subprocess.Popen([_pythonw_exe(), os.path.abspath(__file__)],
                             creationflags=0x08000000)
            self.root.destroy()

    def run(self) -> None:
        try:
            self.root.mainloop()
        finally:
            if self._tray:
                self._tray.stop()


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main() -> None:
    if not acquire_single_instance():
        return          # another overlay already has the tray icon
    config = load_config()
    if not config:
        setup  = SetupWindow()
        config = setup.result
        if not config:
            return
    JiraOverlay(config).run()


def _report_fatal(detail: str) -> None:
    """Show why the overlay couldn't start.

    Running under pythonw there is no console, so an unhandled exception
    otherwise kills the process with nothing on screen and nothing on disk —
    the app simply appears not to work.
    """
    try:
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                               "jira_overlay_error.log"),
                  "a", encoding="utf-8") as f:
            f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S}\n{detail}\n\n")
    except OSError:
        pass
    try:
        root = tk.Tk()
        root.withdraw()
        messagebox.showerror("Jira Overlay — could not start", detail[-1500:])
        root.destroy()
    except Exception:
        pass


if __name__ == "__main__":
    try:
        main()
    except Exception:
        _report_fatal(traceback.format_exc())
        # The real traceback is already in the log and the dialog.
        raise SystemExit(1) from None
