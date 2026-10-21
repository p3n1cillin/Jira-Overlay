"""Public GitHub updates; code-only packages, verified staging and rollback.

No Jira session, credentials, config, learner data, or third-party imports.
Trust is HTTPS plus the configured GitHub repository, not a code-signing claim.
"""
from __future__ import annotations

import ast
import contextlib
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import stat
import tempfile
import threading
import time
from datetime import datetime, timezone
import urllib.error
import urllib.parse
import urllib.request
import zipfile

from app_release import VERSION, REPOSITORY

MAX_DOWNLOAD = 20 * 1024 * 1024
MAX_INSTALLER = 150 * 1024 * 1024
MAX_UNPACKED = 40 * 1024 * 1024
MANIFEST = "release-manifest.json"
MUTEXES = ("JiraOverlaySingleInstance", "JiraTriageLearner", "JiraOverlayGuardian")
_HOSTS = {"api.github.com", "github.com", "release-assets.githubusercontent.com",
          "objects.githubusercontent.com", "github-releases.githubusercontent.com"}
UPDATE_STATE = "app_update_status.json"
AUTO_RETRY_SECONDS = 6 * 60 * 60


class UpdateError(Exception):
    pass


def source_checkout(root):
    root = Path(root)
    return (root / ".git").exists() or (root / "app" / "app_release.py").is_file()


def update_state(root):
    """Read bounded, non-secret lifecycle diagnostics, not the Jira config."""
    try:
        path = _inside_file(Path(root).resolve(), UPDATE_STATE)
        if path.stat().st_size > 16384:
            return {}
        value = json.loads(path.read_text(encoding="utf-8-sig"))
        if not isinstance(value, dict):
            return {}
        version_key(value.get("version"))
        attempted = value.get("attemptedAt")
        if isinstance(attempted, bool) or not isinstance(attempted, (int, float)) or not math.isfinite(attempted):
            return {}
        return value
    except (OSError, ValueError, UpdateError):
        return {}


def record_update(root, version, phase, detail="", *, notified=False, log=""):
    root = Path(root).resolve()
    version_key(version)
    previous = update_state(root)
    attempted = previous.get("attemptedAt", time.time()) if previous.get("version") == version and phase != "downloading" else time.time()
    value = {"schema": 1, "version": version, "attemptedAt": attempted,
             "phase": phase, "detail": str(detail)[:1500], "notified": notified, "log": str(log)[:1500]}
    _atomic(_inside_file(root, UPDATE_STATE), json.dumps(value).encode("utf-8"))
    return value


def automatic_update_due(root, version, now=None):
    if source_checkout(root):
        return False
    previous = update_state(root)
    if previous.get("version") != version or previous.get("phase") == "cancelled":
        return True
    # One attempt after clock rollback repairs the timestamp rather than
    # postponing all future updates until an erroneous future date.
    now = time.time() if now is None else now
    return now < previous["attemptedAt"] or now - previous["attemptedAt"] >= AUTO_RETRY_SECONDS


def version_key(value):
    if not isinstance(value, str) or not re.fullmatch(r"v?\d{4}\.\d{2}\.\d{2}(?:\.\d+)?", value):
        raise UpdateError("Unsupported release version")
    parts = tuple(int(p) for p in value.removeprefix("v").split("."))
    try:
        datetime(*parts[:3])
    except ValueError as exc:
        raise UpdateError("Invalid release date") from exc
    return parts + (0,) * (4-len(parts))


def managed_name(name):
    # Root-level executable code plus explicit documentation/bootstrap files.
    # In particular, no arbitrary JSON, JSONL, reports, logs or directories.
    return isinstance(name, str) and bool(
        re.fullmatch(r"[a-z][a-z0-9_]*\.pyw?", name)
        or name in {"update.bat", "launch.bat", "setup.bat", "setup.ps1", "requirements.txt",
                    "README.md", "CHANGELOG.md", "SECURITY.md", "LICENSE", "jira_config.example.json",
                    "INSTALLATION.md", "PROJECT_STRUCTURE.md"})


def _safe_url(url):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.hostname not in _HOSTS or parsed.username or parsed.password or parsed.port not in (None, 443):
        raise UpdateError("Untrusted update URL")
    return url


class _Redirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return super().redirect_request(req, fp, code, msg, headers, _safe_url(newurl))


def _fetch(url, limit):
    request = urllib.request.Request(_safe_url(url), headers={
        "User-Agent": f"JiraOverlay/{VERSION}", "Accept": "application/vnd.github+json"})
    # Deliberately independent of the Jira Session: no Authorization or cookies.
    with urllib.request.build_opener(_Redirects()).open(request, timeout=20) as response:
        _safe_url(response.geturl())
        if int(response.headers.get("Content-Length", "0")) > limit:
            raise UpdateError("Update download exceeds the size limit")
        data = response.read(limit+1)
    if len(data) > limit:
        raise UpdateError("Update download exceeds the size limit")
    return data


