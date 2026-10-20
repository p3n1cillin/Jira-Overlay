"""Build the offline, per-user Windows installer from an allowlisted bundle.

Only CPython's standard library/runtime and freshly installed locked wheels
are copied. Never copy the build machine's site-packages or user profile.
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from source_layout import activate
activate(ROOT)
from app_release import VERSION
from app_updates import unpack_package
from release_tools import build_release


def copy_tree(source, target):
    shutil.copytree(source, target, ignore=shutil.ignore_patterns(
        "__pycache__", "*.pyc", "test", "tests", "idle_test", "demos"))


def runtime(target):
    base = Path(sys.base_prefix)
    target.mkdir()
    names = ("python.exe", "pythonw.exe", "python3.dll",
             f"python{sys.version_info.major}{sys.version_info.minor}.dll",
             "vcruntime140.dll", "vcruntime140_1.dll", "LICENSE.txt")
    for name in names:
        shutil.copy2(base / name, target / name)
    for name in ("DLLs", "tcl"):
        copy_tree(base / name, target / name)
    lib = target / "Lib"
    lib.mkdir()
    # sys.stdlib_module_names is CPython's own inventory, not an environment scan.
    for name in sorted(sys.stdlib_module_names):
        source = base / "Lib" / name
        if source.is_dir():
            copy_tree(source, lib / name)
        elif source.with_suffix(".py").is_file():
            shutil.copy2(source.with_suffix(".py"), lib / (name + ".py"))
    # Prevent registry Python paths, PYTHONPATH and user site-packages leakage.
    (target / f"python{sys.version_info.major}{sys.version_info.minor}._pth").write_text(
        ".\n..\nLib\nDLLs\nLib/site-packages\nimport site\n", encoding="utf-8")
    subprocess.run([sys.executable, "-m", "pip", "--isolated", "install", "--only-binary=:all:",
                    "--no-compile", "--target", str(lib / "site-packages"),
                    "-r", str(ROOT / "installer" / "requirements.lock")], check=True)
    subprocess.run([str(target / "python.exe"), "-B", "-c",
        "import tkinter, requests, PIL, pystray, tzdata, ssl, sqlite3, ctypes; "
        "from zoneinfo import ZoneInfo; ZoneInfo('Europe/London'); "
        "r=tkinter.Tk(); r.withdraw(); r.update(); r.destroy(); "
        "import app_release, app_updates; print(app_release.VERSION)"], check=True)


def build(compiler, output):
    if sys.platform != "win32" or sys.version_info[:2] != (3, 14) or struct.calcsize("P") != 8:
        raise RuntimeError("Build on Windows with a full 64-bit CPython 3.14+ installation (including Tk)")
    output = Path(output).resolve()
    archive = build_release(ROOT, output)
    with tempfile.TemporaryDirectory(prefix="jira-installer-build-") as scratch:
        stage = Path(scratch) / "app"
        unpack_package(archive, stage, VERSION)
        (stage / "docs").mkdir()
        for name in ("INSTALLATION.md", "PROJECT_STRUCTURE.md"):
            shutil.copy2(ROOT / "docs" / name, stage / "docs" / name)
        shutil.copy2(ROOT / "README.md", stage / "README.md")
        runtime(stage / "runtime")
        shutil.copy2(ROOT / "installer" / "support.ps1", stage / "installer_support.ps1")
        shutil.copy2(ROOT / "installer" / "legacy_shutdown.cs", stage / "legacy_shutdown.cs")
        subprocess.run([str(compiler), "/DAppVersion=" + VERSION,
                        "/DStageDir=" + str(stage), "/DOutputDir=" + str(output),
                        str(ROOT / "installer" / "JiraOverlay.iss")], check=True)
    target = output / f"JiraOverlay-{VERSION}-Setup.exe"
    digest = hashlib.sha256(target.read_bytes()).hexdigest()
    target.with_suffix(".exe.sha256").write_text(f"{digest}  {target.name}\n", encoding="utf-8")
    return target


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compiler", required=True)
    parser.add_argument("--output", default=str(ROOT / "dist"))
    args = parser.parse_args()
    print(build(args.compiler, args.output))
