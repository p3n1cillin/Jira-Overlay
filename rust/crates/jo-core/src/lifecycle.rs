//! Local lifecycle records and ordinary user-session recovery.
//!
//! Only component names, PIDs, exit codes and times are logged; never Jira
//! data. No scheduled task, service or script host is created or executed.
//! File formats match the Python build's `.app-lifecycle` folder.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::VERSION;
use crate::atomic::{WriteOptions, canonical, inside_file, to_json_line, write_atomic};
use crate::config;
use crate::ipc;

/// A supervised component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Overlay,
    Learner,
}

impl Role {
    pub const ALL: [Role; 2] = [Role::Overlay, Role::Learner];

    pub fn name(self) -> &'static str {
        match self {
            Role::Overlay => "overlay",
            Role::Learner => "learner",
        }
    }

    pub fn mutex(self) -> &'static str {
        match self {
            Role::Overlay => ipc::OVERLAY_MUTEX,
            Role::Learner => ipc::LEARNER_MUTEX,
        }
    }
}

const RECORD_NAMES: [&str; 4] = ["overlay", "learner", "guardian", "recovery"];
const ROTATE_BYTES: u64 = 1024 * 1024;

pub fn now_epoch() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `<root>\.app-lifecycle`, refusing links and anything that is not a plain
/// directory directly under `root`. Never created unless `create`.
pub fn folder(root: &Path, create: bool) -> io::Result<PathBuf> {
    let path = root.join(".app-lifecycle");
    let unsafe_dir = || io::Error::other("Unsafe lifecycle directory");
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => return Err(unsafe_dir()),
        Ok(_) => {
            if canonical(&path)?.parent() != Some(canonical(root)?.as_path()) {
                return Err(unsafe_dir());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if create {
                fs::create_dir(&path)?;
            }
        }
        Err(e) => return Err(e),
    }
    Ok(path)
}

fn record_path(root: &Path, name: &str, create: bool) -> io::Result<PathBuf> {
    inside_file(&folder(root, create)?, name).map_err(io::Error::other)
}

/// A component's status record; `{}` when missing, oversized or malformed.
pub fn read(root: &Path, record: &str) -> Map<String, Value> {
    assert!(RECORD_NAMES.contains(&record), "Unknown application component");
    let read = || -> io::Result<Map<String, Value>> {
        let path = record_path(root, &format!("{record}.json"), false)?;
        if fs::metadata(&path)?.len() > 16384 {
            return Ok(Map::new());
        }
        match serde_json::from_slice(&fs::read(&path)?) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Ok(Map::new()),
        }
    };
    read().unwrap_or_default()
}

/// Merge `fields` into a record (adds `version` and `updatedAt`).
pub fn write(root: &Path, record: &str, fields: Map<String, Value>) -> io::Result<()> {
    let mut value = read(root, record);
    value.extend(fields);
    value.insert("version".into(), json!(VERSION));
    value.insert("updatedAt".into(), json!(now_epoch()));
    let path = record_path(root, &format!("{record}.json"), true)?;
    let data = to_json_line(&value).map_err(io::Error::other)?;
    write_atomic(&path, data.as_bytes(), WriteOptions { prefix: Some(".update-"), sync: true, ..Default::default() })
}

/// Append one event line, rotating to `.jsonl.1` above 1 MiB.
pub fn event(root: &Path, record: &str, action: &str, fields: Map<String, Value>) -> io::Result<()> {
    let dir = folder(root, true)?;
    let path = inside_file(&dir, &format!("{record}.jsonl")).map_err(io::Error::other)?;
    if fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let backup = inside_file(&dir, &format!("{record}.jsonl.1")).map_err(io::Error::other)?;
        fs::rename(&path, backup)?;
    }
    let mut value = Map::new();
    value.insert("time".into(), json!(now_epoch()));
    value.insert("version".into(), json!(VERSION));
    value.insert("component".into(), json!(record));
    value.insert("event".into(), json!(action));
    value.insert("pid".into(), json!(std::process::id()));
    value.extend(fields);
    let mut line = to_json_line(&value).map_err(io::Error::other)?;
    line.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.flush()
}

