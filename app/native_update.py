"""Compatibility guard: executable updates require user/IT-managed deployment.

Background checks never execute downloads. No temporary monitor script,
PowerShell child, policy override, silent setup or automatic relaunch exists.
"""
import app_updates as updates


def launch(root, installer, release, *, relaunch=False):
    raise updates.UpdateError(
        "[JIRA-U106] Automatic installer execution was retired. "
        "Download the reviewed installer from the release page and run it "
        "manually under your organisation's application-control policy.")
