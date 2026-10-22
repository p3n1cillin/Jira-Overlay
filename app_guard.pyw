"""Managed per-user guardian; no Jira access or credentials in scheduled tasks."""
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
    if args.register or args.unregister:
        app_lifecycle.configure_task(root, "remove" if args.unregister else "register")
    else:
        app_lifecycle.guardian(root)


if __name__ == "__main__":
    main()
