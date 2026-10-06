use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::overlay::Overlay;
use super::shell::{ActiveTab, PaneFocus};
use super::update::{CheckOutcome, UpdateStatus};
use super::widgets::{self, ConfirmButton, ConfirmButtons, left_click, truncate_to_width};
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
const MISSING_CONFIG: &str = "Could not locate the Herdr config";

struct ConfigResult {
    saved: Option<Result<(), String>>,
    kitty_graphics: Result<Option<bool>, String>,
}

pub(super) struct SettingsState {
    pub(super) selected: usize,
    pub(super) config_path: Option<PathBuf>,
    kitty_graphics: Result<Option<bool>, String>,
    kitty_graphics_saved: bool,
    pending: Option<Receiver<ConfigResult>>,
    row_areas: Vec<(SettingsRow, Rect)>,
}

impl SettingsState {
    pub(super) fn new() -> Self {
        Self {
            selected: 0,
            config_path: herdr::config_path(),
            kitty_graphics: Ok(None),
            kitty_graphics_saved: false,
            pending: None,
            row_areas: Vec::new(),
        }
    }

    pub(super) fn reload(&mut self) {
        self.request(None);
    }

    fn request(&mut self, save: Option<bool>) {
        let Some(path) = self.config_path.clone() else {
            self.pending = None;
            self.kitty_graphics = Err(MISSING_CONFIG.to_owned());
            return;
        };
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("herdr-git-settings".into())
            .spawn(move || {
                let saved = save.map(|enable| herdr::set_kitty_graphics(&path, enable));
                let _ = tx.send(ConfigResult {
                    saved,
                    kitty_graphics: herdr::kitty_graphics(&path),
                });
            });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(error) => {
                self.pending = None;
                self.kitty_graphics = Err(format!("Could not read the Herdr config: {error}"));
            }
        }
    }

    fn kitty_graphics_enabled(&self) -> bool {
        matches!(self.kitty_graphics, Ok(Some(true) | None))
    }

    fn row_at(&self, pointer: (u16, u16)) -> Option<usize> {
        self.row_areas
            .iter()
            .position(|(_, area)| area.contains(pointer.into()))
    }
}

