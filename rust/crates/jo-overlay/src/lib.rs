//! The Jira Overlay desktop UI: always-on-top queue overlay, tray icon,
//! alerts, settings, setup and the team report.
//!
//! Port of `jira_overlay.pyw`. The UI-free core (`poll`, `state`, `alerts`,
//! `stats`, `report`, `settings`, `placement`, `learner`, `toast`,
//! `tray_image`, `updates_ui`) carries the behaviour; `app`, `view`,
//! `windows`, `tray` and `setup` are the egui shell around it.

use std::cell::Cell;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use eframe::egui::{self, ViewportBuilder};
use jo_core::lifecycle::{self, Session};

pub mod alerts;
pub mod app;
pub mod learner;
pub mod model;
pub mod placement;
pub mod platform;
pub mod poll;
pub mod report;
pub mod settings;
pub mod setup;
pub mod state;
pub mod stats;
pub mod toast;
pub mod tray;
pub mod tray_image;
pub mod updates_ui;
pub mod util;
pub mod view;
pub mod widgets;
pub mod windows;

mod theme;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod ui_tests;

use app::{OverlayApp, Services, UiEvent};

/// How the overlay ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Quit, setup cancelled or stopped for an update (reason already recorded).
    Quit,
    /// "Reconfigure credentials": the config was removed; the caller releases
    /// the single-instance mutex and starts a fresh overlay process.
    Restart,
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The eframe shell: `logic` runs even while the overlay is hidden.
struct Shell<'a> {
    app: OverlayApp<'a>,
    exit: Rc<Cell<Exit>>,
}

impl eframe::App for Shell<'_> {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
        self.exit.set(self.app.exit);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
        self.exit.set(self.app.exit);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::c::BG.to_normalized_gamma_f32()
    }
}

/// Run the overlay until Quit, Reconfigure or an update stop request.
///
/// The caller has already checked the security hold, taken the
/// `JiraOverlaySingleInstance` mutex (a duplicate launch instead signals the
/// show event and exits) and opened `session`. This function creates and
/// waits on the show event, runs first-time setup when there is no config,
/// launches `<current exe> --learner` unless supervised, listens for the
/// per-root stop event and records quit reasons on `session`.
///
/// `Err(message)` is a fatal start-up error to show the user.
pub fn run(root: &Path, session: &Session) -> Result<Exit, String> {
    let windows_platform = Arc::new(platform::Windows::new(root));
    let services = Services {
        platform: windows_platform.clone(),
        make_transport: poll::http_transports(),
        latest_release: updates_ui::github(),
    };
    let show_event = jo_core::ipc::event(jo_core::ipc::SHOW_EVENT).ok();

    let cfg = match jo_core::config::load(&root.join(jo_core::config::FILE_NAME)) {
        Some(cfg) => cfg,
        None => match setup::run(root, services.platform.clone(), services.make_transport.clone())? {
            Some(cfg) => cfg,
            None => {
                session.reason("setup-cancelled", true);
                let _ = lifecycle::set_wanted(root, "learner", false);
                return Ok(Exit::Quit);
            }
        },
    };

    let exit = Rc::new(Cell::new(Exit::Quit));
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Jira Overlay")
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_visible(false)
            .with_active(false)
            .with_resizable(false)
            .with_inner_size([260.0, 60.0])
            .with_position([100.0, 100.0]),
        persist_window: false,
        ..Default::default()
    };
    let shell_exit = exit.clone();
    eframe::run_native(
        "Jira Overlay",
        options,
        Box::new(move |cc| {
            let hwnd = platform::hwnd_of(cc);
            if let Some(h) = hwnd {
                windows_platform.set_owner(h);
            }
            let app = OverlayApp::new(
                &cc.egui_ctx,
                root,
                cfg,
                services,
                Box::new(move |reason, quit| session.reason(reason, quit)),
                app::Options { hwnd, tray: true, poller: true },
            );
            session.ready();
            start_background(root, &app);
            Ok(Box::new(Shell { app, exit: shell_exit }))
        }),
    )
    .map_err(|e| format!("Could not open the overlay window: {e}"))?;
    drop(show_event);
    Ok(exit.get())
}

/// Stop listener, the learner companion and the show-event waiter.
fn start_background(root: &Path, app: &OverlayApp<'_>) {
    let (tx, waker) = (app.tx.clone(), app.waker.clone());
    if let Err(err) = jo_core::ipc::listen_for_stop(root, move || {
        let _ = tx.send(UiEvent::StopForUpdate);
        waker();
    }) {
        util::log_error(root, &format!("Could not listen for update requests: {err}"));
    }

    // The headless learner follows the overlay unless the guardian runs it.
    if !lifecycle::supervised(root) {
        let spawned = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .arg("--learner")
                .arg("--root")
                .arg(root)
                .current_dir(root)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
        });
        if let Err(err) = spawned {
            util::log_error(root, &format!("Could not start the learner: {err}"));
        }
    }

    // A second launch signals the show event: bring the overlay up.
    if let Ok(event) = jo_core::ipc::event(jo_core::ipc::SHOW_EVENT) {
        let (tx, waker) = (app.tx.clone(), app.waker.clone());
        let _ = std::thread::Builder::new().name("show-waiter".into()).spawn(move || {
            while event.wait(None) {
                let _ = event.reset();
                if tx.send(UiEvent::Show).is_err() {
                    break;
                }
                waker();
            }
        });
    }
}