/// Shorthand for building record fields.
#[macro_export]
macro_rules! fields {
    ($($key:literal : $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = serde_json::Map::new();
        $( map.insert($key.to_owned(), serde_json::json!($value)); )*
        map
    }};
}

pub fn wanted(root: &Path, record: &str) -> bool {
    read(root, record).get("wanted").is_none_or(|v| v == &Value::Bool(true))
}

pub fn set_wanted(root: &Path, record: &str, enabled: bool) -> io::Result<()> {
    write(root, record, fields! {"wanted": enabled})
}

/// Was this process started by the guardian for this root?
pub fn supervised(root: &Path) -> bool {
    let Ok(value) = std::env::var(SUPERVISED_ENV) else { return false };
    let normal = |p: &Path| canonical(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
    normal(Path::new(&value)) == normal(root)
}

pub const SUPERVISED_ENV: &str = "JIRA_OVERLAY_SUPERVISED_ROOT";

/// How a component's `main` ended.
#[derive(Debug)]
pub struct Failure {
    pub exit_code: i32,
    /// Type name only: exception text may contain request data.
    pub kind: &'static str,
}

/// The running component's lifecycle (Python's module-level `_active`).
pub struct Session {
    root: PathBuf,
    record: &'static str,
    reason: Mutex<String>,
}

impl Session {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Record why the process is ending. `quit` marks a deliberate Quit
    /// (`wanted=false`), which the guardian will not undo.
    pub fn reason(&self, value: &str, quit: bool) {
        *self.reason.lock().unwrap_or_else(|p| p.into_inner()) = value.to_owned();
        let mut f = fields! {"reason": value};
        if quit {
            f.insert("wanted".into(), Value::Bool(false));
        }
        let _ = write(&self.root, self.record, f);
        let _ = event(&self.root, self.record, value, Map::new());
    }

    pub fn ready(&self) {
        let _ =
            write(&self.root, self.record, fields! {"phase": "running", "reason": "running", "exitCode": Value::Null});
    }

    fn current_reason(&self) -> String {
        self.reason.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Run a component's `main` with start/exit records. Panics are recorded as
/// exit code 1 with kind `"panic"` and then resumed.
pub fn run<T>(
    root: &Path,
    record: &'static str,
    main: impl FnOnce(&Session) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let root = canonical(root).unwrap_or_else(|_| root.to_path_buf());
    let session = Session { root: root.clone(), record, reason: Mutex::new("normal-exit".into()) };
    let _ = write(
        &root,
        record,
        fields! {"wanted": true, "phase": "starting", "pid": std::process::id(),
        "startedAt": now_epoch(), "reason": "starting", "exitCode": Value::Null},
    );
    let _ = event(&root, record, "started", Map::new());
    rotate_fault_log(&root, record);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| main(&session)));
    let (result, code, kind) = match outcome {
        Ok(Ok(value)) => (Some(Ok(value)), 0, None),
        Ok(Err(failure)) => {
            let (code, kind) = (failure.exit_code, failure.kind);
            (Some(Err(failure)), code, Some(kind))
        }
        Err(_) => (None, 1, Some("panic")),
    };
    match kind {
        None => {
            let reason = session.current_reason();
            let _ = write(&root, record, fields! {"phase": "exited", "reason": reason, "exitCode": 0});
            let _ = event(&root, record, "exited", fields! {"reason": reason, "exitCode": 0});
        }
        Some(kind) => {
            let _ = write(&root, record, fields! {"phase": "exited", "reason": "exception", "exitCode": code});
            let _ = event(
                &root,
                record,
                "exited",
                fields! {"reason": "exception", "exitCode": code, "exceptionType": kind},
            );
        }
    }
    match result {
        Some(r) => r,
        None => std::panic::resume_unwind(Box::new("component panicked")),
    }
}

/// Rotate `<record>.fault.log` (panic reports) and make sure it exists.
fn rotate_fault_log(root: &Path, record: &str) {
    let Ok(dir) = folder(root, true) else { return };
    let path = dir.join(format!("{record}.fault.log"));
    crate::errlog::rotate(&path, ROTATE_BYTES, "1");
    let _ = OpenOptions::new().create(true).append(true).open(path);
}

/// Path of a component's fault log, for the panic hook.
pub fn fault_log(root: &Path, record: &str) -> PathBuf {
    root.join(".app-lifecycle").join(format!("{record}.fault.log"))
}

/// Read-only recovery status for Settings. Never trusts a saved PID.
pub fn recovery_status(root: &Path, running: impl Fn(&str) -> bool) -> (bool, String) {
    if !running(ipc::GUARD_MUTEX) {
        return (
            false,
            "User-session recovery is not running [JIRA-R101]. Start manually only after IT approval.".into(),
        );
    }
    let (mut paused, mut waiting) = (Vec::new(), Vec::new());
    for role in Role::ALL {
        if !wanted(root, role.name()) {
            paused.push(role.name());
        } else if role == Role::Learner && !config::configured(root) {
            waiting.push("learner awaiting setup".to_owned());
        } else if !running(role.mutex()) {
            waiting.push(format!("{} restarting with backoff", role.name()));
        }
    }
    let mut details = Vec::new();
    if !paused.is_empty() {
        details.push(format!("deliberately closed: {}", paused.join(", ")));
    }
    details.extend(waiting.iter().cloned());
    let mut message = "User-session recovery active (no scheduled task)".to_owned();
    if !details.is_empty() {
        message.push_str("; ");
        message.push_str(&details.join("; "));
    }
    (waiting.is_empty(), message)
}

/// Bounded recovery, including exit-code zero without an explicit Quit.
#[derive(Debug, Default)]
pub struct RestartBudget {
    failures: Vec<f64>,
    pub next_attempt: f64,
}

impl RestartBudget {
    pub fn exited(&mut self, now: f64, lifetime: f64) -> u32 {
        if lifetime >= 300.0 {
            self.failures.clear();
        } else {
            self.failures.retain(|t| now - t < 600.0);
        }
        self.failures.push(now);
        let n = self.failures.len();
        let delay = if n <= 3 { [5, 15, 60][n - 1] } else { 300 };
        self.next_attempt = now + f64::from(delay);
        delay
    }
}

/// A launched child the guardian supervises.
pub trait Child {
    fn poll(&mut self) -> Option<i32>;
    fn pid(&self) -> u32;
}

/// The guardian's environment, injectable for tests.
pub trait Supervisor {
    fn stop_requested(&self) -> bool;
    fn running(&self, role: Role) -> bool;
    fn launch(&mut self, role: Role) -> io::Result<Box<dyn Child>>;
    fn clock(&mut self) -> f64;
    fn wait(&mut self);
}

/// Policy loop. Never force-kills a writer.
pub fn watch(root: &Path, session: Option<&Session>, env: &mut dyn Supervisor) {
    let mut children: Vec<(Role, Box<dyn Child>, f64)> = Vec::new();
    let mut budgets = [RestartBudget::default(), RestartBudget::default()];
    let index = |role: Role| if role == Role::Overlay { 0 } else { 1 };
    if let Some(s) = session {
        s.ready();
    }
    loop {
        let now = env.clock();
        let mut i = 0;
        while i < children.len() {
            if let Some(code) = children[i].1.poll() {
                let (role, child, started) = children.remove(i);
                let stop = env.stop_requested();
                let expected = stop || !wanted(root, role.name());
                let _ = event(
                    root,
                    "guardian",
                    "child-exited",
                    fields! {"child": role.name(), "childPid": child.pid(), "exitCode": code, "expected": expected},
                );
                if wanted(root, role.name()) && !stop {
                    let delay = budgets[index(role)].exited(now, now - started);
                    let _ =
                        event(root, "guardian", "recovery-delayed", fields! {"child": role.name(), "seconds": delay});
                }
            } else {
                i += 1;
            }
        }
        let any_running = Role::ALL.iter().any(|r| env.running(*r));
        if env.stop_requested() {
            if children.is_empty() && !any_running {
                let _ = event(root, "guardian", "stopped-for-update", Map::new());
                if let Some(s) = session {
                    s.reason("update", false);
                }
                return;
            }
        } else {
            let desired: Vec<Role> = Role::ALL.into_iter().filter(|r| wanted(root, r.name())).collect();
            if desired.is_empty() && children.is_empty() && !any_running {
                let _ = event(root, "guardian", "stopped-by-user", Map::new());
                if let Some(s) = session {
                    s.reason("user-quit", true);
                }
                return;
            }
            for role in desired {
                if children.iter().any(|(r, _, _)| *r == role)
                    || env.running(role)
                    || now < budgets[index(role)].next_attempt
                    || (role == Role::Learner && !config::configured(root))
                {
                    continue;
                }
                match env.launch(role) {
                    Ok(child) => {
                        let pid = child.pid();
                        children.push((role, child, now));
                        let _ =
                            event(root, "guardian", "child-started", fields! {"child": role.name(), "childPid": pid});
                    }
                    Err(_) => {
                        let delay = budgets[index(role)].exited(now, 0.0);
                        let _ =
                            event(root, "guardian", "launch-failed", fields! {"child": role.name(), "seconds": delay});
                    }
                }
            }
        }
        env.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn events(root: &Path, record: &str) -> Vec<Value> {
        fs::read_to_string(folder(root, false).unwrap().join(format!("{record}.jsonl")))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn configure(root: &Path) {
        fs::write(root.join("jira_config.json"), r#"{"domain":"example.invalid","email":"fake","token":"fake"}"#)
            .unwrap();
    }

    #[test]
    fn records_quit_update_and_failures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(read(root, "overlay").is_empty());
        assert_eq!(fs::read_dir(root).unwrap().count(), 0, "reading never creates files");

        run(root, "overlay", |s| {
            s.reason("user-quit", true);
            Ok(())
        })
        .unwrap();
        assert!(!wanted(root, "overlay"));
        assert_eq!(read(root, "overlay")["reason"], "user-quit");

        run(root, "overlay", |s| {
            s.reason("update", false);
            Ok(())
        })
        .unwrap();
        assert!(wanted(root, "overlay"), "update shutdown keeps the desired running state");

        let failed = run::<()>(root, "learner", |_| Err(Failure { exit_code: 1, kind: "RuntimeError" }));
        assert!(failed.is_err());
        let log = fs::read_to_string(folder(root, false).unwrap().join("learner.jsonl")).unwrap();
        assert!(log.contains(r#""exceptionType": "RuntimeError""#));
        assert_eq!(read(root, "learner")["exitCode"], 1);
        assert!(fault_log(root, "learner").exists());

        let observed = run(root, "learner", |s| {
            s.ready();
            Ok(read(root, "learner"))
        })
        .unwrap();
        assert_eq!(observed["phase"], "running");
        assert_eq!(observed["exitCode"], Value::Null);
    }

    #[test]
    fn panics_are_recorded_by_type_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let outcome = std::panic::catch_unwind(|| {
            let _ = run::<()>(&root, "overlay", |_| panic!("FAKE_SECRET_SHOULD_NOT_BE_LOGGED"));
        });
        assert!(outcome.is_err());
        let log = fs::read_to_string(root.join(".app-lifecycle/overlay.jsonl")).unwrap();
        assert!(log.contains(r#""exceptionType": "panic""#) && !log.contains("FAKE_SECRET"));
    }

    #[test]
    fn recovery_status_messages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (healthy, msg) = recovery_status(root, |_| false);
        assert!(!healthy && msg.contains("JIRA-R101"));
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);

        write(root, "guardian", fields! {"phase": "running", "pid": 123}).unwrap();
        assert!(recovery_status(root, |_| false).1.contains("not running"));
        configure(root);
        assert_eq!(
            recovery_status(root, |_| true),
            (true, "User-session recovery active (no scheduled task)".to_owned())
        );
        set_wanted(root, "learner", false).unwrap();
        let (healthy, msg) = recovery_status(root, |_| true);
        assert!(healthy && msg.contains("deliberately closed: learner"));
        set_wanted(root, "learner", true).unwrap();
        let (healthy, msg) = recovery_status(root, |n| n != ipc::LEARNER_MUTEX);
        assert!(!healthy && msg.contains("restarting with backoff"));
        fs::remove_file(root.join("jira_config.json")).unwrap();
        assert!(recovery_status(root, |_| true).1.contains("awaiting setup"));
    }

    #[test]
    fn restart_budget() {
        let mut b = RestartBudget::default();
        let delays: Vec<u32> = (0..4).map(|i| b.exited(f64::from(i), 0.0)).collect();
        assert_eq!(delays, [5, 15, 60, 300]);
        assert_eq!(b.exited(1000.0, 301.0), 5);
    }

    struct FakeChild {
        pid: u32,
        code: Rc<RefCell<Option<i32>>>,
    }

    impl Child for FakeChild {
        fn poll(&mut self) -> Option<i32> {
            *self.code.borrow()
        }
        fn pid(&self) -> u32 {
            self.pid
        }
    }

    struct Fake<'a> {
        root: &'a Path,
        scenario: &'static str,
        tick: u32,
        stop: bool,
        children: HashMap<Role, Rc<RefCell<Option<i32>>>>,
        launches: Vec<(Role, u32)>,
    }

    impl Supervisor for Fake<'_> {
        fn stop_requested(&self) -> bool {
            self.stop
        }
        fn running(&self, role: Role) -> bool {
            self.children.get(&role).is_some_and(|c| c.borrow().is_none())
        }
        fn launch(&mut self, role: Role) -> io::Result<Box<dyn Child>> {
            let code = Rc::new(RefCell::new(None));
            self.children.insert(role, code.clone());
            self.launches.push((role, self.tick));
            write(self.root, role.name(), fields! {"phase": "running"}).unwrap();
            Ok(Box::new(FakeChild { pid: 100 + self.launches.len() as u32, code }))
        }
        fn clock(&mut self) -> f64 {
            f64::from(self.tick)
        }
        fn wait(&mut self) {
            self.tick += 1;
            assert!(self.tick < 30, "guardian policy did not terminate");
            match self.scenario {
                "crash" => {
                    if self.tick == 1 {
                        *self.children[&Role::Overlay].borrow_mut() = Some(7);
                    }
                    if self.tick == 8 {
                        for role in Role::ALL {
                            set_wanted(self.root, role.name(), false).unwrap();
                            *self.children[&role].borrow_mut() = Some(0);
                        }
                    }
                }
                "update" => {
                    self.stop = true;
                    if self.tick == 3 {
                        for c in self.children.values() {
                            *c.borrow_mut() = Some(0);
                        }
                    }
                }
                _ => {
                    set_wanted(self.root, "overlay", false).unwrap();
                    *self.children[&Role::Overlay].borrow_mut() = Some(0);
                }
            }
        }
    }

    #[test]
    fn guardian_scenarios() {
        for scenario in ["crash", "update", "quit-learner"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            configure(root);
            if scenario == "quit-learner" {
                set_wanted(root, "learner", false).unwrap();
            }
            let mut fake = Fake { root, scenario, tick: 0, stop: false, children: HashMap::new(), launches: vec![] };
            watch(root, None, &mut fake);
            let ev = events(root, "guardian");
            let roles: Vec<Role> = fake.launches.iter().map(|(r, _)| *r).collect();
            match scenario {
                "crash" => {
                    assert_eq!(roles, [Role::Overlay, Role::Learner, Role::Overlay]);
                    assert!(fake.launches.last().unwrap().1 >= 6);
                    assert!(
                        ev.iter().any(|e| e["event"] == "child-exited" && e["exitCode"] == 7 && e["expected"] == false)
                    );
                }
                "update" => {
                    assert_eq!(fake.tick, 3);
                    assert_eq!(fake.launches.len(), 2);
                    assert_eq!(ev.last().unwrap()["event"], "stopped-for-update");
                }
                _ => assert_eq!(roles, [Role::Overlay]),
            }
        }
    }

    #[test]
    fn unsafe_lifecycle_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".app-lifecycle"), b"not a dir").unwrap();
        assert!(folder(dir.path(), true).is_err());
        assert!(read(dir.path(), "overlay").is_empty());
    }
}
