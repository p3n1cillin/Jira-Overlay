//! First-run Setup: domain, email and API token, checked against
//! `/rest/api/3/myself` before anything is saved.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui::{self, Align2, CornerRadius, Role, Sense, Ui, ViewportBuilder, vec2};
use jo_core::config::{self, Config};
use jo_jira::{Jira, Method, Retry};
use serde_json::{Value, json};

use crate::platform::{self, Platform};
use crate::poll::MakeTransport;
use crate::theme::{self, c};
use crate::widgets;

pub const TOKEN_URL: &str = "https://id.atlassian.com/manage-profile/security/api-tokens";

/// `(label, placeholder, secret)`.
pub const FIELDS: [(&str, &str, bool); 3] = [
    ("Jira Domain", "yourcompany.atlassian.net", false),
    ("Email", "you@company.com", false),
    ("API Token", "paste your token here", true),
];

/// Check the three fields; `Err((title, message))` for the dialog.
pub fn validate(values: &[String; 3]) -> Result<(String, String, String), (String, String)> {
    for ((label, hint, _), value) in FIELDS.iter().zip(values) {
        let v = value.trim();
        if v.is_empty() || v == *hint {
            return Err(("Missing".into(), format!("{label} is required.")));
        }
    }
    let domain = jo_core::jql::normalise_domain(&values[0]);
    if domain.is_empty() {
        return Err((
            "Jira Domain".into(),
            "That doesn't look like a Jira address.\n\nIt should be just the host, like:\n    yourcompany.atlassian.net".into(),
        ));
    }
    Ok((domain, values[1].trim().to_owned(), values[2].trim().to_owned()))
}

/// Try the credentials: `Ok(display name)` or a problem description.
pub fn check_credentials(domain: &str, email: &str, token: &str, make: &MakeTransport) -> Result<String, String> {
    let jira = Jira::new(domain, email, token, make());
    let r =
        jira.send(Method::Get, "/rest/api/3/myself", &[], None, Duration::from_secs(15), Retry::Once).map_err(|e| {
            let text = e.to_string().to_lowercase();
            let kind =
                if text.contains("timed out") || text.contains("timeout") { "Timeout" } else { "ConnectionError" };
            format!("Could not reach {domain}.\n\nCheck the domain and your internet connection.\n\n({kind})")
        })?;
    match r.status {
        401 | 403 => Err("Jira rejected the email or API token.\n\nThe token must belong to the same account as the email, and tokens expire - create a fresh one if in doubt.".into()),
        404 => Err(format!("{domain} answered, but doesn't look like a Jira site.\n\nCheck the domain.")),
        s if s >= 400 => Err(format!("Jira returned HTTP {s} when signing in.")),
        _ => Ok(r
            .json()
            .and_then(|v| v.get("displayName").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_owned))
            .unwrap_or_else(|| email.to_owned())),
    }
}

/// A running sign-in check and the values it is checking.
type Checking = (Receiver<Result<String, String>>, (String, String, String));

pub struct SetupApp {
    root: PathBuf,
    platform: Arc<dyn Platform>,
    make: MakeTransport,
    pub values: [String; 3],
    pub status: String,
    checking: Option<Checking>,
    pub result: Rc<RefCell<Option<Config>>>,
    pub done: bool,
    /// Fonts and visuals are installed on the first frame.
    styled: bool,
}

impl SetupApp {
    pub fn new(
        ctx: &egui::Context,
        root: &Path,
        platform: Arc<dyn Platform>,
        make: MakeTransport,
        result: Rc<RefCell<Option<Config>>>,
    ) -> Self {
        let _ = ctx;
        Self {
            root: root.to_path_buf(),
            platform,
            make,
            values: Default::default(),
            status: String::new(),
            checking: None,
            result,
            done: false,
            styled: false,
        }
    }

