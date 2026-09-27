use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::git::{ChangeSection, DiffSummary, GitOperation, WorkingChange};
use crate::ui::review::ReviewSide;

use super::diff::horizontal_line_slice;
use super::effect::{ForegroundRequest, RefreshScope, RequestId};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::shell::{ActiveTab, PaneFocus};
use super::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState, TextField,
    counted_title, left_click, picker_edit, scrolled_content_row_at, shortcut_spans, update_picker,
    viewport_offset,
};
use super::{App, theme};
use staging::{FilesAction, StagingEffect, StagingKey, StagingState, update_staging};
pub(super) use staging::{StagingOwner, staging_progress_label};
#[cfg(not(test))]
use tree::TreeRowKind;
#[cfg(test)]
pub(super) use tree::TreeRowKind;
use tree::{TreeRow, rows as tree_rows};

mod staging;
mod tree;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct TreeSelectionKey {
    pub(super) section: ChangeSection,
    pub(super) path: String,
}

const FILES_SEARCH_HEIGHT: u16 = 22;

pub(super) struct FilesState {
    pub(super) changes: Vec<WorkingChange>,
    pub(super) change_summaries: Vec<(ChangeSection, DiffSummary)>,
    pub(super) change_selected: usize,
    pub(super) collapsed: HashSet<String>,
    pub(super) tree_rows: Vec<TreeRow>,
    pub(super) tree_selected: usize,
    pub(super) tree_scroll: usize,
    pub(super) tree_horizontal_scroll: usize,
    pub(super) tree_selection: HashSet<TreeSelectionKey>,
    pub(super) tree_visual_anchor: Option<usize>,
    pub(super) tree_keyboard_selecting: bool,
    pub(super) tree_drag_anchor: Option<usize>,
    pub(super) tree_dragging_selection: bool,
    pub(super) files_area: Rect,
    pub(super) list_area: Rect,
    pub(super) max_line_width: usize,
}

impl FilesState {
    pub(super) fn new(changes: Vec<WorkingChange>) -> Self {
        let collapsed = HashSet::new();
        let rows = tree_rows(&changes, &collapsed);
        let mut files = Self {
            changes,
            change_summaries: Vec::new(),
            change_selected: 0,
            collapsed,
            tree_rows: Vec::new(),
            tree_selected: 0,
            tree_scroll: 0,
            tree_horizontal_scroll: 0,
            tree_selection: HashSet::new(),
            tree_visual_anchor: None,
            tree_keyboard_selecting: false,
            tree_drag_anchor: None,
            tree_dragging_selection: false,
            files_area: Rect::default(),
            list_area: Rect::default(),
            max_line_width: 0,
        };
        files.set_rows(rows);
        files
    }

    pub(super) fn set_rows(&mut self, rows: Vec<TreeRow>) {
        self.tree_rows = rows;
        self.max_line_width = self
            .tree_rows
            .iter()
            .map(|row| self.tree_line(row).width())
            .max()
            .unwrap_or_default();
    }

    fn change_summary(&self, section: ChangeSection) -> DiffSummary {
        self.change_summaries
            .iter()
            .find_map(|(candidate, summary)| (*candidate == section).then_some(*summary))
            .unwrap_or_default()
    }

    pub(super) fn tree_line(&self, row: &TreeRow) -> Line<'static> {
        let indent = "  ".repeat(row.depth);
        match &row.kind {
            TreeRowKind::Directory {
                expanded, section, ..
            } => {
                let label = format!(
                    "{indent}{} {}",
                    if *expanded { "▾" } else { "▸" },
                    row.label
                );
                let mut spans = vec![match section {
                    Some(_) => Span::styled(label, theme::section_header()),
                    None => Span::raw(label),
                }];
                if let Some(section) = section {
                    let files = self
                        .changes
                        .iter()
                        .filter(|change| change.section == *section)
                        .count();
                    let summary = self.change_summary(*section);
                    spans.push(Span::raw(" "));
                    spans.extend(change_summary_spans(
                        files,
                        summary.additions,
                        summary.deletions,
                    ));
                }
                Line::from(spans)
            }
            TreeRowKind::File { change_index } => {
                let change = &self.changes[*change_index];
                Line::from(vec![
                    Span::raw(indent),
                    Span::styled(
                        format!("{:<4}", status_label(&change.status)),
                        status_style(&change.status),
                    ),
                    Span::raw(format!(" {}", row.label)),
                ])
            }
        }
    }

    pub(super) fn is_selecting(&self) -> bool {
        self.tree_visual_anchor.is_some()
            || self.tree_keyboard_selecting
            || self.tree_drag_anchor.is_some()
            || self.tree_dragging_selection
    }

    pub(super) fn replace_changes(
        &mut self,
        changes: Vec<WorkingChange>,
        selected: usize,
        summaries: Vec<(ChangeSection, DiffSummary)>,
    ) {
        self.changes = changes;
        self.change_selected = selected;
        self.change_summaries = summaries;
        self.collapsed.clear();
        self.tree_selection.clear();
        self.tree_visual_anchor = None;
        self.tree_keyboard_selecting = false;
    }

    pub(super) fn clear(&mut self) {
        self.changes.clear();
        self.change_summaries.clear();
        self.tree_rows.clear();
        self.max_line_width = 0;
    }
}

#[derive(Debug)]
pub(super) struct FilesSearch {
    pub(super) query: TextField,
    pub(super) cursor: ListCursor,
    pub(super) list_area: Rect,
    pub(super) buttons: ConfirmButtons,
    pub(super) origin_tree_selected: usize,
}

impl FilesSearch {
    pub(super) fn new(origin_tree_selected: usize) -> Self {
        Self {
            query: TextField::new(),
            cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
            origin_tree_selected,
        }
    }
}

impl App {
    pub(super) fn select_change(&mut self, index: usize) {
        self.clear_pending_diff_focus();
        if !self.files.changes.is_empty() {
            let next = index.min(self.files.changes.len() - 1);
            if next != self.files.change_selected {
                self.diff.fold_toggles.clear();
                self.clear_selection();
                self.diff.diff_horizontal_scroll = 0;
            }
            self.files.change_selected = next;
            if let Some(target) = self.files.changes[self.files.change_selected]
                .section
                .diff_target()
            {
                self.diff.diff_target = target;
            }
            self.request_file_diff();
        }
    }

    pub(super) fn select_tree(&mut self, index: usize) {
        if self.files.tree_rows.is_empty() {
            return;
        }
        self.files.tree_selected = index.min(self.files.tree_rows.len() - 1);
        if let TreeRowKind::File { change_index } =
            self.files.tree_rows[self.files.tree_selected].kind
        {
            self.select_change(change_index);
        }
    }

    fn max_tree_horizontal_scroll(&self) -> usize {
        let viewport = self.files.list_area.width.saturating_sub(2) as usize;
        self.files.max_line_width.saturating_sub(viewport)
    }

    fn scroll_tree_horizontal(&mut self, delta: isize) {
        self.files.tree_horizontal_scroll = self
            .files
            .tree_horizontal_scroll
            .saturating_add_signed(delta)
            .min(self.max_tree_horizontal_scroll());
    }

    pub(super) fn tree_selection_key(&self, row: usize) -> Option<TreeSelectionKey> {
        let TreeRowKind::File { change_index } = self.files.tree_rows.get(row)?.kind else {
            return None;
        };
        let change = self.files.changes.get(change_index)?;
        Some(TreeSelectionKey {
            section: change.section,
            path: change.path.clone(),
        })
    }

