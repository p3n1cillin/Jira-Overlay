//! Named kernel objects shared by every Jira Overlay component and the
//! installer helper. Names are cross-version contracts; do not change them.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenMutexW, ReleaseMutex, ResetEvent,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::core::HSTRING;

pub const OVERLAY_MUTEX: &str = "JiraOverlaySingleInstance";
pub const LEARNER_MUTEX: &str = "JiraTriageLearner";
pub const GUARD_MUTEX: &str = "JiraOverlayGuardian";
pub const MUTEXES: [&str; 3] = [OVERLAY_MUTEX, LEARNER_MUTEX, GUARD_MUTEX];
pub const SHOW_EVENT: &str = "JiraOverlayShowRequest";

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("{0}")]
    Windows(#[from] windows::core::Error),
    #[error("Cannot verify whether an application is running; close both apps and retry")]
    Unverifiable,
    #[error("An update is already in progress; please wait")]
    Busy,
    #[error("Apps did not stop. Quit Jira Overlay and try again; no code was replaced.")]
    StillRunning,
}

/// An owned kernel handle, closed on drop.
#[derive(Debug)]
pub struct Handle(HANDLE);

// SAFETY: kernel handles are process-wide values usable from any thread.
unsafe impl Send for Handle {}
// SAFETY: as above; the wrapped operations are thread-safe kernel calls.
unsafe impl Sync for Handle {}

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Keep the handle open until the process ends (the guardian holds its
    /// mutex so setup cannot replace files while shutdown is in progress).
    pub fn leak(self) {
        std::mem::forget(self);
    }

    pub fn set(&self) -> Result<(), IpcError> {
        // SAFETY: valid event handle owned by self.
        unsafe { SetEvent(self.0)? };
        Ok(())
    }

    pub fn reset(&self) -> Result<(), IpcError> {
        // SAFETY: valid event handle owned by self.
        unsafe { ResetEvent(self.0)? };
        Ok(())
    }

    /// Wait for the object; `true` when signalled (or an abandoned mutex).
    pub fn wait(&self, timeout: Option<Duration>) -> bool {
        let ms = timeout.map_or(INFINITE, |d| d.as_millis().min(u128::from(INFINITE - 1)) as u32);
        // SAFETY: valid handle owned by self.
        let result = unsafe { WaitForSingleObject(self.0, ms) };
        result == WAIT_OBJECT_0 || result == WAIT_ABANDONED
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `SHA256(lowercase(full path without trailing '\'))[:24]`, as the installer
/// helper computes it, so setup can signal a running Rust build.
pub fn root_digest(root: &Path) -> String {
    let full = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let text = full.to_string_lossy();
    let text = text.trim_end_matches('\\').to_lowercase();
    hex::encode(Sha256::digest(text.as_bytes()))[..24].to_owned()
}

/// `Local\JiraOverlay-<h24>-<suffix>`.
pub fn root_object_name(root: &Path, suffix: &str) -> String {
    format!("Local\\JiraOverlay-{}-{suffix}", root_digest(root))
}

/// Create a named mutex (not initially owned). `Ok(None)` when another
/// process already holds the name: the caller is a duplicate and must exit.
pub fn single_instance(name: &str) -> Result<Option<Handle>, IpcError> {
    // SAFETY: plain Win32 call with an owned wide string.
    let handle = unsafe { CreateMutexW(None, false, &HSTRING::from(name))? };
    // SAFETY: read immediately after the call that set it.
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let handle = Handle(handle);
    Ok(if already { None } else { Some(handle) })
}

/// Whether a component's mutex exists. Only "not found" counts as absent;
/// anything else fails closed.
pub fn component_running(name: &str) -> Result<bool, IpcError> {
    // SAFETY: plain Win32 call with an owned wide string.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &HSTRING::from(name)) } {
        Ok(handle) => {
            drop(Handle(handle));
            Ok(true)
        }
        Err(err) if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(false),
        Err(_) => Err(IpcError::Unverifiable),
    }
}

pub fn apps_running() -> Result<bool, IpcError> {
    for name in MUTEXES {
        if component_running(name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Create (or open) a manual-reset, initially unsignalled named event.
pub fn event(name: &str) -> Result<Handle, IpcError> {
    // SAFETY: plain Win32 call with an owned wide string.
    Ok(Handle(unsafe { CreateEventW(None, true, false, &HSTRING::from(name))? }))
}

/// Signal an existing event without creating it. `Ok(false)` if absent.
pub fn signal_existing(name: &str) -> Result<bool, IpcError> {
    // SAFETY: plain Win32 call with an owned wide string.
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(name)) } {
        Ok(handle) => {
            let handle = Handle(handle);
            handle.set()?;
            Ok(true)
        }
        Err(err) if err.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Run `callback` on a background thread when setup/the updater asks this
/// installation's apps to stop.
pub fn listen_for_stop(root: &Path, callback: impl FnOnce() + Send + 'static) -> Result<(), IpcError> {
    let handle = event(&root_object_name(root, "stop"))?;
    thread::Builder::new()
        .name("stop-listener".into())
        .spawn(move || {
            if handle.wait(None) {
                callback();
            }
        })
        .map_err(|e| IpcError::Windows(windows::core::Error::from(e)))?;
    Ok(())
}

/// Ask a running overlay to show itself (second launch / launcher).
pub fn request_show(create: bool) -> Result<bool, IpcError> {
    if create {
        event(SHOW_EVENT)?.set()?;
        Ok(true)
    } else {
        signal_existing(SHOW_EVENT)
    }
}

/// Per-root update transaction lock. Abandoned counts as acquired.
pub struct UpdateLock(Handle);

impl UpdateLock {
    pub fn acquire(root: &Path, timeout: Duration) -> Result<Self, IpcError> {
        // SAFETY: plain Win32 call with an owned wide string.
        let handle =
            Handle(unsafe { CreateMutexW(None, false, &HSTRING::from(root_object_name(root, "transaction")))? });
        if handle.wait(Some(timeout)) { Ok(Self(handle)) } else { Err(IpcError::Busy) }
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        // SAFETY: we acquired the mutex in `acquire`.
        unsafe {
            let _ = ReleaseMutex(self.0.raw());
        }
    }
}

/// Request graceful shutdown of this root's apps and wait until every
/// component mutex is gone. Never force-kills.
pub fn stop_apps(root: &Path, timeout: Duration, mut progress: impl FnMut()) -> Result<Handle, IpcError> {
    let handle = event(&root_object_name(root, "stop"))?;
    handle.set()?;
    let until = Instant::now() + timeout;
    while apps_running()? {
        if Instant::now() >= until {
            return Err(IpcError::StillRunning);
        }
        progress();
        thread::sleep(Duration::from_millis(200));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(prefix: &str) -> String {
        format!("{prefix}-test-{}-{:?}", std::process::id(), Instant::now())
    }

    #[test]
    fn digest_matches_installer_helper_form() {
        // SHA256("c:\users\x\appdata\local\programs\jiraoverlay")[:24]
        let a = root_digest(Path::new(r"C:\Users\X\AppData\Local\Programs\JiraOverlay\"));
        let b = root_digest(Path::new(r"c:\users\x\appdata\local\programs\jiraoverlay"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 24);
        let expected = hex::encode(Sha256::digest(br"c:\users\x\appdata\local\programs\jiraoverlay"));
        assert_eq!(a, expected[..24]);
    }

    #[test]
    fn duplicate_instance_is_detected_and_probe_sees_it() {
        let name = unique("JoMutex");
        assert!(!component_running(&name).unwrap());
        let first = single_instance(&name).unwrap().expect("first owner");
        assert!(single_instance(&name).unwrap().is_none());
        assert!(component_running(&name).unwrap());
        drop(first);
        assert!(!component_running(&name).unwrap());
    }

    #[test]
    fn show_event_round_trip() {
        let name = unique("JoShow");
        assert!(!signal_existing(&name).unwrap());
        let ev = event(&name).unwrap();
        assert!(!ev.wait(Some(Duration::ZERO)));
        assert!(signal_existing(&name).unwrap());
        assert!(ev.wait(Some(Duration::ZERO)));
        ev.reset().unwrap();
        assert!(!ev.wait(Some(Duration::ZERO)));
    }

    #[test]
    fn stop_listener_fires_callback() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        listen_for_stop(dir.path(), move || tx.send(()).unwrap()).unwrap();
        event(&root_object_name(dir.path(), "stop")).unwrap().set().unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("callback ran");
    }

    #[test]
    fn update_lock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let _held = UpdateLock::acquire(dir.path(), Duration::ZERO).unwrap();
        let path = dir.path().to_path_buf();
        let other = thread::spawn(move || UpdateLock::acquire(&path, Duration::from_millis(50)).is_err());
        assert!(other.join().unwrap());
    }
}
