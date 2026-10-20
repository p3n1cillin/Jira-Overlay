"""Local recovery archives. Call while the learner's writer is idle."""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
import zipfile
from datetime import date
from pathlib import Path


def daily_backup(state_path, history_path, log_path):
    root = Path(state_path).parent / "triage_backups"
    root.mkdir(exist_ok=True)
    target = root / (date.today().isoformat() + ".zip")
    if target.exists():
        return target
    paths = [
        Path(state_path),
        Path(history_path),
        Path(log_path),
        Path(str(log_path) + ".metrics.json"),
    ]
    manifest = {}
    fd, tmp = tempfile.mkstemp(dir=root, suffix=".tmp")
    os.close(fd)
    try:
        with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as archive:
            for path in paths:
                if path.exists():
                    data = path.read_bytes()
                    archive.writestr(path.name, data)
                    manifest[path.name] = hashlib.sha256(data).hexdigest()
            archive.writestr("manifest.json", json.dumps(manifest, indent=2))
        verify_backup(tmp)
        os.replace(tmp, target)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    return target


def verify_backup(path):
    with zipfile.ZipFile(path) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        for name, digest in manifest.items():
            if Path(name).name != name or "\\" in name:
                raise ValueError("Unsafe archive member")
            if hashlib.sha256(archive.read(name)).hexdigest() != digest:
                raise ValueError("Backup checksum mismatch: " + name)
    return manifest


if __name__ == "__main__":
    import sys

    print(json.dumps(verify_backup(sys.argv[1]), indent=2))