    fn tree_selection_view(&self) -> HashSet<(ChangeSection, &str)> {
        let mut selection = self
            .files
            .tree_selection
            .iter()
            .map(|key| (key.section, key.path.as_str()))
            .collect::<HashSet<_>>();
        let Some(anchor) = self.files.tree_visual_anchor else {
            return selection;
        };
        for row in anchor.min(self.files.tree_selected)..=anchor.max(self.files.tree_selected) {
            if let Some(TreeRowKind::File { change_index }) =
                self.files.tree_rows.get(row).map(|row| &row.kind)
                && let Some(change) = self.files.changes.get(*change_index)
            {
                selection.insert((change.section, change.path.as_str()));
            }
        }
        selection
    }

    fn effective_tree_selection(&self) -> HashSet<TreeSelectionKey> {
        self.tree_selection_view()
            .into_iter()
            .map(|(section, path)| TreeSelectionKey {
                section,
                path: path.to_owned(),
            })
            .collect()
    }

    fn begin_tree_selection(&mut self, row: usize, keyboard: bool) {
        if self.tree_selection_key(row).is_none() {
            self.show_action_error("Focus a file before starting selection.");
            return;
        }
        self.focus = PaneFocus::Files;
        self.files.tree_selected = row;
        self.files.tree_visual_anchor = Some(row);
        self.files.tree_keyboard_selecting = keyboard;
        self.files.tree_dragging_selection = !keyboard;
    }

    pub(super) fn finish_tree_selection(&mut self) {
        self.files.tree_selection = self.effective_tree_selection();
        self.files.tree_visual_anchor = None;
        self.files.tree_keyboard_selecting = false;
        self.files.tree_dragging_selection = false;
        self.files.tree_drag_anchor = None;
    }

    pub(super) fn clear_tree_selection(&mut self) {
        self.files.tree_selection.clear();
        self.files.tree_visual_anchor = None;
        self.files.tree_keyboard_selecting = false;
        self.files.tree_dragging_selection = false;
        self.files.tree_drag_anchor = None;
    }

    fn prune_tree_selection(&mut self) {
        let available = self
            .files
            .changes
            .iter()
            .map(|change| TreeSelectionKey {
                section: change.section,
                path: change.path.clone(),
            })
            .collect::<HashSet<_>>();
        self.files
            .tree_selection
            .retain(|key| available.contains(key));
        if self
            .files
            .tree_visual_anchor
            .is_some_and(|row| self.tree_selection_key(row).is_none())
        {
            self.files.tree_visual_anchor = None;
            self.files.tree_keyboard_selecting = false;
            self.files.tree_dragging_selection = false;
        }
    }

    pub(super) fn activate_tree_row(&mut self) {
        let Some(row) = self.files.tree_rows.get(self.files.tree_selected) else {
            return;
        };
        match &row.kind {
            TreeRowKind::Directory { .. } => self.toggle_tree_directory(),
            TreeRowKind::File { change_index } => {
                self.select_change(*change_index);
                self.focus = PaneFocus::Diff;
                self.diff.diff_side = ReviewSide::After;
                self.diff.focus_after_changes_refresh = true;
            }
        }
    }

    fn toggle_tree_directory(&mut self) {
        let Some(TreeRow {
            kind: TreeRowKind::Directory { key, .. },
            ..
        }) = self.files.tree_rows.get(self.files.tree_selected)
        else {
            return;
        };
        let key = key.clone();
        if !self.files.collapsed.remove(&key) {
            self.files.collapsed.insert(key);
        }
        self.rebuild_tree();
    }

    fn tree_staging_action_at(&self, row: usize, column: u16) -> Option<GitOperation> {
        let operation =
            staging_operation_for_row(self.files.tree_rows.get(row)?, &self.files.changes)?;
        let area = staging_overlay_area(
            self.files.list_area,
            row.checked_sub(self.files.tree_scroll)?,
        )?;
        if matches!(
            self.diff.diff_target,
            crate::git::DiffTarget::WorkingTreeAgainstRevision { .. }
        ) && area.x >= self.files.list_area.x + 3
            && (area.x - 3..area.x).contains(&column)
        {
            return match operation {
                GitOperation::StageAll => Some(GitOperation::UnstageAll),
                GitOperation::StagePath(path) => Some(GitOperation::UnstagePath(path)),
                _ => None,
            };
        }
        (area.x..area.right())
            .contains(&column)
            .then_some(operation)
    }

    fn run_focused_staging(&mut self, stage: bool) {
        let selection = self.effective_tree_selection();
        if !selection.is_empty() {
            self.finish_tree_selection();
            let section = if stage {
                ChangeSection::Unstaged
            } else {
                ChangeSection::Staged
            };
            let mut paths = selection
                .into_iter()
                .filter_map(|key| {
                    (key.section == section || key.section == ChangeSection::Working)
                        .then_some(key.path)
                })
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            if paths.is_empty() {
                self.show_action_error(if stage {
                    "Select an Unstaged file to stage."
                } else {
                    "Select a Staged file to unstage."
                });
                return;
            }
            let operation = if stage {
                GitOperation::StagePaths(paths)
            } else {
                GitOperation::UnstagePaths(paths)
            };
            self.run_staging_operation(operation);
            return;
        }
        let Some(mut operation) = self
            .files
            .tree_rows
            .get(self.files.tree_selected)
            .and_then(|row| staging_operation_for_row(row, &self.files.changes))
        else {
            return;
        };
        if !stage
            && matches!(
                self.diff.diff_target,
                crate::git::DiffTarget::WorkingTreeAgainstRevision { .. }
            )
        {
            operation = match operation {
                GitOperation::StageAll => GitOperation::UnstageAll,
                GitOperation::StagePath(path) => GitOperation::UnstagePath(path),
                other => other,
            };
        }
        if matches!(
            (&operation, stage),
            (
                GitOperation::StageAll | GitOperation::StagePath(_) | GitOperation::StagePaths(_),
                true
            ) | (
                GitOperation::UnstageAll
                    | GitOperation::UnstagePath(_)
                    | GitOperation::UnstagePaths(_),
                false
            )
        ) {
            self.run_staging_operation(operation);
        }
    }

    fn run_staging_operation(&mut self, operation: GitOperation) {
        let owner = self
            .files
            .tree_rows
            .get(self.files.tree_selected)
            .and_then(|row| staging_owner_for_row(row, &self.files.changes));
        self.apply_staging_action(FilesAction::Requested {
            operation,
            owner,
            now: Instant::now(),
        });
    }

    fn apply_staging_action(&mut self, action: FilesAction) {
        let repository = self
            .repository
            .as_ref()
            .map(|repository| repository.root().to_owned());
        let state = StagingState {
            repository: repository.as_deref(),
            foreground_action: &mut self.foreground.action,
            next_id: &mut self.foreground.next_id,
        };
        for effect in update_staging(state, action) {
            match effect {
                StagingEffect::InvalidateRefresh => self.advance_refresh_generation(),
                StagingEffect::Run(key) => {
                    let request = ForegroundRequest::Staging {
                        id: key.id,
                        path: key.repository.clone(),
                        operation: key.operation.clone(),
                    };
                    let next = match self.foreground.send(request) {
                        Ok(()) => FilesAction::Dispatched { key },
                        Err(error) => FilesAction::DispatchFailed { key, error },
                    };
                    self.apply_staging_action(next);
                }
                StagingEffect::ShowError(error) => self.show_action_error(&error),
                StagingEffect::RefreshChanges {
                    completion,
                    success,
                } => {
                    self.show_result(super::commands::OperationResultView::named_message(
                        "Staging",
                        success,
                        &completion,
                    ));
                    self.request_operation_refresh("Refreshing changes", RefreshScope::Changes);
                }
            }
        }
    }

