"""Shared plumbing for the test suite.

The apps are single-file .pyw scripts, so each test loads them the way the
runtime does rather than importing a package. Every test that touches
configuration must point CONFIG_FILE at a temporary directory first: a suite
run once wrote a stub over a live install and destroyed the credentials, so
isolation here is not a nicety.
"""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load(script: str, name: str):
    """Load one of the app scripts as a module."""
    spec = importlib.util.spec_from_file_location(
        name, os.path.join(PROJECT, script))
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


def load_overlay(isolate_config: bool = True):
    ov = load("jira_overlay.pyw", "ov")
    if isolate_config:
        ov.CONFIG_FILE = os.path.join(tempfile.mkdtemp(), "jira_config.json")
    return ov


def load_learner(isolate_files: bool = True):
    tl = load("triage_learner.pyw", "tl")
    if isolate_files:
        tmp = tempfile.mkdtemp()
        for attr in ("CONFIG_FILE", "STATE_FILE", "LOG_FILE", "HISTORY_FILE",
                     "REPORT_FILE", "ERROR_LOG", "SUGGEST_FILE", "STATUS_FILE"):
            if hasattr(tl, attr):
                setattr(tl, attr, os.path.join(tmp, attr.lower() + ".dat"))
    return tl


class Checker:
    """Minimal assertion recorder, so a failure names the behaviour."""

    def __init__(self, title: str) -> None:
        print(f"--- {title} ---")
        self.ok = True

    def __call__(self, name: str, condition: bool, detail: str = "") -> bool:
        print(("PASS  " if condition else "FAIL  ") + name
              + (f"   {detail}" if detail else ""))
        self.ok = self.ok and bool(condition)
        return bool(condition)

    def done(self) -> int:
        print("RESULT:", "ALL PASS" if self.ok else "FAILURES PRESENT")
        return 0 if self.ok else 1
