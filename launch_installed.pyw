"""Launch the overlay, which starts the learner after credentials are ready."""
from pathlib import Path
import subprocess
import sys


def main():
    root = Path(__file__).resolve().parent
    interpreter = root / "runtime" / "pythonw.exe"
    if not interpreter.is_file():
        interpreter = Path(sys.executable).with_name("pythonw.exe")
    subprocess.Popen([str(interpreter), str(root / "jira_overlay.pyw")], cwd=root,
                     creationflags=0x08000000)


if __name__ == "__main__":
    main()
