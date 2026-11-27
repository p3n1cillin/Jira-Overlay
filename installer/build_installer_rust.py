"""Build the per-user Windows installer for the native (Rust) build.

Same Inno Setup script as the Python build, compiled with /DNative. The stage
holds only the release executable, documentation, the C# lifecycle helper and
an ``app_release.py`` version marker: older Python installers read that file's
``VERSION = "..."`` line to refuse downgrading over a newer installation.

    python installer/build_installer_rust.py --compiler <ISCC.exe> [--output dist/native]
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
import sys  # noqa: E402

sys.path.insert(0, str(ROOT / "installer"))
from build_helper import build_helper, sign_if_configured  # noqa: E402

DOCS = ("README.md", "CHANGELOG.md", "SECURITY.md", "LICENSE")


def native_version() -> str:
    text = (ROOT / "rust" / "crates" / "jo-core" / "src" / "lib.rs").read_text(encoding="utf-8")
    match = re.search(r'pub const VERSION: &str = "([0-9.]+)";', text)
    if not match:
        raise RuntimeError("VERSION not found in jo-core")
    return match.group(1)


def build(compiler: Path, output: Path) -> Path:
    version = native_version()
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "jira-overlay"], cwd=ROOT / "rust", check=True)
    exe = ROOT / "rust" / "target" / "release" / "jira-overlay.exe"
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="jira-native-build-") as scratch:
        stage = Path(scratch) / "app"
        (stage / "docs").mkdir(parents=True)
        shutil.copy2(exe, stage / "JiraOverlay.exe")
        sign_if_configured(stage / "JiraOverlay.exe")
        for name in DOCS:
            shutil.copy2(ROOT / name, stage / name)
        for name in ("INSTALLATION.md", "PROJECT_STRUCTURE.md"):
            shutil.copy2(ROOT / "docs" / name, stage / "docs" / name)
        (stage / "app_release.py").write_text(
            f'"""Version marker for installers; the application is JiraOverlay.exe."""\nVERSION = "{version}"\n',
            encoding="utf-8")
        helper = build_helper(output / "JiraOverlay-InstallerHelper.exe")
        shutil.copy2(helper, stage / "installer_helper.exe")
        subprocess.run([str(compiler), "/DNative", "/DAppVersion=" + version, "/DStageDir=" + str(stage),
                        "/DOutputDir=" + str(output), str(ROOT / "installer" / "JiraOverlay.iss")], check=True)
    target = output / f"JiraOverlay-{version}-Setup.exe"
    sign_if_configured(target)
    digest = hashlib.sha256(target.read_bytes()).hexdigest()
    target.with_suffix(".exe.sha256").write_text(f"{digest}  {target.name}\n", encoding="utf-8")
    return target


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compiler", required=True, type=Path)
    parser.add_argument("--output", default=ROOT / "dist" / "native", type=Path)
    print(build(parser.parse_args().compiler, parser.parse_args().output))