def latest_release(repository=REPOSITORY, current=VERSION):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise UpdateError("Invalid update repository")
    try:
        raw = _fetch(f"https://api.github.com/repos/{repository}/releases/latest", 512*1024)
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise UpdateError(f"GitHub update check failed (HTTP {exc.code}); try again later") from exc
    release = json.loads(raw)
    if not isinstance(release, dict) or release.get("draft") or release.get("prerelease"):
        return None
    tag = release.get("tag_name", "")
    if version_key(tag) <= version_key(current):
        return None
    version = tag.removeprefix("v")
    name = f"JiraOverlay-{version}.zip"
    assets = [a for a in release.get("assets", []) if isinstance(a, dict) and a.get("name") == name]
    if len(assets) != 1:
        raise UpdateError("This release has no complete application package")
    asset = assets[0]
    expected = f"https://github.com/{repository}/releases/download/{tag}/{name}"
    if asset.get("browser_download_url") != expected or not re.fullmatch(r"sha256:[0-9a-f]{64}", str(asset.get("digest", ""))):
        raise UpdateError("GitHub did not provide a verifiable package; use the release page")
    page = f"https://github.com/{repository}/releases/tag/{tag}"
    result = {"version": version, "url": expected, "sha256": asset["digest"][7:],
              "page": page, "repository": repository}
    setup_name = f"JiraOverlay-{version}-Setup.exe"
    installers = [a for a in release.get("assets", []) if isinstance(a, dict) and a.get("name") == setup_name]
    if installers:
        setup_url = f"https://github.com/{repository}/releases/download/{tag}/{setup_name}"
        if len(installers) != 1 or installers[0].get("browser_download_url") != setup_url or not re.fullmatch(
                r"sha256:[0-9a-f]{64}", str(installers[0].get("digest", ""))):
            raise UpdateError("GitHub did not provide a verifiable installer")
        result["installer"] = {"url": setup_url, "sha256": installers[0]["digest"][7:]}
    return result


def native_install(root):
    root = Path(root)
    return (root / "unins000.exe").is_file() and (root / "runtime" / "pythonw.exe").is_file()


def stage_installer(release):
    asset = release.get("installer")
    if not asset:
        raise UpdateError("This release has no complete Windows installer; use the release page")
    data = _fetch(asset["url"], MAX_INSTALLER)
    if hashlib.sha256(data).hexdigest() != asset["sha256"]:
        raise UpdateError("Downloaded installer does not match GitHub's SHA-256 digest")
    folder = Path(tempfile.mkdtemp(prefix="jira-setup-"))
    target = folder / "JiraOverlay-Setup.exe"
    target.write_bytes(data)
    return target


def _manifest(value, expected_version=None, repository=REPOSITORY):
    if not isinstance(value, dict) or value.get("schema") != 1 or value.get("repository") != repository:
        raise UpdateError("Invalid release manifest or repository")
    version_key(value.get("version"))
    if expected_version and value["version"] != expected_version:
        raise UpdateError("Package and advertised version differ")
    files = value.get("files")
    required = {"app_release.py", "app_updates.py", "app_startup.py", "native_update.py", "app_lifecycle.py", "app_guard.pyw", "update_app.py", "jira_overlay.pyw", "triage_learner.pyw",
                "triage_support.py", "triage_evaluation.py", "triage_jira.py", "triage_storage.py",
                "setup.py", "requirements.txt", "setup.bat", "setup.ps1", "launch.bat", "update.bat", "release_tools.py", "source_layout.py"}
    if not isinstance(files, dict) or not required <= files.keys() or len(files) > 100:
        raise UpdateError("Incomplete application package")
    for name, digest in files.items():
        if not managed_name(name) or not isinstance(digest, str) or not re.fullmatch("[0-9a-f]{64}", digest):
            raise UpdateError("Package contains an unsafe filename or checksum")
    return value


