"""User-launched supervisor. Never creates a scheduled task or service."""
import argparse
from pathlib import Path
from source_layout import activate
activate(Path(__file__).resolve().parent)
import app_lifecycle


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--register", action="store_true")
    parser.add_argument("--unregister", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    if (root / ".jira-overlay-security-hold").exists():
        raise SystemExit("Jira Overlay is held closed pending IT security approval.")
    if args.register or args.unregister:
        parser.error("Scheduled recovery was retired. Use the reviewed installer for legacy-task cleanup.")
    else:
        app_lifecycle.guardian(root)


if __name__ == "__main__":
    main()
