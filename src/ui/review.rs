use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::git::{BlameInfo, DiffTarget, ReadError};
mod selection;

pub(super) use selection::{CodeSelection, ReviewSide};
use selection::{copy_selection_text, line_numbers_text, selected_code};

use super::diff::blame_revision;
use super::effect::{
    ForegroundRequest, LineHistoryTarget, ReadGeneration, RequestId, SelectionBlameTarget,
};
use super::overlay::Overlay;
use super::shell::{ActiveTab, PaneFocus};
use super::syntax::DiffDocument;
use super::widgets::{self, anchored, centered, left_click};
use super::{App, theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionSurface {
    Changes,
    Preview,
    File,
}

#[derive(Default)]
pub(super) struct Selection {
    pub(super) code_selection: Option<CodeSelection>,
    pub(super) visual_anchor: Option<usize>,
    pub(super) dragging_selection: bool,
    pub(super) keyboard_selecting: bool,
}

impl Selection {
    pub(super) fn is_selecting(&self) -> bool {
        self.visual_anchor.is_some() || self.keyboard_selecting || self.dragging_selection
    }

    fn is_active(&self) -> bool {
        self.is_selecting() || self.code_selection.is_some()
    }
}

pub(super) struct SelectionBlame {
    target: SelectionBlameTarget,
    read: Option<(RequestId, ReadGeneration)>,
    entries: Vec<BlameInfo>,
}

pub(super) struct ReviewState {
    pub(super) changes: Selection,
    pub(super) preview: Selection,
    pub(super) preview_cursor: (ReviewSide, usize),
    pub(super) file: Selection,
    pub(super) file_cursor: usize,
    blame: Option<SelectionBlame>,
}

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            changes: Selection::default(),
            preview: Selection::default(),
            preview_cursor: (ReviewSide::After, 0),
            file: Selection::default(),
            file_cursor: 0,
            blame: None,
        }
    }
}

impl ReviewState {
    pub(super) fn is_selecting(&self) -> bool {
        self.changes.is_selecting() || self.preview.is_selecting() || self.file.is_selecting()
    }

    pub(super) fn is_dragging(&self) -> bool {
        self.changes.dragging_selection
            || self.preview.dragging_selection
            || self.file.dragging_selection
    }
}

#[derive(Debug)]
pub(super) struct CopySelectionDialog {
    focused: usize,
    checked: [bool; 2],
    option_areas: [Rect; 2],
}

impl Default for CopySelectionDialog {
    fn default() -> Self {
        Self {
            focused: 0,
            checked: [true, true],
            option_areas: Default::default(),
        }
    }
}

const COPY_OPTIONS: [&str; 2] = ["Code", "Line"];

impl App {
    pub(super) fn selection_surface(&self) -> SelectionSurface {
        match self.shell.active_tab {
            ActiveTab::History => SelectionSurface::Preview,
            ActiveTab::Files => SelectionSurface::File,
            ActiveTab::Changes => SelectionSurface::Changes,
        }
    }

    pub(super) fn selection(&self, surface: SelectionSurface) -> &Selection {
        match surface {
            SelectionSurface::Changes => &self.review.changes,
            SelectionSurface::Preview => &self.review.preview,
            SelectionSurface::File => &self.review.file,
        }
    }

    fn selection_mut(&mut self, surface: SelectionSurface) -> &mut Selection {
        match surface {
            SelectionSurface::Changes => &mut self.review.changes,
            SelectionSurface::Preview => &mut self.review.preview,
            SelectionSurface::File => &mut self.review.file,
        }
    }

    fn surface_document(&self, surface: SelectionSurface) -> &DiffDocument {
        match surface {
            SelectionSurface::Changes => &self.diff.diff_document,
            SelectionSurface::Preview => &self.inspect.commit_preview_document,
            SelectionSurface::File => self.explorer.preview_document(),
        }
    }

    fn surface_path(&self, surface: SelectionSurface) -> Option<&str> {
        match surface {
            SelectionSurface::Changes => self
                .files
                .changes
                .get(self.files.change_selected)
                .map(|change| change.path.as_str()),
            SelectionSurface::Preview => self.inspect.commit_preview_loaded_path.as_deref(),
            SelectionSurface::File => self.explorer.preview_path(),
        }
    }