def unpack_package(archive_path, payload, expected_version=None, repository=REPOSITORY):
    payload = Path(payload)
    if payload.exists():
        raise UpdateError("Staging folder already exists")
    with zipfile.ZipFile(archive_path) as archive:
        entries = archive.infolist()
        names = [item.filename for item in entries]
        if len(names) != len(set(n.casefold() for n in names)) or len(names) > 101:
            raise UpdateError("Duplicate or excessive package entries")
        if MANIFEST not in names or sum(item.file_size for item in entries) > MAX_UNPACKED:
            raise UpdateError("Missing manifest or oversized package")
        for item in entries:
            if (item.filename != MANIFEST and not managed_name(item.filename)) or item.file_size > 8*1024*1024:
                raise UpdateError("Unsafe or oversized package entry")
            if stat.S_ISLNK(item.external_attr >> 16) or item.flag_bits & 1:
                raise UpdateError("Links and encrypted entries are not supported")
        manifest = _manifest(json.loads(archive.read(MANIFEST)), expected_version, repository)
        if set(names) != set(manifest["files"]) | {MANIFEST}:
            raise UpdateError("Manifest does not match archive contents")
        # Read and authenticate everything before creating staged executable code.
        contents = {name: archive.read(name) for name in manifest["files"]}
        for name, data in contents.items():
            if hashlib.sha256(data).hexdigest() != manifest["files"][name]:
                raise UpdateError("Package checksum mismatch: " + name)
        declarations = {}
        for node in ast.parse(contents["app_release.py"].decode("utf-8")).body:
            if isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant):
                for target in node.targets:
                    if isinstance(target, ast.Name):
                        declarations[target.id] = node.value.value
        if declarations.get("VERSION") != manifest["version"] or declarations.get("REPOSITORY") != repository:
            raise UpdateError("Bundle version declarations do not match its manifest")
        payload.mkdir()
        for name, data in contents.items():
            (payload / name).write_bytes(data)
        (payload / MANIFEST).write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    return payload


def stage_release(release):
    stage = Path(tempfile.mkdtemp(prefix="jira-update-"))
    try:
        data = _fetch(release["url"], MAX_DOWNLOAD)
        if hashlib.sha256(data).hexdigest() != release["sha256"]:
            raise UpdateError("Downloaded package does not match GitHub's SHA-256 digest")
        archive = stage / "release.zip"
        archive.write_bytes(data)
        return unpack_package(archive, stage / "payload", release["version"], release["repository"])
    except Exception:
        shutil.rmtree(stage)
        raise


def verify_payload(payload):
    payload = Path(payload).resolve()
    manifest = _manifest(json.loads((payload / MANIFEST).read_text(encoding="utf-8")))
    if {p.name for p in payload.iterdir()} != set(manifest["files"]) | {MANIFEST}:
        raise UpdateError("Staged files were added or removed")
    for name, digest in manifest["files"].items():
        path = payload / name
        if path.is_symlink() or not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise UpdateError("Staged package was modified: " + name)
    return manifest


def _target(root):
    root = Path(root).resolve()
    if not (root / "jira_overlay.pyw").is_file() or not (root / "triage_learner.pyw").is_file():
        raise UpdateError("Select the existing application folder, not a parent or data folder")
    if (root / ".git").exists():
        raise UpdateError("This is a Git checkout. Close both apps and use git pull --ff-only instead; local edits will not be overwritten.")
    if (root / "app" / "app_release.py").is_file():
        raise UpdateError("This is an organised source tree. Use Git to update it, or migrate its data with the Windows installer into a separate folder.")
    return root


def _inside_file(root, name):
    path = root / name
    if path.is_symlink() or path.resolve().parent != root.resolve() or (path.exists() and not path.is_file()):
        raise UpdateError("Refusing to overwrite a linked or non-file application path")
    return path


