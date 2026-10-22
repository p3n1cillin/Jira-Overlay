"""Native helper tests using isolated files and unique IPC names only."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid
from unittest.mock import patch
from _harness import Checker, PROJECT as PROJECT_DIR
import app_updates as u
from app_release import VERSION, REPOSITORY

PROJECT=Path(PROJECT_DIR)
sys.path.insert(0,str(PROJECT/"installer"))
from build_helper import build_helper
check=Checker("native installer lifecycle")

with tempfile.TemporaryDirectory(prefix="jira-native-unit-") as directory:
    root=Path(directory)
    body=b"isolated installer fixture; never executed"
    metadata={"tag_name":"v"+VERSION,"assets":[
        {"name":f"JiraOverlay-{VERSION}.zip","digest":"sha256:"+"1"*64,"browser_download_url":f"https://github.com/{REPOSITORY}/releases/download/v{VERSION}/JiraOverlay-{VERSION}.zip"},
        {"name":f"JiraOverlay-{VERSION}-Setup.exe","digest":"sha256:"+hashlib.sha256(body).hexdigest(),"browser_download_url":f"https://github.com/{REPOSITORY}/releases/download/v{VERSION}/JiraOverlay-{VERSION}-Setup.exe"}]}
    with patch.object(u,"_fetch",return_value=json.dumps(metadata).encode()):
        info=u.latest_release(current="2020.01.01")
        check("release metadata binds installers to the advertised repository and version", info["installer"]["url"].endswith(f"/v{VERSION}/JiraOverlay-{VERSION}-Setup.exe"))
    with patch.object(u,"_fetch",return_value=body):
        staged=u.stage_installer(info)
        check("explicit installer staging verifies the published digest",staged.read_bytes()==body)
        staged.unlink()
        staged.parent.rmdir()
        try:
            u.stage_installer(dict(info,installer=dict(info["installer"],sha256="0"*64)))
            rejected=False
        except u.UpdateError: rejected=True
        check("tampered downloads are rejected",rejected)

    helper=build_helper(root/"installer_helper.exe")
    harness=root/"helper_test.exe"
    compiler=Path(os.environ["SystemRoot"])/"Microsoft.NET/Framework64/v4.0.30319/csc.exe"
    subprocess.run([str(compiler),"/nologo","/target:exe","/platform:x64","/out:"+str(harness),"/reference:"+str(helper),str(PROJECT/"tests/fixtures/installer_helper_test.cs")],check=True)
    def run(*args):
        return subprocess.run([str(harness),*map(str,args)],capture_output=True,text=True,timeout=20)
    old,new=root/"Old ZIP",root/"Application with spaces"
    old.mkdir()
    personal={"jira_config.json":b'{"token":"FAKE-private"}',"triage_state.json":b'{"model":"fake"}',"triage_history.jsonl":b'{"decision":"fake"}\n'}
    for name,data in personal.items(): (old/name).write_bytes(data)
    (old/"do_not_import.py").write_text("old code")
    result=run("migrate",old,new)
    check("native migration succeeds",result.returncode==0,result.stderr)
    check("credentials and models are copied byte-for-byte and originals retained",all((new/n).read_bytes()==v and (old/n).read_bytes()==v for n,v in personal.items()))
    check("no executable or arbitrary old files are imported",not(new/"do_not_import.py").exists())
    check("two models cannot be silently merged",run("migrate",old,new).returncode!=0)
    check("drive and profile roots are rejected",run("validate",Path.home()).returncode!=0 and run("validate",Path(root.anchor)).returncode!=0)
    unrelated=root/"Unrelated"
    unrelated.mkdir()
    (unrelated/"keep.txt").write_text("private")
    check("unrelated non-empty destinations are refused",run("validate",unrelated).returncode!=0 and (unrelated/"keep.txt").read_text()=="private")
    source=root/"Source"
    (source/".git").mkdir(parents=True)
    check("Git checkouts are never overwritten",run("validate",source).returncode!=0)
    (new/"app_release.py").write_text('VERSION = "2099.01.01"')
    check("downgrades are rejected",run("validate",new,"2020.01.01").returncode!=0)
    check("startup ownership uses exact quoted application paths",run("ownership",new).returncode==0)
    for mode in ("cooperative","legacy"):
        mutex="Local\\JiraHelperUnit-"+uuid.uuid4().hex
        ipc=root/mode
        ipc.mkdir()
        code=r'''
import ctypes,sys
k=ctypes.windll.kernel32
k.CreateMutexW.argtypes=[ctypes.c_void_p,ctypes.c_bool,ctypes.c_wchar_p];k.CreateMutexW.restype=ctypes.c_void_p
k.CreateEventW.argtypes=[ctypes.c_void_p,ctypes.c_bool,ctypes.c_bool,ctypes.c_wchar_p];k.CreateEventW.restype=ctypes.c_void_p
k.WaitForSingleObject.argtypes=[ctypes.c_void_p,ctypes.c_uint32];k.CloseHandle.argtypes=[ctypes.c_void_p]
m=k.CreateMutexW(None,False,sys.argv[1]);e=k.CreateEventW(None,True,False,sys.argv[2]) if sys.argv[3]=='cooperative' else None
print('READY',flush=True)
try:
 if e: assert k.WaitForSingleObject(e,20000)==0
 else: sys.stdin.readline()
finally:
 if e:k.CloseHandle(e)
 k.CloseHandle(m)
'''
        child=subprocess.Popen([sys.executable,"-c",code,mutex,u._event_name(ipc,"stop"),mode],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        try:
            ready = child.stdout.readline().strip()
            assert ready == "READY", (ready,child.stderr.read())
            result=run("stop",ipc,mutex)
            if mode=="cooperative":
                child.wait(timeout=5)
                check("native shutdown signals an isolated cooperative writer",result.returncode==0)
            else:
                check("legacy apps receive a manual-Quit instruction without forced termination",result.returncode!=0 and "BOTH Jira tray icons" in result.stderr and child.poll() is None)
        finally: child.communicate("\n",timeout=25)

source=(PROJECT/"installer/lifecycle_helper.cs").read_text()
check("native helper has no task registration or remote-memory feature",not any(s in source for s in ("RegisterTaskDefinition","VirtualAllocEx","WriteProcessMemory","CreateRemoteThread")))
check("installer invokes a native helper instead of a PowerShell script","WindowsPowerShell" not in (PROJECT/"installer/JiraOverlay.iss").read_text())
raise SystemExit(check.done())
