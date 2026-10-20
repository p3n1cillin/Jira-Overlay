"""Launch both companions using this installation's interpreter, never PATH."""
from pathlib import Path
import subprocess
import sys


def main():
    root = Path(__file__).resolve().parent
    interpreter = root / "runtime" / "pythonw.exe"
    if not interpreter.is_file():
        interpreter = Path(sys.executable).with_name("pythonw.exe")
    for script in ("jira_overlay.pyw", "triage_learner.pyw"):
        subprocess.Popen([str(interpreter), str(root / script)], cwd=root,
                         creationflags=0x08000000)


if __name__ == "__main__":
    main()
