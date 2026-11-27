//! `JiraOverlay.exe`: one native executable with four roles.
//!
//! * no role       launcher (Start menu / opt-in startup entry)
//! * `--guardian`  user-session recovery supervisor
//! * `--overlay`   the desktop overlay
//! * `--learner`   the triage learner
//! * `--triage-eval ...` / `--triage-review ...` offline tools (console)
//!
//! `--root <dir>` runs against another installation/data folder (testing);
//! `--resume` launches without resetting deliberate Quit choices.

#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use jo_core::guardian::{self, Launch};
use jo_core::lifecycle::{self, Failure, Session};
use jo_core::{ipc, security_hold};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::HSTRING;

#[derive(Debug, PartialEq, Eq)]
enum Role {
    Launcher { resume: bool },
    Guardian,
    Overlay,
    Learner,
    Eval(Vec<String>),
    Review(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    role: Role,
    root: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut role = None;
    let mut root = None;
    let mut resume = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                i += 1;
                root = Some(PathBuf::from(args.get(i).ok_or("--root needs a folder")?));
            }
            "--resume" => resume = true,
            "--guardian" => role = Some(Role::Guardian),
            "--overlay" => role = Some(Role::Overlay),
            "--learner" => role = Some(Role::Learner),
            flag @ ("--triage-eval" | "--triage-review") => {
                let rest = args[i + 1..].to_vec();
                role = Some(if flag == "--triage-eval" { Role::Eval(rest) } else { Role::Review(rest) });
                break;
            }
            // The retired scheduled-recovery switches stay refused.
            "--register" | "--unregister" => return Err("Scheduled recovery was retired.".into()),
            other => return Err(format!("Unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(Args { role: role.unwrap_or(Role::Launcher { resume }), root })
}

fn message(title: &str, text: &str) {
    // SAFETY: plain Win32 call with owned wide strings and no owner window.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR);
    }
}

/// Record panics (type and location only) in the component's fault log.
fn install_panic_hook(root: PathBuf, record: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        jo_core::errlog::append_block(&lifecycle::fault_log(&root, record), &format!("panic at {location}"), 1 << 20);
        previous(info);
    }));
}

fn failure(kind: &'static str) -> Failure {
    Failure { exit_code: 1, kind }
}

fn run_overlay(root: &Path, exe: &Path, root_override: bool) -> Result<(), Failure> {
    install_panic_hook(root.to_path_buf(), "overlay");
    lifecycle::run(root, "overlay", |session: &Session| {
        if security_hold(root) {
            session.reason("security-hold", true);
            return Ok(());
        }
        let Some(instance) = ipc::single_instance(ipc::OVERLAY_MUTEX).map_err(|_| failure("IpcError"))? else {
            // Already running: launching again means "bring it up".
            let _ = ipc::request_show(false);
            return Ok(());
        };
        match jo_overlay::run(root, session) {
            Ok(jo_overlay::Exit::Quit) => Ok(()),
            Ok(jo_overlay::Exit::Restart) => {
                drop(instance);
                guardian::command(exe, "--overlay", root, root_override)
                    .spawn()
                    .map(|_| ())
                    .map_err(|_| failure("RestartFailed"))
            }
            Err(text) => {
                jo_core::errlog::append_block(&root.join("jira_overlay_error.log"), &text, 5 << 20);
                message("Jira Overlay — cannot start", &text);
                Err(failure("StartupError"))
            }
        }
    })
}

fn run_learner(root: &Path) -> Result<(), Failure> {
    install_panic_hook(root.to_path_buf(), "learner");
    lifecycle::run(root, "learner", |session: &Session| {
        if security_hold(root) {
            session.reason("security-hold", true);
            return Ok(());
        }
        let Some(_instance) = ipc::single_instance(ipc::LEARNER_MUTEX).map_err(|_| failure("IpcError"))? else {
            return Ok(());
        };
        if let Err(text) = triage_learner::run(root, session) {
            jo_core::errlog::append_line(&root.join("triage_learner.log"), &text);
            message("Triage Learner — cannot start", &text);
        }
        Ok(())
    })
}

fn code(result: Result<(), Failure>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => ExitCode::from(u8::try_from(f.exit_code).unwrap_or(1)),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&argv) {
        Ok(a) => a,
        Err(text) => {
            message("Jira Overlay", &text);
            return ExitCode::from(2);
        }
    };
    let Ok(exe) = std::env::current_exe() else { return ExitCode::from(1) };
    let root_override = args.root.is_some();
    let root = args.root.unwrap_or_else(|| exe.parent().map(Path::to_path_buf).unwrap_or_default());
    let root = jo_core::atomic::canonical(&root).unwrap_or(root);

    match args.role {
        Role::Launcher { resume } => match guardian::launch(&root, &exe, resume, root_override) {
            Ok(Launch::Held) => ExitCode::from(1),
            Ok(Launch::Supervised) => ExitCode::SUCCESS,
            Ok(Launch::RunOverlay) => code(run_overlay(&root, &exe, root_override)),
            Err(e) => {
                message("Jira Overlay", &format!("Could not start Jira Overlay: {e}"));
                ExitCode::from(1)
            }
        },
        Role::Guardian => {
            if security_hold(&root) {
                return ExitCode::from(1);
            }
            install_panic_hook(root.clone(), "guardian");
            code(guardian::run_guardian(&root, &exe, root_override))
        }
        Role::Overlay => code(run_overlay(&root, &exe, root_override)),
        Role::Learner => code(run_learner(&root)),
        Role::Eval(rest) => {
            attach_console();
            ExitCode::from(u8::try_from(triage_learner::cli::eval(&root, &rest)).unwrap_or(1))
        }
        Role::Review(rest) => {
            attach_console();
            ExitCode::from(u8::try_from(triage_learner::cli::review(&root, &rest)).unwrap_or(1))
        }
    }
}

/// Offline tools print to the terminal they were started from.
fn attach_console() {
    // SAFETY: attaching to the parent console has no preconditions.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn roles_and_flags() {
        assert_eq!(args(&[]).unwrap(), Args { role: Role::Launcher { resume: false }, root: None });
        assert_eq!(args(&["--resume"]).unwrap().role, Role::Launcher { resume: true });
        assert_eq!(args(&["--guardian"]).unwrap().role, Role::Guardian);
        let a = args(&["--learner", "--root", r"C:\copy"]).unwrap();
        assert_eq!((a.role, a.root), (Role::Learner, Some(PathBuf::from(r"C:\copy"))));
        assert_eq!(
            args(&["--triage-eval", "build", "--x"]).unwrap().role,
            Role::Eval(vec!["build".into(), "--x".into()])
        );
        assert!(args(&["--register"]).is_err(), "scheduled recovery stays retired");
        assert!(args(&["--root"]).is_err());
        assert!(args(&["--bogus"]).is_err());
    }
}
