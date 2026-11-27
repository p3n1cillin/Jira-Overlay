//! The launcher and the user-session recovery guardian (Python
//! `launch_installed.pyw` and `app_lifecycle.guardian`).
//!
//! No scheduled task, service or script host is involved: the guardian is an
//! ordinary process started from the Start menu (or the user's opt-in Run
//! entry) that restarts crashed components with bounded backoff while it is
//! alive. It never force-kills anything.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child as Process, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::ipc;
use crate::lifecycle::{self, Child, Role, SUPERVISED_ENV, Session, Supervisor};
use crate::stop::Stop;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Command-line flag selecting each role of the single executable.
pub fn role_flag(role: Role) -> &'static str {
    match role {
        Role::Overlay => "--overlay",
        Role::Learner => "--learner",
    }
}

/// `exe <flag> [--root <root>]` with no console window.
pub fn command(exe: &Path, flag: &str, root: &Path, root_override: bool) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg(flag);
    if root_override {
        cmd.arg("--root").arg(root);
    }
    cmd.current_dir(root).creation_flags(CREATE_NO_WINDOW);
    cmd
}

struct Launched(Process);

impl Child for Launched {
    fn poll(&mut self) -> Option<i32> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(1)),
            Ok(None) => None,
            Err(_) => Some(1),
        }
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

/// The real environment: component mutexes, child processes, wall clock.
pub struct Processes {
    pub root: PathBuf,
    pub exe: PathBuf,
    pub root_override: bool,
    pub stop: Stop,
    started: Instant,
}

impl Processes {
    pub fn new(root: &Path, exe: &Path, root_override: bool, stop: Stop) -> Self {
        Self { root: root.to_path_buf(), exe: exe.to_path_buf(), root_override, stop, started: Instant::now() }
    }

    fn stderr_log(&self, role: Role) -> io::Result<fs::File> {
        let dir = lifecycle::folder(&self.root, true)?;
        let path = dir.join(format!("{}.stderr.log", role.name()));
        crate::errlog::rotate(&path, 1024 * 1024, "1");
        OpenOptions::new().create(true).append(true).open(path)
    }
}

impl Supervisor for Processes {
    fn stop_requested(&self) -> bool {
        self.stop.is_set()
    }

    fn running(&self, role: Role) -> bool {
        // Unverifiable counts as running: never start a duplicate writer.
        ipc::component_running(role.mutex()).unwrap_or(true)
    }

    fn launch(&mut self, role: Role) -> io::Result<Box<dyn Child>> {
        let log = self.stderr_log(role)?;
        let err = log.try_clone()?;
        let child = command(&self.exe, role_flag(role), &self.root, self.root_override)
            .env(SUPERVISED_ENV, &self.root)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .spawn()?;
        Ok(Box::new(Launched(child)))
    }

    fn clock(&mut self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn wait(&mut self) {
        thread::sleep(Duration::from_secs(1));
    }
}

/// Run the guardian. Returns without touching any record when another
/// guardian is already running.
pub fn run_guardian(root: &Path, exe: &Path, root_override: bool) -> Result<(), lifecycle::Failure> {
    let guard = match ipc::single_instance(ipc::GUARD_MUTEX) {
        Ok(Some(handle)) => handle,
        Ok(None) => return Ok(()),
        Err(_) => return Err(lifecycle::Failure { exit_code: 1, kind: "IpcError" }),
    };
    let stop = Stop::new();
    let signal = stop.clone();
    let _ = ipc::listen_for_stop(root, move || signal.set());
    let result = lifecycle::run(root, "guardian", |session: &Session| {
        let mut env = Processes::new(session.root(), exe, root_override, stop.clone());
        lifecycle::watch(session.root(), Some(session), &mut env);
        Ok(())
    });
    // Hold the mutex until Windows ends the process, so setup cannot replace
    // files while shutdown is still running.
    guard.leak();
    result
}

/// What the launcher did.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Held closed pending IT approval.
    Held,
    /// Native install: guardian ensured, overlay asked to show itself.
    Supervised,
    /// Portable/dev folder: the caller should run the overlay itself.
    RunOverlay,
}

/// The default action of `JiraOverlay.exe` (Start menu, startup entry).
pub fn launch(root: &Path, exe: &Path, resume: bool, root_override: bool) -> io::Result<Launch> {
    if crate::security_hold(root) {
        return Ok(Launch::Held);
    }
    if !crate::updates::native_install(root) {
        return Ok(Launch::RunOverlay);
    }
    if !resume {
        // A manual Start deliberately resumes both companions.
        for role in Role::ALL {
            lifecycle::set_wanted(root, role.name(), true)?;
        }
    }
    if !ipc::component_running(ipc::GUARD_MUTEX).unwrap_or(true) {
        let mut cmd = Command::new(exe);
        cmd.arg("--guardian");
        if root_override {
            cmd.arg("--root").arg(root);
        }
        cmd.current_dir(root).spawn()?;
    }
    let _ = ipc::request_show(true);
    Ok(Launch::Supervised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_respects_hold_and_portable_folders() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("JiraOverlay.exe");
        assert_eq!(launch(dir.path(), &exe, false, false).unwrap(), Launch::RunOverlay);
        fs::write(dir.path().join(crate::SECURITY_HOLD), b"held").unwrap();
        assert_eq!(launch(dir.path(), &exe, false, false).unwrap(), Launch::Held);
        assert!(!dir.path().join(".app-lifecycle").exists(), "a held launch writes nothing");
    }

    #[test]
    fn supervised_children_get_the_root_and_their_role() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = command(Path::new("JiraOverlay.exe"), role_flag(Role::Learner), dir.path(), true);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[0], "--learner");
        assert_eq!(args[1], "--root");
        assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    }
}
