"""Release privacy, untrusted archives, transactional updates and shutdown."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch
import uuid
import zipfile

from _harness import Checker, PROJECT
import app_updates as u
from app_release import VERSION, REPOSITORY, RELEASE_FILES
from release_tools import build_release

check = Checker("application updates")


def rejected(call):
    try:
        call()
    except (u.UpdateError, ValueError, OSError, zipfile.BadZipFile):
        return True
    return False


with tempfile.TemporaryDirectory() as folder:
    tmp = Path(folder)
    archive = build_release(PROJECT, tmp/"dist")
    with zipfile.ZipFile(archive) as zipped:
        contents = {name: zipped.read(name) for name in zipped.namelist()}
    check("public ZIP contains exactly the code allowlist and manifest",
          set(contents) == set(RELEASE_FILES) | {u.MANIFEST})
    check("release never includes live credentials, models, logs, reports or reviews",
          all(name not in contents for name in ("jira_config.json", "triage_state.json", "triage_history.jsonl",
              "triage_reviews.json", "triage_log.jsonl", "TRIAGE_REPORT.md", "TRIAGE_REVIEW_QUEUE.md")))
    second = build_release(PROJECT, tmp/"second")
    check("identical source produces a reproducible ZIP and checksum",
          archive.read_bytes() == second.read_bytes()
          and (tmp/"dist"/(archive.name+".sha256")).read_text().startswith(hashlib.sha256(archive.read_bytes()).hexdigest()))
    check("numeric version comparison handles patch releases and rejects injection",
          u.version_key("v2026.10.20.10") > u.version_key("2026.10.20.2")
          and rejected(lambda: u.version_key("v2026.10.20/../evil")))
    denied_kernel = SimpleNamespace(OpenMutexW=lambda *a: 0, GetLastError=lambda: 5)
    with patch.object(u, "_kernel", return_value=denied_kernel):
        check("access denied checking a process is not mistaken for a stopped application",
              rejected(u.apps_running))

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    metadata = {"tag_name": "v"+VERSION, "assets": [{"name": archive.name, "digest": "sha256:"+digest,
                "browser_download_url": f"https://github.com/{REPOSITORY}/releases/download/v{VERSION}/{archive.name}"}]}
    with patch.object(u, "_fetch", return_value=json.dumps(metadata).encode()):
        info = u.latest_release(current="2020.01.01")
        check("public update metadata selects a verified full package", info["sha256"] == digest)
        check("an installed version is not downgraded or reinstalled", u.latest_release(current=VERSION) is None)
    requests = []
    class Response:
        headers = {}
        def __enter__(self): return self
        def __exit__(self, *args): pass
        def geturl(self): return "https://api.github.com/repos/owner/project/releases/latest"
        def read(self, limit): return b"{}"
    def request_probe(request, **kwargs):
        requests.append(request)
        return Response()
    with patch.object(u.urllib.request, "build_opener", return_value=SimpleNamespace(open=request_probe)):
        u._fetch("https://api.github.com/repos/owner/project/releases/latest", 100)
    check("GitHub traffic cannot inherit Jira credentials or cookies",
          len(requests) == 1 and {name.lower() for name, _ in requests[0].header_items()} == {"user-agent", "accept"})
    with patch.object(u, "_fetch", return_value=archive.read_bytes()):
        check("the downloaded ZIP must match GitHub's independent asset digest",
              rejected(lambda: u.stage_release(dict(info, sha256="0"*64))))
    for field, value in (("draft", True), ("prerelease", True)):
        with patch.object(u, "_fetch", return_value=json.dumps(dict(metadata, **{field: value})).encode()):
            check("draft and prerelease packages are ignored", u.latest_release(current="2020.01.01") is None)
    bad = dict(metadata, assets=[dict(metadata["assets"][0], browser_download_url="https://evil.invalid/release.zip")])
    with patch.object(u, "_fetch", return_value=json.dumps(bad).encode()):
        check("a package URL outside the chosen repository is rejected",
              rejected(lambda: u.latest_release(current="2020.01.01")))
    for address in ("http://github.com/download", "https://github.com.evil.invalid/x", "https://user:password@github.com/x"):
        check("untrusted redirects are rejected before following them",
              rejected(lambda: u._Redirects().redirect_request(None, None, 302, "", {}, address)))

    payload = u.unpack_package(archive, tmp/"payload", VERSION)
    check("staged packages are verified again before use", u.verify_payload(payload)["version"] == VERSION)

    def bad_archive(changes, suffix):
        data = dict(contents)
        data.update(changes)
        path = tmp/(suffix+".zip")
        with zipfile.ZipFile(path, "w") as zipped:
            for name, value in data.items():
                zipped.writestr(name, value)
        return rejected(lambda: u.unpack_package(path, tmp/(suffix+"-payload"), VERSION))

    check("path traversal and private configuration entries cannot be extracted",
          bad_archive({"../escaped.py": b"bad"}, "traversal")
          and bad_archive({"jira_config.json": b"bad"}, "config"))
    check("modified code is rejected before extraction",
          bad_archive({"jira_overlay.pyw": b"bad"}, "hash"))
    manifest = json.loads(contents[u.MANIFEST])
    check("advertised and bundled versions must agree",
          bad_archive({u.MANIFEST: json.dumps(dict(manifest, version="2020.01.01")).encode()}, "version"))
    incomplete = tmp/"incomplete.zip"
    with zipfile.ZipFile(incomplete, "w") as zipped:
        for name, value in contents.items():
            if name != "triage_jira.py":
                zipped.writestr(name, value)
    check("a partial application bundle cannot be installed",
          rejected(lambda: u.unpack_package(incomplete, tmp/"partial", VERSION)))

    target = tmp/"installed"
    target.mkdir()
    original = {"jira_overlay.pyw": b"original overlay", "triage_learner.pyw": b"original learner"}
    for name, data in original.items():
        (target/name).write_bytes(data)
    private = {"jira_config.json": b"private credentials", "triage_state.json": b"private model",
               "triage_history.jsonl": b"private history", "triage_reviews.json": b"private reviews",
               "TRIAGE_REPORT.md": b"private report", "custom-note.txt": b"user data"}
    for name, data in private.items():
        (target/name).write_bytes(data)
    with u.update_lock(target):
        backup = u.install_payload(target, payload)
    check("complete code updates install together while every personal file is preserved",
          all((target/name).read_bytes() == contents[name] for name in manifest["files"])
          and all((target/name).read_bytes() == data for name, data in private.items()))
    check("code backup excludes personal files", not set(private) & {p.name for p in backup.iterdir()})
    u.restore_backup(target, backup)
    check("rollback restores old code and removes only newly installed code files",
          all((target/name).read_bytes() == data for name, data in original.items())
          and not (target/"app_release.py").exists()
          and all((target/name).read_bytes() == data for name, data in private.items()))

    actual_atomic = u._atomic
    writes = []
    def fail_once(path, data):
        writes.append(path.name)
        if len(writes) == 5:
            raise PermissionError("simulated locked code file")
        return actual_atomic(path, data)
    with patch.object(u, "_atomic", side_effect=fail_once):
        try:
            u.install_payload(target, payload)
        except PermissionError:
            pass
    check("a mid-update copy failure automatically rolls back all changed code",
          all((target/name).read_bytes() == data for name, data in original.items())
          and not (target/"app_release.py").exists()
          and not (target/".app-updates"/"active.json").exists())
    # Simulate a crash after the backup/journal but before code replacement ends.
    backup = u.install_payload(target, payload)
    (target/".app-updates"/"active.json").write_text(json.dumps({"backup": str(backup)}))
    check("the next startup recovers an interrupted update from its verified backup",
          u.recover_pending(target) and (target/"jira_overlay.pyw").read_bytes() == original["jira_overlay.pyw"])
    (target/".git").mkdir()
    check("updating a Git checkout never overwrites working files", rejected(lambda: u.install_payload(target, payload)))
    (target/".git").rmdir()
    (payload/"setup.py").write_bytes(b"tampered staging")
    check("staged code tampering is detected before installation", rejected(lambda: u.install_payload(target, payload)))
    check("all private data remains byte-identical after success, failure and recovery",
          all((target/name).read_bytes() == data for name, data in private.items()))

    if os.name == "nt":
        # Real Windows IPC, but unique test mutex/event names: never target the
        # user's running overlay or learner.
        mutex = "JiraOverlay-test-" + uuid.uuid4().hex
        program = """
import sys,threading
from source_layout import activate
activate(__import__('os').getcwd())
import app_updates as u
k=u._kernel(); h=k.CreateMutexW(None,False,sys.argv[2]); stop=threading.Event()
u.listen_for_update(sys.argv[1],stop.set)
print('ready',flush=True)
stop.wait(20)
print('saved' if stop.is_set() else 'timeout',flush=True)
k.CloseHandle(h)
"""
        child = subprocess.Popen([sys.executable, "-c", program, str(target), mutex], cwd=PROJECT,
                                 stdout=subprocess.PIPE, text=True, creationflags=0x08000000)
        try:
            ready = child.stdout.readline().strip()
            with patch.object(u, "MUTEXES", (mutex,)):
                with u.stop_apps(target, timeout=10):
                    child.wait(timeout=5)
            check("real graceful update signaling lets a writer save before process exit",
                  ready == "ready" and child.stdout.readline().strip() == "saved" and child.returncode == 0)
        finally:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=5)
            child.stdout.close()

sys.exit(check.done())