impl App {
    pub(super) fn poll_settings(&mut self) -> bool {
        let Some(rx) = &self.settings.pending else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => ConfigResult {
                saved: None,
                kitty_graphics: Err("Settings worker stopped before finishing".to_owned()),
            },
        };
        self.settings.pending = None;
        match result.saved {
            Some(Ok(())) => self.settings.kitty_graphics_saved = true,
            Some(Err(error)) => self.shell.error = Some(error),
            None => {}
        }
        self.settings.kitty_graphics = result.kitty_graphics;
        true
    }

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
            SettingsRow::KittyGraphics if self.settings.pending.is_some() => {}
            SettingsRow::KittyGraphics => match &self.settings.kitty_graphics {
                Ok(_) => {
                    self.overlay = Overlay::KittyGraphics {
                        enable: !self.settings.kitty_graphics_enabled(),
                        buttons: ConfirmButtons::default(),
                    };
                }
                Err(error) => self.shell.error = Some(error.clone()),
            },
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

    pub(super) fn handle_kitty_graphics_confirmation(&mut self, input: &Event) {
        let Overlay::KittyGraphics { enable, buttons } = &self.overlay else {
            return;
        };
        let enable = *enable;
        let action = match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => Some(ConfirmButton::Primary),
                KeyCode::Esc => Some(ConfirmButton::Secondary),
                _ => None,
            },
            _ => left_click(input).and_then(|pointer| buttons.hit(pointer)),
        };
        match action {
            Some(ConfirmButton::Primary) => {
                self.overlay = Overlay::None;
                self.save_kitty_graphics(enable);
            }
            Some(ConfirmButton::Secondary) => self.overlay = Overlay::None,
            None => {}
        }
    }

    fn save_kitty_graphics(&mut self, enable: bool) {
        if self.settings.config_path.is_none() {
            self.shell.error = Some(MISSING_CONFIG.to_owned());
            return;
        }
        self.settings.request(Some(enable));
    }

    pub(super) fn draw_kitty_graphics_confirmation(
        &self,
        frame: &mut Frame<'_>,
        enable: bool,
        buttons: &mut ConfirmButtons,
    ) {
        let (verb, value) = if enable {
            ("on", "true")
        } else {
            ("off", "false")
        };
        let inner = widgets::dialog_frame(frame, "Kitty Graphics", theme::DIALOG_MEDIUM, 12);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(inner);
        let path = self
            .settings
            .config_path
            .as_deref()
            .map(display_path)
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(format!(
                "Turn Kitty graphics {verb}?\n\nThis sets kitty_graphics = {value} under [terminal] in {path}. Restart Herdr to apply it."
            ))
            .wrap(Wrap { trim: true }),
            regions[0],
        );
        *buttons = widgets::dialog_footer(
            frame,
            regions[1],
            [&format!("[Enter] Turn {verb}"), "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
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
                let control = if self.settings.pending.is_some() {
                    Span::styled("  …  ", inert)
                } else if self.settings.kitty_graphics_enabled() {
                    Span::styled(" On  ", action)
                } else {
                    Span::styled(" Off ", inert)
                };
                let status = match &self.settings.kitty_graphics {
                    _ if self.settings.pending.is_some() => {
                        Span::styled("Reading Herdr config…", theme::hint())
                    }
                    Err(error) => Span::styled(error.clone(), theme::error_text()),
                    Ok(_) if self.settings.kitty_graphics_saved => {
                        Span::styled("Restart Herdr to apply", theme::warning_text())
                    }
                    Ok(_) if self.curves.available() => {
                        Span::styled("Active in this pane", theme::hint())
                    }
                    Ok(_) => Span::styled("Not active in this pane", theme::hint()),
                };
                let path = self
                    .settings
                    .config_path
                    .as_deref()
                    .map(display_path)
                    .unwrap_or_default();
                (
                    "Kitty graphics",
                    control,
                    status,
                    format!("Antialiased graph curves and commit separators · {path}"),
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

fn display_path(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(relative) => format!("~/{}", relative.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::ui::test_support::{find_text, offline_app, press, render};

    fn temp_config(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("herdr-git-settings-{name}-{unique}"))
            .join("config.toml")
    }

    fn wait_for_settings(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.settings.pending.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "settings worker timed out"
            );
            app.poll_settings();
            std::thread::yield_now();
        }
    }

    fn open_settings(config: Option<PathBuf>) -> App {
        let mut app = offline_app();
        app.settings.config_path = config;
        assert!(app.handle_shortcut(KeyEvent::new(KeyCode::Char(','), KeyModifiers::ALT)));
        assert_eq!(app.shell.active_tab, ActiveTab::Settings);
        assert_eq!(app.focus, PaneFocus::Settings);
        wait_for_settings(&mut app);
        app
    }

    #[test]
    fn alt_comma_opens_settings_as_the_fifth_header_item() {
        let mut app = open_settings(None);
        let buffer = render(&mut app, 120, 30);
        let (column, row) = find_text(&buffer, "Settings").expect("header item");
        assert_eq!(row, app.shell.tab_area.y);
        assert_eq!(
            super::super::shell::header_action_at(column - app.shell.tab_area.x),
            Some(super::super::shell::HeaderAction::Settings)
        );
        assert!(find_text(&buffer, "Kitty graphics").is_some());
        assert!(find_text(&buffer, "Check for updates").is_some());
        assert!(find_text(&buffer, "Could not locate the Herdr config").is_some());
    }

    #[test]
    fn kitty_graphics_asks_before_writing_the_herdr_config() {
        let config = temp_config("kitty");
        let mut app = open_settings(Some(config.clone()));
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, " On  ").is_some());

        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.overlay,
            Overlay::KittyGraphics { enable: false, .. }
        ));
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, "Turn Kitty graphics off?").is_some());
        app.handle_kitty_graphics_confirmation(&Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(!config.exists());

        press(&mut app, KeyCode::Enter);
        app.handle_kitty_graphics_confirmation(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(matches!(app.overlay, Overlay::None));
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, "Reading Herdr config…").is_some());
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        wait_for_settings(&mut app);
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "[terminal]\nkitty_graphics = false\n"
        );
        let buffer = render(&mut app, 120, 30);
        assert!(find_text(&buffer, " Off ").is_some());
        assert!(find_text(&buffer, "Restart Herdr to apply").is_some());

        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.overlay,
            Overlay::KittyGraphics { enable: true, .. }
        ));
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_update_row_reports_checks_and_offers_found_versions() {
        let mut app = open_settings(None);
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
