"""Repeatable developer checks and narrowly scoped generated-cache cleanup.

python tools/maintain.py --check
python tools/maintain.py --clean

Never touches credentials, learner data/backups, Git history or release assets.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def clean_generated(root=ROOT):
    root = Path(root).resolve()
    targets = [root / name for name in (".ruff_cache", ".mypy_cache", "__pycache__")]
    for directory in ("app", "tools", "installer", "tests"):
        folder = root / directory
        if folder.is_dir() and not folder.is_symlink():
            targets.extend(folder.rglob("__pycache__"))
    count = 0
    for target in targets:
        resolved = target.resolve()
        if not target.exists():
            continue
        if target.is_symlink() or root not in resolved.parents or target.name not in {
                "__pycache__", ".ruff_cache", ".mypy_cache"}:
            raise ValueError("Refusing unsafe cache removal: " + str(target))
        shutil.rmtree(target)
        count += 1
        print("Removed generated cache:", target.relative_to(root))
    return count


def check():
    commands = ([sys.executable, "-m", "ruff", "check", "."],
                [sys.executable, "tests/run_all.py"],
                [sys.executable, "release_tools.py", "--output", "dist"])
    for command in commands:
        subprocess.run(command, cwd=ROOT, check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--clean", action="store_true")
    args = parser.parse_args()
    if not args.check and not args.clean:
        parser.print_help()
    if args.clean:
        clean_generated()
    if args.check:
        check()
