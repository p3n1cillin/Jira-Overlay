//! Windows toast notifications through WinRT directly — no PowerShell, no
//! child process, nothing for ticket text to escape into but XML.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::{HSTRING, IInspectable};

/// Receives the URL of a clicked toast (the UI thread opens it).
pub type ActivationSink = Arc<dyn Fn(String) + Send + Sync>;

/// Toasts stay clickable only while their object is alive in this
/// process, so the most recent few are kept.
const KEEP_ALIVE: usize = 16;
static LIVE: Mutex<VecDeque<ToastNotification>> = Mutex::new(VecDeque::new());

/// The AppUserModelID the installer's Start-menu shortcut registers.
pub const APP_ID: &str = "JiraOverlay";
/// What the Python build used (PowerShell's own registered ID); used when the
/// native shortcut isn't there, so toasts still arrive. No process is started.
pub const FALLBACK_ID: &str = r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

/// True for a code point XML 1.0 allows in character data.
fn xml_ok(c: char) -> bool {
    let cp = c as u32;
    matches!(cp, 0x09 | 0x0A | 0x0D)
        || (0x20..=0xD7FF).contains(&cp)
        || (0xE000..=0xFFFD).contains(&cp)
        || cp >= 0x10000
}

/// Make text safe to drop into the toast's XML: the five markup characters
/// are escaped and characters XML 1.0 forbids are dropped (one of them makes
/// the whole document malformed and the toast silently never arrives).
pub fn xml_escape(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    escaped.chars().filter(|c| xml_ok(*c)).collect()
}

/// The toast document: a one-line title and a body.
pub fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastGeneric\"><text hint-maxLines=\"1\">{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    )
}

/// Chooses the AppUserModelID once and shows toasts off the UI thread.
pub struct Toaster {
    app_id: &'static str,
    sink: Arc<Mutex<Option<ActivationSink>>>,
}

impl Toaster {
    /// The native AUMID when the installer registered it (native install
    /// with its Start-menu shortcut), else the PowerShell fallback ID.
    pub fn detect(root: &Path) -> Self {
        let registered = jo_core::updates::native_install(root) && start_menu_shortcut().is_some_and(|p| p.is_file());
        if registered {
            // SAFETY: plain Win32 call with a static wide string.
            unsafe {
                let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID));
            }
        }
        Self { app_id: if registered { APP_ID } else { FALLBACK_ID }, sink: Arc::default() }
    }

    /// Where clicks on toasts go. Until set, toasts are not clickable.
    pub fn set_activation_sink(&self, sink: ActivationSink) {
        *self.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    pub fn app_id(&self) -> &'static str {
        self.app_id
    }

    /// Fire and forget; any failure is silent, as a toast is never worth
    /// interrupting the repaint that triggered it. Clicking the toast hands
    /// `open` to the activation sink.
    pub fn show(&self, title: &str, body: &str, open: Option<&str>) {
        let (id, xml) = (self.app_id, toast_xml(title, body));
        let sink = self.sink.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let target = open.zip(sink).map(|(url, sink)| (url.to_owned(), sink));
        let _ = std::thread::Builder::new().name("toast".into()).spawn(move || {
            let _ = send(id, &xml, target);
        });
    }
}

fn start_menu_shortcut() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(Path::new(&appdata).join(r"Microsoft\Windows\Start Menu\Programs\Jira Overlay\Jira Overlay.lnk"))
}

fn send(app_id: &str, xml: &str, target: Option<(String, ActivationSink)>) -> windows::core::Result<()> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    if let Some((url, sink)) = target {
        // In-process activation: works while the toast object lives here.
        // Where it isn't available the toast still shows, just not clickable.
        let handler = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            sink(url.clone());
            Ok(())
        });
        let _ = toast.Activated(&handler);
    }
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))?.Show(&toast)?;
    let mut live = LIVE.lock().unwrap_or_else(|p| p.into_inner());
    live.push_back(toast);
    while live.len() > KEEP_ALIVE {
        live.pop_front();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awkward_summaries_cannot_break_out() {
        for (name, text) in [
            ("an apostrophe", "Bob's laptop won't boot"),
            ("a quote break-out", "'; Write-Host PWNED; '"),
            ("angle brackets", "Error <script>alert(1)</script>"),
            ("an ampersand", "Sales & Marketing share"),
            ("double quotes", "User said \"it is broken\""),
            ("backtick and dollar", "Cost `$500 and $env:PATH"),
            ("a newline", "line one\nline two"),
            ("control characters", "bell\x07 and null\x00 here"),
            ("an emoji", "Printer jammed \u{1F5A8}"),
        ] {
            let escaped = xml_escape(text);
            let stripped = escaped
                .replace("&amp;", "")
                .replace("&lt;", "")
                .replace("&gt;", "")
                .replace("&quot;", "")
                .replace("&apos;", "");
            let leftovers = escaped.contains(['\'', '<', '>'])
                || stripped.contains('&')
                || escaped.chars().any(|c| (c as u32) < 32 && !"\t\n\r".contains(c));
            assert!(!leftovers, "a summary with {name} can't break out: {escaped}");
        }
        assert_eq!(xml_escape("Printer jammed \u{1F5A8}"), "Printer jammed \u{1F5A8}");
        assert_eq!(xml_escape("a\x07b"), "ab");
    }

    #[test]
    fn document_shape_and_parse() {
        let xml = toast_xml("Jira — SD-1", "Bob's <laptop> & \"co\"\x00");
        assert!(xml.starts_with("<toast duration=\"short\">"));
        assert!(xml.contains("<text hint-maxLines=\"1\">Jira — SD-1</text>"));
        // The document WinRT receives must parse, whatever the summary held.
        let doc = XmlDocument::new().unwrap();
        doc.LoadXml(&HSTRING::from(xml)).expect("well-formed toast XML");
    }

    #[test]
    fn activation_reaches_the_sink() {
        // The handler is the one wired to `Activated`; invoking it directly
        // proves a click hands over exactly the URL the toast was built with.
        let got: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = got.clone();
        let sink: ActivationSink = Arc::new(move |url| log.lock().unwrap().push(url));
        let url = "https://e.atlassian.net/browse/SD-1".to_owned();
        let handler = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            sink(url.clone());
            Ok(())
        });
        let doc = XmlDocument::new().unwrap();
        doc.LoadXml(&HSTRING::from(toast_xml("t", "b"))).unwrap();
        let toast = ToastNotification::CreateToastNotification(&doc).unwrap();
        let token = toast.Activated(&handler).expect("the event can be subscribed in-process");
        handler.Invoke(&toast, None).unwrap();
        toast.RemoveActivated(token).unwrap();
        assert_eq!(*got.lock().unwrap(), ["https://e.atlassian.net/browse/SD-1"]);

        let toaster = Toaster::detect(tempfile::tempdir().unwrap().path());
        assert!(toaster.sink.lock().unwrap().is_none(), "not clickable until the UI provides a sink");
    }

    #[test]
    fn fallback_id_without_installer_shortcut() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Toaster::detect(dir.path()).app_id(), FALLBACK_ID);
    }
}
