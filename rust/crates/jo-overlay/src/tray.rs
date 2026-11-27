//! The notification-area icon: count badge, tooltip and menu. Lives on the
//! UI thread, whose event loop also pumps its messages.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::app::{UiEvent, Waker};
use crate::state::{TrayState, tray_tooltip};
use crate::tray_image;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    ShowHide,
    TeamReport,
    LearnerReport,
    CheckUpdates,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    menu: Menu,
    learner: MenuItem,
    learner_shown: bool,
    ids: Vec<(MenuId, Item)>,
}

fn image(count: u64, alert: bool) -> Option<Icon> {
    Icon::from_rgba(tray_image::render(count, alert), tray_image::SIZE, tray_image::SIZE).ok()
}

impl Tray {
    pub fn new(root: &Path, tx: Sender<UiEvent>, waker: Waker) -> Option<Self> {
        let show = MenuItem::new("Show / Hide", true, None);
        let team = MenuItem::new("Team report", true, None);
        let learner = MenuItem::new("Learner report", true, None);
        let check = MenuItem::new("Check for updates", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let learner_shown = root.join("TRIAGE_REPORT.md").is_file();
        menu.append(&show).ok()?;
        menu.append(&team).ok()?;
        if learner_shown {
            menu.append(&learner).ok()?;
        }
        menu.append(&check).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&quit).ok()?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false)
            .with_tooltip("Jira Overlay")
            .with_icon(image(0, false)?)
            .build()
            .ok()?;
        let ids = vec![
            (show.id().clone(), Item::ShowHide),
            (team.id().clone(), Item::TeamReport),
            (learner.id().clone(), Item::LearnerReport),
            (check.id().clone(), Item::CheckUpdates),
            (quit.id().clone(), Item::Quit),
        ];
        let (menu_tx, menu_waker) = (tx.clone(), waker.clone());
        MenuEvent::set_event_handler(Some(std::sync::Arc::new(move |event: MenuEvent| {
            let _ = menu_tx.send(UiEvent::Menu(event.id));
            menu_waker();
        })));
        // Hover refreshes whether "Learner report" is listed (pystray asked
        // on every menu open); throttled, as moves arrive constantly.
        let last_hover = AtomicU64::new(0);
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            let message = match event {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => {
                    Some(UiEvent::TrayClick)
                }
                TrayIconEvent::Enter { .. } | TrayIconEvent::Move { .. } => {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
                    (now.saturating_sub(last_hover.swap(now, Ordering::SeqCst)) > 1000).then_some(UiEvent::TrayHover)
                }
                _ => None,
            };
            if let Some(message) = message {
                let _ = tx.send(message);
                waker();
            }
        }));
        Some(Self { icon, menu, learner, learner_shown, ids })
    }

    pub fn item(&self, id: &MenuId) -> Option<Item> {
        self.ids.iter().find(|(i, _)| i == id).map(|(_, item)| *item)
    }

    /// "Learner report" appears only when the learner has written its report.
    pub fn refresh_learner_item(&mut self, root: &Path) {
        let exists = root.join("TRIAGE_REPORT.md").is_file();
        if exists == self.learner_shown {
            return;
        }
        let changed =
            if exists { self.menu.insert(&self.learner, 2).is_ok() } else { self.menu.remove(&self.learner).is_ok() };
        if changed {
            self.learner_shown = exists;
        }
    }

    pub fn show_state(&mut self, state: &TrayState) {
        let (count, alert, _) = state;
        let _ = self.icon.set_icon(image(*count, *alert));
        let _ = self.icon.set_tooltip(Some(tray_tooltip(state)));
    }
}