    fn surface_cursor(&self, surface: SelectionSurface) -> (ReviewSide, usize) {
        match surface {
            SelectionSurface::Changes => (self.diff.diff_side, self.diff.focused_diff_row),
            SelectionSurface::Preview => self.review.preview_cursor,
            SelectionSurface::File => (ReviewSide::After, self.review.file_cursor),
        }
    }

    pub(super) fn surface_source(
        &self,
        surface: SelectionSurface,
        side: ReviewSide,
        row: usize,
    ) -> Option<&str> {
        let document = self.surface_document(surface);
        match side {
            ReviewSide::Before => document.before_source(row),
            ReviewSide::After => document.after_source(row),
        }
    }

    fn surface_line_number(
        &self,
        surface: SelectionSurface,
        side: ReviewSide,
        row: usize,
    ) -> Option<usize> {
        let document = self.surface_document(surface);
        match side {
            ReviewSide::Before => document.before_line_number(row),
            ReviewSide::After => document.after_line_number(row),
        }
    }

    pub(super) fn effective_selection_on(
        &self,
        surface: SelectionSurface,
    ) -> Option<CodeSelection> {
        let state = self.selection(surface);
        let Some(anchor) = state.visual_anchor else {
            return state
                .code_selection
                .clone()
                .filter(|selection| !selection.is_empty());
        };
        let (side, cursor) = self.surface_cursor(surface);
        let mut selection = state
            .code_selection
            .clone()
            .filter(|selection| selection.side == side)
            .unwrap_or_else(|| CodeSelection::from_rows(side, []));
        for row in anchor.min(cursor)..=anchor.max(cursor) {
            if self.surface_source(surface, side, row).is_some() {
                selection.insert(row);
            }
        }
        (!selection.is_empty()).then_some(selection)
    }

    pub(super) fn begin_visual_selection(&mut self, side: ReviewSide, row: usize, keyboard: bool) {
        let surface = self.selection_surface();
        if self.surface_source(surface, side, row).is_none() {
            self.show_action_error("Focus a source line before starting visual selection.");
            return;
        }
        match surface {
            SelectionSurface::Changes => {
                self.diff.diff_side = side;
                self.diff.focused_diff_row = row;
            }
            SelectionSurface::Preview => self.review.preview_cursor = (side, row),
            SelectionSurface::File => self.review.file_cursor = row,
        }
        let state = self.selection_mut(surface);
        state.code_selection = None;
        state.visual_anchor = Some(row);
        state.keyboard_selecting = keyboard;
        state.dragging_selection = !keyboard;
    }

    pub(super) fn finish_visual_selection(&mut self) {
        let surface = self.selection_surface();
        let selection = self.effective_selection_on(surface);
        let state = self.selection_mut(surface);
        state.code_selection = selection;
        state.visual_anchor = None;
        state.keyboard_selecting = false;
        state.dragging_selection = false;
    }

    pub(super) fn open_copy_selection(&mut self) -> bool {
        let surface = self.selection_surface();
        if self.selection(surface).keyboard_selecting {
            self.finish_visual_selection();
        }
        if self.selection(surface).code_selection.is_none() {
            return false;
        }
        self.overlay = Overlay::CopySelection(CopySelectionDialog::default());
        true
    }

    pub(super) fn clear_selection(&mut self) {
        self.review.changes = Selection::default();
    }

    pub(super) fn clear_preview_selection(&mut self) {
        self.review.preview = Selection::default();
    }

    pub(super) fn clear_file_selection(&mut self) {
        self.review.file = Selection::default();
    }

    fn clear_active_selection(&mut self) {
        let surface = self.selection_surface();
        *self.selection_mut(surface) = Selection::default();
    }

