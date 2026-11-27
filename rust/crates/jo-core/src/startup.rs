//! "Run on Windows startup": one `HKCU\...\Run` value, written only with the
//! user's explicit consent and never rewritten when unchanged.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::atomic::canonical;

pub const PRIMARY: &str = "JiraOverlay";
/// Retired independent learner entry, removed when startup is enabled.
pub const LEGACY: &str = "JiraTriageLearner";
pub const REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Python-era launch scripts this installation may own.
const LEGACY_SCRIPTS: [&str; 3] = ["jira_overlay.pyw", "triage_learner.pyw", "launch_installed.pyw"];

static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*"([^"\r\n]+)"(?:\s+"([^"\r\n]+)")?\s*$"#).expect("valid regex"));

/// The values this code reads and writes.
pub trait Registry {
    fn get(&self, name: &str) -> Option<String>;
    fn set(&mut self, name: &str, value: &str) -> io::Result<()>;
    fn delete(&mut self, name: &str) -> io::Result<()>;
}

/// The real `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` key.
pub struct RunKey;

impl Registry for RunKey {
    fn get(&self, name: &str) -> Option<String> {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        hkcu.open_subkey(REGISTRY_PATH).ok()?.get_value(name).ok()
    }
    fn set(&mut self, name: &str, value: &str) -> io::Result<()> {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(REGISTRY_PATH)?;
        key.set_value(name, &value)
    }
    fn delete(&mut self, name: &str) -> io::Result<()> {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey_with_flags(REGISTRY_PATH, winreg::enums::KEY_SET_VALUE) {
            Ok(key) => key,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        match key.delete_value(name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// The launch command for this executable: `"<exe>"`.
pub fn command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Quoted paths in a Run value, resolved to real files (short/long aliases
/// compare equal). One path for a native command, two for Python's
/// `"<interpreter>" "<script>"`.
pub fn command_paths(value: &str) -> Option<Vec<PathBuf>> {
    let caps = QUOTED.captures(value)?;
    caps.iter().skip(1).flatten().map(|m| canonical(Path::new(m.as_str())).ok()).collect()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (canonical(a), canonical(b)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    }
}

/// Is startup enabled for exactly this executable?
pub fn enabled(exe: &Path, registry: &dyn Registry) -> bool {
    registry
        .get(PRIMARY)
        .and_then(|v| command_paths(&v))
        .is_some_and(|paths| paths.len() == 1 && same_file(&paths[0], exe))
}

/// Does a Run value launch something from this installation folder (the
/// native exe, or one of the Python-era scripts)?
pub fn owned(value: &str, root: &Path, exe: &Path) -> bool {
    match command_paths(value).as_deref() {
        Some([single]) => same_file(single, exe),
        Some([_, script]) => LEGACY_SCRIPTS.iter().any(|s| same_file(script, &root.join(s))),
        _ => false,
    }
}

/// Enable or disable startup. Enabling writes the value only if it differs
/// and retires the legacy learner entry; disabling removes only values that
/// belong to this installation.
pub fn set_enabled(root: &Path, exe: &Path, enable: bool, registry: &mut dyn Registry) -> io::Result<()> {
    let current: Vec<(&str, String)> =
        [PRIMARY, LEGACY].into_iter().filter_map(|n| registry.get(n).map(|v| (n, v))).collect();
    if enable {
        let wanted = command(exe);
        if current.iter().find(|(n, _)| *n == PRIMARY).map(|(_, v)| v) != Some(&wanted) {
            registry.set(PRIMARY, &wanted)?;
        }
    }
    for (name, value) in &current {
        if (enable && *name == LEGACY) || (!enable && owned(value, root, exe)) {
            registry.delete(name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    #[derive(Default)]
    struct Fake {
        values: BTreeMap<String, String>,
        writes: u32,
    }

    impl Registry for Fake {
        fn get(&self, name: &str) -> Option<String> {
            self.values.get(name).cloned()
        }
        fn set(&mut self, name: &str, value: &str) -> io::Result<()> {
            self.writes += 1;
            self.values.insert(name.into(), value.into());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> io::Result<()> {
            self.values.remove(name);
            Ok(())
        }
    }

    #[test]
    fn enable_disable_and_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let exe = root.join("JiraOverlay.exe");
        fs::write(&exe, b"").unwrap();
        for s in LEGACY_SCRIPTS {
            fs::write(root.join(s), b"").unwrap();
        }
        let python = root.join("pythonw.exe");
        fs::write(&python, b"").unwrap();
        let mut reg = Fake::default();
        reg.values.insert(
            LEGACY.into(),
            format!("\"{}\" \"{}\"", python.display(), root.join("triage_learner.pyw").display()),
        );
        reg.values.insert("Other".into(), "\"C:\\Elsewhere\\x.exe\"".into());

        assert!(!enabled(&exe, &reg));
        set_enabled(root, &exe, true, &mut reg).unwrap();
        assert!(enabled(&exe, &reg));
        assert!(!reg.values.contains_key(LEGACY), "legacy learner entry retired");
        set_enabled(root, &exe, true, &mut reg).unwrap();
        assert_eq!(reg.writes, 1, "an unchanged command is not rewritten");

        set_enabled(root, &exe, false, &mut reg).unwrap();
        assert!(!reg.values.contains_key(PRIMARY));
        assert!(reg.values.contains_key("Other"), "never touches other apps");
    }

    #[test]
    fn python_era_commands_are_recognised_as_owned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let exe = root.join("JiraOverlay.exe");
        fs::write(&exe, b"").unwrap();
        fs::write(root.join("launch_installed.pyw"), b"").unwrap();
        let python = root.join("pythonw.exe");
        fs::write(&python, b"").unwrap();
        let mut reg = Fake::default();
        reg.values.insert(
            PRIMARY.into(),
            format!("\"{}\" \"{}\"", python.display(), root.join("launch_installed.pyw").display()),
        );
        assert!(!enabled(&exe, &reg));
        set_enabled(root, &exe, false, &mut reg).unwrap();
        assert!(reg.values.is_empty());
        assert!(command_paths("garbage").is_none());
    }
}
