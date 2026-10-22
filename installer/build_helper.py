"""Compile the transparent lifecycle helper at build time, never on a client."""
import os
from pathlib import Path
import subprocess
import re


def build_helper(output):
    output = Path(output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    compiler = Path(os.environ["SystemRoot"]) / "Microsoft.NET/Framework64/v4.0.30319/csc.exe"
    subprocess.run([str(compiler), "/nologo", "/target:exe", "/platform:x64", "/optimize+",
        "/reference:Microsoft.CSharp.dll", "/out:" + str(output),
        str(Path(__file__).with_name("lifecycle_helper.cs"))], check=True)
    sign_if_configured(output)
    return output


def sign_if_configured(path):
    """Build-only signing through an explicitly configured certificate store.

    No certificates are generated, purchased, trusted or installed here.
    CI builds may be unsigned for isolated tests, but cannot pass release gates.
    """
    thumbprint = os.environ.get("JIRA_SIGNING_THUMBPRINT", "")
    if not thumbprint:
        return
    tool = Path(os.environ.get("JIRA_SIGNTOOL", ""))
    timestamp = os.environ.get("JIRA_TIMESTAMP_URL", "")
    if not re.fullmatch(r"[a-fA-F0-9]{40}", thumbprint) or not tool.is_absolute() or not tool.is_file() or tool.name.lower() != "signtool.exe" or not timestamp.startswith("https://"):
        raise RuntimeError("Configure the approved certificate thumbprint, absolute SignTool path and HTTPS timestamp service before signing")
    subprocess.run([str(tool), "sign", "/sha1", thumbprint, "/fd", "SHA256", "/tr", timestamp, "/td", "SHA256", str(path)], check=True)
