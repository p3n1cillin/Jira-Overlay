//! The global show/hide hotkey: `RegisterHotKey` on a dedicated thread
//! whose message loop forwards `WM_HOTKEY` to the UI channel. A combination
//! another app already owns is reported (Settings shows it, the error log
//! gets one line), never fatal.

use std::sync::{Arc, Mutex};

use jo_core::config::Config;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_APP, WM_HOTKEY, WM_QUIT,
};

/// The default combination.
pub const DEFAULT: &str = "Ctrl+Alt+J";
/// What Settings offers: a few combinations no common app claims, and Off.
pub const CHOICES: [&str; 5] = ["Ctrl+Alt+J", "Ctrl+Alt+Shift+J", "Ctrl+Shift+F12", "Ctrl+Alt+O", "Off"];

const HOTKEY_ID: i32 = 0x4A4F; // "JO"
const WM_REREGISTER: u32 = WM_APP + 1;

/// A parsed combination: Win32 modifier flags and virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: u32,
    pub vk: u32,
}

/// `"Ctrl+Alt+J"` → modifiers + key. Needs at least one of Ctrl/Alt/Win
/// (Shift alone would swallow ordinary typing) and one key: A–Z, 0–9 or
/// F1–F24. `None` for "Off", empty or anything unparseable.
pub fn parse(text: &str) -> Option<Combo> {
    let mut modifiers = 0;
    let mut vk = None;
    for part in text.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
            "alt" => modifiers |= MOD_ALT.0,
            "shift" => modifiers |= MOD_SHIFT.0,
            "win" => modifiers |= MOD_WIN.0,
            key if vk.is_none() => vk = Some(key_code(key)?),
            _ => return None,
        }
    }
    let strong = MOD_CONTROL.0 | MOD_ALT.0 | MOD_WIN.0;
    (modifiers & strong != 0).then_some(())?;
    Some(Combo { modifiers, vk: vk? })
}

fn key_code(key: &str) -> Option<u32> {
    let upper = key.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    match bytes {
        [c] if c.is_ascii_uppercase() || c.is_ascii_digit() => Some(u32::from(*c)),
        [b'F', ..] => {
            let n: u32 = upper[1..].parse().ok()?;
            (1..=24).contains(&n).then_some(0x70 + n - 1) // VK_F1 = 0x70
        }
        _ => None,
    }
}

/// The configured combination text: `hotkey`, absent meaning the default,
/// anything unparseable or "Off" meaning none.
pub fn configured(cfg: &Config) -> String {
    match cfg.get("hotkey") {
        None => DEFAULT.to_owned(),
        Some(v) => {
            let text = v.as_str().unwrap_or("").trim();
            if parse(text).is_some() { text.to_owned() } else { "Off".to_owned() }
        }
    }
}

/// What happened to the last registration request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Off,
    Registered(String),
    /// `(combo, reason)`: usually another app holds it.
    Failed(String, String),
}

impl Status {
    /// One line for Settings; `None` when there is nothing to say.
    pub fn problem(&self) -> Option<String> {
        match self {
            Status::Failed(combo, _) => Some(format!("{combo} is already used by another app — pick another")),
            _ => None,
        }
    }
}

/// The running hotkey thread.
pub struct Hotkey {
    thread_id: u32,
    /// `(request number, combination)` the UI asked for last.
    wanted: Arc<Mutex<(u64, String)>>,
    /// `(request number acted on, outcome)`.
    status: Arc<Mutex<(u64, Status)>>,
}

