"""Organised source, legacy bundle compatibility and non-destructive cleanup."""
from pathlib import Path
import importlib.util
import re
import subprocess
import sys
import tempfile

from _harness import Checker, PROJECT as PROJECT_DIR
from app_release import RELEASE_FILES, VERSION
from app_updates import unpack_package
from release_tools import build_release
from source_layout import release_source, SHARED_FILES

PROJECT = Path(PROJECT_DIR)
check = Checker("project structure")
spec = importlib.util.spec_from_file_location("maintenance", PROJECT / "tools/maintain.py")
maintenance = importlib.util.module_from_spec(spec)
spec.loader.exec_module(maintenance)

check("shared modules live in a dedicated source folder", all(
    release_source(PROJECT, name).parent == PROJECT / "app" for name in SHARED_FILES))
check("entry-point names remain stable for existing launch and startup commands", all(
    (PROJECT / name).is_file() for name in ("jira_overlay.pyw", "triage_learner.pyw", "launch.bat", "setup.bat", "update.bat")))
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    for folder in ("__pycache__", ".ruff_cache", ".mypy_cache", "app/__pycache__", "tests/__pycache__"):
        (root / folder).mkdir(parents=True)
        (root / folder / "cache").write_text("regeneratable")
    private = {"jira_config.json": b"private", "triage_state.json": b"model",
               "triage_history.jsonl": b"history", "triage_backups/keep.json": b"backup",
               "dist/keep.zip": b"release"}
    for name, data in private.items():
        path = root / name
        path.parent.mkdir(exist_ok=True)
        path.write_bytes(data)
    check("cleanup removes only generated caches in known code folders", maintenance.clean_generated(root) == 5)
    check("cleanup preserves credentials, models, history, backups and release assets", all(
        (root / name).read_bytes() == data for name, data in private.items()))
    archive = build_release(PROJECT, root / "packages")
    payload = unpack_package(archive, root / "flat", VERSION)
    check("older updater clients still receive an explicitly allowlisted flat bundle",
          all((payload / name).is_file() for name in RELEASE_FILES))
    legacy_documents = {"update.bat", "launch.bat", "setup.bat", "setup.ps1", "requirements.txt",
                        "README.md", "CHANGELOG.md", "SECURITY.md", "LICENSE", "jira_config.example.json"}
    check("the first published updater accepts every filename in the new source ZIP", all(
        re.fullmatch(r"[a-z][a-z0-9_]*\.pyw?", name) or name in legacy_documents for name in RELEASE_FILES))
    proc = subprocess.run([sys.executable, "-I", "-c",
        "import sys;sys.path.insert(0,sys.argv[1]);import source_layout;source_layout.activate(sys.argv[1]);"
        "import app_release, app_updates, triage_jira, triage_storage;print(app_release.VERSION)", str(payload)],
        capture_output=True, text=True, timeout=30)
    check("published modules work without the developer source layout or PYTHONPATH",
          proc.returncode == 0 and VERSION in proc.stdout)
    check("documentation links also work in the flat downloadable bundle",
          "/blob/main/docs/INSTALLATION.md)" in (payload / "README.md").read_text(encoding="utf-8"))

raise SystemExit(check.done())