    pub(super) fn selection_lines_on(
        &self,
        surface: SelectionSurface,
    ) -> Option<(String, ReviewSide, Vec<usize>)> {
        let selection = self.effective_selection_on(surface)?;
        let path = self.surface_path(surface)?.to_owned();
        let selected = selection
            .rows()
            .filter_map(|row| self.surface_line_number(surface, selection.side, row))
            .collect::<Vec<_>>();
        (!selected.is_empty()).then_some((path, selection.side, selected))
    }

    pub(super) fn selection_title(&self, surface: SelectionSurface) -> Option<Line<'static>> {
        let (path, side, lines) = self.selection_lines_on(surface)?;
        let name = path.rsplit('/').next().unwrap_or(&path);
        let side = match surface {
            SelectionSurface::File => String::new(),
            _ => format!("{} · ", side.label()),
        };
        Some(Line::from(vec![
            Span::raw(side),
            Span::styled(
                format!("{name}:{}", line_numbers_text(&lines)),
                theme::accent_bold(),
            ),
        ]))
    }

    pub(super) fn restore_selection(&mut self, saved: Option<(String, ReviewSide, Vec<usize>)>) {
        let Some((path, side, selected_lines)) = saved else {
            return;
        };
        if self.surface_path(SelectionSurface::Changes) != Some(path.as_str()) {
            self.clear_selection();
            return;
        }
        let rows = selected_lines
            .into_iter()
            .filter_map(|selected| self.row_for_line(side, selected))
            .collect::<Vec<_>>();
        self.review.changes.code_selection =
            (!rows.is_empty()).then(|| CodeSelection::from_rows(side, rows));
    }

    pub(super) fn handle_review_key(&mut self, key: KeyEvent) -> bool {
        let surface = self.selection_surface();
        let focus = match surface {
            SelectionSurface::Changes => PaneFocus::Diff,
            SelectionSurface::Preview => PaneFocus::Preview,
            SelectionSurface::File => PaneFocus::FilePreview,
        };
        if self.focus != focus {
            return false;
        }
        match key.code {
            KeyCode::Char('y') => return self.open_copy_selection(),
            KeyCode::Char('h') if self.effective_selection_on(surface).is_some() => {
                self.open_line_history();
            }
            KeyCode::Esc if self.selection(surface).is_active() => self.clear_active_selection(),
            _ => return false,
        }
        true
    }

    pub(super) fn line_history_target(&self) -> Option<(String, LineHistoryTarget)> {
        let surface = self.selection_surface();
        let selection = self.effective_selection_on(surface)?;
        let (file, _, shown) = self.selection_lines_on(surface)?;
        let target = match surface {
            SelectionSurface::Changes => Some(&self.diff.diff_target),
            SelectionSurface::Preview => {
                Some(&self.inspect.commit_preview_loaded_target.as_ref()?.target)
            }
            SelectionSurface::File => None,
        };
        let (side, revision) = match (target, selection.side) {
            (None, _) => (ReviewSide::Before, None),
            (Some(DiffTarget::CommitAgainstParent { commit, .. }), ReviewSide::After) => {
                (ReviewSide::After, Some(commit.clone()))
            }
            (Some(DiffTarget::CommitAgainstParent { parent, .. }), ReviewSide::Before) => {
                (ReviewSide::Before, Some(parent.clone()?))
            }
            (Some(DiffTarget::WorkingTreeAgainstRevision { base }), _) => {
                (ReviewSide::Before, Some(base.clone()))
            }
            (Some(DiffTarget::WorkingTreeAgainstIndex | DiffTarget::IndexAgainstHead), _) => {
                (ReviewSide::Before, None)
            }
        };
        let lines = selection
            .rows()
            .filter_map(|row| self.surface_line_number(surface, side, row))
            .collect::<Vec<_>>();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for line in lines {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == line => *end = line,
                _ => ranges.push((line, line)),
            }
        }
        if ranges.is_empty() {
            return None;
        }
        let name = file.rsplit('/').next().unwrap_or(&file).to_owned();
        Some((
            format!("{name}:{}", line_numbers_text(&shown)),
            LineHistoryTarget {
                path: self.repository.as_ref()?.root().to_owned(),
                file,
                ranges,
                revision,
            },
        ))
    }

    fn selection_blame_target(&self) -> Option<SelectionBlameTarget> {
        let surface = self.selection_surface();
        let state = self.selection(surface);
        if state.dragging_selection {
            return self.review.blame.as_ref().map(|blame| blame.target.clone());
        }
        let (file, side, lines) = self.selection_lines_on(surface)?;
        if side != ReviewSide::After {
            return None;
        }
        let revision = match surface {
            SelectionSurface::Changes => blame_revision(&self.diff.diff_target),
            SelectionSurface::Preview => self
                .inspect
                .commit_preview_loaded_target
                .as_ref()
                .and_then(|target| blame_revision(&target.target)),
            SelectionSurface::File => None,
        };
        Some(SelectionBlameTarget {
            path: self.repository.as_ref()?.root().to_owned(),
            file,
            lines,
            revision,
        })
    }

    pub(super) fn sync_selection_blame(&mut self) -> bool {
        let target = self.selection_blame_target();
        if self.review.blame.as_ref().map(|blame| &blame.target) == target.as_ref() {
            return false;
        }
        self.review.blame = None;
        self.foreground.reads.selection_blame.advance();
        let Some(target) = target else {
            return true;
        };
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.selection_blame.generation;
        let read = self
            .foreground
            .request(ForegroundRequest::SelectionBlame {
                id,
                generation,
                target: target.clone(),
            })
            .ok()
            .map(|id| (id, generation));
        self.review.blame = Some(SelectionBlame {
            target,
            read,
            entries: Vec::new(),
        });
        true
    }

    pub(super) fn apply_selection_blame(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        target: SelectionBlameTarget,
        result: Result<Vec<BlameInfo>, ReadError>,
    ) {
        let current = self.foreground.reads.selection_blame.is_current(generation);
        let Some(blame) = self.review.blame.as_mut() else {
            return;
        };
        if !current || blame.read != Some((id, generation)) || blame.target != target {
            return;
        }
        blame.read = None;
        if let Ok(entries) = result {
            blame.entries = entries;
        }
    }

    pub(super) fn selection_blame_status(&self) -> Option<Span<'static>> {
        let blame = self.review.blame.as_ref()?;
        if self.selection_blame_target().as_ref() != Some(&blame.target) {
            return None;
        }
        if blame.read.is_some() {
            return Some(Span::styled("Blame…", theme::hint()));
        }
        let last = blame.entries.iter().max_by_key(|entry| entry.line)?;
        let mut others = blame
            .entries
            .iter()
            .map(|entry| entry.author.as_str())
            .filter(|author| *author != last.author)
            .collect::<Vec<_>>();
        others.sort_unstable();
        others.dedup();
        let mut text = format!("Blame {}", selection_blame_text(last));
        match others.len() {
            0 => {}
            1 => text.push_str(" +1 author"),
            count => text.push_str(&format!(" +{count} authors")),
        }
        Some(Span::styled(text, theme::accent()))
    }

    pub(super) fn draw_selection_decorations(
        &self,
        frame: &mut Frame<'_>,
        surface: SelectionSurface,
        [before, after]: [Rect; 2],
        scroll: usize,
    ) {
        let Some(selection) = self.effective_selection_on(surface) else {
            return;
        };
        if !matches!(self.overlay, Overlay::None) {
            return;
        }
        let area = match selection.side {
            ReviewSide::Before => before,
            ReviewSide::After => after,
        };
        draw_selection_hint(frame, area, &selection, scroll);
    }

    pub(super) fn handle_copy_selection(&mut self, input: &Event) {
        let Overlay::CopySelection(dialog) = &mut self.overlay else {
            return;
        };
        let toggle = match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    return;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    dialog.focused = 0;
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    dialog.focused = 1;
                    return;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    dialog.focused = 1 - dialog.focused;
                    return;
                }
                KeyCode::Char(' ') => Some(dialog.focused),
                KeyCode::Char('1') => Some(0),
                KeyCode::Char('2') => Some(1),
                KeyCode::Enter | KeyCode::Char('y') => {
                    let [code, line] = dialog.checked;
                    if code || line {
                        self.copy_selected_lines(code, line);
                    } else {
                        self.show_action_error("Check Code or Line to copy.");
                    }
                    return;
                }
                _ => None,
            },
            _ => left_click(input).and_then(|pointer| {
                dialog
                    .option_areas
                    .iter()
                    .position(|area| area.contains(pointer.into()))
            }),
        };
        if let Some(index) = toggle {
            dialog.focused = index;
            dialog.checked[index] = !dialog.checked[index];
        }
    }

    fn copy_selected_lines(&mut self, include_code: bool, include_line: bool) {
        let surface = self.selection_surface();
        let Some(selection) = self.selection(surface).code_selection.as_ref() else {
            return;
        };
        let Some(path) = self.surface_path(surface) else {
            self.show_action_error("The selected file is unavailable.");
            return;
        };
        let document = self.surface_document(surface);
        let (numbers, sources): (Vec<_>, Vec<_>) = (0..document.len())
            .map(|row| match selection.side {
                ReviewSide::Before => (
                    document.before_line_number(row),
                    document.before_source(row).map(str::to_owned),
                ),
                ReviewSide::After => (
                    document.after_line_number(row),
                    document.after_source(row).map(str::to_owned),
                ),
            })
            .unzip();
        match selected_code(selection, &numbers, &sources) {
            Ok((lines, code)) => {
                self.pending_clipboard =
                    Some(super::PendingClipboard::Selection(copy_selection_text(
                        include_code,
                        include_line,
                        path,
                        (surface != SelectionSurface::File).then_some(selection.side),
                        &lines,
                        &code,
                    )));
                self.overlay = Overlay::None;
            }
            Err(error) => self.show_action_error(&error),
        }
    }

    pub(super) fn finish_copy_selection(&mut self, result: Result<(), String>) {
        let (success, message) = match result {
            Ok(()) => (true, "Selection copied to the clipboard.".to_owned()),
            Err(error) => (false, format!("Could not copy selection: {error}")),
        };
        self.clear_active_selection();
        self.show_result(super::commands::OperationResultView::named_message(
            "Copy Selection",
            success,
            &message,
        ));
    }

    pub(super) fn draw_copy_selection(
        &self,
        frame: &mut Frame<'_>,
        dialog: &mut CopySelectionDialog,
    ) {
        let area = centered(52, 9, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(widgets::pane_block(Line::raw("Copy Selection"), true), area);
        let inner = Rect::new(
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(4),
            area.height.saturating_sub(2),
        );
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);
        let context = self
            .selection_lines_on(self.selection_surface())
            .map(|(path, side, lines)| {
                format!(
                    "{path} · {} lines {}",
                    side.label(),
                    line_numbers_text(&lines)
                )
            })
            .unwrap_or_default();
        frame.render_widget(Paragraph::new(context).style(theme::hint()), rows[0]);
        for (index, label) in COPY_OPTIONS.iter().enumerate() {
            dialog.option_areas[index] = rows[index + 1];
            let style = if index == dialog.focused {
                theme::selection_row()
            } else {
                theme::hint()
            };
            let mark = if dialog.checked[index] { "x" } else { " " };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(format!("  [{mark}] ")),
                    Span::raw(*label),
                ]))
                .style(style),
                rows[index + 1],
            );
        }
        frame.render_widget(
            Paragraph::new("Space toggle · y copy · Esc close").style(theme::hint()),
            rows[4],
        );
    }
}

