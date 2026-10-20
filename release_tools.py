"""Build a deterministic, code-only public release. Never ZIP the workspace."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import tempfile
import zipfile

from app_release import VERSION, REPOSITORY, RELEASE_FILES
from app_updates import MANIFEST, unpack_package


def build_release(root, output):
    root, output = Path(root).resolve(), Path(output)
    for name in RELEASE_FILES:
        path = root/name
        if path.is_symlink() or path.resolve().parent != root:
            raise ValueError("Refusing to publish linked files")
    # All allowlisted assets are text; normalise checkout line endings so a
    # Windows build and the Linux publisher produce identical file contents.
    contents = {name: (root/name).read_bytes().replace(b"\r\n", b"\n") for name in RELEASE_FILES}
    manifest = {"schema": 1, "version": VERSION, "repository": REPOSITORY,
                "files": {name: hashlib.sha256(data).hexdigest() for name, data in contents.items()}}
    contents[MANIFEST] = json.dumps(manifest, indent=2, sort_keys=True).encode("utf-8")
    output.mkdir(parents=True, exist_ok=True)
    target = output / f"JiraOverlay-{VERSION}.zip"
    with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(contents.items()):
            entry = zipfile.ZipInfo(name, (2020, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = 0o100644 << 16
            archive.writestr(entry, data)
    # Use exactly the same verifier as the updater, not a weaker build-only check.
    with tempfile.TemporaryDirectory() as folder:
        unpack_package(target, Path(folder)/"payload", VERSION, REPOSITORY)
    digest = hashlib.sha256(target.read_bytes()).hexdigest()
    (output / (target.name + ".sha256")).write_text(f"{digest}  {target.name}\n", encoding="utf-8")
    changelog = (root/"CHANGELOG.md").read_text(encoding="utf-8")
    marker = f"## [{VERSION}]"
    if marker not in changelog:
        raise ValueError("Add this release's CHANGELOG section before publishing")
    notes = changelog.split(marker, 1)[1].split("\n## [", 1)[0].strip()
    (output / "RELEASE_NOTES.md").write_text(notes + "\n", encoding="utf-8")
    return target


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", default="dist")
    parser.add_argument("--check-tag")
    args = parser.parse_args()
    if args.check_tag and args.check_tag != "v" + VERSION:
        parser.error("Tag must exactly match app_release.VERSION")
    print(build_release(Path(__file__).resolve().parent, args.output))