    fn save(&mut self, ctx: &egui::Context) {
        if self.checking.is_some() {
            return;
        }
        let (domain, email, token) = match validate(&self.values) {
            Ok(v) => v,
            Err((title, message)) => return self.platform.error(&title, &message),
        };
        self.status = "Checking your details...".into();
        let (tx, rx) = mpsc::channel();
        let (d, e, t, make, repaint) = (domain.clone(), email.clone(), token.clone(), self.make.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("setup-check".into()).spawn(move || {
            let _ = tx.send(check_credentials(&d, &e, &t, &make));
            repaint.request_repaint();
        });
        self.checking = Some((rx, (domain, email, token)));
    }

    /// The sign-in check finished.
    pub fn poll_check(&mut self, ctx: &egui::Context) {
        let Some((rx, _)) = &self.checking else { return };
        let Ok(outcome) = rx.try_recv() else { return };
        let (_, (domain, email, token)) = self.checking.take().expect("checked");
        self.status.clear();
        match outcome {
            Err(problem) => self.platform.error("Could not sign in", &problem),
            Ok(who) => {
                self.platform.info("Signed in", &format!("Connected to Jira as {who}."));
                let fresh = json!({"domain": domain, "email": email, "token": token});
                let fresh = fresh.as_object().cloned().unwrap_or_default();
                let path = self.root.join(config::FILE_NAME);
                if let Err(err) = config::save(&path, &fresh) {
                    crate::util::log_error(&self.root, &format!("Could not save the configuration: {err}"));
                }
                *self.result.borrow_mut() = Some(config::load(&path).unwrap_or(fresh));
                self.done = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if !self.styled {
            theme::install_fonts(&ctx);
            theme::install_style(&ctx);
            self.styled = true;
        }
        self.poll_check(&ctx);
        ui.painter().rect_filled(ui.max_rect(), CornerRadius::ZERO, c::BG_DARK);
        let mut save = false;
        egui::Frame::new().inner_margin(egui::Margin::symmetric(20, 20)).show(ui, |ui| {
            ui.vertical_centered(|ui| widgets::label(ui, "Jira Overlay Setup", theme::bold(14.0), c::FG_BRIGHT));
            ui.add_space(16.0);
            egui::Grid::new("setup-fields").num_columns(2).spacing(vec2(8.0, 12.0)).show(ui, |ui| {
                for ((label, hint, secret), value) in FIELDS.iter().zip(self.values.iter_mut()) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        widgets::label(ui, &format!("{label}:"), theme::regular(9.0), c::FG);
                    });
                    let edit = egui::TextEdit::singleline(value)
                        .hint_text(egui::RichText::new(*hint).color(c::FG_DIM))
                        .password(*secret)
                        .desired_width(260.0)
                        .margin(vec2(6.0, 5.0));
                    let resp = ui.add(edit);
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        save = true;
                    }
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.vertical_centered(|ui| {
                let text = "Generate an API token at id.atlassian.com";
                let font = theme::regular(8.0);
                let size = widgets::text_size(ui, text, &font);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                let colour = if resp.hovered() { c::ACCENT_HI } else { c::ACCENT };
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, colour);
                ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, egui::Stroke::new(1.0, colour));
                widgets::describe(&resp, Role::Link, text);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.platform.open_url(TOKEN_URL);
                }
                ui.add_space(10.0);
                let busy = self.checking.is_some();
                if widgets::flat_button(
                    ui,
                    "Save & Launch",
                    theme::bold(10.0),
                    (c::WHITE, c::ACCENT, c::ACCENT_HI),
                    None,
                    !busy,
                )
                .clicked()
                {
                    save = true;
                }
                ui.add_space(4.0);
                widgets::label(ui, &self.status, theme::regular(8.0), c::FG_DIM);
            });
        });
        if save {
            self.save(&ctx);
        }
    }
}

impl eframe::App for SetupApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        SetupApp::ui(self, ui);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        c::BG_DARK.to_normalized_gamma_f32()
    }
}

/// Show Setup until it saves a working configuration or is closed.
pub fn run(root: &Path, platform: Arc<dyn Platform>, make: MakeTransport) -> Result<Option<Config>, String> {
    let result = Rc::new(RefCell::new(None));
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Jira Overlay — Setup")
            .with_inner_size([430.0, 320.0])
            .with_resizable(false)
            .with_maximize_button(false),
        centered: true,
        persist_window: false,
        ..Default::default()
    };
    let shared = result.clone();
    eframe::run_native(
        "Jira Overlay — Setup",
        options,
        Box::new(move |cc| {
            if let Some(h) = platform::hwnd_of(cc) {
                platform::round_corners(h, false);
            }
            Ok(Box::new(SetupApp::new(&cc.egui_ctx, root, platform, make, shared)))
        }),
    )
    .map_err(|e| format!("Could not open the setup window: {e}"))?;
    Ok(result.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeJira, ok_json, status};

    fn vals(a: &str, b: &str, c: &str) -> [String; 3] {
        [a.into(), b.into(), c.into()]
    }

    #[test]
    fn validation_messages() {
        assert_eq!(validate(&vals("", "e@x", "t")).unwrap_err(), ("Missing".into(), "Jira Domain is required.".into()));
        assert_eq!(
            validate(&vals("acme.atlassian.net", "you@company.com", "t")).unwrap_err().1,
            "Email is required.",
            "the placeholder itself is not a value"
        );
        assert_eq!(validate(&vals("acme.atlassian.net", "e@x", " ")).unwrap_err().1, "API Token is required.");
        assert_eq!(validate(&vals("localhost", "e@x", "t")).unwrap_err().0, "Jira Domain");
        assert_eq!(
            validate(&vals("https://Acme.atlassian.net/browse/ITS-1", " e@x ", "t")).unwrap(),
            ("acme.atlassian.net".into(), "e@x".into(), "t".into())
        );
    }

    #[test]
    fn credential_check_messages() {
        let check = |fake: FakeJira| check_credentials("acme.atlassian.net", "e@x", "t", &fake.make());
        let fake = FakeJira::new(|req| {
            assert_eq!(req.url, "https://acme.atlassian.net/rest/api/3/myself");
            ok_json(json!({"displayName": "Sam Carter"}))
        });
        assert_eq!(check(fake), Ok("Sam Carter".into()));
        assert_eq!(check(FakeJira::new(|_| ok_json(json!([1])))), Ok("e@x".into()), "no name falls back to the email");
        for code in [401, 403] {
            assert!(
                check(FakeJira::new(move |_| status(code)))
                    .unwrap_err()
                    .starts_with("Jira rejected the email or API token.")
            );
        }
        assert!(check(FakeJira::new(|_| status(404))).unwrap_err().contains("doesn't look like a Jira site"));
        assert_eq!(check(FakeJira::new(|_| status(502))).unwrap_err(), "Jira returned HTTP 502 when signing in.");
        let offline = check(FakeJira::new(|_| Err(jo_jira::TransportError("dns error".into())))).unwrap_err();
        assert!(offline.starts_with("Could not reach acme.atlassian.net.") && offline.ends_with("(ConnectionError)"));
        let slow = check(FakeJira::new(|_| Err(jo_jira::TransportError("operation timed out".into())))).unwrap_err();
        assert!(slow.ends_with("(Timeout)"));
    }
}