fn selection_blame_text(entry: &BlameInfo) -> String {
    if entry.sha.chars().all(|character| character == '0') {
        return "Uncommitted".to_owned();
    }
    format!(
        "{} · {} · {}",
        &entry.sha[..entry.sha.len().min(7)],
        entry.author,
        entry.summary
    )
}

fn draw_selection_hint(
    frame: &mut Frame<'_>,
    area: Rect,
    selection: &CodeSelection,
    scroll: usize,
) {
    let mut visible = selection
        .rows()
        .filter(|row| (scroll..scroll + area.height as usize).contains(row))
        .map(|row| area.y + (row - scroll) as u16);
    let Some(first) = visible.next() else {
        return;
    };
    let last = visible.last().unwrap_or(first);
    let keys = Line::from(vec![
        Span::styled("y", theme::accent_bold()),
        Span::styled(" Yank · ", theme::hint()),
        Span::styled("h", theme::accent_bold()),
        Span::styled(" History · ", theme::hint()),
        Span::styled("Esc", theme::accent_bold()),
        Span::styled(" Cancel", theme::hint()),
    ]);
    let (width, height) = (keys.width() as u16 + 4, 3);
    let y = if last + 1 + height <= area.bottom() {
        last + 1
    } else {
        first.saturating_sub(height)
    };
    let hint = anchored((area.x + 6, y), width, height, area);
    frame.render_widget(Clear, hint);
    frame.render_widget(
        Paragraph::new(keys).block(widgets::pane_block(Line::raw(""), true)),
        hint,
    );
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::text::Line;

    use crate::ui::test_support::{
        buffer_text, committed_change, intercept_foreground, offline_app, press, render,
        test_diff_document_with_after_metadata,
    };

    use super::*;

    fn selected_two_lines(name: &str) -> (std::path::PathBuf, App) {
        let (root, mut app) = committed_change(name);
        app.diff.diff_document = test_diff_document_with_after_metadata(
            vec![Line::raw("before 1"), Line::raw("before 2")],
            vec![Line::raw("after 1"), Line::raw("after 2")],
            vec![Some(10), Some(11)],
            vec![
                Some("let first = 1;".into()),
                Some("let second = 2;".into()),
            ],
        );
        app.focus = PaneFocus::Diff;
        app.diff.diff_side = ReviewSide::After;
        app.diff.focused_diff_row = 0;
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        (root, app)
    }

    fn queued(app: &App) -> Option<&str> {
        match app.pending_clipboard.as_ref() {
            Some(super::super::PendingClipboard::Selection(value)) => Some(value),
            _ => None,
        }
    }

    #[test]
    fn y_opens_checked_code_and_line_and_y_copies_both() {
        let (root, mut app) = selected_two_lines("copy-both");
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.overlay, Overlay::CopySelection(_)));
        let text = buffer_text(&render(&mut app, 100, 30));
        assert!(text.contains("[x] Code"));
        assert!(text.contains("[x] Line"));
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.overlay, Overlay::None));
        let expected = format!(
            "File: {}\nSide: After\nLines: 10–11\n\nlet first = 1;\nlet second = 2;",
            app.files.changes[app.files.change_selected].path
        );
        assert_eq!(queued(&app), Some(expected.as_str()));
        app.finish_copy_selection(Ok(()));
        assert!(app.review.changes.code_selection.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchecking_line_copies_only_code() {
        let (root, mut app) = selected_two_lines("copy-code-only");
        press(&mut app, KeyCode::Char('y'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        let text = buffer_text(&render(&mut app, 100, 30));
        assert!(text.contains("[ ] Line"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(queued(&app), Some("let first = 1;\nlet second = 2;"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchecking_both_refuses_to_copy() {
        let (root, mut app) = selected_two_lines("copy-nothing");
        press(&mut app, KeyCode::Char('y'));
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Enter);
        assert!(queued(&app).is_none());
        assert!(app.review.changes.code_selection.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn y_without_selection_does_not_open_copy() {
        let (root, mut app) = committed_change("copy-no-selection");
        app.focus = PaneFocus::Diff;
        press(&mut app, KeyCode::Char('y'));
        assert!(!matches!(app.overlay, Overlay::CopySelection(_)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn esc_cancels_selection_without_opening_copy() {
        let (root, mut app) = selected_two_lines("copy-esc");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(!app.review.is_selecting());
        assert!(app.review.changes.code_selection.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn blame(line: usize, sha: char, summary: &str) -> BlameInfo {
        BlameInfo {
            sha: sha.to_string().repeat(40),
            author: "Ada".to_owned(),
            author_email: "ada@example.com".to_owned(),
            author_time: 0,
            summary: summary.to_owned(),
            line,
        }
    }

    #[test]
    fn status_bar_summarizes_the_last_line_blame_and_other_authors() {
        let (root, mut app) = selected_two_lines("selection-blame");
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        assert!(app.sync_selection_blame());
        let Ok(ForegroundRequest::SelectionBlame {
            id,
            generation,
            target,
        }) = request_rx.try_recv()
        else {
            panic!("selection blame request");
        };
        assert_eq!(target.lines, [10, 11]);
        assert!(!app.sync_selection_blame());
        assert!(app.status_bar_text().contains("Blame…"));
        app.apply_selection_blame(
            id,
            generation,
            target,
            Ok(vec![blame(10, 'a', "First"), blame(11, 'b', "Second")]),
        );
        assert!(
            app.status_bar_text()
                .contains("Blame bbbbbbb · Ada · Second")
        );
        assert!(!app.status_bar_text().contains("author"));

        app.review.blame.as_mut().unwrap().entries[0].author = "Bob".to_owned();
        assert!(
            app.status_bar_text()
                .contains("Blame bbbbbbb · Ada · Second +1 author")
        );
        let text = buffer_text(&render(&mut app, 200, 30));
        assert!(!text.contains("aaaaaaa"));

        app.diff.diff_target = crate::git::DiffTarget::CommitAgainstParent {
            commit: "HEAD".to_owned(),
            parent: None,
        };
        app.review.blame.as_mut().unwrap().target.revision = Some("HEAD".to_owned());
        render(&mut app, 200, 30);
        let blame_area = app.shell.status_blame_area;
        assert!(blame_area.width > 0);
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: blame_area.x,
            row: blame_area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::LineHistory(_)));
        assert!(matches!(
            request_rx.try_recv(),
            Ok(ForegroundRequest::LineHistory { target, .. })
                if target.ranges == [(10, 11)] && target.revision.as_deref() == Some("HEAD")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn preview_app() -> App {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.inspect.commit_preview_path = Some("src/short.rs".to_owned());
        app.inspect.commit_preview_loaded_path = Some("src/short.rs".to_owned());
        app.inspect.commit_preview_document = test_diff_document_with_after_metadata(
            vec![Line::raw("before 1"), Line::raw("before 2")],
            vec![Line::raw("after 1"), Line::raw("after 2")],
            vec![Some(20), Some(21)],
            vec![Some("let a = 1;".into()), Some("let b = 2;".into())],
        );
        render(&mut app, 160, 40);
        app.focus = PaneFocus::Preview;
        app
    }

    #[test]
    fn graph_preview_visual_selection_yanks_like_changes() {
        let mut app = preview_app();
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        let text = buffer_text(&render(&mut app, 160, 40));
        assert!(text.contains("After · short.rs:20–21"));
        assert!(text.contains("y Yank · h History · Esc Cancel"));
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.overlay, Overlay::CopySelection(_)));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(
            queued(&app),
            Some("File: src/short.rs\nSide: After\nLines: 20–21\n\nlet a = 1;\nlet b = 2;")
        );
        app.finish_copy_selection(Ok(()));
        assert!(app.review.preview.code_selection.is_none());
    }

    #[test]
    fn graph_preview_drag_selects_rows_and_survives_changes_refresh() {
        let mut app = preview_app();
        let source = app.commit_preview_source_area(app.inspect.commit_preview_after_area);
        let mouse = |kind, row| {
            Event::Mouse(MouseEvent {
                kind,
                column: source.x + 4,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle(mouse(MouseEventKind::Down(MouseButton::Left), source.y))
            .unwrap();
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), source.y + 1))
            .unwrap();
        app.handle(mouse(MouseEventKind::Up(MouseButton::Left), source.y + 1))
            .unwrap();
        assert_eq!(
            app.selection_lines_on(SelectionSurface::Preview),
            Some(("src/short.rs".to_owned(), ReviewSide::After, vec![20, 21]))
        );
        app.clear_selection();
        assert!(app.review.preview.code_selection.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.review.preview.code_selection.is_none());
    }
}
