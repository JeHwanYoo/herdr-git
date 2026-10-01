use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Paragraph, Wrap};

use super::overlay::Overlay;
use super::widgets::{self, ConfirmButton, ConfirmButtons, left_click};
use super::{App, theme};
use crate::herdr;

const CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(Default, Debug, PartialEq, Eq)]
pub(super) enum UpdateStatus {
    #[default]
    Current,
    Available(String),
    Installing(String),
    Ready,
}

enum UpdateResult {
    Checked(Option<String>),
    Installed(Result<(), String>),
}

#[derive(Default)]
pub(super) struct UpdateState {
    pub(super) status: UpdateStatus,
    pub(super) area: Rect,
    pub(super) skip_area: Rect,
    skipped: Option<String>,
    pub(super) restart_requested: bool,
    pub(super) executable: Option<PathBuf>,
    pending: Option<Receiver<UpdateResult>>,
    last_check: Option<Instant>,
}

impl UpdateState {
    pub(super) fn from_environment() -> Self {
        Self {
            skipped: herdr::skipped_version(),
            ..Self::default()
        }
    }

    fn offers_update(&self) -> bool {
        matches!(&self.status, UpdateStatus::Available(tag) if self.skipped.as_ref() != Some(tag))
    }

    pub(super) fn enabled(&self) -> bool {
        self.offers_update() || self.status == UpdateStatus::Ready
    }

    fn label(&self) -> String {
        let version = env!("CARGO_PKG_VERSION");
        match &self.status {
            UpdateStatus::Current => format!(" v{version} "),
            UpdateStatus::Available(_) if self.offers_update() => {
                format!(" v{version} · Update available ")
            }
            UpdateStatus::Available(_) => format!(" v{version} "),
            UpdateStatus::Installing(_) => format!(" v{version} · Updating… "),
            UpdateStatus::Ready => format!(" v{version} · Restart to update "),
        }
    }
}

impl App {
    pub(super) fn check_for_update(&mut self) {
        if self.update.pending.is_some()
            || matches!(
                self.update.status,
                UpdateStatus::Installing(_) | UpdateStatus::Ready
            )
        {
            return;
        }
        self.update.last_check = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        if thread::Builder::new()
            .name("herdr-git-update-check".into())
            .spawn(move || {
                let _ = tx.send(UpdateResult::Checked(herdr::latest_version()));
            })
            .is_ok()
        {
            self.update.pending = Some(rx);
        }
    }

