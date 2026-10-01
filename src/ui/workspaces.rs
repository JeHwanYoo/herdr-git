use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::git::{BranchStatus, ChangeOverview, LocalIdentity};
use crate::project::{ProjectRegistry, ProjectStatus, WorktreeStatus};

use super::commands::{CommandId, OperationResultView};
use super::effect::{
    ForegroundRequest, InspectedWorktrees, ProjectMutationOutcome, RefreshScope, RequestId,
    SwitchTarget,
};
use super::files::{change_summary_spans, path_label};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::shell::{ActiveTab, PaneFocus};
use super::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState, area_hovered,
    left_click, picker_edit, shortcut_spans, truncate_to_width, update_picker,
};
use super::{App, theme};

const REPOSITORY_ROW_HEIGHT: usize = 2;
const REPOSITORY_GROUP_GAP: usize = 1;
const WORKSPACE_PICKER_CHROME: u16 = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RepositoryRowKind {
    Current,
    Project { root: PathBuf },
    Worktree { project_root: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepositoryRow {
    pub(super) kind: RepositoryRowKind,
    pub(super) path: PathBuf,
    pub(super) overview: Option<ChangeOverview>,
    pub(super) branch_status: Option<BranchStatus>,
    pub(super) worktree_count: usize,
    pub(super) error: Option<String>,
    pub(super) fingerprint: Option<u64>,
}

pub(super) struct WorkspacesState {
    pub(super) project_registry: ProjectRegistry,
    pub(super) project_statuses: Vec<Result<ProjectStatus, String>>,
    pub(super) repository_rows: Vec<RepositoryRow>,
    pub(super) repository_expanded: HashSet<PathBuf>,
    pub(super) repository_selected: usize,
    pub(super) repository_scroll: usize,
    pub(super) repository_area: Rect,
    pub(super) repository_list_area: Rect,
    pub(super) add_project_area: Rect,
    pub(super) repository_row_areas: Vec<(usize, Rect)>,
    pub(super) repository_remove_areas: Vec<(usize, Rect)>,
    pub(super) pending_switch: Option<SwitchTarget>,
}

impl WorkspacesState {
    pub(super) fn new(project_registry: ProjectRegistry) -> Self {
        Self {
            project_registry,
            project_statuses: Vec::new(),
            repository_rows: Vec::new(),
            repository_expanded: HashSet::new(),
            repository_selected: 0,
            repository_scroll: 0,
            repository_area: Rect::default(),
            repository_list_area: Rect::default(),
            add_project_area: Rect::default(),
            repository_row_areas: Vec::new(),
            repository_remove_areas: Vec::new(),
            pending_switch: None,
        }
    }

    pub(super) fn clear_areas(&mut self) {
        self.repository_row_areas.clear();
        self.repository_remove_areas.clear();
        self.repository_area = Rect::default();
        self.repository_list_area = Rect::default();
        self.add_project_area = Rect::default();
    }

    pub(super) fn selected_row(&self) -> Option<&RepositoryRow> {
        self.repository_rows.get(self.repository_selected)
    }

    pub(super) fn selected_is_project(&self) -> bool {
        self.selected_row()
            .is_some_and(|row| matches!(row.kind, RepositoryRowKind::Project { .. }))
    }

    fn current_row(&self) -> Option<&RepositoryRow> {
        self.repository_rows
            .iter()
            .find(|row| matches!(row.kind, RepositoryRowKind::Current))
    }

    pub(super) fn inspected_worktrees(&self) -> InspectedWorktrees {
        let current = self.current_row().and_then(|row| {
            Some(WorktreeStatus {
                path: row.path.clone(),
                overview: row.overview?,
                branch_status: row.branch_status.clone()?,
                fingerprint: row.fingerprint?,
            })
        });
        let projects = self
            .project_statuses
            .iter()
            .flatten()
            .flat_map(|status| std::iter::once(&status.primary).chain(&status.worktrees))
            .cloned()
            .collect();
        InspectedWorktrees { current, projects }
    }

    pub(super) fn expanded_projects(&self) -> Vec<PathBuf> {
        let mut expanded = self.repository_expanded.iter().cloned().collect::<Vec<_>>();
        expanded.sort();
        expanded
    }

    fn row_index(&self, kind: &RepositoryRowKind, path: &Path) -> Option<usize> {
        self.repository_rows
            .iter()
            .position(|row| &row.kind == kind && row.path == path)
    }

    fn row_hit(areas: &[(usize, Rect)], pointer: (u16, u16)) -> Option<usize> {
        areas
            .iter()
            .find_map(|(index, area)| area.contains(pointer.into()).then_some(*index))
    }
}

#[derive(Debug)]
pub(super) struct WorkspacePicker {
    pub(super) rows: Vec<RepositoryRow>,
    pub(super) cursor: ListCursor,
    pub(super) list_area: Rect,
    pub(super) buttons: ConfirmButtons,
}

impl App {
    pub(super) fn refresh_remove_project_capability(&mut self) {
        self.ops.command_context.can_remove_project = self.workspaces.selected_is_project();
    }

    pub(super) fn workspace_rows(&self) -> Vec<RepositoryRow> {
        let Some(current) = self.workspaces.current_row() else {
            return Vec::new();
        };
        let project_roots = self.workspaces.project_registry.roots();
        repository_rows_from_statuses(
            current,
            project_roots,
            &self.workspaces.project_statuses,
            project_roots,
        )
    }

    pub(super) fn open_workspace_picker(&mut self) {
        let rows = self.workspace_rows();
        let selected = self
            .workspaces
            .selected_row()
            .and_then(|selected| {
                rows.iter()
                    .position(|row| row.kind == selected.kind && row.path == selected.path)
            })
            .or_else(|| rows.iter().position(|row| row.path == self.active_path))
            .unwrap_or(0);
        self.overlay = Overlay::Workspace(WorkspacePicker {
            rows,
            cursor: ListCursor {
                selected,
                scroll: 0,
            },
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
        });
    }

    fn select_workspace(&mut self) {
        let Overlay::Workspace(picker) = &self.overlay else {
            return;
        };
        let Some(row) = picker.rows.get(picker.cursor.selected).cloned() else {
            return;
        };
        self.overlay = Overlay::None;
        if let RepositoryRowKind::Worktree { project_root } = &row.kind {
            self.workspaces
                .repository_expanded
                .insert(project_root.clone());
            self.rebuild_repository_rows_from_cached_statuses();
        }
        if let Some(index) = self.workspaces.row_index(&row.kind, &row.path) {
            self.switch_repository_row(index);
        }
    }

    pub(super) fn handle_workspace_picker(&mut self, input: &Event) {
        let Overlay::Workspace(picker) = &mut self.overlay else {
            return;
        };
        if let Some(point) = left_click(input) {
            match picker.buttons.hit(point) {
                Some(ConfirmButton::Primary) => {
                    self.select_workspace();
                    return;
                }
                Some(ConfirmButton::Secondary) => {
                    self.overlay = Overlay::None;
                    return;
                }
                None => {}
            }
        }
        if let Event::Key(key) = input
            && key.kind == KeyEventKind::Press
            && let KeyCode::Char(digit @ '1'..='9') = key.code
        {
            let index = usize::from(digit as u8 - b'1');
            if index < picker.rows.len() {
                picker.cursor.selected = index;
                self.select_workspace();
            }
            return;
        }
        let Some(edit) = picker_edit(input, picker.list_area, picker.cursor.scroll, false) else {
            return;
        };
        let len = picker.rows.len();
        let page = usize::from(picker.list_area.height);
        match update_picker(
            PickerState {
                query: None,
                cursor: &mut picker.cursor,
                len,
                page,
            },
            edit,
        ) {
            PickerOutcome::Activate => self.select_workspace(),
            PickerOutcome::Cancel => self.overlay = Overlay::None,
            PickerOutcome::Moved | PickerOutcome::Filtered | PickerOutcome::Unchanged => {}
        }
    }

    pub(super) fn apply_repository_rows(
        &mut self,
        project_statuses: Vec<Result<ProjectStatus, String>>,
        repository_rows: Vec<RepositoryRow>,
    ) {
        self.workspaces.project_statuses = project_statuses;
        self.replace_repository_rows(repository_rows);
        if matches!(self.overlay, Overlay::Workspace(_)) {
            let rows = self.workspace_rows();
            if let Overlay::Workspace(picker) = &mut self.overlay {
                picker.cursor.clamp(rows.len());
                picker.rows = rows;
            }
        }
    }

    fn replace_repository_rows(&mut self, repository_rows: Vec<RepositoryRow>) {
        let selected = self
            .workspaces
            .selected_row()
            .map(|row| (row.kind.clone(), row.path.clone()));
        self.workspaces.repository_rows = repository_rows;
        self.workspaces.repository_selected = selected
            .and_then(|(kind, path)| self.workspaces.row_index(&kind, &path))
            .unwrap_or_else(|| {
                self.workspaces
                    .repository_selected
                    .min(self.workspaces.repository_rows.len().saturating_sub(1))
            });
        self.refresh_remove_project_capability();
    }

    fn rebuild_repository_rows_from_cached_statuses(&mut self) {
        let Some(current) = self.workspaces.current_row() else {
            return;
        };
        let repository_rows = repository_rows_from_statuses(
            current,
            self.workspaces.project_registry.roots(),
            &self.workspaces.project_statuses,
            &self.workspaces.expanded_projects(),
        );
        self.replace_repository_rows(repository_rows);
    }

    fn toggle_selected_project(&mut self) {
        let Some(RepositoryRow {
            kind: RepositoryRowKind::Project { root },
            ..
        }) = self.workspaces.selected_row()
        else {
            return;
        };
        let root = root.clone();
        self.advance_refresh_generation();
        if !self.workspaces.repository_expanded.remove(&root) {
            self.workspaces.repository_expanded.insert(root);
        }
        self.rebuild_repository_rows_from_cached_statuses();
    }

    fn move_repository_selection(&mut self, delta: isize) {
        if self.workspaces.repository_rows.is_empty() {
            return;
        }
        self.focus = PaneFocus::Workspaces;
        self.workspaces.repository_selected = self
            .workspaces
            .repository_selected
            .saturating_add_signed(delta)
            .min(self.workspaces.repository_rows.len() - 1);
        self.refresh_remove_project_capability();
    }

    fn select_repository_row(&mut self, index: usize) {
        if index >= self.workspaces.repository_rows.len() {
            return;
        }
        self.focus = PaneFocus::Workspaces;
        self.workspaces.repository_selected = index;
        self.refresh_remove_project_capability();
    }

    fn workspaces_page(&self) -> isize {
        isize::try_from(self.workspaces.repository_row_areas.len().max(1)).unwrap_or(isize::MAX)
    }

    pub(super) fn select_active_repository_row(&mut self) {
        self.enter_tab(ActiveTab::Changes);
        self.focus = PaneFocus::Workspaces;
        self.workspaces.repository_selected = self
            .workspaces
            .repository_rows
            .iter()
            .position(|row| self.repository.is_some() && row.path == self.active_path)
            .unwrap_or_default();
    }

    pub(super) fn add_project_from_picker(&mut self) {
        if self.foreground.action.is_some() {
            self.show_action_error("Wait for the current Git operation.");
            return;
        }
        let id = self.foreground.next_id;
        let request = ForegroundRequest::AddProject {
            id,
            registry: self.workspaces.project_registry.clone(),
        };
        match self
            .foreground
            .request_foreground(request, ForegroundKind::AddProject)
        {
            Ok(_) => {}
            Err(error) => self.show_action_error(&format!("Project worker stopped: {error}")),
        }
    }

    pub(super) fn confirm_remove_selected_project(&mut self) {
        if self.foreground.action.is_some() {
            self.show_action_error("Wait for the current Git operation.");
            return;
        }
        let Some(RepositoryRow {
            kind: RepositoryRowKind::Project { root },
            ..
        }) = self.workspaces.selected_row()
        else {
            self.show_action_error("Select a registered Project");
            return;
        };
        self.overlay = Overlay::RemoveProject {
            root: root.clone(),
            buttons: ConfirmButtons::default(),
        };
    }

    pub(super) fn handle_remove_project_confirmation(&mut self, input: &Event) {
        let Overlay::RemoveProject { root, buttons } = &self.overlay else {
            return;
        };
        let root = root.clone();
        let buttons = *buttons;
        let confirmed = match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => true,
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    return;
                }
                _ => return,
            },
            _ => match left_click(input).and_then(|pointer| buttons.hit(pointer)) {
                Some(ConfirmButton::Primary) => true,
                Some(ConfirmButton::Secondary) => {
                    self.overlay = Overlay::None;
                    return;
                }
                None => return,
            },
        };
        if confirmed {
            self.overlay = Overlay::None;
            self.remove_project(root);
        }
    }

    pub(super) fn draw_remove_project_confirmation(
        &self,
        frame: &mut Frame<'_>,
        root: &Path,
        buttons: &mut ConfirmButtons,
    ) {
        let inner = widgets::dialog_frame(frame, "Remove Project", theme::DIALOG_MEDIUM, 10);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(3)])
            .split(inner);
        frame.render_widget(
            Paragraph::new(format!(
                "Remove this Project from Workspaces?\n{}\n\nFiles and worktrees stay on disk.",
                root.display()
            ))
            .wrap(Wrap { trim: true }),
            regions[0],
        );
        *buttons = widgets::dialog_footer(
            frame,
            regions[1],
            ["[Enter] Remove", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }

    fn remove_project(&mut self, root: PathBuf) {
        let id = self.foreground.next_id;
        let request = ForegroundRequest::RemoveProject {
            id,
            registry: self.workspaces.project_registry.clone(),
            root: root.clone(),
        };
        match self
            .foreground
            .request_foreground(request, ForegroundKind::RemoveProject { root })
        {
            Ok(_) => {}
            Err(error) => self.show_action_error(&format!("Project worker stopped: {error}")),
        }
    }

    pub(super) fn apply_project_mutation(
        &mut self,
        id: RequestId,
        result: Result<(ProjectRegistry, ProjectMutationOutcome), String>,
    ) {
        let Some(kind) = self.foreground.take_matching_action(id, |kind| {
            matches!(
                kind,
                ForegroundKind::AddProject | ForegroundKind::RemoveProject { .. }
            )
        }) else {
            return;
        };
        match result {
            Ok((registry, outcome)) => {
                self.workspaces.project_registry = registry;
                match outcome {
                    ProjectMutationOutcome::Added => {
                        self.show_result(OperationResultView::named_message(
                            "Add Project",
                            true,
                            "Project added",
                        ));
                        self.request_operation_refresh(
                            "Refreshing Projects",
                            RefreshScope::Workspaces,
                        );
                    }
                    ProjectMutationOutcome::Removed => {
                        if let ForegroundKind::RemoveProject { root } = kind {
                            self.workspaces.repository_expanded.remove(&root);
                        }
                        self.show_result(OperationResultView::named_message(
                            "Remove Project",
                            true,
                            "Project removed",
                        ));
                        self.request_operation_refresh(
                            "Refreshing Projects",
                            RefreshScope::Workspaces,
                        );
                    }
                    ProjectMutationOutcome::AlreadyRegistered => {
                        self.show_result(OperationResultView::message(
                            Some(CommandId::AddProject),
                            false,
                            "Project is already registered",
                        ));
                    }
                    ProjectMutationOutcome::NotRegistered => {
                        self.show_result(OperationResultView::message(
                            Some(CommandId::RemoveProject),
                            false,
                            "Project is not registered",
                        ));
                    }
                    ProjectMutationOutcome::Cancelled => {}
                }
            }
            Err(error) => {
                let command = match kind {
                    ForegroundKind::RemoveProject { .. } => CommandId::RemoveProject,
                    _ => CommandId::AddProject,
                };
                self.show_result(OperationResultView::message(Some(command), false, &error));
            }
        }
    }

    pub(super) fn handle_workspaces_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != PaneFocus::Workspaces {
            return false;
        }
        match key.code {
            KeyCode::Enter => self.switch_repository_row(self.workspaces.repository_selected),
            KeyCode::Char(' ') => self.toggle_selected_project(),
            KeyCode::Down | KeyCode::Char('j') => self.move_repository_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_repository_selection(-1),
            KeyCode::PageDown => self.move_repository_selection(self.workspaces_page()),
            KeyCode::PageUp => self.move_repository_selection(-self.workspaces_page()),
            KeyCode::Home => self.select_repository_row(0),
            KeyCode::End => {
                self.select_repository_row(self.workspaces.repository_rows.len().saturating_sub(1))
            }
            _ => return false,
        }
        true
    }

    pub(super) fn handle_workspaces_mouse(&mut self, mouse: MouseEvent) -> bool {
        let pointer = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown
                if self
                    .workspaces
                    .repository_list_area
                    .contains(pointer.into()) =>
            {
                self.move_repository_selection(1);
            }
            MouseEventKind::ScrollUp
                if self
                    .workspaces
                    .repository_list_area
                    .contains(pointer.into()) =>
            {
                self.move_repository_selection(-1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.workspaces.add_project_area.contains(pointer.into()) {
                    self.dispatch_command(CommandId::AddProject);
                } else if let Some(index) =
                    WorkspacesState::row_hit(&self.workspaces.repository_remove_areas, pointer)
                {
                    self.workspaces.repository_selected = index;
                    self.dispatch_command(CommandId::RemoveProject);
                } else if let Some(index) =
                    WorkspacesState::row_hit(&self.workspaces.repository_row_areas, pointer)
                {
                    self.workspaces.repository_selected = index;
                    self.focus = PaneFocus::Workspaces;
                    let project = self.workspaces.selected_is_project();
                    self.switch_repository_row(index);
                    if project {
                        self.toggle_selected_project();
                    }
                } else {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }

    fn switch_elapsed(&self, row: &RepositoryRow) -> Option<Duration> {
        if let Some(target) = self.workspaces.pending_switch.as_ref() {
            return (target.path == row.path && target.row_kind == row.kind)
                .then_some(target.started.elapsed());
        }
        self.foreground.action.as_ref().and_then(|action| {
            matches!(
                &action.kind,
                ForegroundKind::Switch { path, row_kind, .. }
                    if path == &row.path && row_kind == &row.kind
            )
            .then_some(action.started.elapsed())
        })
    }

    pub(super) fn repository_row_line(&self, row: &RepositoryRow, width: u16) -> Line<'static> {
        let active = self.repository.is_some() && self.active_path == row.path;
        let mut spans = if let Some(elapsed) = self.switch_elapsed(row) {
            vec![theme::spinner_span(elapsed), Span::raw(" ")]
        } else if active {
            vec![Span::styled(theme::LIST_MARKER, theme::accent())]
        } else {
            let marker = match row.kind {
                RepositoryRowKind::Current | RepositoryRowKind::Project { .. } => " ",
                RepositoryRowKind::Worktree { .. } => " ",
            };
            vec![Span::styled(marker, theme::hint())]
        };
        match &row.kind {
            RepositoryRowKind::Current => {
                spans.push(Span::raw(path_label(&row.path)));
            }
            RepositoryRowKind::Project { .. } => {
                let worktree_label = format!(
                    " · {} {}",
                    row.worktree_count,
                    if row.worktree_count == 1 {
                        "worktree"
                    } else {
                        "worktrees"
                    }
                );
                let available = width.saturating_sub(2) as usize;
                let worktrees = if available
                    >= path_label(&row.path).chars().count() + worktree_label.chars().count()
                {
                    worktree_label
                } else {
                    String::new()
                };
                spans.push(Span::raw(path_label(&row.path)));
                spans.push(Span::styled(worktrees, theme::hint()));
            }
            RepositoryRowKind::Worktree { project_root } => {
                spans.push(Span::raw("  "));
                spans.push(Span::raw(worktree_path_label(&row.path, project_root)));
            }
        }
        Line::from(spans)
    }

    pub(super) fn repository_summary_line(&self, row: &RepositoryRow, width: u16) -> Line<'static> {
        if self.switch_elapsed(row).is_some() {
            return Line::from(Span::styled("Loading repository", theme::accent()));
        }
        let mut spans = Vec::new();
        if let Some(error) = &row.error {
            let style = if matches!(row.kind, RepositoryRowKind::Current) {
                theme::warning_text()
            } else {
                theme::error_text()
            };
            spans.push(Span::styled(error.clone(), style));
            return Line::from(spans);
        }
        let Some(branch) = &row.branch_status else {
            return Line::from(spans);
        };
        let working = if let Some(overview) = row.overview
            && overview.changed_paths > 0
        {
            let mut spans = vec![Span::styled("● ", theme::warning_text())];
            spans.extend(change_summary_spans(
                overview.changed_paths,
                overview.additions,
                overview.deletions,
            ));
            spans
        } else {
            Vec::new()
        };
        let branch_prefix = vec![Span::styled(
            format!("{} ", theme::BRANCH_GLYPH),
            theme::accent(),
        )];
        let available = width as usize;
        let suffix_width = if working.is_empty() {
            0
        } else {
            3 + Line::from(working.clone()).width()
        };
        let prefix_width = Line::from(branch_prefix.clone()).width();
        let branch_width = available
            .saturating_sub(prefix_width.saturating_add(suffix_width))
            .max(1)
            .min(available.saturating_sub(prefix_width).max(1));
        spans.extend(branch_prefix);
        spans.push(Span::styled(
            truncate_to_width(&branch.checked_out, branch_width),
            theme::accent(),
        ));
        if !working.is_empty() {
            spans.push(Span::raw(" · "));
            spans.extend(working);
        }
        Line::from(spans)
    }

    pub(super) fn repository_upstream_line(
        &self,
        row: &RepositoryRow,
        width: u16,
    ) -> Option<Line<'static>> {
        let branch = row.branch_status.as_ref()?;
        let upstream = branch.upstream.as_ref()?;
        if self.switch_elapsed(row).is_some() {
            return Some(Line::default());
        }

        let success = Style::default().fg(theme::SUCCESS);
        let mut synchronization = Vec::new();
        if branch.ahead == 0 && branch.behind == 0 {
            synchronization.push(Span::styled("✓", success));
        } else {
            if branch.ahead > 0 {
                synchronization.push(Span::styled(format!("↑{}", branch.ahead), success));
            }
            if branch.ahead > 0 && branch.behind > 0 {
                synchronization.push(Span::raw(" "));
            }
            if branch.behind > 0 {
                synchronization.push(Span::styled(
                    format!("↓{}", branch.behind),
                    theme::warning_text(),
                ));
            }
        }

        let prefix = vec![Span::styled(
            format!("{} ", theme::REMOTE_GLYPH),
            theme::accent(),
        )];
        let suffix_width = 3 + Line::from(synchronization.clone()).width();
        let prefix_width = Line::from(prefix.clone()).width();
        let available = width as usize;
        let upstream_width = available
            .saturating_sub(prefix_width.saturating_add(suffix_width))
            .max(1)
            .min(available.saturating_sub(prefix_width).max(1));
        let mut spans = prefix;
        spans.push(Span::styled(
            truncate_to_width(upstream, upstream_width),
            theme::hint(),
        ));
        spans.push(Span::raw(" · "));
        spans.extend(synchronization);
        Some(Line::from(spans))
    }

    pub(super) fn draw_workspaces(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.workspaces.repository_area = area;
        let title = if self.refresh.running_progress().is_some() && self.refresh.workspaces_pending
        {
            Line::from(vec![
                theme::spinner_span(self.refresh.last_check.elapsed()),
                Span::raw(if self.workspaces.repository_rows.is_empty() {
                    " Loading Workspaces"
                } else {
                    " Refreshing Workspaces"
                }),
            ])
        } else if self.shell.shortcut_hints {
            Line::from(shortcut_spans(
                'W',
                "Workspaces",
                " ",
                "",
                Style::default(),
                true,
            ))
        } else {
            Line::raw("Workspaces")
        };
        let block = widgets::pane_block(title, self.focus == PaneFocus::Workspaces);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            self.workspaces.repository_list_area = Rect::default();
            return;
        }

        self.workspaces.repository_list_area = inner;
        let available_height = inner.height as usize;

        let rows = &self.workspaces.repository_rows;
        let mut row_tops = Vec::with_capacity(rows.len());
        if let Some(row) = rows.first() {
            row_tops.push((0, 2usize, repository_row_height(row)));
        }
        let mut next_top = 8usize;
        let mut has_project_group = false;
        for (index, row) in rows.iter().enumerate().skip(1) {
            if matches!(row.kind, RepositoryRowKind::Project { .. }) {
                if has_project_group {
                    next_top = next_top.saturating_add(REPOSITORY_GROUP_GAP);
                }
                has_project_group = true;
            }
            let height = repository_row_height(row);
            row_tops.push((index, next_top, height));
            next_top = next_top.saturating_add(height);
        }

        let content_height = next_top.max(8);
        let selected = self.workspaces.repository_selected;
        let scroll = &mut self.workspaces.repository_scroll;
        if selected == 0 {
            *scroll = 0;
        } else if let Some((_, selected_top, selected_height)) =
            row_tops.iter().find(|(index, _, _)| *index == selected)
        {
            let selected_bottom = selected_top.saturating_add(*selected_height);
            if *selected_top < *scroll {
                *scroll = *selected_top;
            } else if selected_bottom > scroll.saturating_add(available_height) {
                *scroll = selected_bottom.saturating_sub(available_height);
            }
        }
        *scroll = (*scroll).min(content_height.saturating_sub(available_height));

        let viewport_start = *scroll;
        let viewport_end = viewport_start.saturating_add(available_height);
        self.workspaces.repository_row_areas = row_tops
            .iter()
            .filter_map(|(index, top, height)| {
                let start = (*top).max(viewport_start);
                let end = top.saturating_add(*height).min(viewport_end);
                (start < end).then_some((
                    *index,
                    Rect::new(
                        inner.x,
                        inner
                            .y
                            .saturating_add(start.saturating_sub(viewport_start) as u16),
                        inner.width,
                        end.saturating_sub(start) as u16,
                    ),
                ))
            })
            .collect();
        let hovered = self.shell.mouse_position.and_then(|position| {
            WorkspacesState::row_hit(&self.workspaces.repository_row_areas, position)
        });

        let screen_y = |virtual_y: usize| {
            (virtual_y >= viewport_start && virtual_y < viewport_end).then_some(
                inner
                    .y
                    .saturating_add(virtual_y.saturating_sub(viewport_start) as u16),
            )
        };
        if let Some(y) = screen_y(0) {
            frame.render_widget(
                Paragraph::new("Current").style(theme::section_header()),
                Rect::new(inner.x, y, inner.width, 1),
            );
        }
        if let Some(y) = screen_y(5) {
            frame.render_widget(
                Paragraph::new("─".repeat(inner.width as usize))
                    .style(Style::default().fg(theme::RULE)),
                Rect::new(inner.x, y, inner.width, 1),
            );
        }
        if let Some(y) = screen_y(6) {
            frame.render_widget(
                Paragraph::new("Projects").style(theme::section_header()),
                Rect::new(inner.x, y, inner.width, 1),
            );
            let (label, button_width, button_y) = if inner.width >= 34 {
                ("[ Add Project ]", 15, y)
            } else {
                ("[+]", 3.min(inner.width), y.saturating_add(1))
            };
            self.workspaces.add_project_area = Rect::new(
                inner.right().saturating_sub(button_width),
                button_y,
                button_width,
                1,
            );
            frame.render_widget(
                Paragraph::new(label).style(theme::hover(
                    theme::accent_bold(),
                    area_hovered(self.shell.mouse_position, self.workspaces.add_project_area),
                )),
                self.workspaces.add_project_area,
            );
        }

        let focused = self.focus == PaneFocus::Workspaces;
        for (index, top, height) in row_tops {
            let start = top.max(viewport_start);
            let end = top.saturating_add(height).min(viewport_end);
            if start >= end {
                continue;
            }
            let Some(row) = self.workspaces.repository_rows.get(index) else {
                continue;
            };
            let area = Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(start.saturating_sub(viewport_start) as u16),
                inner.width,
                end.saturating_sub(start) as u16,
            );
            let mut lines = vec![
                self.repository_row_line(row, inner.width),
                self.repository_summary_line(row, inner.width),
            ];
            if let Some(upstream) = self.repository_upstream_line(row, inner.width) {
                lines.push(upstream);
            }
            let lines = lines
                .into_iter()
                .skip(start.saturating_sub(top))
                .take(end.saturating_sub(start))
                .collect::<Vec<_>>();
            let style = if focused && selected == index {
                theme::focus_row()
            } else {
                theme::hover(
                    Style::default().bg(theme::SURFACE_PANEL),
                    hovered == Some(index),
                )
            };
            frame.render_widget(Paragraph::new(lines).style(style), area);

            let actions_visible = hovered == Some(index) || (focused && selected == index);
            let label_visible = top >= viewport_start && top < viewport_end;
            if actions_visible
                && label_visible
                && matches!(row.kind, RepositoryRowKind::Project { .. })
                && inner.width > 3
            {
                let action = Rect::new(
                    inner.right().saturating_sub(3),
                    inner
                        .y
                        .saturating_add(top.saturating_sub(viewport_start) as u16),
                    3,
                    1,
                );
                self.workspaces
                    .repository_remove_areas
                    .push((index, action));
                frame.render_widget(
                    Paragraph::new("[-]").style(theme::hover(
                        theme::error_text(),
                        area_hovered(self.shell.mouse_position, action),
                    )),
                    action,
                );
            }
        }
    }

    pub(super) fn draw_workspace_picker(
        &self,
        frame: &mut Frame<'_>,
        picker: &mut WorkspacePicker,
    ) {
        let height = (picker.rows.len() as u16)
            .saturating_add(WORKSPACE_PICKER_CHROME)
            .clamp(8, 24);
        let inner = widgets::dialog_frame(frame, "Select Workspace", theme::DIALOG_MEDIUM, height);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        picker.list_area = regions[0];
        let hovered = self.shell.mouse_position.and_then(|pointer| {
            picker
                .cursor
                .row_at(regions[0], pointer, picker.rows.len(), 0)
        });
        let items = picker.rows.iter().enumerate().map(|(index, row)| {
            let name = workspace_row_name(row);
            let label = match row.kind {
                RepositoryRowKind::Current => format!("Current · {name}"),
                RepositoryRowKind::Project { .. } => format!(" {name}"),
                RepositoryRowKind::Worktree { .. } => format!("   {name}"),
            };
            let mut spans = if index < 9 {
                vec![
                    Span::styled("[", theme::hint()),
                    Span::styled((index + 1).to_string(), theme::accent_bold()),
                    Span::styled("] ", theme::hint()),
                ]
            } else {
                vec![Span::raw("    ")]
            };
            spans.push(Span::raw(label));
            ListItem::new(Line::from(spans))
                .style(theme::hover(Style::default(), hovered == Some(index)))
        });
        let mut state = ListState::default()
            .with_selected((!picker.rows.is_empty()).then_some(picker.cursor.selected));
        *state.offset_mut() = picker.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[0],
            &mut state,
        );
        picker.cursor.scroll = state.offset();
        frame.render_widget(
            widgets::footer_hint("1–9 Switch · ↑/↓ j/k Move"),
            regions[1],
        );
        picker.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Enter] Switch", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
    }
}

