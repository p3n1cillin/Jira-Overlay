"""One Windows startup command; the overlay starts its learner after setup."""
from pathlib import Path

PRIMARY = "JiraOverlay"
LEGACY = "JiraTriageLearner"
REGISTRY_PATH = r"Software\Microsoft\Windows\CurrentVersion\Run"


def commands(root, interpreter):
    return {PRIMARY: f'"{interpreter}" "{Path(root) / "jira_overlay.pyw"}"'}


def entries(registry, path=REGISTRY_PATH):
    result = {}
    try:
        with registry.OpenKey(registry.HKEY_CURRENT_USER, path, 0, registry.KEY_READ) as key:
            for name in (PRIMARY, LEGACY):
                try:
                    result[name] = registry.QueryValueEx(key, name)[0]
                except FileNotFoundError:
                    pass
    except FileNotFoundError:
        pass
    return result


def enabled(root, interpreter, registry, path=REGISTRY_PATH):
    value = entries(registry, path).get(PRIMARY)
    return isinstance(value, str) and value.casefold() == commands(root, interpreter)[PRIMARY].casefold()


def set_enabled(root, interpreter, enable, registry, path=REGISTRY_PATH):
    if enable:
        key = registry.CreateKeyEx(registry.HKEY_CURRENT_USER, path, 0, registry.KEY_SET_VALUE)
    else:
        try:
            key = registry.OpenKey(registry.HKEY_CURRENT_USER, path, 0, registry.KEY_SET_VALUE | registry.KEY_READ)
        except FileNotFoundError:
            return
    current = entries(registry, path)
    with key:
        if enable:
            registry.SetValueEx(key, PRIMARY, 0, registry.REG_SZ, commands(root, interpreter)[PRIMARY])
        # Promoting an active installation retires the old, independently
        # launched learner. Disabling only removes commands belonging here.
        for name, value in current.items():
            owned = isinstance(value, str) and any(
                f'"{Path(root) / script}"'.casefold() in value.casefold()
                for script in ("jira_overlay.pyw", "triage_learner.pyw", "launch_installed.pyw"))
            if (enable and name == LEGACY) or (not enable and owned):
                try:
                    registry.DeleteValue(key, name)
                except FileNotFoundError:
                    pass
