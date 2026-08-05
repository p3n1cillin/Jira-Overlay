#!/usr/bin/env python3
"""Run every test. No dependencies beyond the app's own.

    python tests/run_all.py

Each test is a separate process, because several of them monkey-patch
imports or Tk and must not contaminate the others.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SUITES = ["test_config.py", "test_overlay.py", "test_report.py",
          "test_learner.py", "test_setup.py"]


def main() -> int:
    env = dict(os.environ, PYTHONIOENCODING="utf-8")
    results = []
    for name in SUITES:
        path = os.path.join(HERE, name)
        if not os.path.exists(path):
            results.append((name, None, 0.0, "missing"))
            continue
        started = time.perf_counter()
        proc = subprocess.run([sys.executable, path], cwd=HERE, env=env,
                              capture_output=True, text=True, timeout=600)
        elapsed = time.perf_counter() - started
        results.append((name, proc.returncode, elapsed, proc.stdout))

    failed = [r for r in results if r[1] != 0]
    for name, code, elapsed, out in results:
        if code != 0:
            print(out or "")
    print("=" * 58)
    for name, code, elapsed, _out in results:
        state = "PASS" if code == 0 else ("MISSING" if code is None else "FAIL")
        print(f"  {state:<8} {name:<22} {elapsed:5.1f}s")
    print("=" * 58)
    if failed:
        print(f"{len(failed)} of {len(results)} suites failed")
        return 1
    print(f"all {len(results)} suites passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
