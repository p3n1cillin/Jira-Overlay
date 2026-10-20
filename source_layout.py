"""Shared-module locations in a source checkout and legacy flat releases.

Launch scripts and personal data retain their old paths. Public ZIP payloads
remain flat so already-released updaters can still verify/install them.
"""
from pathlib import Path
import sys

SHARED_FILES = frozenset({"app_release.py", "app_updates.py", "triage_support.py",
    "triage_evaluation.py", "triage_jira.py", "triage_storage.py"})
DOCUMENTATION_FILES = frozenset({"INSTALLATION.md", "PROJECT_STRUCTURE.md"})


def activate(root):
    root = Path(root).resolve()
    folder = root / "app"
    location = str(folder if (folder / "app_release.py").is_file() else root)
    if location not in sys.path:
        sys.path.insert(0, location)


def release_source(root, name):
    root = Path(root).resolve()
    folder = "app" if name in SHARED_FILES else "docs" if name in DOCUMENTATION_FILES else ""
    candidate = root / folder / name if folder and (root / folder).is_dir() else root / name
    if candidate.is_symlink() or candidate.resolve().parent not in (root, root / "app", root / "docs"):
        raise ValueError("Refusing to publish linked/out-of-tree source files")
    return candidate
