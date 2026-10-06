use std::thread;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::shell::{ActiveTab, PaneFocus};
use super::update::{CheckOutcome, UpdateStatus};
use super::widgets::{self, truncate_to_width};
use super::{App, theme};
use crate::herdr;

const LABEL_WIDTH: usize = 20;
const ROW_HEIGHT: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SettingsRow {
    KittyGraphics,
    Versions,
}

const ROWS: [SettingsRow; 2] = [SettingsRow::KittyGraphics, SettingsRow::Versions];

#[derive(Default)]
pub(super) struct SettingsState {
    pub(super) selected: usize,
    row_areas: Vec<(SettingsRow, Rect)>,
}

impl SettingsState {
    fn row_at(&self, pointer: (u16, u16)) -> Option<usize> {
        self.row_areas
            .iter()
            .position(|(_, area)| area.contains(pointer.into()))
    }
}

impl App {
    pub(super) fn handle_settings_key(&mut self, key: KeyEvent) -> bool {
        if self.shell.active_tab != ActiveTab::Settings {
            return false;
        }
        let last = ROWS.len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings.selected = self.settings.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings.selected = (self.settings.selected + 1).min(last);
            }
            KeyCode::Home => self.settings.selected = 0,
            KeyCode::End => self.settings.selected = last,
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate_setting(ROWS[self.settings.selected])
            }
            _ => return false,
        }
        true
    }

    pub(super) fn handle_settings_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.shell.active_tab != ActiveTab::Settings
            || mouse.kind != MouseEventKind::Down(MouseButton::Left)
        {
            return false;
        }
        let Some(index) = self.settings.row_at((mouse.column, mouse.row)) else {
            return false;
        };
        self.focus = PaneFocus::Settings;
        self.settings.selected = index;
        self.activate_setting(ROWS[index]);
        true
    }

    fn activate_setting(&mut self, row: SettingsRow) {
        match row {
            SettingsRow::KittyGraphics => self.toggle_pane_graphics(),
            SettingsRow::Versions => match self.update.status {
                _ if self.update.checking() => {}
                UpdateStatus::Installing(_) => {}
                UpdateStatus::Available(_) | UpdateStatus::Ready => {
                    self.open_update_confirmation();
                }
                UpdateStatus::Current => self.check_for_update(),
            },
        }
    }

    fn toggle_pane_graphics(&mut self) {
        let enabled = !self.curves.enabled();
        self.curves.set_enabled(enabled);
        let _ = thread::Builder::new()
            .name("herdr-git-save-pane-graphics".into())
            .spawn(move || herdr::save_pane_graphics_enabled(enabled));
    }

    pub(super) fn draw_settings(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = widgets::pane_block("Settings", self.focus == PaneFocus::Settings);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.settings.row_areas.clear();
        let width = usize::from(inner.width);
        let mut y = inner.y.saturating_add(1);
        for (index, row) in ROWS.into_iter().enumerate() {
            if y.saturating_add(ROW_HEIGHT) > inner.bottom() {
                break;
            }
            let row_area = Rect::new(inner.x, y, inner.width, ROW_HEIGHT);
            let hovered = self
                .shell
                .mouse_position
                .is_some_and(|pointer| row_area.contains(pointer.into()));
            let style = if index == self.settings.selected {
                theme::selection_row().remove_modifier(Modifier::BOLD)
            } else {
                theme::hover(Style::default(), hovered)
            };
            let (label, control, status, description) = self.setting_parts(row);
            let mut title = vec![Span::styled(
                format!("  {label:<width$}", width = LABEL_WIDTH - 2),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            title.push(control);
            title.push(Span::raw("  "));
            title.push(status);
            let detail = Span::styled(
                truncate_to_width(&format!("{:LABEL_WIDTH$}{description}", ""), width),
                theme::secondary(),
            );
            frame.render_widget(
                Paragraph::new(Line::from(title)).style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
            frame.render_widget(
                Paragraph::new(Line::from(detail)).style(style),
                Rect::new(inner.x, y + 1, inner.width, 1),
            );
            self.settings.row_areas.push((row, row_area));
            y = y.saturating_add(ROW_HEIGHT + 1);
        }
    }

    fn setting_parts(
        &self,
        row: SettingsRow,
    ) -> (&'static str, Span<'static>, Span<'static>, String) {
        let action = Style::default()
            .fg(theme::TEXT_INVERSE)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD);
        let inert = theme::hint().bg(theme::SURFACE_INERT);
        match row {
            SettingsRow::KittyGraphics => {
                let control = if self.curves.enabled() {
                    Span::styled(" On  ", action)
                } else {
                    Span::styled(" Off ", inert)
                };
                let status = if !self.curves.enabled() {
                    Span::styled("Box-drawing characters in use", theme::hint())
                } else if self.curves.connected() {
                    Span::styled("Active in this pane", theme::hint())
                } else {
                    Span::styled("Unavailable in this pane", theme::warning_text())
                };
                (
                    "Kitty graphics",
                    control,
                    status,
                    "Antialiased graph curves and commit rules. Needs kitty_graphics = true under [terminal] in ~/.config/herdr/config.toml".to_owned(),
                )
            }
            SettingsRow::Versions => {
                let version = format!("v{}", env!("CARGO_PKG_VERSION"));
                let (control, status) = match &self.update.status {
                    _ if self.update.checking() => (
                        Span::styled(" Checking… ", inert),
                        Span::styled(format!("Current version {version}"), theme::hint()),
                    ),
                    UpdateStatus::Installing(tag) => (
                        Span::styled(" Updating… ", inert),
                        Span::styled(format!("Installing {tag}"), theme::hint()),
                    ),
                    UpdateStatus::Ready => (
                        Span::styled(" Restart to update ", action),
                        Span::styled("Update installed", theme::hint()),
                    ),
                    UpdateStatus::Available(tag) => (
                        Span::styled(format!(" Install {tag} "), action),
                        Span::styled(format!("{tag} is available"), theme::hint()),
                    ),
                    UpdateStatus::Current => (
                        Span::styled(" Check for updates ", inert),
                        match &self.update.checked {
                            Some(CheckOutcome::UpToDate) => Span::styled(
                                format!("{version} is the latest version"),
                                theme::hint(),
                            ),
                            Some(CheckOutcome::Failed(error)) => {
                                Span::styled(error.clone(), theme::error_text())
                            }
                            None => {
                                Span::styled(format!("Current version {version}"), theme::hint())
                            }
                        },
                    ),
                };
                (
                    "Versions",
                    control,
                    status,
                    "Checks GitHub releases for a newer Herdr Git".to_owned(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::ui::overlay::Overlay;
    use crate::ui::test_support::{find_text, offline_app, press, render};

    fn open_settings() -> App {
        let mut app = offline_app();
        assert!(app.handle_shortcut(KeyEvent::new(KeyCode::Char(','), KeyModifiers::ALT)));
        assert_eq!(app.shell.active_tab, ActiveTab::Settings);
        assert_eq!(app.focus, PaneFocus::Settings);
        app
    }

    #[test]
    fn alt_comma_opens_settings_as_the_fifth_header_item() {
        let mut app = open_settings();
        let buffer = render(&mut app, 120, 30);
        let (column, row) = find_text(&buffer, "Settings").expect("header item");
        assert_eq!(row, app.shell.tab_area.y);
        assert_eq!(
            super::super::shell::header_action_at(column - app.shell.tab_area.x),
            Some(super::super::shell::HeaderAction::Settings)
        );
        assert!(find_text(&buffer, "Kitty graphics").is_some());
        assert!(find_text(&buffer, "Check for updates").is_some());
    }

    #[test]
    fn kitty_graphics_toggles_rendering_without_touching_the_herdr_config() {
        let mut app = open_settings();
        assert!(app.curves.enabled());
        let buffer = render(&mut app, 160, 30);
        assert!(find_text(&buffer, " On  ").is_some());
        assert!(find_text(&buffer, "Unavailable in this pane").is_some());
        assert!(find_text(&buffer, "kitty_graphics = true under [terminal]").is_some());

        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(!app.curves.enabled());
        let buffer = render(&mut app, 160, 30);
        assert!(find_text(&buffer, " Off ").is_some());
        assert!(find_text(&buffer, "Box-drawing characters in use").is_some());

        press(&mut app, KeyCode::Char(' '));
        assert!(app.curves.enabled());
    }

    #[test]
    fn the_update_row_reports_checks_and_offers_found_versions() {
        let mut app = open_settings();
        press(&mut app, KeyCode::Down);
        assert_eq!(app.settings.selected, 1);

        let (_tx, rx) = std::sync::mpsc::channel();
        app.update.pending = Some(rx);
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, "Checking…").is_some());
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        app.update.pending = None;

        app.update.checked = Some(CheckOutcome::UpToDate);
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, "is the latest version").is_some());
        app.update.checked = Some(CheckOutcome::Failed("offline".into()));
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, "offline").is_some());

        app.update.status = UpdateStatus::Available("v9.0.0".into());
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, "Install v9.0.0").is_some());
        let row = app.settings.row_areas[1].1;
        assert!(app.handle_settings_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x + 2,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(matches!(&app.overlay, Overlay::Update { tag, .. } if tag == "v9.0.0"));
    }
}