def _atomic(path, data):
    fd, temporary = tempfile.mkstemp(prefix=".update-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _storage(root):
    storage = root / ".app-updates"
    if storage.is_symlink() or storage.resolve().parent != root:
        raise UpdateError("Unsafe update backup folder")
    storage.mkdir(exist_ok=True)
    backups = storage / "backups"
    if backups.is_symlink() or backups.resolve().parent != storage:
        raise UpdateError("Unsafe backup path")
    backups.mkdir(exist_ok=True)
    return storage, backups


def restore_backup(root, backup):
    root = _target(root)
    storage, backups = _storage(root)
    backup = Path(backup).resolve()
    if backup.parent != backups or backup.is_symlink():
        raise UpdateError("Backup must be inside this installation's backup folder")
    records = json.loads((backup / "backup.json").read_text(encoding="utf-8"))
    if not isinstance(records, dict) or not records:
        raise UpdateError("Invalid rollback manifest")
    for name, digest in records.items():
        if not managed_name(name):
            raise UpdateError("Unsafe rollback filename")
        _inside_file(root, name)
        if digest is not None:
            source = _inside_file(backup, name)
            if hashlib.sha256(source.read_bytes()).hexdigest() != digest:
                raise UpdateError("Rollback backup checksum mismatch")
    for name, digest in records.items():
        target = _inside_file(root, name)
        if digest is None:
            target.unlink(missing_ok=True)
        else:
            _atomic(target, (backup / name).read_bytes())
    (storage / "active.json").unlink(missing_ok=True)


def recover_pending(root):
    root = Path(root).resolve()
    journal = root / ".app-updates" / "active.json"
    with update_lock(root, timeout=5000):
        if not journal.exists():
            return False
        _target(root)
        record = json.loads(journal.read_text(encoding="utf-8"))
        restore_backup(root, record["backup"])
    return True


def install_payload(root, payload):
    """Call only while both apps are stopped and update_lock is held."""
    root = _target(root)
    manifest = verify_payload(payload)
    storage, backups = _storage(root)
    if (storage / "active.json").exists():
        raise UpdateError("An interrupted update needs rollback before another install")
    names = sorted(manifest["files"], key=lambda name: (name == "app_release.py", name))
    # Validate destinations before creating a backup or replacing any file.
    for name in names:
        _inside_file(root, name)
    backup = Path(tempfile.mkdtemp(prefix=datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S-"), dir=backups))
    records = {}
    for name in names:
        target = root / name
        data = target.read_bytes() if target.exists() else None
        records[name] = hashlib.sha256(data).hexdigest() if data is not None else None
        if data is not None:
            (backup / name).write_bytes(data)
    (backup / "backup.json").write_text(json.dumps(records, indent=2), encoding="utf-8")
    for name, digest in records.items():
        if digest is not None and hashlib.sha256((backup/name).read_bytes()).hexdigest() != digest:
            raise UpdateError("Backup verification failed before installation")
    _atomic(storage / "active.json", json.dumps({"backup": str(backup)}).encode("utf-8"))
    try:
        for name in names:
            _atomic(root / name, (Path(payload) / name).read_bytes())
        (storage / "active.json").unlink()
    except Exception:
        restore_backup(root, backup)
        raise
    return backup


def _event_name(root, suffix):
    digest = hashlib.sha256(str(Path(root).resolve()).casefold().encode()).hexdigest()[:24]
    return "Local\\JiraOverlay-" + digest + "-" + suffix


def _kernel():
    kernel = ctypes.windll.kernel32
    kernel.CreateEventW.argtypes = [ctypes.c_void_p, ctypes.c_bool, ctypes.c_bool, ctypes.c_wchar_p]
    kernel.CreateEventW.restype = ctypes.c_void_p
    kernel.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_bool, ctypes.c_wchar_p]
    kernel.CreateMutexW.restype = ctypes.c_void_p
    kernel.OpenMutexW.argtypes = [ctypes.c_uint32, ctypes.c_bool, ctypes.c_wchar_p]
    kernel.OpenMutexW.restype = ctypes.c_void_p
    for name in ("CloseHandle", "SetEvent", "ReleaseMutex"):
        getattr(kernel, name).argtypes = [ctypes.c_void_p]
    kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
    return kernel


@contextlib.contextmanager
def update_lock(root, timeout=0):
    if os.name != "nt":   # pure filesystem tests; desktop runtime is Windows
        yield
        return
    kernel = _kernel()
    handle = kernel.CreateMutexW(None, False, _event_name(root, "transaction"))
    if not handle:
        raise UpdateError("Could not create the update lock")
    acquired = kernel.WaitForSingleObject(handle, timeout) in (0, 0x80)
    try:
        if not acquired:
            raise UpdateError("An update is already in progress; please wait")
        yield
    finally:
        if acquired:
            kernel.ReleaseMutex(handle)
        kernel.CloseHandle(handle)


def listen_for_update(root, callback):
    kernel = _kernel()
    handle = kernel.CreateEventW(None, True, False, _event_name(root, "stop"))
    if not handle:
        raise UpdateError("Could not create the graceful update event")

    def wait():
        try:
            if kernel.WaitForSingleObject(handle, 0xFFFFFFFF) == 0:
                callback()
        finally:
            kernel.CloseHandle(handle)
    threading.Thread(target=wait, daemon=True).start()


def component_running(name):
    kernel = _kernel()
    handle = kernel.OpenMutexW(0x100000, False, name)
    if handle:
        kernel.CloseHandle(handle)
        return True
    if kernel.GetLastError() != 2:  # ERROR_FILE_NOT_FOUND is the only safe absence
        raise UpdateError("Cannot verify whether an application is running; close both apps and retry")
    return False


def apps_running():
    return any(component_running(name) for name in MUTEXES)


@contextlib.contextmanager
def stop_apps(root, progress=lambda: None, timeout=600):
    """Request graceful shutdown. Never force-kill a writer or replace live code."""
    kernel = _kernel()
    handle = kernel.CreateEventW(None, True, False, _event_name(root, "stop"))
    if not handle:
        raise UpdateError("Could not signal the applications")
    kernel.SetEvent(handle)
    try:
        until = time.monotonic() + timeout
        while apps_running():
            if time.monotonic() >= until:
                raise UpdateError("Apps did not stop. Close both tray icons and try again; no code was replaced.")
            progress()
            time.sleep(0.2)
        yield
    finally:
        kernel.CloseHandle(handle)
