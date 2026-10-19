use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::git::{BranchInfo, GitOperation};
use crate::ui::effect::{ForegroundRequest, RequestId};
use crate::ui::lanes::ForegroundKind;
use crate::ui::overlay::Overlay;
use crate::ui::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerEdit, PickerOutcome, PickerState,
    TextEdit, TextField, left_click, picker_edit, update_picker,
};
use crate::ui::{App, theme};

use super::{CommandId, search_box};

#[derive(Clone, Copy)]
enum BranchAction {
    New,
    Rename,
    Delete,
    Refresh,
    Close,
}

const ACTIONS: [BranchAction; 5] = [
    BranchAction::New,
    BranchAction::Rename,
    BranchAction::Delete,
    BranchAction::Refresh,
    BranchAction::Close,
];

#[derive(Debug)]
enum BranchManagerState {
    List,
    Rename {
        branch: BranchInfo,
        input: TextField,
        buttons: ConfirmButtons,
    },
    Delete {
        branch: BranchInfo,
        buttons: ConfirmButtons,
    },
}

#[derive(Debug)]
pub(in crate::ui) struct BranchManager {
    path: PathBuf,
    entries: Vec<BranchInfo>,
    rows: Vec<usize>,
    search_keys: Vec<String>,
    query: TextField,
    cursor: ListCursor,
    list_area: Rect,
    action_areas: [Rect; 5],
    status_area: Rect,
    status_scroll: u16,
    status_max_scroll: u16,
    loading: bool,
    read_error: Option<String>,
    message: Option<(bool, String)>,
    preferred_ref: Option<String>,
    state: BranchManagerState,
}

