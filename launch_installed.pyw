"""Launch the overlay, which starts the learner after credentials are ready."""
from pathlib import Path
import subprocess
import sys
from source_layout import activate
activate(Path(__file__).resolve().parent)
import app_lifecycle as lifecycle
import app_updates as updates


def main():
    root = Path(__file__).resolve().parent
    if (root / ".jira-overlay-security-hold").exists():
        raise SystemExit("Jira Overlay is held closed pending IT security approval.")
    if (root / "app_guard.pyw").is_file() and updates.native_install(root):
        if "--resume" not in sys.argv:
            for role in lifecycle.ROLES:
                lifecycle.set_wanted(root, role, True)
        if not updates.component_running(lifecycle.GUARD_MUTEX):
            subprocess.Popen([str(root / "runtime/pythonw.exe"), "-B", str(root / "app_guard.pyw")], cwd=root)
        kernel = updates._kernel()
        handle = kernel.CreateEventW(None, True, False, "JiraOverlayShowRequest")
        if handle:
            kernel.SetEvent(handle)
            kernel.CloseHandle(handle)
        return
    interpreter = root / "runtime" / "pythonw.exe"
    if not interpreter.is_file():
        interpreter = Path(sys.executable).with_name("pythonw.exe")
    subprocess.Popen([str(interpreter), str(root / "jira_overlay.pyw")], cwd=root,
                     creationflags=0x08000000)


if __name__ == "__main__":
    main()
