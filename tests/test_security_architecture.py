"""Security regressions use isolated directories and mocked process creation."""
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch
from _harness import Checker, load, PROJECT

check=Checker("security architecture")
launcher=load("launch_installed.pyw","security_launcher")
with tempfile.TemporaryDirectory() as directory:
    root=Path(directory).resolve()
    (root/"runtime").mkdir()
    (root/"runtime/pythonw.exe").write_bytes(b"fixture; never run")
    (root/"unins000.exe").write_bytes(b"fixture; never run")
    (root/"app_guard.pyw").write_text("fixture; never run")
    calls=[]
    kernel=SimpleNamespace(CreateEventW=lambda *a:1,SetEvent=lambda *a:None,CloseHandle=lambda *a:None)
    with patch.object(launcher,"__file__",str(root/"launch_installed.pyw")), \
         patch.object(sys,"argv",["launch_installed.pyw"]), \
         patch.object(launcher.updates,"component_running",return_value=False), \
         patch.object(launcher.updates,"_kernel",return_value=kernel), \
         patch.object(launcher.subprocess,"Popen",side_effect=lambda args,**kw:calls.append((args,kw))):
        launcher.main()
        check("normal launch starts only the local user-session guardian",len(calls)==1 and calls[0][0]==[str(root/"runtime/pythonw.exe"),"-B",str(root/"app_guard.pyw")])
        check("no shell, script host, task command or breakaway flag is used",calls[0][1]=={"cwd":root})
        (root/".jira-overlay-security-hold").write_text("isolated review hold")
        try:
            launcher.main()
            held=False
        except SystemExit: held=True
        check("a security hold refuses launch before any child is created",held and len(calls)==1)

project=Path(PROJECT)
for name in ("app/app_lifecycle.py","app/native_update.py","launch_installed.pyw","app_guard.pyw","installer/JiraOverlay.iss"):
    source=(project/name).read_text(encoding="utf-8")
    check(name+" contains no task registration or policy override",not any(text in source for text in ("RegisterTaskDefinition","ExecutionPolicy","WindowsPowerShell")))
for name in ("setup.bat","update.bat"):
    check(name+" respects the organisation's script policy","ExecutionPolicy" not in (project/name).read_text())
workflow=(project/".github/workflows/release.yml").read_text()
check("configured signing is verified without pretending unsigned builds are signed","Get-AuthenticodeSignature" in workflow and "APPROVED_SIGNER" in workflow and "$files.Count -ne 2" in workflow and "Unsigned build:" in workflow)
check("new installations require explicit startup consent",'Flags: unchecked' in (project/"installer/JiraOverlay.iss").read_text())
raise SystemExit(check.done())