    pub(super) fn rebuild_tree(&mut self) {
        let selected_path = self
            .files
            .changes
            .get(self.files.change_selected)
            .map(|change| (change.section, change.path.clone()));
        let query = match &self.overlay {
            Overlay::FilesSearch(search) => search.query.text.clone(),
            _ => String::new(),
        };
        let rows = if query.is_empty() {
            tree_rows(&self.files.changes, &self.files.collapsed)
        } else {
            filtered_tree_rows(
                tree_rows(&self.files.changes, &HashSet::new()),
                &self.files.changes,
                &query,
            )
        };
        self.files.set_rows(rows);
        self.files.tree_selected = selected_path
            .and_then(|(section, path)| {
                self.files.tree_rows.iter().position(|row| match row.kind {
                    TreeRowKind::File { change_index } => {
                        let change = &self.files.changes[change_index];
                        change.section == section && change.path == path
                    }
                    _ => false,
                })
            })
            .unwrap_or_else(|| {
                self.files
                    .tree_selected
                    .min(self.files.tree_rows.len().saturating_sub(1))
            });
        self.files.tree_scroll = self
            .files
            .tree_scroll
            .min(self.files.tree_rows.len().saturating_sub(1));
        self.prune_tree_selection();
    }

    pub(super) fn open_files_search(&mut self) {
        self.enter_tab(ActiveTab::Changes);
        self.focus = PaneFocus::Files;
        self.overlay = Overlay::FilesSearch(FilesSearch::new(self.files.tree_selected));
        self.rebuild_files_search_tree();
        self.select_first_search_file();
    }

    fn select_first_search_file(&mut self) {
        let first_file = self
            .files
            .tree_rows
            .iter()
            .position(|row| matches!(row.kind, TreeRowKind::File { .. }));
        if let Overlay::FilesSearch(search) = &mut self.overlay {
            search.cursor.scroll = 0;
            if let Some(index) = first_file {
                search.cursor.selected = index;
            }
        }
    }

    fn rebuild_files_search_tree(&mut self) {
        self.rebuild_tree();
        if let Overlay::FilesSearch(search) = &self.overlay {
            self.files.tree_selected = search
                .origin_tree_selected
                .min(self.files.tree_rows.len().saturating_sub(1));
        }
    }

    fn snap_files_search_to_file(&mut self, before: usize) {
        let Overlay::FilesSearch(search) = &mut self.overlay else {
            return;
        };
        let rows = &self.files.tree_rows;
        let is_file = |index: usize| {
            rows.get(index)
                .is_some_and(|row| matches!(row.kind, TreeRowKind::File { .. }))
        };
        let selected = search.cursor.selected;
        if is_file(selected) {
            return;
        }
        let next = if selected >= before {
            (selected.saturating_add(1)..rows.len()).find(|&index| is_file(index))
        } else {
            (0..selected).rev().find(|&index| is_file(index))
        };
        search.cursor.selected = next.unwrap_or(before);
    }

    fn close_files_search(&mut self) {
        let Overlay::FilesSearch(search) = &self.overlay else {
            return;
        };
        let origin = search.origin_tree_selected;
        self.overlay = Overlay::None;
        self.rebuild_tree();
        self.files.tree_selected = origin.min(self.files.tree_rows.len().saturating_sub(1));
    }

    fn open_files_search_result(&mut self) {
        let Overlay::FilesSearch(search) = &self.overlay else {
            return;
        };
        let change_index =
            self.files
                .tree_rows
                .get(search.cursor.selected)
                .and_then(|row| match row.kind {
                    TreeRowKind::File { change_index } => Some(change_index),
                    TreeRowKind::Directory { .. } => None,
                });
        self.close_files_search();
        let Some(change_index) = change_index else {
            return;
        };
        if let Some(row) = self.files.tree_rows.iter().position(
            |row| matches!(row.kind, TreeRowKind::File { change_index: index } if index == change_index),
        ) {
            self.select_tree(row);
            self.activate_tree_row();
        }
    }

    pub(super) fn apply_staging(
        &mut self,
        id: RequestId,
        path: PathBuf,
        operation: GitOperation,
        result: Result<String, String>,
    ) {
        self.apply_staging_action(FilesAction::Finished {
            key: StagingKey {
                id,
                repository: path,
                operation,
            },
            result,
        });
    }

    fn move_tree_selection(&mut self, delta: isize) {
        if self.files.tree_rows.is_empty() {
            return;
        }
        let last = self.files.tree_rows.len() - 1;
        self.select_tree(
            self.files
                .tree_selected
                .saturating_add_signed(delta)
                .min(last),
        );
    }

    fn tree_page(&self) -> isize {
        isize::try_from(self.files.list_area.height.max(1)).unwrap_or(isize::MAX)
    }

    fn tree_row_at(&self, pointer: (u16, u16)) -> Option<usize> {
        scrolled_content_row_at(
            Some(pointer),
            self.files.list_area,
            self.files.tree_rows.len(),
            self.files.tree_scroll,
        )
    }

    pub(super) fn finish_tree_drag(&mut self) {
        if self.files.tree_dragging_selection {
            self.finish_tree_selection();
        } else {
            self.files.tree_drag_anchor = None;
        }
    }

