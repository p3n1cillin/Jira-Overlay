//! Everything the overlay asks of Windows, behind a trait so the UI can be
//! exercised without opening browsers, dialogs or toasts.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetCursorPos, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, IDYES, LWA_ALPHA, MB_ICONERROR,
    MB_ICONEXCLAMATION, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    MESSAGEBOX_STYLE, MessageBoxW, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_NOACTIVATE,
    SWP_NOZORDER, SetForegroundWindow, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    WS_EX_LAYERED,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::toast;

/// A monitor's work area (excludes the taskbar), in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub trait Platform: Send + Sync {
    /// `webbrowser.open`: true when Windows accepted the URL.
    fn open_url(&self, url: &str) -> bool;
    /// `os.startfile` on a folder.
    fn open_folder(&self, path: &Path) -> io::Result<()>;
    /// Open a file with its associated app, falling back to Notepad.
    fn open_document(&self, path: &Path);
    fn toast(&self, title: &str, body: &str);
    /// `MessageBeep(MB_ICONEXCLAMATION)`.
    fn beep(&self);
    fn info(&self, title: &str, text: &str);
    fn error(&self, title: &str, text: &str);
    fn ask_yes_no(&self, title: &str, text: &str) -> bool;
    fn startup_enabled(&self) -> bool;
    /// False when Windows refused the change.
    fn set_startup(&self, enable: bool) -> bool;
    /// Work area of the monitor nearest to `(x, y)`.
    fn work_area(&self, x: i32, y: i32) -> Option<WorkArea>;
    /// Primary screen size in pixels.
    fn screen_size(&self) -> (i32, i32);
}

/// The real thing.
pub struct Windows {
    root: PathBuf,
    /// Owner window for message boxes (0 until the overlay exists).
    owner: AtomicIsize,
    toaster: toast::Toaster,
}

impl Windows {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf(), owner: AtomicIsize::new(0), toaster: toast::Toaster::detect(root) }
    }

    pub fn set_owner(&self, hwnd: isize) {
        self.owner.store(hwnd, Ordering::SeqCst);
    }

    fn message(&self, title: &str, text: &str, style: MESSAGEBOX_STYLE) -> i32 {
        let owner = self.owner.load(Ordering::SeqCst);
        let hwnd = (owner != 0).then_some(HWND(owner as *mut _));
        // SAFETY: plain Win32 call with owned wide strings.
        unsafe {
            MessageBoxW(hwnd, &HSTRING::from(text), &HSTRING::from(title), style | MB_TOPMOST | MB_SETFOREGROUND).0
        }
    }
}

