"""One Windows startup command; the overlay starts its learner after setup."""
from pathlib import Path
import re

PRIMARY = "JiraOverlay"
LEGACY = "JiraTriageLearner"
REGISTRY_PATH = r"Software\Microsoft\Windows\CurrentVersion\Run"


def commands(root, interpreter):
    script = "launch_installed.pyw" if (Path(root) / "app_guard.pyw").is_file() and (Path(root) / "runtime/pythonw.exe").is_file() else "jira_overlay.pyw"
    return {PRIMARY: f'"{interpreter}" "{Path(root) / script}"'}


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
    actual = command_paths(value)
    return actual is not None and actual[0] == Path(interpreter).resolve() and actual[1] in {
        (Path(root) / "jira_overlay.pyw").resolve(), (Path(root) / "launch_installed.pyw").resolve()}


def command_paths(value):
    """Compare real files, including Windows short/long path aliases."""
    if not isinstance(value, str):
        return None
    match = re.fullmatch(r'\s*"([^"\r\n]+)"\s+"([^"\r\n]+)"\s*', value)
    if not match:
        return None
    try:
        return tuple(Path(part).resolve() for part in match.groups())
    except (OSError, ValueError):
        return None


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
            paths = command_paths(value)
            owned = paths is not None and any(
                (Path(root) / script).resolve() == paths[1]
                for script in ("jira_overlay.pyw", "triage_learner.pyw", "launch_installed.pyw"))
            if (enable and name == LEGACY) or (not enable and owned):
                try:
                    registry.DeleteValue(key, name)
                except FileNotFoundError:
                    pass