    pub(super) fn handle_files_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        match key.code {
            KeyCode::Enter => self.activate_tree_row(),
            KeyCode::Char(' ') => self.toggle_tree_directory(),
            KeyCode::Char('s' | 'S') => self.run_focused_staging(true),
            KeyCode::Char('u' | 'U') => self.run_focused_staging(false),
            KeyCode::Char('v') if !self.files.tree_keyboard_selecting => {
                self.begin_tree_selection(self.files.tree_selected, true);
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_tree_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_tree_selection(-1),
            KeyCode::PageDown => self.move_tree_selection(self.tree_page()),
            KeyCode::PageUp => self.move_tree_selection(-self.tree_page()),
            KeyCode::Home => self.select_tree(0),
            KeyCode::End => self.select_tree(self.files.tree_rows.len().saturating_sub(1)),
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn handle_files_mouse(&mut self, mouse: MouseEvent) -> Result<bool, String> {
        let pointer = (mouse.column, mouse.row);
        if !self.files.list_area.contains(pointer.into()) {
            return Ok(false);
        }
        match mouse.kind {
            MouseEventKind::ScrollRight => self.scroll_tree_horizontal(1),
            MouseEventKind::ScrollLeft => self.scroll_tree_horizontal(-1),
            MouseEventKind::ScrollDown => {
                self.focus = PaneFocus::Files;
                self.move_tree_selection(1);
            }
            MouseEventKind::ScrollUp => {
                self.focus = PaneFocus::Files;
                self.move_tree_selection(-1);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(anchor) = self.files.tree_drag_anchor else {
                    return Ok(false);
                };
                self.focus = PaneFocus::Files;
                let row = self
                    .tree_row_at(pointer)
                    .unwrap_or_else(|| self.files.tree_rows.len().saturating_sub(1));
                if !self.files.tree_dragging_selection {
                    self.begin_tree_selection(anchor, false);
                }
                if self.files.tree_dragging_selection {
                    self.select_tree(row);
                }
            }
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let row = self.tree_row_at(pointer).unwrap_or_default();
                self.focus = PaneFocus::Files;
                self.clear_selection();
                let left = button == MouseButton::Left;
                let staging = left
                    .then(|| self.tree_staging_action_at(row, mouse.column))
                    .flatten();
                self.select_tree(row.min(self.files.tree_rows.len().saturating_sub(1)));
                if let Some(operation) = staging {
                    self.files.tree_drag_anchor = None;
                    self.run_staging_operation(operation);
                } else if left {
                    self.files.tree_drag_anchor = self
                        .tree_selection_key(self.files.tree_selected)
                        .map(|_| self.files.tree_selected);
                    self.activate_tree_row();
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn handle_files_search(&mut self, input: &Event) {
        let Overlay::FilesSearch(search) = &mut self.overlay else {
            return;
        };
        if let Some(button) = left_click(input).and_then(|pointer| search.buttons.hit(pointer)) {
            match button {
                ConfirmButton::Primary => self.open_files_search_result(),
                ConfirmButton::Secondary => self.close_files_search(),
            }
            return;
        }
        let Some(edit) = picker_edit(input, search.list_area, search.cursor.scroll, true) else {
            return;
        };
        let len = self.files.tree_rows.len();
        let page = usize::from(search.list_area.height);
        let before = search.cursor.selected;
        match update_picker(
            PickerState {
                query: Some(&mut search.query),
                cursor: &mut search.cursor,
                len,
                page,
            },
            edit,
        ) {
            PickerOutcome::Activate => self.open_files_search_result(),
            PickerOutcome::Cancel => self.close_files_search(),
            PickerOutcome::Filtered => {
                self.rebuild_files_search_tree();
                self.select_first_search_file();
            }
            PickerOutcome::Moved => self.snap_files_search_to_file(before),
            PickerOutcome::Unchanged => {}
        }
    }

    fn file_tree_item(
        &self,
        row: &TreeRow,
        style: Style,
        horizontal_offset: usize,
        viewport_width: usize,
    ) -> ListItem<'static> {
        ListItem::new(horizontal_line_slice(
            self.files.tree_line(row),
            horizontal_offset,
            viewport_width,
        ))
        .style(style)
    }

    pub(super) fn draw_files(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.files.files_area = area;
        let inner = Block::default().borders(Borders::ALL).inner(area);
        self.files.list_area = inner;
        self.files.tree_horizontal_scroll = self
            .files
            .tree_horizontal_scroll
            .min(self.max_tree_horizontal_scroll());
        self.files.tree_scroll = viewport_offset(
            self.files.tree_scroll,
            self.files.tree_selected,
            self.files.tree_rows.len(),
            inner.height as usize,
        );
        let tree_selection = self.tree_selection_view();
        let title_text = if tree_selection.is_empty() {
            counted_title("Files", self.files.changes.len())
        } else {
            format!(
                "{} · Selected {}",
                counted_title("Files", self.files.changes.len()),
                tree_selection.len()
            )
        };
        let title = if self.shell.shortcut_hints {
            let suffix = title_text.strip_prefix("Files").unwrap_or_default();
            Line::from(shortcut_spans(
                'O',
                "Find Files",
                " ",
                suffix,
                Style::default(),
                true,
            ))
        } else {
            Line::raw(title_text)
        };
        let block = widgets::pane_block(title, self.focus == PaneFocus::Files);
        frame.render_widget(block, area);
        let view_offset = self.files.tree_scroll;
        let height = inner.height as usize;
        let hovered = scrolled_content_row_at(
            self.shell.mouse_position,
            inner,
            self.files.tree_rows.len(),
            view_offset,
        );
        let items = self
            .files
            .tree_rows
            .iter()
            .enumerate()
            .skip(view_offset)
            .take(height)
            .map(|(index, row)| {
                let selected = match row.kind {
                    TreeRowKind::File { change_index } => {
                        let change = &self.files.changes[change_index];
                        tree_selection.contains(&(change.section, change.path.as_str()))
                    }
                    TreeRowKind::Directory { .. } => false,
                };
                let base = if selected {
                    theme::selection_row()
                } else {
                    Style::default()
                };
                self.file_tree_item(
                    row,
                    theme::hover(base, hovered == Some(index)),
                    self.files.tree_horizontal_scroll,
                    inner.width.saturating_sub(2) as usize,
                )
            });
        let selected = self
            .files
            .tree_selected
            .checked_sub(view_offset)
            .filter(|row| *row < height && !self.files.tree_rows.is_empty());
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(theme::focus_row())
                .highlight_symbol(theme::LIST_MARKER),
            inner,
            &mut state,
        );
        if self.repository.is_none() {
            frame.render_widget(
                Paragraph::new("Not a Git repository")
                    .style(theme::warning_text())
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
        if let Some((row_index, operation)) = hovered.and_then(|row_index| {
            staging_operation_for_row(self.files.tree_rows.get(row_index)?, &self.files.changes)
                .map(|operation| (row_index, operation))
        }) && let Some(visible_row) = row_index.checked_sub(view_offset)
            && let Some(overlay_area) = staging_overlay_area(inner, visible_row)
            && let Some((label, style)) = staging_overlay(&operation, true)
        {
            frame.render_widget(Paragraph::new(label).style(style), overlay_area);
            if matches!(
                self.diff.diff_target,
                crate::git::DiffTarget::WorkingTreeAgainstRevision { .. }
            ) && overlay_area.x >= inner.x + 3
            {
                let area = Rect::new(overlay_area.x - 3, overlay_area.y, 3, 1);
                if let Some((label, style)) = staging_overlay(&GitOperation::UnstageAll, true) {
                    frame.render_widget(Paragraph::new(label).style(style), area);
                }
            }
        }
        if let Some(action) = self.foreground.action.as_ref()
            && let ForegroundKind::Staging {
                owner: Some(owner), ..
            } = &action.kind
            && let Some(owner_row) =
                staging_owner_row(owner, &self.files.tree_rows, &self.files.changes)
            && let Some(visible_row) = owner_row.checked_sub(view_offset)
            && let Some(overlay_area) = staging_overlay_area(inner, visible_row)
        {
            frame.render_widget(
                Paragraph::new(Line::from(theme::spinner_span(action.started.elapsed())))
                    .alignment(Alignment::Center)
                    .style(Style::default().bg(theme::SURFACE_HOVER)),
                overlay_area,
            );
        }
    }

    pub(super) fn draw_files_search(&self, frame: &mut Frame<'_>, search: &mut FilesSearch) {
        let inner = widgets::dialog_frame(
            frame,
            "Find Files",
            theme::DIALOG_MEDIUM,
            FILES_SEARCH_HEIGHT,
        );
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let query = Line::from(vec![
            Span::raw(search.query.text.clone()),
            theme::cursor_span(search.query.cursor_started.elapsed()),
        ]);
        frame.render_widget(
            Paragraph::new(query).block(Block::default().borders(Borders::ALL).title("Search")),
            regions[0],
        );
        search.list_area = regions[1];
        let len = self.files.tree_rows.len();
        let height = regions[1].height as usize;
        search.cursor.scroll =
            viewport_offset(search.cursor.scroll, search.cursor.selected, len, height);
        let hovered = self
            .shell
            .mouse_position
            .and_then(|pointer| search.cursor.row_at(regions[1], pointer, len, 0));
        let items = self
            .files
            .tree_rows
            .iter()
            .enumerate()
            .skip(search.cursor.scroll)
            .take(height)
            .map(|(index, row)| {
                self.file_tree_item(
                    row,
                    theme::hover(Style::default(), hovered == Some(index)),
                    0,
                    regions[1].width.saturating_sub(2) as usize,
                )
            });
        let selected = search
            .cursor
            .selected
            .checked_sub(search.cursor.scroll)
            .filter(|row| *row < height && len > 0);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[1],
            &mut state,
        );
        frame.render_widget(
            widgets::footer_hint(&files_search_status(
                &self.files.tree_rows,
                &search.query.text,
            )),
            regions[2],
        );
        search.buttons = widgets::dialog_footer(
            frame,
            regions[3],
            ["[Enter] Open", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
    }
}

fn files_search_status(rows: &[TreeRow], query: &str) -> String {
    let files = rows
        .iter()
        .filter(|row| matches!(row.kind, TreeRowKind::File { .. }))
        .count();
    match (query.is_empty(), files) {
        (true, _) => String::new(),
        (false, 0) => "No matching files".to_owned(),
        (false, 1) => "1 matching file".to_owned(),
        (false, files) => format!("{files} matching files"),
    }
}

fn staging_operation_for_row(row: &TreeRow, changes: &[WorkingChange]) -> Option<GitOperation> {
    match row.kind {
        TreeRowKind::Directory {
            section: Some(ChangeSection::Staged),
            ..
        } => Some(GitOperation::UnstageAll),
        TreeRowKind::Directory {
            section: Some(ChangeSection::Unstaged | ChangeSection::Working),
            ..
        } => Some(GitOperation::StageAll),
        TreeRowKind::File { change_index } => {
            let change = changes.get(change_index)?;
            match change.section {
                ChangeSection::Staged => Some(GitOperation::UnstagePath(change.path.clone())),
                ChangeSection::Working | ChangeSection::Unstaged => {
                    Some(GitOperation::StagePath(change.path.clone()))
                }
                ChangeSection::Commit => None,
            }
        }
        _ => None,
    }
}

fn staging_owner_for_row(row: &TreeRow, changes: &[WorkingChange]) -> Option<StagingOwner> {
    match &row.kind {
        TreeRowKind::Directory { key, .. } => Some(StagingOwner::Directory(key.clone())),
        TreeRowKind::File { change_index } => {
            let change = changes.get(*change_index)?;
            Some(StagingOwner::File(TreeSelectionKey {
                section: change.section,
                path: change.path.clone(),
            }))
        }
    }
}

fn staging_owner_row(
    owner: &StagingOwner,
    rows: &[TreeRow],
    changes: &[WorkingChange],
) -> Option<usize> {
    rows.iter().position(|row| match (owner, &row.kind) {
        (StagingOwner::Directory(owner_key), TreeRowKind::Directory { key, .. }) => {
            owner_key == key
        }
        (StagingOwner::File(owner_key), TreeRowKind::File { change_index }) => {
            changes.get(*change_index).is_some_and(|change| {
                change.section == owner_key.section && change.path == owner_key.path
            })
        }
        _ => false,
    })
}

fn staging_overlay(operation: &GitOperation, hovered: bool) -> Option<(&'static str, Style)> {
    if !hovered {
        return None;
    }
    match operation {
        GitOperation::StageAll | GitOperation::StagePath(_) | GitOperation::StagePaths(_) => {
            Some((
                " + ",
                Style::default()
                    .fg(theme::TEXT_INVERSE)
                    .bg(theme::SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ))
        }
        GitOperation::UnstageAll | GitOperation::UnstagePath(_) | GitOperation::UnstagePaths(_) => {
            Some((
                " - ",
                Style::default()
                    .fg(theme::TEXT_INVERSE)
                    .bg(theme::WARNING)
                    .add_modifier(Modifier::BOLD),
            ))
        }
        _ => None,
    }
}

fn staging_overlay_area(list_area: Rect, row: usize) -> Option<Rect> {
    if list_area.width < 3 || row >= list_area.height as usize {
        return None;
    }
    Some(Rect::new(
        list_area.right().saturating_sub(3),
        list_area.y.saturating_add(row as u16),
        3,
        1,
    ))
}

pub(super) fn path_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

pub(super) fn status_style(status: &str) -> Style {
    let color = match status.chars().next() {
        Some('A' | '?') => theme::SUCCESS,
        Some('M') => theme::WARNING,
        Some('D') => theme::ERROR,
        Some('R' | 'C') => theme::ACCENT,
        Some('T') => theme::STATUS_TYPE_CHANGE,
        Some('U') => theme::STATUS_UNMERGED,
        _ => theme::HINT,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub(super) fn status_label(status: &str) -> &str {
    status.get(..1).unwrap_or(status)
}

pub(super) fn change_summary_spans(
    files: usize,
    additions: usize,
    deletions: usize,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(format!("{files}f"), theme::hint())];
    if additions > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("+{additions}"),
            Style::default().fg(theme::SUCCESS),
        ));
    }
    if deletions > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("-{deletions}"),
            Style::default().fg(theme::ERROR),
        ));
    }
    spans
}

fn filtered_tree_rows(rows: Vec<TreeRow>, changes: &[WorkingChange], query: &str) -> Vec<TreeRow> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return rows;
    }
    let mut retained = HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let TreeRowKind::File { change_index } = row.kind else {
            continue;
        };
        if !changes[change_index].path.to_lowercase().contains(&query) {
            continue;
        }
        retained.insert(index);
        let mut needed_depth = row.depth;
        for ancestor in (0..index).rev() {
            if rows[ancestor].depth < needed_depth {
                needed_depth = rows[ancestor].depth;
                retained.insert(ancestor);
                if needed_depth == 0 {
                    break;
                }
            }
        }
    }
    rows.into_iter()
        .enumerate()
        .filter_map(|(index, row)| retained.contains(&index).then_some(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton,
        MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use crate::git::{ChangeSection, GitOperation, Repository, WorkingChange};
    use crate::ui::effect::ForegroundRequest;
    use crate::ui::overlay::Overlay;
    use crate::ui::shell::{AppShortcut, PaneFocus, app_shortcut};
    use crate::ui::test_support::{
        buffer_text, click, git, intercept_foreground, press, render, temp_repo,
        wait_for_foreground, wait_for_refresh,
    };
    use crate::ui::{App, theme};

    use super::tree::TreeRowKind;
    use super::{
        change_summary_spans, staging_operation_for_row, staging_overlay, staging_owner_for_row,
        staging_owner_row, status_label, status_style,
    };

    fn file_rows(app: &App) -> Vec<usize> {
        app.files
            .tree_rows
            .iter()
            .enumerate()
            .filter_map(|(row, item)| matches!(item.kind, TreeRowKind::File { .. }).then_some(row))
            .collect()
    }

    #[test]
    fn files_pane_marks_focus_with_the_border_and_rows_with_focus_and_selection_styles() {
        let root = temp_repo("files-styles");
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(name), "changed\n").unwrap();
        }
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let rows = file_rows(&app);
        app.select_tree(rows[0]);
        app.files
            .tree_selection
            .insert(app.tree_selection_key(rows[2]).unwrap());
        assert_eq!(app.focus, PaneFocus::Files);

        let buffer = render(&mut app, 120, 30);
        let pane = app.files.files_area;
        let list = app.files.list_area;
        let scroll = app.files.tree_scroll;
        assert_eq!(buffer[(pane.x, pane.y)].fg, theme::ACCENT);
        let row_y = |row: usize| list.y + (row - scroll) as u16;
        let focused = &buffer[(list.x + 3, row_y(rows[0]))];
        assert_eq!(focused.bg, theme::SURFACE_FOCUS);
        assert!(focused.modifier.contains(Modifier::BOLD));
        let selected = &buffer[(list.x + 3, row_y(rows[2]))];
        assert_eq!(selected.bg, theme::SURFACE_SELECTION);
        assert!(selected.modifier.contains(Modifier::BOLD));
        let plain = &buffer[(list.x + 3, row_y(rows[1]))];
        assert_eq!(plain.bg, Color::Reset);
        assert!(!plain.modifier.contains(Modifier::BOLD));

        app.shell.mouse_position = Some((list.x + 3, row_y(rows[2])));
        let buffer = render(&mut app, 120, 30);
        assert_eq!(
            buffer[(list.x + 3, row_y(rows[2]))].bg,
            theme::SURFACE_HOVER
        );

        app.focus = PaneFocus::Diff;
        let buffer = render(&mut app, 120, 30);
        assert_ne!(buffer[(pane.x, pane.y)].fg, theme::ACCENT);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn section_rows_use_the_section_header_style() {
        let root = temp_repo("files-section-header");
        fs::write(root.join("a.txt"), "changed\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.select_tree(file_rows(&app)[0]);

        let buffer = render(&mut app, 120, 30);
        let list = app.files.list_area;
        let header = &buffer[(list.x + 2, list.y)];
        assert_eq!(header.symbol(), "▾");
        assert_eq!(header.fg, theme::ACCENT);
        assert!(header.modifier.contains(Modifier::BOLD));
        let file = &buffer[(list.x + 9, list.y + 1)];
        assert_eq!(file.symbol(), "a");
        assert_eq!(file.fg, Color::Reset);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn find_files_uses_the_medium_dialog_with_clickable_footer_buttons() {
        let root = temp_repo("find-files-footer");
        for name in ["alpha.txt", "beta.txt"] {
            fs::write(root.join(name), "changed\n").unwrap();
        }
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_files_search();
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("[Enter] Open"));
        assert!(text.contains("[Esc] Close"));
        assert!(!text.contains("Type to filter paths"));
        let buttons = app.overlay.files_search().unwrap().buttons;
        assert_eq!(
            buttons.primary.width + buttons.secondary.width,
            theme::DIALOG_MEDIUM - 2
        );
        click(&mut app, buttons.secondary.x + 1, buttons.secondary.y + 1);
        assert!(matches!(app.overlay, Overlay::None));

        app.open_files_search();
        press(&mut app, KeyCode::Char('b'));
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("1 matching file"));
        let buttons = app.overlay.files_search().unwrap().buttons;
        click(&mut app, buttons.primary.x + 1, buttons.primary.y + 1);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(
            app.files.changes[app.files.change_selected].path,
            "beta.txt"
        );
        assert_eq!(app.focus, PaneFocus::Diff);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_tree_pages_and_jumps_without_scrolling_the_diff() {
        let root = temp_repo("files-paging");
        for index in 0..40 {
            fs::write(root.join(format!("file-{index:02}.txt")), "changed\n").unwrap();
        }
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        render(&mut app, 120, 30);
        let page = usize::from(app.files.list_area.height);
        assert!(page > 1);
        let last = app.files.tree_rows.len() - 1;
        assert!(last > page);
        assert_eq!(app.focus, PaneFocus::Files);
        let start = app.files.tree_selected;
        assert!(start < page);

        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.files.tree_selected, start + page);
        assert_eq!(app.diff.diff_scroll, 0);
        press(&mut app, KeyCode::End);
        assert_eq!(app.files.tree_selected, last);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.files.tree_selected, last - page);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.files.tree_selected, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_width_is_recorded_with_the_rows_on_every_rebuild() {
        let root = temp_repo("files-width");
        fs::write(root.join("a.txt"), "changed\n").unwrap();
        fs::write(
            root.join("a-much-longer-file-name-than-the-other.txt"),
            "changed\n",
        )
        .unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let widest = |app: &App| {
            app.files
                .tree_rows
                .iter()
                .map(|row| app.files.tree_line(row).width())
                .max()
                .unwrap_or_default()
        };
        assert!(app.files.max_line_width > "a-much-longer-file-name-than-the-other.txt".len());
        assert_eq!(app.files.max_line_width, widest(&app));

        app.select_tree(0);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.files.tree_rows.len(), 1);
        assert_eq!(app.files.max_line_width, widest(&app));
        assert!(app.files.max_line_width < "a-much-longer-file-name-than-the-other.txt".len());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn change_summary_tokens_use_hint_success_and_error_colors() {
        let spans = change_summary_spans(2, 3, 4);
        assert_eq!(spans[0].style.fg, Some(theme::HINT));
        assert_eq!(spans[2].style.fg, Some(theme::SUCCESS));
        assert_eq!(spans[4].style.fg, Some(theme::ERROR));
    }

    #[test]
    fn staging_enqueues_the_foreground_worker_without_waiting() {
        let root = temp_repo("staging-async");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, _result_tx) = intercept_foreground(&mut app);

        app.run_staging_operation(GitOperation::StageAll);
        assert!(matches!(
            request_rx.try_recv().unwrap(),
            ForegroundRequest::Staging {
                operation: GitOperation::StageAll,
                ..
            }
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staging_spinner_stays_on_the_stable_owner_after_rebuild_and_scroll() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-staging-owner-{unique}"));
        fs::create_dir_all(root.join("aaa-hidden")).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        let paths = std::iter::once("aaa-hidden/a.txt".to_owned())
            .chain((0..30).map(|index| format!("file-{index:02}.txt")))
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(root.join(path), "base\n").unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Base"]);
        for path in &paths {
            fs::write(root.join(path), "base\nchanged\n").unwrap();
        }

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let row_for = |app: &App, path: &str| {
            app.files
                .tree_rows
                .iter()
                .position(|row| {
                    matches!(
                        row.kind,
                        crate::ui::files::tree::TreeRowKind::File { change_index }
                            if app.files.changes[change_index].path == path
                    )
                })
                .unwrap()
        };
        let owner_path = "file-29.txt";
        let owner_row = row_for(&app, owner_path);
        let crate::ui::files::tree::TreeRowKind::File { change_index } =
            app.files.tree_rows[owner_row].kind
        else {
            unreachable!();
        };
        let (_request_rx, _result_tx) = intercept_foreground(&mut app);
        app.files.tree_selected = owner_row;
        app.files.change_selected = change_index;
        app.run_staging_operation(GitOperation::StagePath(owner_path.to_owned()));
        app.files.collapsed.insert("unstaged:aaa-hidden".to_owned());
        app.rebuild_tree();

        let rebuilt_owner_row = row_for(&app, owner_path);
        assert_ne!(owner_row, rebuilt_owner_row);
        let buffer = render(&mut app, 120, 30);
        assert!(rebuilt_owner_row >= app.files.list_area.height as usize);
        let owner_y = (app.files.list_area.y..app.files.list_area.bottom())
            .find(|row| {
                (app.files.list_area.x..app.files.list_area.right())
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains(owner_path)
            })
            .expect("scrolled owner row");
        assert_eq!(
            buffer[(app.files.list_area.right() - 3, owner_y)].bg,
            theme::SURFACE_HOVER
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staging_group_owner_survives_a_tree_rebuild() {
        let staged = WorkingChange {
            section: ChangeSection::Staged,
            status: "M".to_owned(),
            path: "staged.txt".to_owned(),
        };
        let unstaged = WorkingChange {
            section: ChangeSection::Unstaged,
            status: "M".to_owned(),
            path: "unstaged.txt".to_owned(),
        };
        let initial_changes = vec![staged, unstaged.clone()];
        let initial_rows =
            crate::ui::files::tree::rows(&initial_changes, &std::collections::HashSet::new());
        let owner_row = initial_rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    crate::ui::files::tree::TreeRowKind::Directory {
                        section: Some(ChangeSection::Unstaged),
                        ..
                    }
                )
            })
            .unwrap();
        let owner = staging_owner_for_row(&initial_rows[owner_row], &initial_changes)
            .expect("staging group owner");

        let rebuilt_changes = vec![unstaged];
        let rebuilt_rows =
            crate::ui::files::tree::rows(&rebuilt_changes, &std::collections::HashSet::new());

        assert_eq!(
            staging_owner_row(&owner, &rebuilt_rows, &rebuilt_changes),
            Some(0)
        );
    }

    #[test]
    fn change_summaries_omit_each_zero_line_total() {
        let text = |additions, deletions| {
            change_summary_spans(2, additions, deletions)
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(text(0, 0), "2f");
        assert_eq!(text(3, 0), "2f +3");
        assert_eq!(text(0, 4), "2f -4");
        assert_eq!(text(3, 4), "2f +3 -4");
    }

    #[test]
    fn colors_git_status_labels_by_status_class() {
        assert_eq!(status_style("A").fg, Some(theme::SUCCESS));
        assert_eq!(status_style("??").fg, Some(theme::SUCCESS));
        assert_eq!(status_style("M").fg, Some(theme::WARNING));
        assert_eq!(status_style("D").fg, Some(theme::ERROR));
        assert_eq!(status_style("R100").fg, Some(theme::ACCENT));
        assert_eq!(status_style("C75").fg, Some(theme::ACCENT));
        assert_eq!(status_style("T").fg, Some(theme::STATUS_TYPE_CHANGE));
        assert_eq!(status_style("U").fg, Some(theme::STATUS_UNMERGED));
    }

    #[test]
    fn displays_each_raw_git_status_as_one_character() {
        assert_eq!(status_label("A"), "A");
        assert_eq!(status_label("M100"), "M");
        assert_eq!(status_label("R073"), "R");
        assert_eq!(status_label("C098"), "C");
        assert_eq!(status_label("??"), "?");
        assert_eq!(status_label("U"), "U");
    }

    #[test]
    fn files_horizontal_wheel_scrolls_long_rows_without_a_scrollbar() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-files-horizontal-{unique}"));
        let directory = root.join("a-very-long-directory-name-that-overflows-the-files-pane-and-keeps-going-far-beyond-the-complete-terminal-width-for-testing");
        fs::create_dir_all(&directory).unwrap();
        git(&root, &["init", "-b", "main"]);
        fs::write(
            directory.join("a-very-long-file-name-that-must-scroll.txt"),
            "changed\n",
        )
        .unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let files = app.files.list_area;
        let selected = app.files.tree_selected;
        let vertical = app.files.tree_scroll;
        assert!(
            app.max_tree_horizontal_scroll() > 0,
            "rows={:?} area={files:?}",
            app.files
                .tree_rows
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>()
        );
        assert!(files.contains((files.x, files.y).into()));
        assert!(!app.diff_contains(files.x, files.y));

        for _ in 0..20 {
            app.handle(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollRight,
                column: files.x,
                row: files.y,
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
        }

        assert!(app.files.tree_horizontal_scroll > 0);
        assert_eq!(app.files.tree_selected, selected);
        assert_eq!(app.files.tree_scroll, vertical);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!((files.y..files.bottom()).all(|row| {
            (files.x..files.right())
                .all(|column| !matches!(buffer[(column, row)].symbol(), "─" | "▄"))
        }));

        for _ in 0..1_000 {
            app.scroll_tree_horizontal(-1);
        }
        assert_eq!(app.files.tree_horizontal_scroll, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_rows_map_to_direct_stage_and_unstage_operations() {
        let changes = vec![
            WorkingChange {
                section: ChangeSection::Staged,
                status: "M".to_owned(),
                path: "staged.txt".to_owned(),
            },
            WorkingChange {
                section: ChangeSection::Unstaged,
                status: "??".to_owned(),
                path: "new.txt".to_owned(),
            },
        ];
        let rows = crate::ui::files::tree::rows(&changes, &std::collections::HashSet::new());
        let operations = rows
            .iter()
            .filter_map(|row| staging_operation_for_row(row, &changes))
            .collect::<Vec<_>>();
        assert!(operations.contains(&GitOperation::UnstageAll));
        assert!(operations.contains(&GitOperation::StageAll));
        assert!(operations.contains(&GitOperation::UnstagePath("staged.txt".to_owned())));
        assert!(operations.contains(&GitOperation::StagePath("new.txt".to_owned())));
    }

    #[test]
    fn staging_buttons_only_appear_as_right_edge_hover_overlays() {
        assert!(staging_overlay(&GitOperation::StageAll, false).is_none());
        assert!(staging_overlay(&GitOperation::UnstageAll, false).is_none());

        let (stage_label, stage_style) = staging_overlay(&GitOperation::StageAll, true).unwrap();
        assert_eq!(stage_label, " + ");
        assert_eq!(stage_style.bg, Some(theme::SUCCESS));
        assert_eq!(stage_style.fg, Some(theme::TEXT_INVERSE));

        let (unstage_label, unstage_style) =
            staging_overlay(&GitOperation::UnstageAll, true).unwrap();
        assert_eq!(unstage_label, " - ");
        assert_eq!(unstage_style.bg, Some(theme::WARNING));
        assert_eq!(unstage_style.fg, Some(theme::TEXT_INVERSE));
    }

    #[test]
    fn space_folds_directory_rows_and_leaves_file_rows_alone() {
        let root = temp_repo("files-space");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.txt"), "changed\n").unwrap();
        fs::write(root.join("src/b.txt"), "changed\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let rows = file_rows(&app);
        let all_rows = app.files.tree_rows.len();
        app.select_tree(rows[0]);
        assert_eq!(app.focus, PaneFocus::Files);

        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.focus, PaneFocus::Files);
        assert!(app.files.collapsed.is_empty());
        assert_eq!(app.files.tree_rows.len(), all_rows);
        assert!(!app.diff.focus_after_changes_refresh);

        let directory = app
            .files
            .tree_rows
            .iter()
            .position(|row| matches!(row.kind, TreeRowKind::Directory { section: None, .. }))
            .expect("src directory row");
        app.select_tree(directory);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.files.collapsed.len(), 1);
        assert!(app.files.tree_rows.len() < all_rows);
        assert_eq!(app.focus, PaneFocus::Files);
        press(&mut app, KeyCode::Enter);
        assert!(app.files.collapsed.is_empty());
        assert_eq!(app.files.tree_rows.len(), all_rows);

        app.select_tree(rows[0]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus, PaneFocus::Diff);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_tree_visual_ranges_batch_stage_and_unstage_by_keyboard_and_mouse() {
        let root = temp_repo("ui-file-select");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        for path in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(path), "base\n").unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Base"]);
        for path in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(path), format!("base\n{path}\n")).unwrap();
        }

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let file_rows = app
            .files
            .tree_rows
            .iter()
            .enumerate()
            .filter_map(|(row, item)| {
                matches!(item.kind, crate::ui::files::tree::TreeRowKind::File { .. }).then_some(row)
            })
            .collect::<Vec<_>>();
        app.select_tree(file_rows[0]);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        for _ in file_rows[0]..file_rows[2] {
            app.handle(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
                .unwrap();
        }
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        let buffer = render(&mut app, 120, 30);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains('✓'));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);

        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Enter);

        for path in ["a.txt", "c.txt"] {
            assert!(
                app.files.changes.iter().any(|change| {
                    change.path == path
                        && matches!(
                            change.section,
                            ChangeSection::Staged | ChangeSection::Unstaged
                        )
                }),
                "{path} missing in {:?}",
                app.files.changes
            );
        }
        assert!(
            app.files.changes.iter().any(|change| {
                change.path == "b.txt" && change.section == ChangeSection::Unstaged
            })
        );

        let staged_rows = app
            .files
            .tree_rows
            .iter()
            .enumerate()
            .filter_map(|(row, item)| match item.kind {
                crate::ui::files::tree::TreeRowKind::File { change_index }
                    if matches!(
                        app.files.changes[change_index].section,
                        ChangeSection::Staged | ChangeSection::Unstaged
                    ) =>
                {
                    Some(row)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        app.select_tree(staged_rows[0]);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        for _ in staged_rows[0]..staged_rows[2] {
            app.handle(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
                .unwrap();
        }
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Enter);
        assert!(
            app.files
                .changes
                .iter()
                .all(|change| change.section == ChangeSection::Unstaged)
        );

        let file_rows = app
            .files
            .tree_rows
            .iter()
            .enumerate()
            .filter_map(|(row, item)| {
                matches!(item.kind, crate::ui::files::tree::TreeRowKind::File { .. }).then_some(row)
            })
            .collect::<Vec<_>>();
        let list_y = app.files.list_area.y;
        let mouse = |kind, row| {
            Event::Mouse(MouseEvent {
                kind,
                column: 10,
                row: list_y.saturating_add(row as u16),
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle(mouse(MouseEventKind::Down(MouseButton::Left), file_rows[0]))
            .unwrap();
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), file_rows[2]))
            .unwrap();
        app.handle(mouse(MouseEventKind::Up(MouseButton::Left), file_rows[2]))
            .unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Enter);
        assert!(
            app.files
                .changes
                .iter()
                .all(|change| change.section == ChangeSection::Staged)
        );

        assert!(
            app.repository
                .as_ref()
                .unwrap()
                .command_facts()
                .has_staged_changes
        );
        for path in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                fs::read_to_string(root.join(path)).unwrap(),
                format!("base\n{path}\n")
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn alt_o_filters_files_and_escape_restores_the_tree() {
        let root = temp_repo("files-search");
        fs::write(root.join("alpha.txt"), "alpha\n").unwrap();
        fs::write(root.join("beta.txt"), "beta\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let all_rows = app.files.tree_rows.len();
        let original_tree_selected = app.files.tree_selected;
        let (refresh_tx, refresh_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.in_flight = false;
        let (foreground_rx, _foreground_result_tx) = intercept_foreground(&mut app);

        app.handle(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Modifier(ModifierKeyCode::LeftAlt),
            KeyModifiers::ALT,
            KeyEventKind::Press,
        )))
        .unwrap();
        let buffer = render(&mut app, 120, 30);
        let row_text = |row: u16| {
            (0..120)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        };
        let files_row = (0..30)
            .find(|row| row_text(*row).contains("O Find Files · 2"))
            .expect("Alt exposes the Files shortcut before its count");
        let files_text = row_text(files_row);
        let files_characters = files_text.chars().collect::<Vec<_>>();
        let needle = "O Find Files".chars().collect::<Vec<_>>();
        let files_column = files_characters
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap() as u16;
        let hint_cell = &buffer[(files_column, files_row)];
        assert_eq!(hint_cell.fg, theme::ACCENT);
        assert!(hint_cell.modifier.contains(Modifier::BOLD));
        let menu_cell = &buffer[(files_column + 2, files_row)];
        assert_ne!(menu_cell.fg, theme::ACCENT);
        assert!(!menu_cell.modifier.contains(Modifier::BOLD));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(refresh_rx.try_recv().is_err());
        let buffer = render(&mut app, 120, 30);
        let dialog = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(dialog.contains("Search"));
        assert!(!dialog.contains("[Search]"));
        assert!(dialog.contains('▏'));
        app.handle(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();
        assert!(foreground_rx.try_recv().is_err());
        assert_eq!(app.files.tree_selected, original_tree_selected);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        let Overlay::FilesSearch(search) = &app.overlay else {
            panic!("typing keeps Find Files open");
        };
        assert_eq!(search.query.text, "b");
        assert!(
            app.files
                .tree_rows
                .iter()
                .any(|row| row.label == "beta.txt")
        );
        assert!(
            !app.files
                .tree_rows
                .iter()
                .any(|row| row.label == "alpha.txt")
        );
        let buffer = render(&mut app, 120, 30);
        let filtered_dialog = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(filtered_dialog.contains("Find Files"));
        assert!(filtered_dialog.contains('?'));
        assert!(!filtered_dialog.contains("??"));
        assert!(filtered_dialog.contains("beta.txt"));

        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!matches!(app.overlay, Overlay::FilesSearch(_)));
        assert_eq!(app.files.tree_rows.len(), all_rows);
        assert_eq!(app.files.tree_selected, original_tree_selected);

        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('ø'), KeyModifiers::ALT)),
            Some(AppShortcut::FilesSearch)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn find_files_wheel_scrolls_its_own_cursor_and_viewport() {
        let root = temp_repo("find-files-wheel");
        for index in 0..30 {
            fs::write(root.join(format!("file-{index:02}.txt")), "changed\n").unwrap();
        }
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let underlying_selection = app.files.tree_selected;
        app.open_files_search();
        let initial = app.overlay.files_search().unwrap().cursor.selected;
        render(&mut app, 80, 16);
        let area = app.overlay.files_search().unwrap().list_area;
        app.shell.mouse_position = Some((area.x + 4, area.y + 2));
        let buffer = render(&mut app, 80, 16);
        assert_eq!(buffer[(area.x + 4, area.y + 2)].bg, theme::SURFACE_HOVER);

        for _ in 0..15 {
            app.handle(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
            render(&mut app, 80, 16);
        }

        let search = app.overlay.files_search().unwrap();
        assert!(search.cursor.selected > initial);
        assert!(search.cursor.scroll > 0);
        assert_eq!(app.files.tree_selected, underlying_selection);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mouse_and_keyboard_stage_and_unstage_direct_tree_actions() {
        let root = temp_repo("ui-stage-test");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nchanged\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.files.list_area = Rect::new(0, 0, 80, 30);
        let new_file_row = app
            .files
            .tree_rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    crate::ui::files::tree::TreeRowKind::File { change_index }
                        if app.files.changes[change_index].path == "new.txt"
                )
            })
            .unwrap();
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 77,
            row: new_file_row as u16,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Enter);
        assert!(
            app.files.changes.iter().any(|change| {
                change.section == ChangeSection::Staged && change.path == "new.txt"
            })
        );

        let unstaged_group = app
            .files
            .tree_rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    crate::ui::files::tree::TreeRowKind::Directory {
                        section: Some(ChangeSection::Unstaged),
                        ..
                    }
                )
            })
            .unwrap();
        app.select_tree(unstaged_group);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Enter);
        assert!(
            app.files
                .changes
                .iter()
                .all(|change| change.section == ChangeSection::Staged)
        );

        let staged_group = app
            .files
            .tree_rows
            .iter()
            .position(|row| {
                matches!(
                    row.kind,
                    crate::ui::files::tree::TreeRowKind::Directory {
                        section: Some(ChangeSection::Staged),
                        ..
                    }
                )
            })
            .unwrap();
        app.select_tree(staged_group);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert!(
            !app.repository
                .as_ref()
                .unwrap()
                .command_facts()
                .has_staged_changes
        );
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\nchanged\n"
        );
        assert_eq!(fs::read_to_string(root.join("new.txt")).unwrap(), "new\n");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn find_files_types_j_and_k_and_clicks_the_row_under_the_pointer_after_scrolling() {
        let root = temp_repo("find-files-grammar");
        for name in ["jam.txt", "kite.txt", "other.txt"] {
            fs::write(root.join(name), "changed\n").unwrap();
        }
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_files_search();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
        let search = app.overlay.files_search().unwrap();
        assert_eq!(search.query.text, "jk");
        assert!(app.files.tree_rows.iter().all(|row| row.label != "jam.txt"));
        press(&mut app, KeyCode::Esc);
        fs::remove_dir_all(root).unwrap();

        let root = temp_repo("find-files-click");
        for index in 0..30 {
            fs::write(root.join(format!("file-{index:02}.txt")), "changed\n").unwrap();
        }
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_files_search();
        press(&mut app, KeyCode::End);
        render(&mut app, 80, 16);
        let search = app.overlay.files_search().unwrap();
        let scroll = search.cursor.scroll;
        let area = search.list_area;
        assert!(scroll > 0, "End scrolls the list");
        let crate::ui::files::tree::TreeRowKind::File { change_index } =
            app.files.tree_rows[scroll].kind
        else {
            panic!("the first visible row is a file");
        };
        click(&mut app, area.x + 1, area.y);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.files.change_selected, change_index);
        fs::remove_dir_all(root).unwrap();
    }
}
