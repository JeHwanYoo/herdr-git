use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::git::{ChangesComparison, DiffTarget, ResetTarget};

use super::commands::{ActionCell, draw_action_bar};
use super::effect::{ForegroundRequest, RefreshScope, RequestId};
use super::overlay::Overlay;
use super::shell::ActiveTab;
use super::widgets::{
    self, ConfirmButton, ConfirmButtons, TextEdit, TextField, left_click, truncate_to_width,
};
use super::{App, theme};

const COMPARISON_TITLE_MAX_WIDTH: usize = 32;
const COMPARISON_SHORTCUTS: [char; 2] = ['d', 'm'];

pub(super) struct ComparisonState {
    pub mode: Option<ChangesComparison>,
    pub base: String,
    pub compare: String,
    pub areas: [Rect; 2],
    pub commit_titles: Vec<(String, String)>,
}
impl Default for ComparisonState {
    fn default() -> Self {
        Self {
            mode: Some(ChangesComparison::Last),
            base: "HEAD".into(),
            compare: String::new(),
            areas: [Rect::default(); 2],
            commit_titles: Vec::new(),
        }
    }
}
#[derive(Debug)]
pub(super) struct ComparisonDialog {
    base: TextField,
    compare: TextField,
    focused: usize,
    fields: [Rect; 2],
    buttons: ConfirmButtons,
    request: RequestId,
    targets: Vec<ResetTarget>,
    selected: Option<usize>,
    list: Rect,
    loading: bool,
    scroll: usize,
}
impl App {
    pub(super) fn open_comparison_dialog(&mut self) {
        let mut base = TextField::new();
        base.text = self.comparison.base.clone();
        let mut compare = TextField::new();
        compare.text = self.comparison.compare.clone();
        let id = self.foreground.next_id;
        self.overlay = Overlay::Comparison(ComparisonDialog {
            request: id,
            targets: Vec::new(),
            selected: None,
            list: Rect::default(),
            loading: true,
            scroll: 0,
            base,
            compare,
            focused: 1,
            fields: [Rect::default(); 2],
            buttons: ConfirmButtons::default(),
        });
        if let Err(error) = self
            .foreground
            .request(ForegroundRequest::ComparisonTargets {
                id,
                path: self.active_path.clone(),
            })
            && let Overlay::Comparison(dialog) = &mut self.overlay
        {
            dialog.loading = false;
            dialog.base.set_error(error);
        }
    }
    pub(super) fn apply_comparison_targets(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<Vec<ResetTarget>, String>,
    ) {
        if path != self.active_path {
            return;
        }
        if let Overlay::Comparison(dialog) = &mut self.overlay {
            if dialog.request != id {
                return;
            }
            dialog.loading = false;
            match result {
                Ok(targets) => dialog.targets = targets,
                Err(error) => dialog.base.set_error(error),
            }
        }
    }
    fn select_comparison(&mut self, index: usize) {
        if self.repository.is_none() {
            return;
        }
        if index == 1 {
            self.open_comparison_dialog();
            return;
        }
        self.comparison.mode = Some(ChangesComparison::Last);
        self.refresh_comparison();
    }
    fn refresh_comparison(&mut self) {
        self.clear_selection();
        self.diff.fold_toggles.clear();
        self.diff.diff_scroll = 0;
        self.diff.diff_horizontal_scroll = 0;
        self.diff.pending_diff = None;
        self.foreground.reads.diff.advance();
        self.request_background_refresh("Loading changes", RefreshScope::Changes);
    }
    pub(super) fn handle_comparison_controls(&mut self, input: &Event) -> bool {
        if self.shell.active_tab != ActiveTab::Changes {
            return false;
        }
        let index = match input {
            Event::Key(key)
                if key.kind == KeyEventKind::Press && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                match key.code {
                    KeyCode::Char('d' | 'D') => Some(0),
                    KeyCode::Char('m' | 'M') => Some(1),
                    _ => None,
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('∂') => Some(0),
                KeyCode::Char('µ') => Some(1),
                _ => None,
            },
            _ => left_click(input).and_then(|point| {
                self.comparison
                    .areas
                    .iter()
                    .position(|area| area.contains(point.into()))
            }),
        };
        if let Some(index) = index {
            self.select_comparison(index);
            true
        } else {
            false
        }
    }
    pub(super) fn comparison_info(&self, width: u16) -> Line<'static> {
        if matches!(self.diff.diff_target, DiffTarget::WorkingTreeAgainstIndex)
            && !self.ops.command_context.has_changes
        {
            return Line::from(Span::styled("—", theme::hint()));
        }
        let endpoint = |sha: &str| {
            let title = self
                .comparison
                .commit_titles
                .iter()
                .find(|(commit, _)| commit == sha)
                .map(|(_, title)| title.as_str())
                .unwrap_or_default();
            (sha.chars().take(7).collect::<String>(), title)
        };
        let ((before, _), (mut after, after_title), after_is_warning) = match &self.diff.diff_target
        {
            DiffTarget::CommitAgainstParent { commit, parent } => (
                parent
                    .as_deref()
                    .map(endpoint)
                    .unwrap_or_else(|| ("Empty".into(), "")),
                endpoint(commit),
                false,
            ),
            DiffTarget::WorkingTreeAgainstRevision { base } => {
                (endpoint(base), ("Uncommitted".into(), ""), true)
            }
            DiffTarget::WorkingTreeAgainstIndex => {
                (("Staged".into(), ""), ("Working Tree".into(), ""), false)
            }
            DiffTarget::IndexAgainstHead => (
                self.ops
                    .command_context
                    .head_commit
                    .as_deref()
                    .map(endpoint)
                    .unwrap_or_else(|| ("Empty".into(), "")),
                ("Staged".into(), ""),
                false,
            ),
        };
        let suffix = match &self.diff.diff_target {
            DiffTarget::WorkingTreeAgainstIndex => " (Uncommitted)",
            DiffTarget::CommitAgainstParent { .. } if self.ops.command_context.has_changes => {
                " · Uncommitted"
            }
            _ => "",
        };
        let available = usize::from(width)
            .saturating_sub(Line::from(format!("{before} → {after}{suffix}")).width());
        let title_width = available.saturating_sub(1).min(COMPARISON_TITLE_MAX_WIDTH);
        if !after_title.is_empty() && title_width > 0 {
            after.push(' ');
            after.push_str(&truncate_to_width(after_title, title_width));
        }
        Line::from(vec![
            Span::styled(format!("{before} → "), theme::hint()),
            Span::styled(
                after,
                if after_is_warning {
                    theme::warning_text()
                } else {
                    theme::hint()
                },
            ),
            Span::styled(suffix, theme::warning_text()),
        ])
    }

    pub(super) fn draw_comparison_controls(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.comparison.areas = [Rect::default(); 2];
        let active = match self.comparison.mode {
            Some(ChangesComparison::Between { .. }) => 1,
            _ => 0,
        };
        let cell_width = usize::from(area.width / 6);
        let labels = [("Diff Last", "Last"), ("Compare", "Compare")].map(|(full, compact)| {
            if cell_width >= full.len() + 4 {
                full
            } else {
                compact
            }
        });
        let actions = labels
            .into_iter()
            .enumerate()
            .map(|(index, label)| ActionCell {
                label,
                shortcut: COMPARISON_SHORTCUTS[index],
                icon: if active == index { "●" } else { "○" },
                enabled: true,
                selected: active == index,
                running: None,
            })
            .collect::<Vec<_>>();
        let cells = draw_action_bar(
            frame,
            area,
            &actions,
            self.shell.shortcut_hints,
            self.shell.mouse_position,
        );
        self.comparison.areas.copy_from_slice(&cells[..2]);
    }

    pub(super) fn handle_comparison_dialog(&mut self, input: &Event) {
        let Overlay::Comparison(dialog) = &mut self.overlay else {
            return;
        };
        let mut apply = false;
        let mut choose = None;
        if let Some(point) = left_click(input) {
            if let Some(index) = dialog.fields.iter().position(|r| r.contains(point.into())) {
                dialog.focused = index;
            }
            if dialog.list.contains(point.into()) {
                choose = Some(dialog.scroll + usize::from(point.1 - dialog.list.y));
            }
            match dialog.buttons.hit(point) {
                Some(ConfirmButton::Primary) => apply = true,
                Some(ConfirmButton::Secondary) => {
                    self.overlay = Overlay::None;
                    return;
                }
                _ => {}
            }
        }
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    return;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    dialog.focused = 1 - dialog.focused;
                    dialog.selected = None;
                    dialog.scroll = 0;
                }
                KeyCode::PageDown => {
                    let len = dialog.filtered().len();
                    if len > 0 {
                        dialog.selected = Some(
                            (dialog.selected.unwrap_or(0) + dialog.list.height as usize)
                                .min(len - 1),
                        );
                    }
                }
                KeyCode::PageUp => {
                    dialog.selected = Some(
                        dialog
                            .selected
                            .unwrap_or(0)
                            .saturating_sub(dialog.list.height as usize),
                    );
                }
                KeyCode::Home => dialog.selected = Some(0),
                KeyCode::End => dialog.selected = dialog.filtered().len().checked_sub(1),
                KeyCode::Down => {
                    let len = dialog.filtered().len();
                    if len > 0 {
                        dialog.selected = Some(dialog.selected.map_or(0, |n| (n + 1).min(len - 1)));
                    }
                }
                KeyCode::Up => {
                    dialog.selected = Some(dialog.selected.unwrap_or(0).saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(index) = dialog.selected {
                        choose = Some(index);
                    } else {
                        apply = true;
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if dialog.focused == 0 {
                        dialog.base.text.clear();
                    } else {
                        dialog.compare.text.clear();
                    }
                }
                KeyCode::Backspace => {
                    let field = if dialog.focused == 0 {
                        &mut dialog.base
                    } else {
                        &mut dialog.compare
                    };
                    field.edit(TextEdit::Backspace);
                    dialog.selected = None;
                    dialog.scroll = 0;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                {
                    let field = if dialog.focused == 0 {
                        &mut dialog.base
                    } else {
                        &mut dialog.compare
                    };
                    field.edit(TextEdit::Insert(c));
                    dialog.selected = None;
                    dialog.scroll = 0;
                }
                _ => {}
            },
            Event::Paste(text) => {
                let field = if dialog.focused == 0 {
                    &mut dialog.base
                } else {
                    &mut dialog.compare
                };
                field.edit(TextEdit::Paste(text.replace(['\n', '\r'], "")));
                dialog.selected = None;
                dialog.scroll = 0;
            }
            _ => {}
        }
        if let Event::Mouse(mouse) = input
            && dialog.list.contains((mouse.column, mouse.row).into())
        {
            let len = dialog.filtered().len();
            if len > 0 {
                match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollDown => {
                        dialog.selected = Some((dialog.selected.unwrap_or(0) + 1).min(len - 1))
                    }
                    crossterm::event::MouseEventKind::ScrollUp => {
                        dialog.selected = Some(dialog.selected.unwrap_or(0).saturating_sub(1))
                    }
                    _ => {}
                }
            }
        }
        if let Some(index) = choose
            && let Some(reference) = dialog.filtered().get(index).map(|t| t.reference.clone())
        {
            if dialog.focused == 0 {
                dialog.base.text = reference;
            } else {
                dialog.compare.text = reference;
            }
            dialog.selected = None;
            dialog.focused = 1 - dialog.focused;
        }
        if apply {
            if dialog.base.text.trim().is_empty() {
                dialog.base.set_error("Choose a branch or commit.");
                return;
            }
            if dialog.compare.text.trim().is_empty() {
                dialog.compare.set_error("Choose a branch or commit.");
                return;
            }
            self.comparison.base = dialog.base.text.trim().into();
            self.comparison.compare = dialog.compare.text.trim().into();
            self.comparison.mode = Some(ChangesComparison::Between {
                base: self.comparison.base.clone(),
                compare: self.comparison.compare.clone(),
            });
            self.overlay = Overlay::None;
            self.refresh_comparison();
        }
    }
    pub(super) fn draw_comparison_dialog(
        &self,
        frame: &mut Frame<'_>,
        dialog: &mut ComparisonDialog,
    ) {
        let inner = widgets::dialog_frame(frame, "Compare", theme::DIALOG_MEDIUM, 20);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        dialog.fields = [regions[0], regions[1]];
        for (i, field) in [&dialog.base, &dialog.compare].iter().enumerate() {
            let mut text = vec![Span::raw(field.text.clone())];
            if dialog.focused == i {
                text.push(theme::cursor_span(field.cursor_started.elapsed()));
            }
            frame.render_widget(
                Paragraph::new(Line::from(text)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(if i == 0 { "Base" } else { "Compare" })
                        .border_style(if dialog.focused == i {
                            theme::accent()
                        } else {
                            theme::hint()
                        }),
                ),
                regions[i],
            );
        }
        dialog.list = regions[2];
        dialog.scroll = widgets::viewport_offset(
            dialog.scroll,
            dialog.selected.unwrap_or(0),
            dialog.filtered().len(),
            regions[2].height as usize,
        );
        for (index, target) in dialog
            .filtered()
            .iter()
            .enumerate()
            .skip(dialog.scroll)
            .take(regions[2].height as usize)
        {
            frame.render_widget(
                Paragraph::new(format!(
                    "{} {}",
                    if dialog.selected == Some(index) {
                        "▶"
                    } else {
                        " "
                    },
                    target.name
                ))
                .style(if dialog.selected == Some(index) {
                    theme::accent_bold()
                } else {
                    theme::hint()
                }),
                Rect::new(
                    regions[2].x,
                    regions[2].y + (index - dialog.scroll) as u16,
                    regions[2].width,
                    1,
                ),
            );
        }
        if dialog.loading {
            frame.render_widget(
                Paragraph::new("Loading branches").style(theme::hint()),
                regions[3],
            );
        }
        if let Some(error) = dialog.base.error.as_ref().or(dialog.compare.error.as_ref()) {
            frame.render_widget(
                Paragraph::new(error.as_str()).style(theme::error_text()),
                regions[3],
            );
        }
        dialog.buttons = widgets::dialog_footer(
            frame,
            regions[4],
            ["[Enter] Compare", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }
}

impl ComparisonDialog {
    fn filtered(&self) -> Vec<&ResetTarget> {
        let query = if self.focused == 0 {
            &self.base.text
        } else {
            &self.compare.text
        };
        let query = query.to_lowercase();
        self.targets
            .iter()
            .filter(|target| {
                target.name.to_lowercase().contains(&query)
                    || target.reference.to_lowercase().contains(&query)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;

    use crate::ui::test_support::{buffer_text, intercept_foreground, offline_app, render};

    use super::*;

    #[test]
    fn comparison_modes_refresh_real_files_and_last_survives_a_missing_reflog() {
        use crate::git::Repository;
        use crate::ui::test_support::{git, temp_repo, wait_for_diff, wait_for_refresh};
        use std::fs;
        let root = temp_repo("comparison-ui");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(&root, &["switch", "-c", "feature"]);
        fs::write(root.join("one.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "One"]);
        fs::write(root.join("two.txt"), "two\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Two"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        assert_eq!(app.files.changes.len(), 1);
        assert_eq!(app.files.changes[0].path, "two.txt");
        let info = app.comparison_info(120).to_string();
        assert!(!info.contains("One") && info.ends_with("Two"), "{info}");
        let screen = buffer_text(&render(&mut app, 120, 30));
        assert!(
            screen.contains("Diff Last") && screen.contains("Compare"),
            "{screen}"
        );
        assert!(!screen.contains("Parent"), "{screen}");
        let area = app.comparison.areas[1];
        app.handle(Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::Comparison(_)));
        app.handle_comparison_dialog(&Event::Paste("main".into()));
        app.handle_comparison_dialog(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        wait_for_refresh(&mut app);
        assert_eq!(app.files.changes.len(), 2);
        assert!(app.diff.diff_text.contains("-two"));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        wait_for_refresh(&mut app);
        assert_eq!(app.files.changes[0].path, "two.txt");
        fs::write(root.join("three.txt"), "three\n").unwrap();
        app.select_comparison(0);
        wait_for_refresh(&mut app);
        assert_eq!(app.files.changes.len(), 1);
        assert_eq!(app.files.changes[0].path, "three.txt");
        let target = app.diff.diff_target.clone();
        app.select_tree(1);
        wait_for_diff(&mut app);
        assert_eq!(app.diff.diff_target, target);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Three"]);
        git(&root, &["reflog", "expire", "--expire=all", "--all"]);
        app.select_comparison(0);
        wait_for_refresh(&mut app);
        assert!(
            matches!(app.overlay, Overlay::None),
            "Last never opens a dialog"
        );
        assert!(app.shell.error.is_none());
        assert_eq!(app.files.changes.len(), 1);
        assert_eq!(app.files.changes[0].path, "three.txt");
        let reopened = App::load(Repository::discover(&root).unwrap()).unwrap();
        assert!(matches!(reopened.overlay, Overlay::None));
        assert_eq!(reopened.files.changes[0].path, "three.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_dialog_defaults_base_to_head_and_accepts_branches() {
        let mut app = offline_app();
        let (requests, _) = intercept_foreground(&mut app);
        app.open_comparison_dialog();
        assert!(matches!(
            requests.try_recv().unwrap(),
            ForegroundRequest::ComparisonTargets { .. }
        ));
        let text = buffer_text(&render(&mut app, 100, 30));
        assert!(text.contains("Compare") && text.contains("HEAD"));
        assert!(!text.contains("Diff Between"));
        app.handle_comparison_dialog(&Event::Paste("main".into()));
        app.handle_comparison_dialog(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.comparison.mode,
            Some(ChangesComparison::Between {
                base: "HEAD".into(),
                compare: "main".into()
            })
        );
        assert!(matches!(app.overlay, Overlay::None));
    }
    #[test]
    fn comparison_info_hides_previous_title_and_truncates_current_title() {
        let mut app = offline_app();
        app.diff.diff_target = DiffTarget::CommitAgainstParent {
            commit: "abcdef123456".into(),
            parent: Some("123456789abc".into()),
        };
        app.comparison.commit_titles = vec![
            ("123456789abc".into(), "첫 커밋".into()),
            (
                "abcdef123456".into(),
                "이 커밋 메시지는 헤더에 모두 표시하기에 너무 길다".into(),
            ),
        ];
        assert_eq!(
            app.comparison_info(120).to_string(),
            "1234567 → abcdef1 이 커밋 메시지는 헤더에 모두 표…"
        );
        app.ops.command_context.has_changes = true;
        let line = app.comparison_info(40);
        assert!(line.width() <= 40);
        let text = line.to_string();
        for part in ["1234567", "→", "abcdef1", "Uncommitted"] {
            assert!(text.contains(part), "{text}");
        }
        assert!(!text.contains("첫 커밋"), "{text}");
        app.diff.diff_target = DiffTarget::WorkingTreeAgainstRevision {
            base: "abcdef123456".into(),
        };
        assert_eq!(app.comparison_info(80).to_string(), "abcdef1 → Uncommitted");
    }

    #[test]
    fn comparison_info_shows_endpoints_and_uncommitted_independently() {
        use crate::git::DiffTarget;
        let mut app = offline_app();
        app.diff.diff_target = DiffTarget::CommitAgainstParent {
            commit: "abcdef123456".into(),
            parent: Some("123456789abc".into()),
        };
        assert_eq!(app.comparison_info(120).to_string(), "1234567 → abcdef1");
        app.ops.command_context.has_changes = true;
        assert_eq!(
            app.comparison_info(120).to_string(),
            "1234567 → abcdef1 · Uncommitted"
        );
        app.diff.diff_target = DiffTarget::WorkingTreeAgainstRevision {
            base: "abcdef123456".into(),
        };
        assert_eq!(
            app.comparison_info(120).to_string(),
            "abcdef1 → Uncommitted"
        );
        app.diff.diff_target = DiffTarget::CommitAgainstParent {
            commit: "abcdef123456".into(),
            parent: None,
        };
        assert_eq!(
            app.comparison_info(120).to_string(),
            "Empty → abcdef1 · Uncommitted"
        );
        app.diff.diff_target = DiffTarget::WorkingTreeAgainstIndex;
        assert_eq!(
            app.comparison_info(120).to_string(),
            "Staged → Working Tree (Uncommitted)"
        );
        app.diff.diff_target = DiffTarget::IndexAgainstHead;
        app.ops.command_context.head_commit = Some("abcdef123456".into());
        assert_eq!(app.comparison_info(120).to_string(), "abcdef1 → Staged");
    }

    #[test]
    fn comparison_info_uses_action_gap_without_moving_buttons() {
        use crate::git::DiffTarget;
        for (width, height) in [(40, 12), (120, 30)] {
            let mut app = offline_app();
            app.diff.diff_target = DiffTarget::CommitAgainstParent {
                commit: "abcdef123456".into(),
                parent: Some("123456789abc".into()),
            };
            app.ops.command_context.has_changes = true;
            let buffer = render(&mut app, width, height);
            let button = app.comparison.areas[0];
            let gap = u16::from(height > 13);
            let y = button.bottom() + gap;
            let row = (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            assert!(row.starts_with("1234567 → abcdef1 · Uncommitted"), "{row}");
            assert_eq!(app.diff.changes_body_area.y, y + 1 + gap);
        }
    }

    #[test]
    fn comparison_buttons_share_action_bar_surface_spacing_and_hover() {
        let mut app = offline_app();
        let buffer = render(&mut app, 120, 30);
        let areas = app.comparison.areas;
        for area in areas {
            assert_eq!(buffer[(area.x, area.y)].bg, theme::SURFACE_PANEL);
        }
        assert_eq!(areas[0].right() + 1, areas[1].x);
        assert!(areas[0].width.abs_diff(areas[1].width) <= 1);
        app.shell.mouse_position = Some((areas[1].x, areas[1].y));
        let buffer = render(&mut app, 120, 30);
        assert_eq!(buffer[(areas[1].x, areas[1].y)].bg, theme::SURFACE_HOVER);
    }

    #[test]
    fn comparison_buttons_match_graph_slots_and_leave_right_half_empty() {
        for (width, height) in [(40, 12), (80, 16), (121, 30), (180, 40)] {
            let mut app = offline_app();
            app.shell.active_tab = ActiveTab::History;
            render(&mut app, width, height);
            let graph_areas = app.graph.action_areas.clone();
            app.shell.active_tab = ActiveTab::Changes;
            app.shell.error = Some("Select a base branch".into());
            let buffer = render(&mut app, width, height);
            for (index, area) in app.comparison.areas.iter().enumerate() {
                assert_eq!(*area, graph_areas[index].1);
            }
            for (_, area) in &graph_areas[2..] {
                for x in area.x..area.right() {
                    assert_eq!(buffer[(x, area.y)].symbol(), " ");
                    assert_ne!(buffer[(x, area.y)].bg, theme::SURFACE_PANEL);
                }
                assert!(!app.handle_comparison_controls(&Event::Mouse(
                    crossterm::event::MouseEvent {
                        kind: crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left
                        ),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    }
                )));
            }
        }
    }

    #[test]
    fn comparison_labels_fit_their_buttons_when_resized() {
        let mut app = offline_app();
        for (width, labels) in [
            (60, ["Last", "Compare"]),
            (80, ["Diff Last", "Compare"]),
            (90, ["Diff Last", "Compare"]),
            (120, ["Diff Last", "Compare"]),
        ] {
            for hints in [false, true] {
                app.shell.shortcut_hints = hints;
                let buffer = render(&mut app, width, 30);
                for (area, label) in app.comparison.areas.iter().zip(labels) {
                    let text = (area.x..area.right())
                        .map(|x| buffer[(x, area.y)].symbol())
                        .collect::<String>();
                    assert!(text.contains(label), "width={width}, hints={hints}: {text}");
                }
            }
        }
    }

    #[test]
    fn comparison_controls_show_exactly_one_selected_mode() {
        let mut app = offline_app();
        for mode in [
            ChangesComparison::Last,
            ChangesComparison::Between {
                base: "HEAD".into(),
                compare: "main".into(),
            },
        ] {
            app.comparison.mode = Some(mode);
            let buffer = render(&mut app, 120, 30);
            let y = app.comparison.areas[0].y;
            let row = (0..120)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            assert_eq!(row.matches('●').count(), 1);
            assert_eq!(row.matches('○').count(), 1);
        }
    }
}
