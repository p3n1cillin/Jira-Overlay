"""One version and update source for the complete application bundle."""

VERSION = "2026.10.20.2"
REPOSITORY = "p3n1cillin/Jira-Overlay"

# Explicit publishing allowlist: never archive a working directory wholesale.
RELEASE_FILES = (
    "app_release.py", "app_updates.py", "update_app.py", "update.bat", "release_tools.py",
    "jira_overlay.pyw", "triage_learner.pyw", "triage_eval.py",
    "triage_evaluation.py", "triage_jira.py", "triage_review.py",
    "triage_storage.py", "triage_support.py", "setup.py", "setup.bat",
    "setup.ps1", "launch.bat", "requirements.txt", "jira_config.example.json",
    "README.md", "CHANGELOG.md", "LICENSE", "SECURITY.md", "launch_installed.pyw", "source_layout.py",
    "INSTALLATION.md", "PROJECT_STRUCTURE.md",
)