impl BranchManager {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            rows: Vec::new(),
            search_keys: Vec::new(),
            query: TextField::new(),
            cursor: ListCursor::default(),
            list_area: Rect::default(),
            action_areas: [Rect::default(); 5],
            status_area: Rect::default(),
            status_scroll: 0,
            status_max_scroll: 0,
            loading: true,
            read_error: None,
            message: None,
            preferred_ref: None,
            state: BranchManagerState::List,
        }
    }

    pub(in crate::ui) fn is_list(&self) -> bool {
        matches!(self.state, BranchManagerState::List)
    }

    fn scroll_status(&mut self, input: &Event) -> bool {
        let delta = match input {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    _ => return false,
                }
            }
            Event::Mouse(mouse) if self.status_area.contains((mouse.column, mouse.row).into()) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => -1,
                    MouseEventKind::ScrollDown => 1,
                    _ => return false,
                }
            }
            _ => return false,
        };
        self.status_scroll = self
            .status_scroll
            .saturating_add_signed(delta)
            .min(self.status_max_scroll);
        true
    }

    fn selected(&self) -> Option<&BranchInfo> {
        self.rows
            .get(self.cursor.selected)
            .map(|index| &self.entries[*index])
    }

    fn filter(&mut self) {
        let query = self.query.text.to_lowercase();
        self.rows.clear();
        self.rows.extend(
            self.search_keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| key.contains(&query).then_some(index)),
        );
        self.cursor.clamp(self.rows.len());
    }

    fn replace_entries(&mut self, mut entries: Vec<BranchInfo>) {
        entries.sort_by(|left, right| {
            (!left.is_current, !left.is_local(), left.name()).cmp(&(
                !right.is_current,
                !right.is_local(),
                right.name(),
            ))
        });
        self.search_keys = entries
            .iter()
            .map(|branch| branch.reference.to_lowercase())
            .collect();
        self.entries = entries;
        self.filter();
        if let Some(reference) = self.preferred_ref.take()
            && let Some(row) = self
                .rows
                .iter()
                .position(|index| self.entries[*index].reference == reference)
        {
            self.cursor.selected = row;
        }
    }

    fn action_allowed(
        &self,
        action: BranchAction,
        context: &super::CommandContext,
    ) -> Result<(), &'static str> {
        if matches!(action, BranchAction::Close) {
            return Ok(());
        }
        if self.loading {
            return Err("Loading branches. Please wait.");
        }
        match action {
            BranchAction::New => {
                let availability = CommandId::CreateBranchAtHead.availability(context);
                if availability.enabled {
                    Ok(())
                } else {
                    Err(availability.reason.unwrap_or("New is unavailable."))
                }
            }
            BranchAction::Rename | BranchAction::Delete => {
                let branch = self.selected().ok_or("Select a local branch.")?;
                if !branch.is_local() {
                    return Err("Only local branches can be renamed or deleted.");
                }
                if matches!(action, BranchAction::Delete)
                    && (branch.is_current || branch.worktree_path.is_some())
                {
                    return Err("Cannot delete a branch checked out in a worktree.");
                }
                if matches!(action, BranchAction::Rename)
                    && !branch.is_current
                    && branch.worktree_path.is_some()
                {
                    return Err("Cannot rename a branch checked out in another worktree.");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl App {
    pub(in crate::ui) fn open_branch_manager(&mut self) {
        let Some(repository) = &self.repository else {
            return;
        };
        self.overlay = Overlay::Branches(BranchManager::new(repository.root().to_owned()));
        self.request_branch_list();
    }

    pub(in crate::ui) fn request_branch_list(&mut self) {
        let Overlay::Branches(manager) = &mut self.overlay else {
            return;
        };
        if self.foreground.action.is_some() {
            return;
        }
        if manager.preferred_ref.is_none() {
            manager.preferred_ref = manager.selected().map(|branch| branch.reference.clone());
        }
        manager.loading = true;
        manager.status_scroll = 0;
        let path = manager.path.clone();
        let id = self.foreground.next_id;
        if let Err(error) = self.foreground.request_foreground(
            ForegroundRequest::BranchList {
                id,
                path: path.clone(),
            },
            ForegroundKind::BranchList { path },
        ) {
            manager.loading = false;
            manager.entries.clear();
            manager.rows.clear();
            manager.search_keys.clear();
            manager.read_error = Some(format!("F5 to retry. Git worker stopped: {error}"));
        }
    }

    pub(in crate::ui) fn apply_branch_list(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<Vec<BranchInfo>, String>,
    ) {
        if self.foreground.take_matching_action(id, |kind| {
            matches!(kind, ForegroundKind::BranchList { path: active } if active == &path)
        }).is_none() {
            return;
        }
        if self
            .repository
            .as_ref()
            .is_none_or(|repository| repository.root() != path)
        {
            return;
        }
        let Overlay::Branches(manager) = &mut self.overlay else {
            return;
        };
        if manager.path != path {
            return;
        }
        manager.loading = false;
        match result {
            Ok(entries) => {
                manager.read_error = None;
                manager.replace_entries(entries);
            }
            Err(error) => {
                manager.entries.clear();
                manager.rows.clear();
                manager.search_keys.clear();
                manager.cursor = ListCursor::default();
                manager.read_error = Some(format!("F5 to retry. {error}"));
            }
        }
    }

    pub(in crate::ui) fn finish_branch_operation(
        &mut self,
        operation: &GitOperation,
        success: bool,
        output: &str,
    ) -> bool {
        if !matches!(
            operation,
            GitOperation::RenameBranch { .. } | GitOperation::DeleteBranch { .. }
        ) {
            return false;
        }
        let Overlay::Branches(manager) = &mut self.overlay else {
            return false;
        };
        if self
            .repository
            .as_ref()
            .is_none_or(|repository| repository.root() != manager.path)
        {
            return false;
        }
        manager.status_scroll = 0;
        if !success && matches!(operation, GitOperation::RenameBranch { .. }) {
            if let BranchManagerState::Rename { input, .. } = &mut manager.state {
                input.set_error(output);
            } else {
                manager.message = Some((false, output.to_owned()));
            }
            return true;
        }
        manager.state = BranchManagerState::List;
        manager.message = Some((
            success,
            if output.is_empty() {
                if success {
                    "Branch updated."
                } else {
                    "Branch change failed."
                }
                .to_owned()
            } else {
                output.to_owned()
            },
        ));
        manager.preferred_ref = match operation {
            GitOperation::RenameBranch { new_name, .. } => Some(format!("refs/heads/{new_name}")),
            _ => None,
        };
        self.request_branch_list();
        true
    }

    pub(in crate::ui) fn branch_operation_error(&mut self, error: &str) -> bool {
        let Overlay::Branches(manager) = &mut self.overlay else {
            return false;
        };
        manager.status_scroll = 0;
        match &mut manager.state {
            BranchManagerState::Rename { input, .. } => input.set_error(error),
            _ => manager.message = Some((false, error.to_owned())),
        }
        true
    }

    fn close_branch_manager(&mut self) {
        let Overlay::Branches(manager) = &self.overlay else {
            return;
        };
        if self.foreground.action.as_ref().is_some_and(|action| {
            matches!(&action.kind, ForegroundKind::BranchList { path } if path == &manager.path)
        }) {
            self.foreground.action = None;
        }
        self.overlay = Overlay::None;
    }

    fn branch_mutating(&self) -> bool {
        self.foreground.action.as_ref().is_some_and(|action| {
            matches!(
                action.kind,
                ForegroundKind::Operation {
                    operation: GitOperation::RenameBranch { .. }
                        | GitOperation::DeleteBranch { .. },
                    ..
                }
            )
        })
    }

    fn branch_action(&mut self, action: BranchAction) {
        if self.branch_mutating() {
            return;
        }
        let Overlay::Branches(manager) = &mut self.overlay else {
            return;
        };
        if let Err(reason) = manager.action_allowed(action, &self.ops.command_context) {
            manager.message = Some((false, reason.to_owned()));
            return;
        }
        match action {
            BranchAction::Close => self.close_branch_manager(),
            BranchAction::Refresh => {
                manager.state = BranchManagerState::List;
                self.request_branch_list();
            }
            BranchAction::New => {
                self.close_branch_manager();
                self.dispatch_command(CommandId::CreateBranchAtHead);
            }
            BranchAction::Rename => {
                let Some(branch) = manager.selected().cloned() else {
                    return;
                };
                let mut input = TextField::new();
                input.text = branch.name().to_owned();
                manager.message = None;
                manager.status_scroll = 0;
                manager.state = BranchManagerState::Rename {
                    branch,
                    input,
                    buttons: ConfirmButtons::default(),
                };
            }
            BranchAction::Delete => {
                let Some(branch) = manager.selected().cloned() else {
                    return;
                };
                manager.message = None;
                manager.status_scroll = 0;
                manager.state = BranchManagerState::Delete {
                    branch,
                    buttons: ConfirmButtons::default(),
                };
            }
        }
    }

    fn submit_branch_change(&mut self) {
        if self.branch_mutating() {
            return;
        }
        let Overlay::Branches(manager) = &mut self.overlay else {
            return;
        };
        if manager.loading {
            return;
        }
        manager.status_scroll = 0;
        let operation = match &mut manager.state {
            BranchManagerState::Rename { branch, input, .. } => {
                if input.text.is_empty() {
                    input.set_error("Enter a branch name.");
                    return;
                }
                if input.text == branch.name() {
                    input.set_error("Enter a different branch name.");
                    return;
                }
                input.error = None;
                GitOperation::RenameBranch {
                    reference: branch.reference.clone(),
                    new_name: input.text.clone(),
                    expected_commit: branch.commit.clone(),
                }
            }
            BranchManagerState::Delete { branch, .. } => GitOperation::DeleteBranch {
                reference: branch.reference.clone(),
                expected_commit: branch.commit.clone(),
            },
            BranchManagerState::List => return,
        };
        self.start_operation(CommandId::ManageBranches, operation);
    }

    pub(in crate::ui) fn handle_branch_manager(&mut self, input: &Event) {
        if let Overlay::Branches(manager) = &mut self.overlay
            && !manager.loading
            && manager.scroll_status(input)
        {
            return;
        }
        if self.branch_mutating() {
            return;
        }
        let Overlay::Branches(manager) = &mut self.overlay else {
            return;
        };
        if !matches!(manager.state, BranchManagerState::List) {
            let buttons = match &manager.state {
                BranchManagerState::Rename { buttons, .. }
                | BranchManagerState::Delete { buttons, .. } => *buttons,
                _ => return,
            };
            let button = left_click(input).and_then(|point| buttons.hit(point));
            if matches!(button, Some(ConfirmButton::Secondary))
                || matches!(input, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc)
            {
                manager.state = BranchManagerState::List;
                manager.status_scroll = 0;
                return;
            }
            if matches!(input, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::F(5))
                && matches!(manager.state, BranchManagerState::Rename { .. })
            {
                manager.state = BranchManagerState::List;
                self.request_branch_list();
                return;
            }
            if matches!(button, Some(ConfirmButton::Primary))
                || matches!(input, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter)
            {
                self.submit_branch_change();
                return;
            }
            if let BranchManagerState::Rename { input: field, .. } = &mut manager.state
                && let Some(edit) = picker_edit(input, Rect::default(), 0, true)
            {
                match edit {
                    PickerEdit::Insert(character) => {
                        field.edit(TextEdit::Insert(character));
                    }
                    PickerEdit::Paste(text) => {
                        field.edit(TextEdit::Paste(text));
                    }
                    PickerEdit::Backspace => {
                        field.edit(TextEdit::Backspace);
                    }
                    _ => {}
                }
            }
            return;
        }
        if let Some(point) = left_click(input)
            && let Some(index) = manager
                .action_areas
                .iter()
                .position(|area| area.contains(point.into()))
        {
            self.branch_action(ACTIONS[index]);
            return;
        }
        if let Event::Key(key) = input
            && key.kind == KeyEventKind::Press
        {
            let action = match key.code {
                KeyCode::F(2) | KeyCode::Enter => Some(BranchAction::Rename),
                KeyCode::Delete => Some(BranchAction::Delete),
                KeyCode::F(5) => Some(BranchAction::Refresh),
                KeyCode::Esc => Some(BranchAction::Close),
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(BranchAction::New)
                }
                _ => None,
            };
            if let Some(action) = action {
                self.branch_action(action);
                return;
            }
        }
        let Some(edit) = picker_edit(input, manager.list_area, manager.cursor.scroll, true) else {
            return;
        };
        if manager.loading
            && !matches!(
                edit,
                PickerEdit::Insert(_) | PickerEdit::Paste(_) | PickerEdit::Backspace
            )
        {
            return;
        }
        if let PickerEdit::ClickRow(row) = edit {
            if row < manager.rows.len() {
                manager.cursor.selected = row;
            }
            return;
        }
        if update_picker(
            PickerState {
                query: Some(&mut manager.query),
                cursor: &mut manager.cursor,
                len: manager.rows.len(),
                page: usize::from(manager.list_area.height),
            },
            edit,
        ) == PickerOutcome::Filtered
        {
            manager.filter();
            manager.preferred_ref = None;
            manager.status_scroll = 0;
        }
    }

    pub(in crate::ui) fn draw_branch_manager(
        &self,
        frame: &mut Frame<'_>,
        manager: &mut BranchManager,
    ) {
        manager.action_areas = [Rect::default(); 5];
        manager.list_area = Rect::default();
        manager.status_area = Rect::default();
        let inner = widgets::dialog_frame(frame, "Branches", theme::DIALOG_WIDE, 22);
        if !matches!(manager.state, BranchManagerState::List) {
            self.draw_branch_form(frame, manager, inner);
            return;
        }
        let narrow = frame.area().width < 70;
        let action_height = if narrow { 6 } else { 3 };
        let mut status = Vec::new();
        if manager.loading {
            let elapsed = self
                .foreground
                .action
                .as_ref()
                .map(|action| action.started.elapsed())
                .unwrap_or_default();
            status.push(Line::from(vec![
                theme::spinner_span(elapsed),
                Span::raw(" Loading branches"),
            ]));
        }
        if let Some(error) = &manager.read_error {
            status.extend(
                error
                    .lines()
                    .map(|line| Line::styled(line, theme::error_text())),
            );
        } else if !manager.loading {
            if manager.rows.is_empty() {
                status.push(Line::styled(
                    if manager.entries.is_empty() {
                        "No branches yet."
                    } else {
                        "No matching branches."
                    },
                    theme::hint(),
                ));
            } else if let Some(branch) = manager
                .rows
                .get(manager.cursor.selected)
                .map(|index| &manager.entries[*index])
            {
                status.push(Line::raw(branch.reference.as_str()));
            }
            if manager.message.is_none()
                && let Some(reason) = [
                    BranchAction::New,
                    BranchAction::Rename,
                    BranchAction::Delete,
                ]
                .into_iter()
                .find_map(|action| {
                    manager
                        .action_allowed(action, &self.ops.command_context)
                        .err()
                })
            {
                status.push(Line::styled(reason, theme::hint()));
            }
        }
        if let Some((success, message)) = &manager.message {
            status.extend(message.lines().map(|line| {
                Line::styled(
                    line,
                    if *success {
                        theme::accent()
                    } else {
                        theme::error_text()
                    },
                )
            }));
        }
        status.push(Line::styled(
            "Remote branches reflect the last fetch.",
            theme::hint(),
        ));
        let status_height = wrapped_height(&status, inner.width)
            .max(2)
            .min(inner.height.saturating_sub(4 + action_height));
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(status_height),
                Constraint::Length(action_height),
            ])
            .split(inner);
        frame.render_widget(search_box(&manager.query), regions[0]);
        manager.list_area = regions[1];
        let items = manager.rows.iter().map(|index| {
            let branch = &manager.entries[*index];
            let kind = if branch.is_local() { "Local" } else { "Remote" };
            let flag = if branch.is_current {
                " *"
            } else if branch.worktree_path.is_some() {
                " Worktree"
            } else {
                ""
            };
            let commit = &branch.commit[..branch.commit.len().min(8)];
            let name_width = usize::from(regions[1].width.saturating_sub(2))
                .saturating_sub(kind.len() + commit.len() + flag.len() + 2);
            ListItem::new(Line::from(vec![
                Span::styled(
                    kind,
                    if branch.is_local() {
                        theme::accent()
                    } else {
                        theme::secondary()
                    },
                ),
                Span::raw(" "),
                Span::styled(
                    widgets::truncate_to_width(branch.name(), name_width),
                    theme::text(),
                ),
                Span::raw(" "),
                Span::styled(commit, theme::hint()),
                Span::styled(
                    flag,
                    if branch.is_current {
                        theme::accent_bold()
                    } else {
                        theme::warning_text()
                    },
                ),
            ]))
        });
        let mut state = ListState::default()
            .with_selected((!manager.rows.is_empty()).then_some(manager.cursor.selected));
        *state.offset_mut() = manager.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[1],
            &mut state,
        );
        manager.cursor.scroll = state.offset();
        draw_branch_status(
            frame,
            status,
            regions[2],
            &mut manager.status_area,
            &mut manager.status_scroll,
            &mut manager.status_max_scroll,
        );
        let bars = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [Constraint::Length(3); 2]
                    .into_iter()
                    .take(if narrow { 2 } else { 1 }),
            )
            .split(regions[3]);
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(
                [Constraint::Fill(1); 5]
                    .into_iter()
                    .take(if narrow { 3 } else { 5 }),
            )
            .split(bars[0]);
        manager.action_areas[..top.len()].copy_from_slice(&top);
        if narrow {
            let bottom = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Fill(1), Constraint::Fill(1)])
                .split(bars[1]);
            manager.action_areas[3..].copy_from_slice(&bottom);
        }
        let labels = if narrow {
            ["New", "Rename", "Delete", "Refresh", "Close"]
        } else {
            [
                "[Ctrl+N] New",
                "[F2] Rename",
                "[Del] Delete",
                "[F5] Refresh",
                "[Esc] Close",
            ]
        };
        for ((action, area), label) in ACTIONS.into_iter().zip(manager.action_areas).zip(labels) {
            let enabled = manager
                .action_allowed(action, &self.ops.command_context)
                .is_ok();
            let button = widgets::dialog_button(
                label,
                false,
                enabled && widgets::area_hovered(self.shell.mouse_position, area),
            );
            frame.render_widget(
                if enabled {
                    button
                } else {
                    button.style(theme::disabled())
                },
                area,
            );
        }
    }

    fn draw_branch_form(&self, frame: &mut Frame<'_>, manager: &mut BranchManager, inner: Rect) {
        let busy = self.branch_mutating();
        let (branch, input, buttons, label) = match &mut manager.state {
            BranchManagerState::Rename {
                branch,
                input,
                buttons,
            } => (branch, Some(input), buttons, "[Enter] Save"),
            BranchManagerState::Delete { branch, buttons } => {
                (branch, None, buttons, "[Enter] Delete")
            }
            BranchManagerState::List => return,
        };
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(if input.is_some() { 3 } else { 0 }),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let preview = if busy {
            match &self.foreground.action.as_ref().unwrap().kind {
                ForegroundKind::Operation { operation, .. } => operation.preview(),
                _ => unreachable!(),
            }
        } else if let Some(input) = &input {
            GitOperation::RenameBranch {
                reference: branch.reference.clone(),
                new_name: input.text.clone(),
                expected_commit: branch.commit.clone(),
            }
            .preview()
        } else {
            GitOperation::DeleteBranch {
                reference: branch.reference.clone(),
                expected_commit: branch.commit.clone(),
            }
            .preview()
        };
        let mut details = Vec::new();
        if busy {
            let action = self.foreground.action.as_ref().unwrap();
            details.push(Line::from(vec![
                theme::spinner_span(action.started.elapsed()),
                Span::raw(" Applying branch change"),
            ]));
        }
        if let Some(input) = input {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(input.text.as_str()),
                    theme::cursor_span(input.cursor_started.elapsed()),
                ]))
                .block(Block::default().borders(Borders::ALL).title("New name")),
                regions[0],
            );
            if let Some(error) = &input.error {
                details.extend(
                    error
                        .lines()
                        .map(|line| Line::styled(line, theme::error_text())),
                );
                details.push(Line::styled(
                    "F5 refresh; Cancel to reselect.",
                    theme::hint(),
                ));
            }
        } else {
            details.push(Line::raw("Only deletes the local branch. Git refuses unmerged branches; remote branches are unchanged."));
            if let Some((_, message)) = &manager.message {
                details.extend(
                    message
                        .lines()
                        .map(|line| Line::styled(line, theme::error_text())),
                );
            }
        }
        details.push(Line::styled(preview, theme::accent()));
        details.push(Line::raw(branch.reference.as_str()));
        details.push(Line::raw(branch.commit.as_str()));
        details.push(Line::raw(manager.path.display().to_string()));
        *buttons = widgets::dialog_footer(
            frame,
            regions[2],
            [label, "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
        if busy {
            for (area, label) in [
                (buttons.primary, label),
                (buttons.secondary, "[Esc] Cancel"),
            ] {
                frame.render_widget(
                    widgets::dialog_button(label, false, false).style(theme::disabled()),
                    area,
                );
            }
        }
        draw_branch_status(
            frame,
            details,
            regions[1],
            &mut manager.status_area,
            &mut manager.status_scroll,
            &mut manager.status_max_scroll,
        );
    }
}

fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    let width = usize::from(width).max(1);
    lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum::<usize>()
        .min(usize::from(u16::MAX)) as u16
}

fn draw_branch_status(
    frame: &mut Frame<'_>,
    lines: Vec<Line<'_>>,
    area: Rect,
    status_area: &mut Rect,
    scroll: &mut u16,
    max_scroll: &mut u16,
) {
    let width = usize::from(area.width.max(1));
    let mut wrapped = Vec::new();
    for line in lines {
        if line.width() <= width && line.spans.iter().all(|span| !span.content.contains('\n')) {
            wrapped.push(line);
            continue;
        }
        let style = line.style;
        let mut row = Line::default().style(style);
        let mut used = 0;
        for span in line.spans {
            let mut chunk = String::new();
            for character in span.content.chars() {
                let next = character.width().unwrap_or(0);
                if character == '\n' || used + next > width {
                    row.spans
                        .push(Span::styled(std::mem::take(&mut chunk), span.style));
                    wrapped.push(row);
                    row = Line::default().style(style);
                    used = 0;
                }
                if character != '\n' {
                    chunk.push(character);
                    used += next;
                }
            }
            row.spans.push(Span::styled(chunk, span.style));
        }
        wrapped.push(row);
    }
    let overflow = wrapped.len() > usize::from(area.height);
    let body = if overflow && area.height > 1 {
        frame.render_widget(
            Paragraph::new("Ctrl+↑/↓ or wheel: message").style(theme::hint()),
            Rect::new(area.x, area.y, area.width, 1),
        );
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    } else {
        area
    };
    *status_area = area;
    *max_scroll = wrapped
        .len()
        .saturating_sub(usize::from(body.height))
        .min(usize::from(u16::MAX)) as u16;
    *scroll = (*scroll).min(*max_scroll);
    frame.render_widget(Paragraph::new(wrapped).scroll((*scroll, 0)), body);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::git::Repository;
    use crate::ui::effect::{ForegroundResult, load_repository_snapshot};
    use crate::ui::syntax::SyntaxHighlighter;
    use crate::ui::test_support::{
        buffer_text, click, committed_repo, find_text, git, intercept_foreground, press, render,
        row_text, temp_registry, temp_repo, wait_for_foreground,
    };

    use super::*;

    fn app_at(root: &std::path::Path) -> App {
        App::load(Repository::discover(root).unwrap()).unwrap()
    }

    #[test]
    fn branch_management_button_and_alt_b_open_real_local_and_remote_lists() {
        let root = committed_repo("branch-manager-open");
        git(&root, &["branch", "topic"]);
        git(&root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
        let mut app = app_at(&root);
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Branch").unwrap();
        click(&mut app, x, y);
        wait_for_foreground(&mut app);
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("Local topic"));
        assert!(text.contains("Remote origin/topic"));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        wait_for_foreground(&mut app);
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("Local topic"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_empty_and_failed_reads_are_distinct_and_clear_stale_rows() {
        let empty = temp_repo("branch-manager-empty");
        let mut app = app_at(&empty);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("No branches yet."));
        let root = committed_repo("branch-manager-read-error");
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        fs::rename(root.join(".git"), root.join("hidden-git")).unwrap();
        app.request_branch_list();
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager remains open");
        };
        assert!(manager.rows.is_empty());
        assert!(manager.entries.is_empty());
        assert!(manager.read_error.is_some());
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("F5 to retry."));
        assert!(!text.contains("No branches yet."));
        for key in [KeyCode::Enter, KeyCode::F(2), KeyCode::Delete] {
            press(&mut app, key);
        }
        assert!(app.foreground.action.is_none());
        fs::rename(root.join("hidden-git"), root.join(".git")).unwrap();
        fs::remove_dir_all(empty).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_closed_and_superseded_reads_cannot_replace_the_new_manager() {
        let root = committed_repo("branch-manager-stale");
        let mut app = app_at(&root);
        let (requests, results) = intercept_foreground(&mut app);
        app.open_branch_manager();
        let ForegroundRequest::BranchList { id: old, path } = requests.recv().unwrap() else {
            panic!("branch read");
        };
        press(&mut app, KeyCode::Esc);
        assert!(app.foreground.action.is_none());
        app.open_branch_manager();
        let ForegroundRequest::BranchList { id: current, .. } = requests.recv().unwrap() else {
            panic!("branch read");
        };
        let entries = Repository::at_root(root.clone()).branches().unwrap();
        results
            .send(crate::ui::effect::ForegroundResult::BranchList {
                id: old,
                path: path.clone(),
                result: Ok(entries.clone()),
            })
            .unwrap();
        app.receive_foreground_results();
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.loading);
        assert!(manager.entries.is_empty());
        results
            .send(crate::ui::effect::ForegroundResult::BranchList {
                id: current,
                path,
                result: Ok(entries),
            })
            .unwrap();
        app.receive_foreground_results();
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("Local main"));
        fs::remove_dir_all(root).unwrap();
    }

    fn query(app: &mut App, text: &str) {
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        let len = manager.query.text.chars().count();
        for _ in 0..len {
            press(app, KeyCode::Backspace);
        }
        app.handle(Event::Paste(text.to_owned())).unwrap();
    }

    fn rename_to(app: &mut App, name: &str) {
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        let BranchManagerState::Rename { input, .. } = &manager.state else {
            panic!("rename");
        };
        let len = input.text.chars().count();
        for _ in 0..len {
            press(app, KeyCode::Backspace);
        }
        app.handle(Event::Paste(name.to_owned())).unwrap();
        press(app, KeyCode::Enter);
        wait_for_foreground(app);
    }

    #[test]
    fn branch_management_search_and_mouse_selection_change_only_the_captured_full_ref() {
        let root = committed_repo("branch-manager-selected");
        git(&root, &["branch", "topic-one"]);
        git(&root, &["branch", "topic-two"]);
        git(
            &root,
            &["update-ref", "refs/remotes/origin/topic-two", "HEAD"],
        );
        let repository = Repository::at_root(root.clone());
        let original = repository.branches().unwrap();
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "ToPiC");
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Local topic-two").unwrap();
        click(&mut app, x, y);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(matches!(manager.state, BranchManagerState::List));
        press(&mut app, KeyCode::F(2));
        press(&mut app, KeyCode::Esc);
        assert_eq!(repository.branches().unwrap(), original);
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, "renamed");
        let entries = repository.branches().unwrap();
        assert!(
            !entries
                .iter()
                .any(|branch| branch.reference == "refs/heads/topic-two")
        );
        assert!(
            entries
                .iter()
                .any(|branch| branch.reference == "refs/heads/renamed"
                    && branch.commit == original[0].commit)
        );
        assert!(
            entries
                .iter()
                .any(|branch| branch.reference == "refs/remotes/origin/topic-two")
        );
        assert!(
            entries
                .iter()
                .any(|branch| branch.reference == "refs/heads/topic-one")
        );
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(manager.query.text, "ToPiC");
        assert!(
            manager
                .message
                .as_ref()
                .is_some_and(|(success, _)| *success)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_read_only_and_no_match_actions_preserve_git_state() {
        let root = committed_repo("branch-manager-readonly");
        git(&root, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
        let repository = Repository::at_root(root.clone());
        let original = repository.branches().unwrap();
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "origin");
        for key in [KeyCode::Enter, KeyCode::F(2), KeyCode::Delete] {
            press(&mut app, key);
        }
        assert!(app.foreground.action.is_none());
        query(&mut app, "nothing-matches");
        for key in [KeyCode::Enter, KeyCode::F(2), KeyCode::Delete] {
            press(&mut app, key);
        }
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("No matching branches."));
        assert!(app.foreground.action.is_none());
        assert_eq!(repository.branches().unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_delete_cancel_preserves_and_confirm_safely_deletes() {
        let root = committed_repo("branch-manager-delete");
        git(&root, &["branch", "merged"]);
        let repository = Repository::at_root(root.clone());
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "merged");
        press(&mut app, KeyCode::Delete);
        assert!(buffer_text(&render(&mut app, 50, 16)).contains("refs/heads/merged"));
        press(&mut app, KeyCode::Esc);
        assert!(
            repository
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name() == "merged")
        );
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        assert!(
            !repository
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name() == "merged")
        );
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(manager.query.text, "merged");
        assert!(
            manager
                .message
                .as_ref()
                .is_some_and(|(success, _)| *success)
        );
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("No matching branches."));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_rename_keeps_query_and_prefers_new_ref_without_clearing_no_match() {
        let root = committed_repo("branch-manager-rename-selection");
        git(&root, &["branch", "topic-one"]);
        git(&root, &["branch", "topic-two"]);
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, "topic-zero");
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(manager.query.text, "topic");
        assert_eq!(
            manager.selected().unwrap().reference,
            "refs/heads/topic-zero"
        );
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, "elsewhere");
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert_eq!(manager.query.text, "topic");
        query(&mut app, "elsewhere");
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, "outside-filter");
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(manager.query.text, "elsewhere");
        assert!(manager.rows.is_empty());
        let success = manager.message.clone().unwrap();
        assert!(success.0);
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("No matching branches."));
        fs::rename(root.join(".git"), root.join("hidden-git")).unwrap();
        press(&mut app, KeyCode::F(5));
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert_eq!(manager.message.as_ref(), Some(&success));
        assert!(manager.read_error.is_some());
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("F5 to retry."));
        assert!(text.contains(success.1.as_str()));
        fs::rename(root.join("hidden-git"), root.join(".git")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_rename_collision_preserves_editing_and_long_errors_can_scroll() {
        let root = committed_repo("branch-manager-collision");
        let destination = "existing".repeat(28);
        git(&root, &["branch", "topic"]);
        git(&root, &["branch", &destination]);
        let repository = Repository::at_root(root.clone());
        let original = repository.branches().unwrap();
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, &destination);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        let BranchManagerState::Rename { branch, input, .. } = &manager.state else {
            panic!("rename retained");
        };
        assert_eq!(branch.reference, "refs/heads/topic");
        assert_eq!(input.text, destination);
        assert!(
            input
                .error
                .as_ref()
                .is_some_and(|error| error.contains(&destination))
        );
        assert_eq!(repository.branches().unwrap(), original);
        render(&mut app, 50, 16);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.status_max_scroll > 0);
        let mut text = String::new();
        for _ in 0..100 {
            text.push_str(&buffer_text(&render(&mut app, 50, 16)));
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        }
        assert!(text.contains("Cancel to reselect."));
        rename_to(&mut app, "topic-recovered");
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(
            manager.selected().unwrap().reference,
            "refs/heads/topic-recovered"
        );
        assert!(
            repository
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.reference == "refs/heads/topic-recovered")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_stale_rename_target_recovers_with_f5_and_fresh_selection() {
        let root = committed_repo("branch-manager-stale-target");
        git(&root, &["branch", "topic"]);
        let repository = Repository::at_root(root.clone());
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::F(2));
        git(&root, &["commit", "--allow-empty", "-m", "Advanced"]);
        git(&root, &["update-ref", "refs/heads/topic", "HEAD"]);
        let fresh = repository
            .branches()
            .unwrap()
            .into_iter()
            .find(|branch| branch.name() == "topic")
            .unwrap();
        rename_to(&mut app, "topic-renamed");
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        let BranchManagerState::Rename { branch, input, .. } = &manager.state else {
            panic!("rename retained");
        };
        assert_ne!(branch.commit, fresh.commit);
        assert_eq!(input.text, "topic-renamed");
        assert!(input.error.is_some());
        press(&mut app, KeyCode::F(5));
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(manager.query.text, "topic");
        assert_eq!(manager.selected().unwrap().commit, fresh.commit);
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, "topic-renamed");
        assert!(
            repository
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name() == "topic-renamed" && branch.commit == fresh.commit)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_unmerged_delete_returns_to_fresh_list_and_keeps_error() {
        let root = committed_repo("branch-manager-unmerged");
        git(&root, &["switch", "-c", "unmerged"]);
        git(&root, &["commit", "--allow-empty", "-m", "Unmerged"]);
        git(&root, &["switch", "main"]);
        let repository = Repository::at_root(root.clone());
        let original = repository.branches().unwrap();
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "unmerged");
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert!(!manager.loading);
        assert_eq!(manager.query.text, "unmerged");
        assert_eq!(manager.selected().unwrap().reference, "refs/heads/unmerged");
        let error = manager.message.clone().unwrap();
        assert!(!error.0);
        assert!(!error.1.is_empty());
        assert_eq!(repository.branches().unwrap(), original);
        press(&mut app, KeyCode::F(5));
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert_eq!(manager.message.as_ref(), Some(&error));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_delete_clamps_to_nearest_surviving_filtered_row() {
        let root = committed_repo("branch-manager-delete-nearest");
        for name in ["topic-one", "topic-three", "topic-two"] {
            git(&root, &["branch", name]);
        }
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert_eq!(manager.query.text, "topic");
        assert_eq!(
            manager.selected().unwrap().reference,
            "refs/heads/topic-three"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_current_rename_refreshes_header_through_the_event_loop() {
        let root = committed_repo("branch-manager-current-refresh");
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        press(&mut app, KeyCode::F(2));
        rename_to(&mut app, "main-renamed");
        assert_eq!(
            app.ops.command_context.current_branch.as_deref(),
            Some("main")
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while (app.ops.command_context.current_branch.as_deref() != Some("main-renamed")
            || app.refresh.pending
            || app.refresh.in_flight
            || app.refresh.deferred_result.is_some())
            && Instant::now() < deadline
        {
            app.maybe_auto_refresh();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.ops.command_context.current_branch.as_deref(),
            Some("main-renamed")
        );
        assert!(
            !app.refresh.pending && !app.refresh.in_flight && app.refresh.deferred_result.is_none()
        );
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager stays open");
        };
        assert!(manager.is_list());
        assert_eq!(
            manager.selected().unwrap().reference,
            "refs/heads/main-renamed"
        );
        let buffer = render(&mut app, 100, 32);
        assert!(
            row_text(&buffer, buffer.area, buffer.area.bottom() - 1)
                .contains("Branch: main-renamed")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_loading_allows_search_not_actions_and_resets_restored_selection() {
        let root = committed_repo("branch-manager-loading");
        git(&root, &["branch", "topic-one"]);
        git(&root, &["branch", "topic-two"]);
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(5));
        let id = app.foreground.action.as_ref().unwrap().id;
        for key in [
            KeyCode::Enter,
            KeyCode::F(2),
            KeyCode::Delete,
            KeyCode::F(5),
        ] {
            press(&mut app, key);
        }
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
        query(&mut app, "ToPiC");
        assert_eq!(app.foreground.action.as_ref().unwrap().id, id);
        wait_for_foreground(&mut app);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(manager.query.text, "ToPiC");
        assert_eq!(
            manager.selected().unwrap().reference,
            "refs/heads/topic-one"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_busy_mutation_keeps_form_preview_and_rejects_other_input() {
        let root = committed_repo("branch-manager-busy");
        git(&root, &["branch", "topic"]);
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::F(2));
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        app.handle(Event::Paste("renamed".to_owned())).unwrap();
        press(&mut app, KeyCode::Enter);
        let id = app.foreground.action.as_ref().unwrap().id;
        for key in [KeyCode::Enter, KeyCode::Esc, KeyCode::F(5), KeyCode::Delete] {
            press(&mut app, key);
        }
        app.handle(Event::Paste("ignored".to_owned())).unwrap();
        assert_eq!(app.foreground.action.as_ref().unwrap().id, id);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        let BranchManagerState::Rename { input, .. } = &manager.state else {
            panic!("rename");
        };
        assert_eq!(input.text, "renamed");
        let preview = match &app.foreground.action.as_ref().unwrap().kind {
            ForegroundKind::Operation { operation, .. } => operation.preview(),
            _ => panic!("mutation"),
        };
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains(&preview));
        assert!(text.contains("Applying branch change"));
        wait_for_foreground(&mut app);
        assert!(
            Repository::at_root(root.clone())
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name() == "renamed")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_worktree_actions_stay_disabled_and_resized_buttons_hit_current_areas() {
        let root = committed_repo("branch-manager-worktree");
        let linked = root.with_extension("linked");
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        git(&root, &["branch", "topic"]);
        let repository = Repository::at_root(root.clone());
        let original = repository.branches().unwrap();
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        press(&mut app, KeyCode::Delete);
        assert!(app.foreground.action.is_none());
        query(&mut app, "linked");
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "[F2] Rename").unwrap();
        click(&mut app, x, y);
        press(&mut app, KeyCode::F(2));
        press(&mut app, KeyCode::Delete);
        assert!(app.foreground.action.is_none());
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(manager.is_list());
        assert_eq!(repository.branches().unwrap(), original);
        query(&mut app, "topic");
        let buffer = render(&mut app, 50, 16);
        let (x, y) = find_text(&buffer, "Rename").unwrap();
        click(&mut app, x, y);
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("manager");
        };
        assert!(
            matches!(&manager.state, BranchManagerState::Rename { branch, .. } if branch.reference == "refs/heads/topic")
        );
        press(&mut app, KeyCode::Esc);
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "[Esc] Close").unwrap();
        click(&mut app, x, y);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(repository.branches().unwrap(), original);
        git(&root, &["worktree", "remove", linked.to_str().unwrap()]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_old_repository_reads_cannot_replace_switched_manager() {
        let root = committed_repo("branch-manager-old-repository");
        let other = committed_repo("branch-manager-new-repository");
        git(&root, &["branch", "old-only"]);
        git(&other, &["branch", "new-only"]);
        let mut registry = temp_registry("branch-manager-switch");
        registry.add(&other).unwrap();
        let mut app = App::load_registered(&root, registry).unwrap();
        let (requests, results) = intercept_foreground(&mut app);
        app.open_branch_manager();
        let ForegroundRequest::BranchList { id: old, path } = requests.recv().unwrap() else {
            panic!("branch read");
        };
        let old_entries = Repository::at_root(root.clone()).branches().unwrap();
        press(&mut app, KeyCode::Esc);
        let index = app
            .workspaces
            .repository_rows
            .iter()
            .position(|row| row.path == other)
            .unwrap();
        app.switch_repository_row(index);
        let ForegroundRequest::Switch {
            id,
            generation,
            path: switched,
        } = requests.recv().unwrap()
        else {
            panic!("switch");
        };
        let snapshot =
            load_repository_snapshot(&switched, &SyntaxHighlighter::new(), &|| false).unwrap();
        app.apply_switch(id, generation, switched, Ok(Box::new(snapshot)));
        app.open_branch_manager();
        let ForegroundRequest::BranchList {
            id: current,
            path: new_path,
        } = requests.recv().unwrap()
        else {
            panic!("new branch read");
        };
        results
            .send(ForegroundResult::BranchList {
                id: old,
                path: path.clone(),
                result: Ok(old_entries.clone()),
            })
            .unwrap();
        results
            .send(ForegroundResult::BranchList {
                id: current,
                path,
                result: Ok(old_entries),
            })
            .unwrap();
        app.receive_foreground_results();
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("new manager");
        };
        assert_eq!(manager.path, other);
        assert!(manager.loading && manager.entries.is_empty());
        results
            .send(ForegroundResult::BranchList {
                id: current,
                path: new_path,
                result: Repository::at_root(other.clone()).branches(),
            })
            .unwrap();
        app.receive_foreground_results();
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("Local new-only"));
        assert!(!text.contains("old-only"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn branch_management_operation_result_after_repository_change_does_not_refresh_or_replace_overlay()
     {
        let root = committed_repo("branch-manager-mutation-old");
        let other = committed_repo("branch-manager-mutation-new");
        git(&root, &["branch", "topic"]);
        let mut app = app_at(&root);
        app.open_branch_manager();
        wait_for_foreground(&mut app);
        query(&mut app, "topic");
        press(&mut app, KeyCode::F(2));
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        app.handle(Event::Paste("renamed".to_owned())).unwrap();
        press(&mut app, KeyCode::Enter);
        let snapshot =
            load_repository_snapshot(&other, &SyntaxHighlighter::new(), &|| false).unwrap();
        app.apply_repository_snapshot(snapshot);
        app.open_branch_manager();
        let refresh = (
            app.refresh.pending,
            app.refresh.in_flight,
            app.refresh.latest_id,
        );
        wait_for_foreground(&mut app);
        assert_eq!(
            (
                app.refresh.pending,
                app.refresh.in_flight,
                app.refresh.latest_id
            ),
            refresh
        );
        let Overlay::Branches(manager) = &app.overlay else {
            panic!("new manager remains");
        };
        assert_eq!(manager.path, other);
        assert!(manager.message.is_none());
        assert!(
            Repository::at_root(root.clone())
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name() == "renamed")
        );
        assert!(
            !Repository::at_root(other.clone())
                .branches()
                .unwrap()
                .iter()
                .any(|branch| branch.name() == "renamed")
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }
}