impl Hotkey {
    /// Start the thread and register `combo`; `fire` runs (on that thread)
    /// for every press; `log` reports a failed registration once per combo.
    pub fn start(combo: &str, fire: impl Fn() + Send + 'static, log: impl Fn(&str) + Send + 'static) -> Option<Self> {
        let wanted = Arc::new(Mutex::new((1, combo.to_owned())));
        let status = Arc::new(Mutex::new((0, Status::Off)));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (w, st) = (wanted.clone(), status.clone());
        std::thread::Builder::new()
            .name("hotkey".into())
            .spawn(move || {
                // SAFETY: plain Win32 calls on this thread's own queue.
                unsafe {
                    let mut msg = MSG::default();
                    // Create the message queue before anyone posts to it.
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    let _ = ready_tx.send(GetCurrentThreadId());
                    let mut logged: Option<String> = None;
                    let register = |logged: &mut Option<String>| {
                        let _ = UnregisterHotKey(None, HOTKEY_ID);
                        let (request, text) = w.lock().unwrap_or_else(|p| p.into_inner()).clone();
                        let outcome = match parse(&text) {
                            None => Status::Off,
                            Some(c) => match RegisterHotKey(
                                None,
                                HOTKEY_ID,
                                HOT_KEY_MODIFIERS(c.modifiers | MOD_NOREPEAT.0),
                                c.vk,
                            ) {
                                Ok(()) => Status::Registered(text.clone()),
                                Err(e) => Status::Failed(text.clone(), e.message()),
                            },
                        };
                        if let Status::Failed(combo, reason) = &outcome
                            && logged.as_deref() != Some(combo.as_str())
                        {
                            log(&format!("Could not register the show/hide hotkey {combo}: {reason}"));
                            *logged = Some(combo.clone());
                        }
                        *st.lock().unwrap_or_else(|p| p.into_inner()) = (request, outcome);
                    };
                    register(&mut logged);
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        match msg.message {
                            WM_HOTKEY if msg.wParam.0 == HOTKEY_ID as usize => fire(),
                            WM_REREGISTER => register(&mut logged),
                            _ => {}
                        }
                    }
                    let _ = UnregisterHotKey(None, HOTKEY_ID);
                }
            })
            .ok()?;
        let thread_id = ready_rx.recv().ok()?;
        Some(Self { thread_id, wanted, status })
    }

    /// Switch to another combination (or "Off").
    pub fn set(&self, combo: &str) {
        {
            let mut wanted = self.wanted.lock().unwrap_or_else(|p| p.into_inner());
            *wanted = (wanted.0 + 1, combo.to_owned());
        }
        // SAFETY: posting to a thread id this struct owns.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_REREGISTER, WPARAM(0), LPARAM(0));
        }
    }

    /// The outcome of the latest request the thread has acted on.
    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).1.clone()
    }

    /// Wait (briefly) until the thread has acted on the latest request.
    pub fn settle(&self, timeout: std::time::Duration) -> Status {
        let until = std::time::Instant::now() + timeout;
        loop {
            let wanted = self.wanted.lock().unwrap_or_else(|p| p.into_inner()).0;
            let (acted, status) = self.status.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if acted >= wanted || std::time::Instant::now() >= until {
                return status;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        // SAFETY: posting to a thread id this struct owns.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn combos_parse_safely() {
        assert_eq!(parse("Ctrl+Alt+J"), Some(Combo { modifiers: MOD_CONTROL.0 | MOD_ALT.0, vk: u32::from(b'J') }));
        assert_eq!(parse(" ctrl + shift + f12 ").map(|c| c.vk), Some(0x7B));
        assert_eq!(parse("Ctrl+Alt+7").map(|c| c.vk), Some(u32::from(b'7')));
        for bad in ["", "Off", "J", "Shift+J", "Ctrl+Alt", "Ctrl+Alt+J+K", "Ctrl+Alt+Esc", "Ctrl+F25", "Ctrl+Alt+é"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        for choice in CHOICES {
            assert_eq!(parse(choice).is_some(), choice != "Off", "{choice}");
        }
        let cfg = |v: serde_json::Value| v.as_object().unwrap().clone();
        assert_eq!(configured(&cfg(json!({}))), "Ctrl+Alt+J", "on by default");
        assert_eq!(configured(&cfg(json!({"hotkey": "Off"}))), "Off");
        assert_eq!(configured(&cfg(json!({"hotkey": ""}))), "Off");
        assert_eq!(configured(&cfg(json!({"hotkey": 5}))), "Off");
        assert_eq!(configured(&cfg(json!({"hotkey": "Ctrl+Alt+O"}))), "Ctrl+Alt+O");
    }

    #[test]
    fn a_taken_combination_is_reported_not_fatal() {
        // An obscure combination, held only for the length of this test.
        let combo = "Ctrl+Alt+Shift+F23";
        let logs = Arc::new(Mutex::new(Vec::<String>::new()));
        let first = Hotkey::start(combo, || {}, |_| {}).unwrap();
        assert_eq!(first.settle(Duration::from_secs(2)), Status::Registered(combo.into()));
        let log = logs.clone();
        let second = Hotkey::start(combo, || {}, move |m| log.lock().unwrap().push(m.to_owned())).unwrap();
        let status = second.settle(Duration::from_secs(2));
        assert!(matches!(&status, Status::Failed(c, _) if c == combo), "{status:?}");
        assert!(status.problem().unwrap().contains("already used"));
        second.set(combo);
        second.settle(Duration::from_secs(2));
        assert_eq!(logs.lock().unwrap().len(), 1, "logged once, however often it is retried");
        // Switching off releases it; the other instance can then take it.
        first.set("Off");
        assert_eq!(first.settle(Duration::from_secs(2)), Status::Off);
        second.set(combo);
        assert_eq!(second.settle(Duration::from_secs(2)), Status::Registered(combo.into()));
    }
}