fn shell_open(target: &HSTRING) -> bool {
    // SAFETY: plain Win32 call with owned wide strings.
    let result = unsafe { ShellExecuteW(None, w!("open"), target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    result.0 as isize > 32
}

impl Platform for Windows {
    fn open_url(&self, url: &str) -> bool {
        shell_open(&HSTRING::from(url))
    }

    fn open_folder(&self, path: &Path) -> io::Result<()> {
        if shell_open(&HSTRING::from(path.as_os_str())) {
            Ok(())
        } else {
            Err(io::Error::other("Windows could not open the folder"))
        }
    }

    fn open_document(&self, path: &Path) {
        let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        if has_association(&ext) && shell_open(&HSTRING::from(path.as_os_str())) {
            return;
        }
        let _ = Command::new(system_binary("notepad.exe")).arg(path).spawn();
    }

    fn toast(&self, title: &str, body: &str) {
        self.toaster.show(title, body);
    }

    fn beep(&self) {
        // SAFETY: plain Win32 call.
        let _ = unsafe { MessageBeep(MB_ICONEXCLAMATION) };
    }

    fn info(&self, title: &str, text: &str) {
        self.message(title, text, MB_OK | MB_ICONINFORMATION);
    }

    fn error(&self, title: &str, text: &str) {
        self.message(title, text, MB_OK | MB_ICONERROR);
    }

    fn ask_yes_no(&self, title: &str, text: &str) -> bool {
        self.message(title, text, MB_YESNO | MB_ICONQUESTION) == IDYES.0
    }

    fn startup_enabled(&self) -> bool {
        std::env::current_exe().is_ok_and(|exe| jo_core::startup::enabled(&exe, &jo_core::startup::RunKey))
    }

    fn set_startup(&self, enable: bool) -> bool {
        let Ok(exe) = std::env::current_exe() else { return false };
        jo_core::startup::set_enabled(&self.root, &exe, enable, &mut jo_core::startup::RunKey).is_ok()
    }

    fn work_area(&self, x: i32, y: i32) -> Option<WorkArea> {
        // SAFETY: plain Win32 calls with a stack-allocated struct.
        unsafe {
            let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return None;
            }
            let r = info.rcWork;
            Some(WorkArea { left: r.left, top: r.top, right: r.right, bottom: r.bottom })
        }
    }

    fn screen_size(&self) -> (i32, i32) {
        // SAFETY: plain Win32 calls.
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }
}

/// Whether Windows has anything registered to open `ext`: ShellExecute
/// "succeeds" silently with no association, so check first.
pub fn has_association(ext: &str) -> bool {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
    if ext.len() < 2 {
        return false;
    }
    let choice = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\{ext}\UserChoice");
    if RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(choice)
        .and_then(|k| k.get_value::<String, _>("ProgId"))
        .is_ok_and(|v| !v.is_empty())
    {
        return true;
    }
    RegKey::predef(HKEY_CLASSES_ROOT)
        .open_subkey(ext)
        .and_then(|k| k.get_value::<String, _>(""))
        .is_ok_and(|v| !v.is_empty())
}

/// Fully-qualified path to a System32 binary, so nothing earlier in PATH
/// can stand in for it.
pub fn system_binary(relative: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let candidate = root.join("System32").join(relative);
    if candidate.exists() { candidate } else { PathBuf::from(relative) }
}

// ---------------------------------------------------------------------------
// Window chrome helpers (the overlay's own HWND)
// ---------------------------------------------------------------------------

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

/// Windows 11 rounded corners for a borderless window (no-op elsewhere).
pub fn round_corners(raw: isize, small: bool) {
    let pref: DWM_WINDOW_CORNER_PREFERENCE = if small { DWMWCP_ROUNDSMALL } else { DWMWCP_ROUND };
    // SAFETY: valid HWND owned by this process; pointer to a stack value.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd(raw),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const pref).cast(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

/// Layered-window alpha (0..1), adding `WS_EX_LAYERED` if it was dropped.
pub fn set_alpha(raw: isize, alpha: f32) {
    // SAFETY: valid HWND owned by this process.
    unsafe {
        let style = GetWindowLongPtrW(hwnd(raw), GWL_EXSTYLE);
        if style & WS_EX_LAYERED.0 as isize == 0 {
            SetWindowLongPtrW(hwnd(raw), GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
        }
        let byte = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        let _ = SetLayeredWindowAttributes(hwnd(raw), windows::Win32::Foundation::COLORREF(0), byte, LWA_ALPHA);
    }
}

/// Map or unmap the window without activating it (an alert must never steal
/// keyboard focus).
pub fn set_mapped(raw: isize, mapped: bool) {
    // SAFETY: valid HWND owned by this process.
    unsafe {
        let _ = ShowWindow(hwnd(raw), if mapped { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

pub fn focus(raw: isize) {
    // SAFETY: valid HWND owned by this process.
    unsafe {
        let _ = SetForegroundWindow(hwnd(raw));
    }
}

/// `(x, y, w, h)` in physical pixels.
pub fn window_rect(raw: isize) -> Option<(i32, i32, i32, i32)> {
    let mut r = RECT::default();
    // SAFETY: valid HWND; pointer to a stack value.
    unsafe { GetWindowRect(hwnd(raw), &mut r).ok()? };
    Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
}

pub fn set_window_rect(raw: isize, x: i32, y: i32, w: i32, h: i32) {
    // SAFETY: valid HWND owned by this process.
    unsafe {
        let _ = SetWindowPos(hwnd(raw), None, x, y, w.max(1), h.max(1), SWP_NOACTIVATE | SWP_NOZORDER);
    }
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    // SAFETY: pointer to a stack value.
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

/// A top-level window of this process with exactly this title (used to
/// give secondary egui windows the same rounded chrome).
pub fn find_own_window(title: &str) -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};
    // SAFETY: plain Win32 calls with an owned wide string.
    unsafe {
        let found = FindWindowW(PCWSTR::null(), &HSTRING::from(title)).ok()?;
        let mut pid = 0_u32;
        GetWindowThreadProcessId(found, Some(&mut pid));
        (pid == std::process::id()).then_some(found.0 as isize)
    }
}

/// Raw HWND of an eframe window handle.
pub fn hwnd_of(handle: &impl raw_window_handle::HasWindowHandle) -> Option<isize> {
    match handle.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associations() {
        assert!(!has_association(".fake9182nope"), "an extension that plainly doesn't exist has no association");
        assert!(has_association(".txt"), "a real, universally-associated extension is detected");
    }

    #[test]
    fn notepad_is_fully_qualified() {
        let path = system_binary("notepad.exe");
        assert!(path.is_absolute(), "{path:?}");
        assert!(path.to_string_lossy().to_lowercase().ends_with("notepad.exe"));
    }

    #[test]
    fn monitors_resolve_anywhere() {
        let p = Windows::new(Path::new("."));
        for (x, y) in [(0, 0), (-5000, -5000), (99999, 99999)] {
            let r = p.work_area(x, y).expect("a monitor is found");
            assert!(r.right > r.left && r.bottom > r.top, "({x}, {y}) -> {r:?}");
        }
    }
}