    pub(super) fn poll_update(&mut self) -> bool {
        if let Some(rx) = &self.update.pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.update.pending = None;
                    match result {
                        UpdateResult::Checked(Some(tag)) => {
                            self.update.status = UpdateStatus::Available(tag);
                        }
                        UpdateResult::Checked(None) => return false,
                        UpdateResult::Installed(Ok(())) => self.update.status = UpdateStatus::Ready,
                        UpdateResult::Installed(Err(error)) => self.update_failed(error),
                    }
                    return true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.update.pending = None;
                    if matches!(self.update.status, UpdateStatus::Installing(_)) {
                        self.update_failed(
                            "Update worker stopped before installation completed".into(),
                        );
                        return true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self
            .update
            .last_check
            .is_some_and(|last| last.elapsed() >= CHECK_INTERVAL)
        {
            self.check_for_update();
        }
        false
    }

    fn update_failed(&mut self, error: String) {
        if let UpdateStatus::Installing(tag) = &self.update.status {
            self.update.status = if tag.is_empty() {
                UpdateStatus::Current
            } else {
                UpdateStatus::Available(tag.clone())
            };
        }
        self.shell.error = Some(error);
    }

    pub(super) fn activate_update(&mut self) {
        match &self.update.status {
            UpdateStatus::Available(_) if self.update.offers_update() => {
                self.open_update_confirmation()
            }
            UpdateStatus::Available(_) => {}
            UpdateStatus::Ready => {
                if self.foreground.action.is_none() && self.workspaces.pending_switch.is_none() {
                    self.update.restart_requested = true;
                }
            }
            UpdateStatus::Current | UpdateStatus::Installing(_) => {}
        }
    }

    pub(super) fn open_update_confirmation(&mut self) {
        if matches!(self.update.status, UpdateStatus::Installing(_)) {
            return;
        }
        if self.update.status == UpdateStatus::Ready {
            self.activate_update();
            return;
        }
        let tag = match &self.update.status {
            UpdateStatus::Available(tag) => tag.clone(),
            _ => String::new(),
        };
        self.overlay = Overlay::Update {
            tag,
            buttons: ConfirmButtons::default(),
        };
    }

    fn start_update_install(&mut self, tag: String) {
        if matches!(
            self.update.status,
            UpdateStatus::Installing(_) | UpdateStatus::Ready
        ) {
            return;
        }
        self.update.executable = std::env::var_os("HERDR_PLUGIN_ROOT")
            .map(|root| PathBuf::from(root).join("target/release/herdr-git"))
            .or_else(|| std::env::current_exe().ok());
        if self.update.executable.is_none() {
            self.shell.error = Some("Could not locate the executable to restart".into());
            return;
        }
        let (tx, rx) = mpsc::channel();
        let install_tag = tag.clone();
        match thread::Builder::new()
            .name("herdr-git-update-install".into())
            .spawn(move || {
                let _ = tx.send(UpdateResult::Installed(herdr::install_update(&install_tag)));
            }) {
            Ok(_) => {
                self.update.status = UpdateStatus::Installing(tag);
                self.update.pending = Some(rx);
            }
            Err(error) => self.shell.error = Some(format!("Could not start update: {error}")),
        }
    }

    pub(super) fn skip_update(&mut self) {
        if !self.update.offers_update() {
            return;
        }
        let UpdateStatus::Available(tag) = &self.update.status else {
            return;
        };
        let tag = tag.clone();
        self.update.skipped = Some(tag.clone());
        let _ = thread::Builder::new()
            .name("herdr-git-skip-version".into())
            .spawn(move || herdr::save_skipped_version(&tag));
    }

    pub(super) fn handle_update_confirmation(&mut self, input: &Event) {
        let Overlay::Update { tag, buttons } = &self.overlay else {
            return;
        };
        let action = match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => Some(ConfirmButton::Primary),
                KeyCode::Esc => Some(ConfirmButton::Secondary),
                _ => None,
            },
            _ => left_click(input).and_then(|pointer| buttons.hit(pointer)),
        };
        let tag = tag.clone();
        match action {
            Some(ConfirmButton::Primary) => {
                self.overlay = Overlay::None;
                self.start_update_install(tag);
            }
            Some(ConfirmButton::Secondary) => self.overlay = Overlay::None,
            None => {}
        }
    }

    pub(super) fn draw_update_confirmation(
        &self,
        frame: &mut Frame<'_>,
        tag: &str,
        buttons: &mut ConfirmButtons,
    ) {
        let inner = widgets::dialog_frame(frame, "Update Herdr Git", theme::DIALOG_MEDIUM, 11);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(inner);
        let target = if tag.is_empty() {
            "the latest default branch"
        } else {
            tag
        };
        let text = format!(
            "Reinstall Herdr Git from {target}?\n\nHerdr CLI will download and build the plugin. Restart the Git pane after installation to apply the update."
        );
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), regions[0]);
        *buttons = widgets::dialog_footer(
            frame,
            regions[1],
            ["[Enter] Update", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }

    pub(super) fn draw_update(&mut self, frame: &mut Frame<'_>, row: Rect) {
        let label = self.update.label();
        self.update.skip_area = Rect::default();
        let available_width = row.width.saturating_sub(41);
        let skip_label = " Skip this version ";
        let skip_width = if self.update.offers_update()
            && available_width > label.chars().count() as u16 + skip_label.len() as u16
        {
            skip_label.len() as u16 + 1
        } else {
            0
        };
        let width = (label.chars().count() as u16).min(available_width.saturating_sub(skip_width));
        self.update.area = Rect::new(row.right().saturating_sub(width), row.y, width, row.height);
        let hovered = self
            .shell
            .mouse_position
            .is_some_and(|point| self.update.area.contains(point.into()));
        let style = if self.update.enabled() {
            theme::hover(
                Style::default()
                    .fg(theme::TEXT_INVERSE)
                    .bg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
                hovered,
            )
        } else {
            theme::hint().bg(theme::SURFACE_INERT)
        };
        frame.render_widget(Paragraph::new(label).style(style), self.update.area);
        if skip_width > 0 {
            self.update.skip_area = Rect::new(
                self.update.area.x - skip_width,
                row.y,
                skip_width - 1,
                row.height,
            );
            let hovered = self
                .shell
                .mouse_position
                .is_some_and(|point| self.update.skip_area.contains(point.into()));
            frame.render_widget(
                Paragraph::new(skip_label).style(theme::hover(
                    theme::hint().bg(theme::SURFACE_INERT),
                    hovered,
                )),
                self.update.skip_area,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::{UpdateResult, UpdateStatus, mpsc};
    use crate::ui::commands::{CommandId, CommandPalette};
    use crate::ui::overlay::Overlay;
    use crate::ui::test_support::{find_text_in_row, offline_app, render};
    use crate::ui::theme;

    #[test]
    fn commands_show_available_updates_in_the_last_row_even_when_skipped() {
        let mut app = offline_app();
        app.open_commands();
        for skipped in [None, Some("v1.0.0".into())] {
            app.update.skipped = skipped;
            app.update.status = UpdateStatus::Available("v1.0.0".into());
            let buffer = render(&mut app, 120, 40);
            let Overlay::Commands(palette) = &app.overlay else {
                panic!("Commands");
            };
            assert_eq!(palette.rows.last(), Some(&CommandId::Update));
            let y = palette.list_area.y + palette.rows.len() as u16 - 1;
            assert!(find_text_in_row(&buffer, y, "Update Herdr Git…").is_some());
            assert!(find_text_in_row(&buffer, y, "Update available").is_some());
        }
        for status in [
            UpdateStatus::Current,
            UpdateStatus::Installing("v1.0.0".into()),
            UpdateStatus::Ready,
        ] {
            app.update.status = status;
            let buffer = render(&mut app, 120, 40);
            let Overlay::Commands(palette) = &app.overlay else {
                panic!("Commands");
            };
            let y = palette.list_area.y + palette.rows.len() as u16 - 1;
            assert!(find_text_in_row(&buffer, y, "Update available").is_none());
        }
    }

    #[test]
    fn skipping_hides_the_offer_but_commands_still_confirm_that_version() {
        let mut app = offline_app();
        app.update.status = UpdateStatus::Available("v1.0.0".into());
        let buffer = render(&mut app, 120, 16);
        assert!(find_text_in_row(&buffer, app.update.area.y, "Skip this version").is_some());
        app.update.skipped = Some("v1.0.0".into());
        let buffer = render(&mut app, 120, 16);
        assert!(!app.update.enabled());
        assert_eq!(app.update.skip_area.width, 0);
        assert!(find_text_in_row(&buffer, app.update.area.y, "Update available").is_none());
        app.activate_update();
        assert!(matches!(app.overlay, Overlay::None));
        app.dispatch_command(CommandId::Update);
        assert!(matches!(&app.overlay, Overlay::Update { tag, .. } if tag == "v1.0.0"));
        assert!(app.update.pending.is_none());
        app.handle_update_confirmation(&Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.update.pending.is_none());
        app.update.status = UpdateStatus::Available("v1.1.0".into());
        assert!(app.update.enabled());
    }

    #[test]
    fn both_the_offer_and_manual_reinstall_require_confirmation() {
        let mut app = offline_app();
        app.update.status = UpdateStatus::Available("v1.0.0".into());
        app.activate_update();
        assert!(matches!(&app.overlay, Overlay::Update { tag, .. } if tag == "v1.0.0"));
        assert!(app.update.pending.is_none());
        let buffer = render(&mut app, 120, 24);
        assert!(crate::ui::test_support::find_text(&buffer, "[Enter] Update").is_some());
        app.update.status = UpdateStatus::Current;
        app.dispatch_command(CommandId::Update);
        assert!(matches!(&app.overlay, Overlay::Update { tag, .. } if tag.is_empty()));
        assert!(app.update.pending.is_none());
        let mut palette = CommandPalette::new();
        palette.query.text = "update".into();
        palette.filter();
        assert_eq!(palette.rows, vec![CommandId::Update]);
    }

    #[test]
    fn failed_checks_are_silent_and_current_version_is_inert() {
        let mut app = offline_app();
        let (tx, rx) = mpsc::channel();
        app.update.pending = Some(rx);
        tx.send(UpdateResult::Checked(None)).unwrap();
        assert!(!app.poll_update());
        assert!(app.shell.error.is_none());
        assert_eq!(app.update.status, UpdateStatus::Current);
        assert!(!app.update.enabled());
        app.activate_update();
        assert!(app.update.pending.is_none());
        assert!(!app.update.restart_requested);
        let buffer = render(&mut app, 120, 16);
        let area = app.update.area;
        assert_eq!(area.right(), app.shell.tab_area.right());
        assert!(
            find_text_in_row(&buffer, area.y, &format!("v{}", env!("CARGO_PKG_VERSION"))).is_some()
        );
        assert_eq!(buffer[(area.x, area.y)].bg, theme::SURFACE_INERT);
    }

    #[test]
    fn installation_must_complete_before_the_button_can_restart() {
        let mut app = offline_app();
        app.update.status = UpdateStatus::Installing("v1.0.0".into());
        app.activate_update();
        assert!(!app.update.restart_requested);
        assert!(!app.update.enabled());
        let (tx, rx) = mpsc::channel();
        app.update.pending = Some(rx);
        tx.send(UpdateResult::Installed(Ok(()))).unwrap();
        assert!(app.poll_update());
        assert_eq!(app.update.status, UpdateStatus::Ready);
        assert!(app.update.enabled());
        let buffer = render(&mut app, 120, 16);
        let area = app.update.area;
        assert!(find_text_in_row(&buffer, area.y, "Restart to update").is_some());
        assert_eq!(buffer[(area.x, area.y)].bg, theme::ACCENT);
        assert!(app.handle_shell_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.update.restart_requested);
    }

    #[test]
    fn failed_installations_allow_retry_and_never_offer_restart() {
        let mut app = offline_app();
        app.update.status = UpdateStatus::Installing("v1.0.0".into());
        let (tx, rx) = mpsc::channel();
        app.update.pending = Some(rx);
        tx.send(UpdateResult::Installed(Err("build failed".into())))
            .unwrap();
        assert!(app.poll_update());
        assert_eq!(app.update.status, UpdateStatus::Available("v1.0.0".into()));
        assert_eq!(app.shell.error.as_deref(), Some("build failed"));
        assert!(!app.update.restart_requested);
        assert!(app.update.enabled());
    }
}