fn repository_row_height(row: &RepositoryRow) -> usize {
    REPOSITORY_ROW_HEIGHT
        + usize::from(
            row.branch_status
                .as_ref()
                .and_then(|branch| branch.upstream.as_ref())
                .is_some(),
        )
}

fn workspace_row_name(row: &RepositoryRow) -> String {
    row.path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| row.path.display().to_string())
}

fn worktree_path_label(path: &Path, project_root: &Path) -> String {
    if path.file_name() == project_root.file_name()
        && let Some(parent) = path.parent()
    {
        return path_label(parent);
    }
    path_label(path)
}

pub(super) fn identity_text(local: Option<&LocalIdentity>, github: bool) -> String {
    let icon = if github { '\u{f408}' } else { '\u{e702}' };
    match local {
        Some(identity) => format!("{icon} {}({})", identity.name, identity.email),
        None => format!("{icon} Local unavailable"),
    }
}

pub(super) fn repository_rows_from_statuses(
    current: &RepositoryRow,
    project_roots: &[PathBuf],
    project_statuses: &[Result<ProjectStatus, String>],
    expanded_projects: &[PathBuf],
) -> Vec<RepositoryRow> {
    let mut rows = vec![current.clone()];
    for (root, status) in project_roots.iter().zip(project_statuses.iter()) {
        match status {
            Ok(status) => {
                rows.push(RepositoryRow {
                    kind: RepositoryRowKind::Project { root: root.clone() },
                    path: status.primary.path.clone(),
                    overview: Some(status.primary.overview),
                    branch_status: Some(status.primary.branch_status.clone()),
                    worktree_count: status.worktrees.len(),
                    error: None,
                    fingerprint: Some(status.primary.fingerprint),
                });
                if expanded_projects.contains(root) {
                    rows.extend(status.worktrees.iter().map(|worktree| RepositoryRow {
                        kind: RepositoryRowKind::Worktree {
                            project_root: root.clone(),
                        },
                        path: worktree.path.clone(),
                        overview: Some(worktree.overview),
                        branch_status: Some(worktree.branch_status.clone()),
                        worktree_count: 0,
                        error: None,
                        fingerprint: Some(worktree.fingerprint),
                    }));
                }
            }
            Err(_) => rows.push(RepositoryRow {
                kind: RepositoryRowKind::Project { root: root.clone() },
                path: root.clone(),
                overview: None,
                branch_status: None,
                worktree_count: 0,
                error: Some("Unavailable".to_owned()),
                fingerprint: None,
            }),
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use crate::git::{
        BranchStatus, ChangeOverview, ChangeSection, LocalIdentity, ReadError, Repository,
    };
    use crate::ui::commands::CommandId;
    use crate::ui::effect::{
        ForegroundRequest, ForegroundResult, HighlightedDiff, RequestId, load_repository_snapshot,
    };
    use crate::ui::files::path_label;
    use crate::ui::overlay::Overlay;
    use crate::ui::shell::{ActiveTab, PaneFocus};
    use crate::ui::syntax::{DiffDocument, SyntaxHighlighter};
    use crate::ui::test_support::{
        buffer_text, click, git, intercept_foreground, offline_app, press, render, row_text,
        temp_registry, temp_repo, wait_for_foreground, wait_for_refresh,
    };
    use crate::ui::{App, theme};

    use super::{
        ProjectMutationOutcome, RepositoryRow, RepositoryRowKind, identity_text,
        worktree_path_label,
    };

    #[test]
    fn leaving_a_repository_closes_the_graph_filter_and_clears_the_query() {
        let mut app = offline_app();
        app.overlay = Overlay::GraphFilter;
        app.graph.query.text = "fix".to_owned();
        app.load_non_repository_context();
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.graph.query.text, "");
    }

    #[test]
    fn project_mutation_failure_shows_a_failure_card() {
        let mut app = offline_app();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        app.dispatch_command(CommandId::AddProject);
        assert!(app.status_bar_text().contains("Choosing Project"));
        let ForegroundRequest::AddProject { id, .. } = request_rx.recv().unwrap() else {
            panic!("expected an AddProject request");
        };

        result_tx
            .send(ForegroundResult::ProjectMutation {
                id,
                result: Err("HERDR_PLUGIN_STATE_DIR is not set; Projects are not saved".to_owned()),
            })
            .unwrap();
        app.receive_foreground_results();

        let card = app.overlay.result().expect("failure card");
        assert!(!card.success);
        assert_eq!(card.title(), "Git operation failed");
        let text = card
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text,
            "HERDR_PLUGIN_STATE_DIR is not set; Projects are not saved"
        );
        assert!(app.foreground.action.is_none());
        assert!(!app.status_bar_text().contains("Choosing Project"));
    }

    #[test]
    fn project_added_uses_a_result_dialog_instead_of_the_status_bar() {
        let mut app = offline_app();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        app.dispatch_command(CommandId::AddProject);
        let ForegroundRequest::AddProject { id, .. } = request_rx.recv().unwrap() else {
            panic!("expected an AddProject request");
        };
        result_tx
            .send(ForegroundResult::ProjectMutation {
                id,
                result: Ok((
                    app.workspaces.project_registry.clone(),
                    ProjectMutationOutcome::Added,
                )),
            })
            .unwrap();
        app.receive_foreground_results();

        let result = app.overlay.result().expect("completion dialog");
        assert!(result.success);
        assert_eq!(result.title(), "Add Project complete");
        assert!(!app.status_bar_text().contains("Project added"));
    }

    fn project_row(root: &Path, error: Option<&str>) -> RepositoryRow {
        RepositoryRow {
            kind: RepositoryRowKind::Project {
                root: root.to_owned(),
            },
            path: root.to_owned(),
            overview: None,
            branch_status: None,
            worktree_count: 0,
            error: error.map(str::to_owned),
            fingerprint: None,
        }
    }

    #[test]
    fn workspaces_pane_marks_focus_with_the_border_and_heads_sections_in_accent() {
        let mut app = offline_app();
        app.workspaces.repository_rows.push(project_row(
            Path::new("/tmp/registered"),
            Some("Unavailable"),
        ));
        app.focus = PaneFocus::Files;
        let buffer = render(&mut app, 200, 34);
        let area = app.workspaces.repository_area;
        assert_eq!(buffer[(area.x, area.y)].fg, Color::Reset);
        let text = buffer_text(&buffer);
        assert!(text.contains("Projects"));
        assert!(!text.contains("Added Repositories"));
        let heading_y = (area.y..area.bottom())
            .find(|y| row_text(&buffer, area, *y).contains("Projects"))
            .expect("Projects heading");
        let heading = &buffer[(area.x + 1, heading_y)];
        assert_eq!(heading.symbol(), "P");
        assert_eq!(heading.fg, theme::ACCENT);
        assert!(heading.modifier.contains(Modifier::BOLD));
        let current_heading = &buffer[(area.x + 1, area.y + 1)];
        assert_eq!(current_heading.symbol(), "C");
        assert_eq!(current_heading.fg, theme::ACCENT);
        let add = app.workspaces.add_project_area;
        assert_eq!(row_text(&buffer, add, add.y), "[ Add Project ]");
        assert_eq!(buffer[(add.x, add.y)].fg, theme::ACCENT);
        assert!(buffer[(add.x, add.y)].modifier.contains(Modifier::BOLD));
        let current = app.workspaces.repository_row_areas[0].1;
        assert!(row_text(&buffer, current, current.y + 1).starts_with("Not a Git repository"));
        assert_eq!(buffer[(current.x, current.y + 1)].fg, theme::WARNING);
        let project = app.workspaces.repository_row_areas[1].1;
        assert!(row_text(&buffer, project, project.y + 1).starts_with("Unavailable"));
        assert_eq!(buffer[(project.x, project.y + 1)].fg, theme::ERROR);
        assert!(app.workspaces.repository_remove_areas.is_empty());

        app.focus = PaneFocus::Workspaces;
        app.workspaces.repository_selected = 1;
        let buffer = render(&mut app, 200, 34);
        assert_eq!(buffer[(area.x, area.y)].fg, theme::ACCENT);
        assert_eq!(buffer[(project.x, project.y)].bg, theme::SURFACE_FOCUS);
        assert!(
            buffer[(project.x, project.y)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let remove = app.workspaces.repository_remove_areas[0].1;
        assert_eq!(row_text(&buffer, remove, remove.y), "[-]");
        assert_eq!(buffer[(remove.x, remove.y)].fg, theme::ERROR);
    }

    #[test]
    fn workspaces_keys_follow_the_list_grammar() {
        let mut app = offline_app();
        app.workspaces
            .repository_rows
            .push(project_row(Path::new("/tmp/one"), None));
        app.workspaces
            .repository_rows
            .push(project_row(Path::new("/tmp/two"), None));
        app.focus = PaneFocus::Workspaces;

        press(&mut app, KeyCode::End);
        assert_eq!(app.workspaces.repository_selected, 2);
        assert!(app.ops.command_context.can_remove_project);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.workspaces.repository_selected, 0);
        assert!(!app.ops.command_context.can_remove_project);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.workspaces.repository_selected, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.workspaces.repository_selected, 2);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.workspaces.repository_selected, 2);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.workspaces.repository_selected, 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.workspaces.repository_selected, 0);

        app.focus = PaneFocus::Files;
        press(&mut app, KeyCode::End);
        assert_eq!(app.workspaces.repository_selected, 0);
    }

    #[test]
    fn workspaces_page_by_the_visible_rows_on_both_tabs() {
        let mut app = offline_app();
        for index in 0..12 {
            app.workspaces
                .repository_rows
                .push(project_row(&PathBuf::from(format!("/tmp/p{index}")), None));
        }
        app.focus = PaneFocus::Workspaces;
        render(&mut app, 120, 40);
        let page = app.workspaces.repository_row_areas.len();
        assert!(page > 1 && page < 12, "visible rows: {page}");

        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.workspaces.repository_selected, page);
        assert_eq!(app.diff.diff_scroll, 0);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.workspaces.repository_selected, 0);
        press(&mut app, KeyCode::End);
        assert_eq!(app.workspaces.repository_selected, 12);

        app.set_tab(ActiveTab::History);
        app.focus = PaneFocus::Workspaces;
        render(&mut app, 120, 40);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.workspaces.repository_selected, 0);
        let page = app.workspaces.repository_row_areas.len();
        assert!(page > 1, "visible rows: {page}");
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.workspaces.repository_selected, page);
        assert_eq!(app.graph.selected, 0);
        press(&mut app, KeyCode::End);
        assert_eq!(app.workspaces.repository_selected, 12);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.workspaces.repository_selected, 12 - page);
        assert_eq!(app.focus, PaneFocus::Workspaces);
    }

    #[test]
    fn repository_identity_uses_directory_names() {
        assert_eq!(
            path_label(Path::new("/Users/example/worktrees/feature-a")),
            "feature-a"
        );
        assert_eq!(
            worktree_path_label(
                Path::new("/Users/example/.worktrees/WISELY-1035/wise"),
                Path::new("/Users/example/wise"),
            ),
            "WISELY-1035"
        );
    }

    #[test]
    fn repository_rows_show_directory_and_checked_out_branch_on_two_lines() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("herdr-git-ui-status-{unique}"));
        let root = parent.join("project");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-m", "Base"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let buffer = render(&mut app, 100, 24);
        let area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 0).then_some(*area))
            .unwrap();
        let line = |row| {
            (area.x..area.right())
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        };

        assert_eq!(area.height, 2);
        assert!(line(area.y).contains("project"));
        assert!(line(area.y + 1).starts_with(&format!("{} main", theme::BRANCH_GLYPH)));
        assert!(!line(area.y + 1).contains('✓'));
        assert!(app.status_bar_text().contains(&root.display().to_string()));

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn repository_status_keeps_working_changes_separate_from_branch_sync() {
        let app = offline_app();
        let row = RepositoryRow {
            kind: RepositoryRowKind::Current,
            path: PathBuf::from("/tmp/project"),
            overview: Some(ChangeOverview {
                changed_paths: 2,
                additions: 10,
                deletions: 3,
            }),
            branch_status: Some(BranchStatus {
                checked_out: "feature/refactor".to_owned(),
                upstream: Some("origin/main".to_owned()),
                ahead: 1,
                behind: 2,
            }),
            worktree_count: 0,
            error: None,
            fingerprint: None,
        };
        let text = app
            .repository_summary_line(&row, 120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(
            text,
            format!("{} feature/refactor · ● 2f +10 -3", theme::BRANCH_GLYPH)
        );
        let upstream = app
            .repository_upstream_line(&row, 120)
            .expect("upstream line")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            upstream,
            format!("{} origin/main · ↑1 ↓2", theme::REMOTE_GLYPH)
        );
    }

    #[test]
    fn synchronized_upstream_renders_with_the_checked_out_branch() {
        let mut app = offline_app();
        let row = RepositoryRow {
            kind: RepositoryRowKind::Current,
            path: PathBuf::from("/tmp/project"),
            overview: Some(ChangeOverview::default()),
            branch_status: Some(BranchStatus {
                checked_out: "main".to_owned(),
                upstream: Some("origin/main".to_owned()),
                ahead: 0,
                behind: 0,
            }),
            worktree_count: 0,
            error: None,
            fingerprint: None,
        };
        let text = app
            .repository_summary_line(&row, 120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, format!("{} main", theme::BRANCH_GLYPH));
        let upstream = app
            .repository_upstream_line(&row, 120)
            .expect("upstream line")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(upstream, format!("{} origin/main · ✓", theme::REMOTE_GLYPH));
        assert_eq!(super::repository_row_height(&row), 3);

        app.workspaces.repository_rows = vec![row];
        let buffer = render(&mut app, 100, 24);
        let area = app.workspaces.repository_row_areas[0].1;
        let line = |row| {
            (area.x..area.right())
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        };
        assert_eq!(area.height, 3);
        assert!(line(area.y + 1).starts_with(&format!("{} main", theme::BRANCH_GLYPH)));
        assert!(line(area.y + 2).starts_with(&format!("{} origin/main · ✓", theme::REMOTE_GLYPH)));
    }

    #[test]
    fn narrow_repository_status_shortens_the_branch_before_hiding_state() {
        let app = offline_app();
        let row = RepositoryRow {
            kind: RepositoryRowKind::Current,
            path: PathBuf::from("/tmp/project"),
            overview: Some(ChangeOverview::default()),
            branch_status: Some(BranchStatus {
                checked_out: "feature/really-long-name".to_owned(),
                upstream: Some("origin/main".to_owned()),
                ahead: 0,
                behind: 14,
            }),
            worktree_count: 0,
            error: None,
            fingerprint: None,
        };
        let text = app
            .repository_upstream_line(&row, 35)
            .expect("upstream line")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.starts_with(&format!("{} ", theme::REMOTE_GLYPH)));
        assert!(text.ends_with(" · ↓14"));
        assert!(!text.contains('✓'));
    }

    #[test]
    fn narrow_diverged_worktree_keeps_icon_status_without_indentation() {
        let app = offline_app();
        let row = RepositoryRow {
            kind: RepositoryRowKind::Worktree {
                project_root: PathBuf::from("/tmp/project"),
            },
            path: PathBuf::from("/tmp/worktree/project"),
            overview: Some(ChangeOverview::default()),
            branch_status: Some(BranchStatus {
                checked_out: "feature/really-long-name".to_owned(),
                upstream: Some("origin/main".to_owned()),
                ahead: 3,
                behind: 11,
            }),
            worktree_count: 0,
            error: None,
            fingerprint: None,
        };
        let text = app
            .repository_upstream_line(&row, 35)
            .expect("upstream line")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.starts_with(&format!("{} ", theme::REMOTE_GLYPH)));
        assert!(text.ends_with(" · ↑3 ↓11"));
        assert!(!text.contains('✓'));
    }

    #[test]
    fn non_repository_current_starts_on_changes_and_switches_to_a_registered_project() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-ui-context-{unique}"));
        let plain = base.join("plain");
        let project = base.join("project");
        fs::create_dir_all(&plain).unwrap();
        fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-b", "main"]);
        git(&project, &["config", "user.name", "Test Author"]);
        git(&project, &["config", "user.email", "test@example.com"]);
        fs::write(project.join("tracked.txt"), "base\n").unwrap();
        git(&project, &["add", "tracked.txt"]);
        git(&project, &["commit", "-m", "Base"]);
        let mut registry = temp_registry("plain-current");
        registry.add(&project).unwrap();

        let mut app = App::load_registered(&plain, registry).unwrap();
        let buffer = render(&mut app, 120, 32);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(app.shell.active_tab, ActiveTab::Changes);
        assert!(app.repository.is_none());
        assert!(rendered.contains("Workspaces"));
        assert!(rendered.contains("Current"));
        assert!(rendered.contains("Not a Git repository"));
        let project_line = app
            .repository_row_line(&app.workspaces.repository_rows[1], 120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(project_line.contains(&path_label(&project)));
        assert!(
            app.workspaces
                .repository_area
                .height
                .abs_diff(app.files.files_area.height)
                <= 1
        );
        assert!(app.files.files_area.y < app.workspaces.repository_area.y);

        app.focus = PaneFocus::Workspaces;
        app.workspaces.repository_selected = 1;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        assert_eq!(
            app.repository.as_ref().unwrap().root(),
            fs::canonicalize(&project).unwrap()
        );
        assert!(app.overlay.result().is_none());
        assert!(
            app.status_bar_text()
                .contains(&fs::canonicalize(&project).unwrap().display().to_string())
        );
        assert!(app.graph.commits.is_empty());
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);
        assert_eq!(app.graph.commits.len(), 1);
        app.set_tab(ActiveTab::Changes);
        wait_for_refresh(&mut app);

        app.workspaces.repository_selected = 0;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(app.repository.is_none());
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Enter);
        render(&mut app, 120, 32);
        let project_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 1).then_some(*area))
            .unwrap();
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: project_area.x.saturating_add(8),
            row: project_area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        wait_for_foreground(&mut app);
        assert!(app.repository.is_some());

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn repository_switch_on_the_files_tab_lists_the_new_repository_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-switch-files-{unique}"));
        let current = base.join("current");
        let registered = base.join("registered");
        for root in [&current, &registered] {
            fs::create_dir_all(root).unwrap();
            git(root, &["init", "-b", "main"]);
            git(root, &["config", "user.name", "Test Author"]);
            git(root, &["config", "user.email", "test@example.com"]);
            fs::write(root.join("tracked.txt"), "base\n").unwrap();
            git(root, &["add", "tracked.txt"]);
            git(root, &["commit", "-m", "Base"]);
        }
        let mut registry = temp_registry("switch-files");
        registry.add(&registered).unwrap();
        let mut app = App::load_registered(&current, registry).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        app.set_tab(ActiveTab::Files);
        while request_rx.try_recv().is_ok() {}

        app.switch_repository_row(1);
        let Ok(ForegroundRequest::Switch {
            id,
            generation,
            path,
        }) = request_rx.try_recv()
        else {
            panic!("repository switch request");
        };
        let snapshot =
            load_repository_snapshot(&path, &SyntaxHighlighter::new(), &|| false).unwrap();
        let new_root = snapshot.repository.root().to_owned();
        result_tx
            .send(ForegroundResult::Switch {
                id,
                generation,
                path,
                result: Ok(Box::new(snapshot)),
            })
            .unwrap();
        app.receive_foreground_results();

        let listed = std::iter::from_fn(|| request_rx.try_recv().ok()).any(|request| {
            matches!(request, ForegroundRequest::RepositoryFiles { root, .. } if root == new_root)
        });
        assert!(
            listed,
            "the Files tab requests the switched repository's files"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn repository_switch_enqueues_then_atomically_applies_only_the_matching_snapshot() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-switch-async-{unique}"));
        let current = base.join("current");
        let registered = base.join("registered");
        for root in [&current, &registered] {
            fs::create_dir_all(root).unwrap();
            git(root, &["init", "-b", "main"]);
            git(root, &["config", "user.name", "Test Author"]);
            git(root, &["config", "user.email", "test@example.com"]);
            fs::write(root.join("tracked.txt"), "base\n").unwrap();
            git(root, &["add", "tracked.txt"]);
            git(root, &["commit", "-m", "Base"]);
        }
        fs::write(registered.join("tracked.txt"), "base\nregistered\n").unwrap();
        let mut registry = temp_registry("stale-switch");
        registry.add(&registered).unwrap();
        let mut app = App::load_registered(&current, registry).unwrap();
        let old_root = app.repository.as_ref().unwrap().root().to_owned();
        let old_diff = app.diff.diff_text.clone();
        let (request_rx, result_tx) = intercept_foreground(&mut app);

        app.switch_repository_row(1);
        assert_eq!(app.repository.as_ref().unwrap().root(), old_root);
        assert_eq!(app.diff.diff_text, old_diff);
        let request = request_rx.try_recv().expect("repository switch request");
        let (id, generation, path) = match request {
            ForegroundRequest::Switch {
                id,
                generation,
                path,
            } => (id, generation, path),
            other => panic!("unexpected request: {other:?}"),
        };
        let row = app.repository_row_line(&app.workspaces.repository_rows[1], 120);
        assert_eq!(
            row.spans[0].content.as_ref(),
            theme::spinner_frame(Duration::ZERO)
        );
        assert_eq!(row.spans[0].style.fg, Some(theme::ACCENT));
        app.foreground.action.as_mut().unwrap().started =
            Instant::now() - theme::SPINNER_FRAME_DURATION;
        let advanced_row = app.repository_row_line(&app.workspaces.repository_rows[1], 120);
        assert_eq!(
            advanced_row.spans[0].content.as_ref(),
            theme::spinner_frame(theme::SPINNER_FRAME_DURATION)
        );
        app.handle(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.shell.active_tab, ActiveTab::History);
        assert_eq!(app.repository.as_ref().unwrap().root(), old_root);

        let snapshot =
            load_repository_snapshot(&path, &SyntaxHighlighter::new(), &|| false).unwrap();
        result_tx
            .send(ForegroundResult::Switch {
                id: RequestId::new(id.get().wrapping_add(1)),
                generation,
                path: path.clone(),
                result: Err(ReadError::Diagnostic("stale".to_owned())),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(app.repository.as_ref().unwrap().root(), old_root);
        assert!(app.foreground.action.is_some());

        result_tx
            .send(ForegroundResult::Switch {
                id,
                generation,
                path: path.clone(),
                result: Ok(Box::new(snapshot)),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(app.repository.as_ref().unwrap().root(), path);
        assert!(app.overlay.result().is_none());
        assert!(app.diff.diff_text.contains("registered"));
        assert!(!app.graph.history_loaded);
        assert!(app.graph.commits.is_empty());
        assert!(app.foreground.action.is_none());

        let registered_root = app.repository.as_ref().unwrap().root().to_owned();
        let registered_diff = app.diff.diff_text.clone();
        app.switch_repository_row(0);
        let request = request_rx.try_recv().expect("second switch request");
        let (second_id, second_generation, second_path) = match request {
            ForegroundRequest::Switch {
                id,
                generation,
                path,
            } => (id, generation, path),
            other => panic!("unexpected request: {other:?}"),
        };
        result_tx
            .send(ForegroundResult::Switch {
                id: second_id,
                generation: second_generation,
                path: second_path,
                result: Err(ReadError::Diagnostic("unavailable".to_owned())),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(app.repository.as_ref().unwrap().root(), registered_root);
        assert_eq!(app.diff.diff_text, registered_diff);
        assert!(app.foreground.action.is_none());
        let result = app.overlay.result().expect("switch failure dialog");
        assert!(!result.success);
        assert_eq!(result.title(), "Repository switch failed");

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn latest_repository_switch_cancels_and_replaces_the_running_read() {
        let root = temp_repo("switch-latest");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        let current = app.workspaces.repository_rows[0].clone();
        app.workspaces.repository_rows = vec![
            current,
            RepositoryRow {
                kind: RepositoryRowKind::Project {
                    root: first.clone(),
                },
                path: first.clone(),
                overview: Some(ChangeOverview::default()),
                branch_status: None,
                worktree_count: 0,
                error: None,
                fingerprint: None,
            },
            RepositoryRow {
                kind: RepositoryRowKind::Project {
                    root: second.clone(),
                },
                path: second.clone(),
                overview: Some(ChangeOverview::default()),
                branch_status: None,
                worktree_count: 0,
                error: None,
                fingerprint: None,
            },
        ];
        let (request_rx, result_tx) = intercept_foreground(&mut app);

        app.switch_repository_row(1);
        let ForegroundRequest::Switch {
            id,
            generation: first_generation,
            path,
        } = request_rx.try_recv().expect("first switch")
        else {
            panic!("unexpected request")
        };
        assert_eq!(path, first);

        app.switch_repository_row(2);
        assert!(app.foreground.reads.switch.generation > first_generation);
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let old_marker = app
            .repository_row_line(&app.workspaces.repository_rows[1], 120)
            .spans[0]
            .content
            .to_string();
        let latest_marker = app
            .repository_row_line(&app.workspaces.repository_rows[2], 120)
            .spans[0]
            .content
            .to_string();
        assert!(!theme::SPINNER_FRAMES.contains(&old_marker.as_str()));
        assert!(theme::SPINNER_FRAMES.contains(&latest_marker.as_str()));
        assert_eq!(
            app.repository_summary_line(&app.workspaces.repository_rows[2], 120)
                .to_string(),
            "Loading repository"
        );
        result_tx
            .send(ForegroundResult::Switch {
                id,
                generation: first_generation,
                path,
                result: Err(ReadError::Cancelled),
            })
            .unwrap();
        app.receive_foreground_results();

        let ForegroundRequest::Switch {
            generation: second_generation,
            path,
            ..
        } = request_rx.try_recv().expect("latest switch")
        else {
            panic!("unexpected request")
        };
        assert_eq!(path, second);
        assert!(second_generation > first_generation);
        assert!(app.foreground.action.is_some());
        assert!(app.overlay.result().is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selecting_the_active_repository_cancels_a_pending_switch() {
        let root = temp_repo("switch-cancel");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let target = root.join("target");
        let current = app.workspaces.repository_rows[0].clone();
        app.workspaces.repository_rows.push(RepositoryRow {
            kind: RepositoryRowKind::Project {
                root: target.clone(),
            },
            path: target,
            overview: Some(ChangeOverview::default()),
            branch_status: None,
            worktree_count: 0,
            error: None,
            fingerprint: None,
        });
        assert_eq!(app.workspaces.repository_rows[0], current);
        let (request_rx, _result_tx) = intercept_foreground(&mut app);

        app.switch_repository_row(0);
        assert!(app.overlay.result().is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        app.switch_repository_row(1);
        let ForegroundRequest::Switch { generation, .. } =
            request_rx.try_recv().expect("switch request")
        else {
            panic!("unexpected request")
        };
        app.switch_repository_row(0);

        assert!(app.foreground.reads.switch.generation > generation);
        assert!(app.workspaces.pending_switch.is_none());
        assert_eq!(app.active_path, current.path);
        assert!(app.overlay.result().is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn alt_w_numbers_same_path_current_and_project_and_selects_exact_row() {
        let root = temp_repo("workspace-picker");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nchanged\n").unwrap();

        let mut registry = temp_registry("picker");
        registry.add(&root).unwrap();
        let mut app = App::load_registered(&root, registry).unwrap();
        let rows = app.workspace_rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0].kind, RepositoryRowKind::Current));
        assert!(matches!(rows[1].kind, RepositoryRowKind::Project { .. }));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        let Overlay::Workspace(picker) = &app.overlay else {
            panic!("Alt+W opens Select Workspace");
        };
        assert_eq!(picker.rows.len(), 2);
        assert_eq!(picker.rows[0].path.file_name(), root.file_name());
        let buffer = render(&mut app, 100, 35);
        let rendered = buffer
            .content()
            .iter()
            .fold(String::new(), |mut text, cell| {
                text.push_str(cell.symbol());
                text
            });
        assert!(rendered.contains("Select Workspace"));
        assert!(!rendered.contains("Search"));
        assert!(rendered.contains("[1] Current"));
        assert!(rendered.contains("[2] "));
        assert!(rendered.contains("1–9 Switch"));
        assert!(rendered.contains("[Enter] Switch"));
        assert!(rendered.contains("[Esc] Close"));
        let picker = app.overlay.workspace().unwrap();
        assert_eq!(picker.list_area.width, theme::DIALOG_MEDIUM - 2);
        let status_y = picker.list_area.bottom();
        assert_eq!(buffer[(picker.list_area.x, status_y)].fg, theme::HINT);
        let selected_y = picker.list_area.y;
        assert_eq!(buffer[(picker.list_area.x, selected_y)].symbol(), "▶");
        assert_eq!(
            buffer[(picker.list_area.x, selected_y)].bg,
            theme::SURFACE_FOCUS
        );
        assert_eq!(
            buffer[(picker.list_area.x + 3, selected_y)].fg,
            theme::ACCENT,
            "digit shortcut is accented"
        );

        press(&mut app, KeyCode::Char('9'));
        let Overlay::Workspace(picker) = &app.overlay else {
            panic!("an out-of-range digit keeps Select Workspace open");
        };
        assert_eq!(picker.cursor.selected, 0);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.overlay.workspace().unwrap().cursor.selected, 1);
        press(&mut app, KeyCode::Char(' '));
        let picker = app
            .overlay
            .workspace()
            .expect("Space does not activate Select Workspace");
        assert_eq!(picker.cursor.selected, 1);
        assert_eq!(app.workspaces.repository_selected, 0);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.overlay.workspace().unwrap().cursor.selected, 0);

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(app.overlay.workspace().is_none());
        assert_eq!(app.workspaces.repository_selected, 1);
        assert!(app.ops.command_context.can_remove_project);
        assert_eq!(
            app.repository.as_ref().unwrap().root(),
            fs::canonicalize(&root).unwrap()
        );
        assert!(app.foreground.action.is_none());

        app.open_workspace_picker();
        render(&mut app, 100, 35);
        let list_area = app.overlay.workspace().unwrap().list_area;
        click(
            &mut app,
            list_area.x.saturating_add(4),
            list_area.y.saturating_add(1),
        );
        assert!(app.overlay.workspace().is_none());
        assert_eq!(app.workspaces.repository_selected, 1);

        app.open_workspace_picker();
        render(&mut app, 100, 35);
        let buttons = app.overlay.workspace().unwrap().buttons;
        click(&mut app, buttons.secondary.x + 1, buttons.secondary.y + 1);
        assert!(app.overlay.workspace().is_none(), "[Esc] Close closes");
        assert_eq!(app.workspaces.repository_selected, 1);

        app.workspaces.repository_selected = 0;
        app.open_workspace_picker();
        press(&mut app, KeyCode::Down);
        render(&mut app, 100, 35);
        let buttons = app.overlay.workspace().unwrap().buttons;
        click(&mut app, buttons.primary.x + 1, buttons.primary.y + 1);
        assert!(app.overlay.workspace().is_none(), "[Enter] Switch switches");
        assert_eq!(app.workspaces.repository_selected, 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn alt_w_digit_enqueues_a_distinct_repository_switch() {
        let root = temp_repo("workspace-number");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let target = root.join("registered");
        let target_row = RepositoryRow {
            kind: RepositoryRowKind::Project {
                root: target.clone(),
            },
            path: target.clone(),
            overview: Some(ChangeOverview::default()),
            branch_status: None,
            worktree_count: 0,
            error: None,
            fingerprint: None,
        };
        app.workspaces.repository_rows.push(target_row.clone());
        let (request_rx, _result_tx) = intercept_foreground(&mut app);

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        if let Overlay::Workspace(picker) = &mut app.overlay {
            picker.rows.push(target_row);
        }
        let started = Instant::now();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(app.overlay.workspace().is_none());
        let ForegroundRequest::Switch { path, .. } =
            request_rx.try_recv().expect("numbered workspace switch")
        else {
            panic!("unexpected request")
        };
        assert_eq!(path, target);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_rows_never_expose_staging_actions() {
        let root = temp_repo("ui-top-stage");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nchanged\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let buffer = render(&mut app, 120, 30);
        let current_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 0).then_some(*area))
            .unwrap();
        let summary_row = current_area.y + 1;
        let repository_summary = |buffer: &Buffer| {
            (0..120)
                .map(|column| buffer[(column, summary_row)].symbol())
                .collect::<String>()
        };
        let summary = repository_summary(&buffer);
        assert!(summary.contains("+2"));
        assert!(!summary.contains("-0"));
        app.shell.mouse_position = Some((current_area.x, current_area.y));
        let buffer = render(&mut app, 120, 30);
        let hovered = repository_summary(&buffer);
        assert!(hovered.contains("+2"));
        assert!(!hovered.contains("-0"));

        app.focus = PaneFocus::Workspaces;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(
            app.files
                .changes
                .iter()
                .all(|change| change.section == ChangeSection::Unstaged)
        );
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\nchanged\n"
        );
        assert_eq!(fs::read_to_string(root.join("new.txt")).unwrap(), "new\n");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repository_kinds_share_one_surface_and_replace_kind_icons_when_active() {
        let root = temp_repo("ui-row-surface");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let project_one = root.join("registered-one");
        let worktree_one = project_one.join("feature-one");
        let project_two = root.join("registered-two");
        app.workspaces.repository_rows.push(RepositoryRow {
            kind: RepositoryRowKind::Project {
                root: project_one.clone(),
            },
            path: project_one.clone(),
            overview: None,
            branch_status: None,
            worktree_count: 1,
            error: None,
            fingerprint: None,
        });
        app.workspaces.repository_rows.push(RepositoryRow {
            kind: RepositoryRowKind::Worktree {
                project_root: project_one.clone(),
            },
            path: worktree_one,
            overview: None,
            branch_status: None,
            worktree_count: 0,
            error: None,
            fingerprint: None,
        });
        app.workspaces.repository_rows.push(RepositoryRow {
            kind: RepositoryRowKind::Project {
                root: project_two.clone(),
            },
            path: project_two,
            overview: None,
            branch_status: None,
            worktree_count: 0,
            error: None,
            fingerprint: None,
        });
        let assert_background = |buffer: &Buffer, area: Rect, expected: Color| {
            for row in area.y..area.y.saturating_add(super::REPOSITORY_ROW_HEIGHT as u16) {
                for column in area.x..area.right() {
                    assert_eq!(
                        buffer[(column, row)].bg,
                        expected,
                        "unexpected background at ({column}, {row})"
                    );
                }
            }
        };

        let buffer = render(&mut app, 100, 50);
        let repository_lines = (app.workspaces.repository_area.y
            ..app.workspaces.repository_area.bottom())
            .map(|row| {
                (app.workspaces.repository_area.x..app.workspaces.repository_area.right())
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let current_heading = repository_lines
            .iter()
            .position(|line| line.contains("Current"))
            .expect("Current heading");
        let added_heading = repository_lines
            .iter()
            .position(|line| line.contains("Projects"))
            .expect("Projects heading");
        let current_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 0).then_some(*area))
            .expect("Current row area");
        let current_row = current_area
            .y
            .saturating_sub(app.workspaces.repository_area.y) as usize;
        let separator = added_heading.saturating_sub(1);
        assert!(current_heading < current_row);
        assert!(current_row < separator);
        assert!(separator < added_heading);
        assert!(
            repository_lines[separator].matches('─').count()
                >= app.workspaces.repository_list_area.width as usize
        );
        let row_y = |target| {
            app.workspaces
                .repository_row_areas
                .iter()
                .find_map(|(index, area)| (*index == target).then_some(area.y))
                .expect("visible repository row")
        };
        let first_project = row_y(1);
        let first_worktree = row_y(2);
        let second_project = row_y(3);
        assert_eq!(first_worktree, first_project + 2);
        assert_eq!(second_project, first_worktree + 3);
        let gap_row = first_worktree
            .saturating_add(2)
            .saturating_sub(app.workspaces.repository_area.y) as usize;
        assert!(
            repository_lines[gap_row]
                .trim_matches(['│', ' '])
                .is_empty()
        );
        let selected_before_gap_click = app.workspaces.repository_selected;
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.workspaces.repository_list_area.x.saturating_add(1),
            row: first_worktree.saturating_add(2),
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert_eq!(
            app.workspaces.repository_selected,
            selected_before_gap_click
        );
        let row_area = current_area;
        assert_background(&buffer, row_area, theme::SURFACE_PANEL);
        let active_line =
            app.repository_row_line(&app.workspaces.repository_rows[0], row_area.width);
        assert_eq!(active_line.spans[0].content.as_ref(), "▶ ");
        let inactive_project =
            app.repository_row_line(&app.workspaces.repository_rows[1], row_area.width);
        assert_eq!(inactive_project.spans[0].content.as_ref(), " ");
        let inactive_worktree =
            app.repository_row_line(&app.workspaces.repository_rows[2], row_area.width);
        assert_eq!(inactive_worktree.spans[0].content.as_ref(), " ");
        for span in active_line.spans.iter().skip(1) {
            assert_ne!(span.style.fg, Some(theme::ACCENT));
            assert!(!span.style.add_modifier.contains(Modifier::BOLD));
        }
        let summary_line =
            app.repository_summary_line(&app.workspaces.repository_rows[0], row_area.width);
        let summary_text = summary_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!summary_text.contains('─'));

        app.active_path = root.join("inactive");
        let buffer = render(&mut app, 100, 50);
        assert_background(&buffer, row_area, theme::SURFACE_PANEL);
        let inactive_line =
            app.repository_row_line(&app.workspaces.repository_rows[0], row_area.width);
        assert_eq!(inactive_line.spans[0].content.as_ref(), " ");

        app.active_path = app.workspaces.repository_rows[2].path.clone();
        let active_worktree =
            app.repository_row_line(&app.workspaces.repository_rows[2], row_area.width);
        assert_eq!(active_worktree.spans[0].content.as_ref(), "▶ ");

        app.shell.mouse_position = Some((row_area.x, row_area.y + 1));
        let buffer = render(&mut app, 100, 50);
        assert_background(&buffer, row_area, theme::SURFACE_HOVER);

        app.shell.mouse_position = None;
        app.focus = PaneFocus::Workspaces;
        app.workspaces.repository_selected = 0;
        let buffer = render(&mut app, 100, 50);
        assert_background(&buffer, row_area, theme::SURFACE_FOCUS);

        app.workspaces.repository_selected = 3;
        render(&mut app, 100, 18);
        let last_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 3).then_some(*area))
            .expect("selected Project remains visible after scrolling");
        assert_eq!(last_area.height, super::REPOSITORY_ROW_HEIGHT as u16);
        assert!(app.workspaces.repository_scroll > 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_accordion_discovers_worktrees_and_remove_keeps_directories() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-ui-worktrees-{unique}"));
        let root = base.join("project");
        let linked = base.join("feature-a");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "feature-a",
                linked.to_str().unwrap(),
            ],
        );
        fs::write(linked.join("tracked.txt"), "base\nfeature\n").unwrap();
        let mut registry = temp_registry("linked-worktree");
        registry.add(&root).unwrap();
        let mut app = App::load_registered(&root, registry).unwrap();

        assert_eq!(app.workspaces.repository_rows.len(), 2);
        assert_eq!(app.workspaces.repository_rows[1].worktree_count, 1);
        let workspace_rows = app.workspace_rows();
        assert_eq!(workspace_rows.len(), 3);
        assert!(matches!(workspace_rows[0].kind, RepositoryRowKind::Current));
        assert!(matches!(
            workspace_rows[1].kind,
            RepositoryRowKind::Project { .. }
        ));
        assert!(matches!(
            workspace_rows[2].kind,
            RepositoryRowKind::Worktree { .. }
        ));
        assert!(
            workspace_rows
                .iter()
                .any(|row| row.path == fs::canonicalize(&linked).unwrap())
        );
        let collapsed_project = app.repository_row_line(&app.workspaces.repository_rows[1], 200);
        let collapsed_text = collapsed_project
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!collapsed_text.contains('▸'));
        assert!(!collapsed_text.contains('▾'));
        assert_eq!(
            collapsed_project.spans[1].content.as_ref(),
            path_label(&root)
        );
        let narrow_project = app
            .repository_row_line(&app.workspaces.repository_rows[1], 20)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(narrow_project.contains(&path_label(&root)));
        assert!(!narrow_project.contains('…'));
        render(&mut app, 200, 50);
        let project_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 1).then_some(*area))
            .unwrap();
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: project_area.x.saturating_add(8),
            row: project_area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert!(app.overlay.result().is_none());
        assert!(app.foreground.action.is_none());
        assert_eq!(app.workspaces.repository_rows.len(), 3);
        assert_eq!(
            app.repository.as_ref().unwrap().root(),
            fs::canonicalize(&root).unwrap()
        );

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.workspaces.repository_rows.len(), 2);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.workspaces.repository_rows.len(), 3);
        assert!(matches!(
            app.workspaces.repository_rows[2].kind,
            RepositoryRowKind::Worktree { .. }
        ));
        assert_eq!(
            app.workspaces.repository_rows[2]
                .overview
                .unwrap()
                .changed_paths,
            1
        );
        assert_eq!(app.workspace_rows(), workspace_rows);
        let wide_project = app
            .repository_row_line(&app.workspaces.repository_rows[1], 200)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(wide_project.contains("1 worktree"));

        app.focus = PaneFocus::Files;
        let buffer = render(&mut app, 200, 50);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let row_text = |row: u16| {
            (0..200)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        };
        let current_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 0).then_some(*area))
            .unwrap();
        let project_area = app
            .workspaces
            .repository_row_areas
            .iter()
            .find_map(|(index, area)| (*index == 1).then_some(*area))
            .unwrap();
        let current_label = row_text(current_area.y);
        let current_summary = row_text(current_area.y + 1);
        let project_label = row_text(project_area.y);
        let project_summary = row_text(project_area.y + 1);
        assert!(rendered.contains("Current"));
        assert!(current_label.contains("project"));
        assert_eq!(
            buffer[(current_area.x, current_area.y + 1)].symbol(),
            theme::BRANCH_GLYPH
        );
        assert!(!current_label.contains("+0 -0"));
        assert!(!current_summary.contains("0f"));
        assert!(!current_summary.contains("+0"));
        assert!(!current_summary.contains("-0"));
        assert!(project_label.contains("project"));
        assert_eq!(
            buffer[(project_area.x, project_area.y + 1)].symbol(),
            theme::BRANCH_GLYPH
        );
        assert!(!project_label.contains("+0 -0"));
        assert!(!project_summary.contains("0f"));
        assert!(!project_summary.contains("+0"));
        assert!(!project_summary.contains("-0"));
        assert!(!rendered.contains("1W · 0"));
        assert!(rendered.contains("feature-a"));
        assert!(rendered.contains("+1"));

        app.focus = PaneFocus::Workspaces;
        app.workspaces.repository_selected = 1;
        render(&mut app, 200, 50);
        let remove = app
            .workspaces
            .repository_remove_areas
            .iter()
            .find_map(|(index, area)| (*index == 1).then_some(*area))
            .unwrap();
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: remove.x.saturating_add(1),
            row: remove.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::RemoveProject { .. }));
        let dialog = buffer_text(&render(&mut app, 200, 50));
        assert!(dialog.contains("Remove this Project from Workspaces?"));
        assert!(dialog.contains("Files and worktrees stay on disk."));
        let registered_root = fs::canonicalize(&root).unwrap();
        assert!(
            app.workspaces
                .project_registry
                .roots()
                .contains(&registered_root)
        );

        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(
            app.workspaces
                .project_registry
                .roots()
                .contains(&registered_root)
        );

        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: remove.x.saturating_add(1),
            row: remove.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert_eq!(app.workspaces.repository_rows.len(), 1);
        assert!(root.exists());
        assert!(linked.exists());

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn status_identity_uses_the_github_icon_only_for_github_origins() {
        let local = LocalIdentity {
            name: "Local Name".to_owned(),
            email: "local@example.com".to_owned(),
        };
        assert_eq!(
            identity_text(Some(&local), true),
            "\u{f408} Local Name(local@example.com)"
        );
        assert_eq!(
            identity_text(Some(&local), false),
            "\u{e702} Local Name(local@example.com)"
        );
    }

    #[test]
    fn switching_repositories_rejects_every_in_flight_read() {
        let root = temp_repo("switch-rejects-reads");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "latest\n").unwrap();
        git(&root, &["commit", "-am", "Latest"]);
        fs::write(root.join("tracked.txt"), "edited\n").unwrap();
        let other = temp_repo("switch-rejects-reads-target");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        let old_diff = app.diff.diff_text.clone();
        let old_sha = app.inspect.details.as_ref().unwrap().commit.sha.clone();

        app.move_selection(1);
        let ForegroundRequest::CommitDetails {
            id: details_id,
            generation: details_generation,
            target: details_target,
        } = request_rx.try_recv().expect("commit details request")
        else {
            panic!("unexpected request")
        };
        app.request_file_diff();
        let ForegroundRequest::Diff {
            id: diff_id,
            generation: diff_generation,
            owner,
            path,
            file,
            target,
            fold_toggles,
        } = request_rx.try_recv().expect("file diff request")
        else {
            panic!("unexpected request")
        };
        let row = (0..app.diff.diff_document.len())
            .find(|&row| app.diff.diff_document.after_line_number(row).is_some())
            .expect("diff line");
        app.diff.focused_diff_row = row;
        app.load_blame_for_row(row);
        let ForegroundRequest::Blame {
            id: blame_id,
            generation: blame_generation,
            path: blame_path,
            file: blame_file,
            line,
            revision,
        } = request_rx.try_recv().expect("blame request")
        else {
            panic!("unexpected request")
        };
        app.workspaces.repository_rows.push(RepositoryRow {
            kind: RepositoryRowKind::Project {
                root: other.clone(),
            },
            path: other.clone(),
            overview: Some(ChangeOverview::default()),
            branch_status: None,
            worktree_count: 0,
            error: None,
            fingerprint: None,
        });

        app.switch_repository_row(app.workspaces.repository_rows.len() - 1);

        assert!(app.foreground.reads.diff.generation > diff_generation);
        assert!(app.foreground.reads.details.generation > details_generation);
        assert!(app.foreground.reads.blame.generation > blame_generation);
        let repository = Repository::discover(&root).unwrap();
        result_tx
            .send(ForegroundResult::Diff {
                id: diff_id,
                generation: diff_generation,
                owner,
                path,
                file,
                target,
                fold_toggles,
                result: Ok((
                    "late diff".to_owned(),
                    HighlightedDiff {
                        language: "Plain text".to_owned(),
                        split: DiffDocument::plain("late diff"),
                        key: 0,
                    },
                )),
            })
            .unwrap();
        result_tx
            .send(ForegroundResult::CommitDetails {
                id: details_id,
                generation: details_generation,
                result: Box::new(Ok(repository.details(&details_target.commit).unwrap())),
                target: details_target,
            })
            .unwrap();
        result_tx
            .send(ForegroundResult::Blame {
                id: blame_id,
                generation: blame_generation,
                path: blame_path,
                file: blame_file,
                line,
                revision,
                result: Err(ReadError::Diagnostic("late blame".to_owned())),
            })
            .unwrap();
        app.receive_foreground_results();

        assert_eq!(app.diff.diff_text, old_diff);
        assert_eq!(app.inspect.details.as_ref().unwrap().commit.sha, old_sha);
        assert!(app.diff.blame.is_none());
        assert!(app.diff.blame_error.is_none());

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }
}
