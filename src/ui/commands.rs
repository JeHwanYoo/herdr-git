use std::mem;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::git::{GitOperation, ResetTarget};

use super::effect::{ForegroundRequest, RefreshScope, RequestId};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState, TextEdit,
    TextField, area_hovered, chord, left_click, picker_edit, shortcut_spans, update_picker,
};
use super::{App, theme};
#[cfg(not(test))]
use catalog::COMMANDS;
#[cfg(test)]
pub(super) use catalog::COMMANDS;
pub(super) use catalog::{Availability, CommandContext, CommandId, QUICK_ACTIONS};
#[cfg(test)]
pub(super) use commit::AgentPicker;
pub(super) use commit::CommitDialog;
pub(super) use rebase::RebaseEditor;
pub(super) use remote::{AddRemoteDialog, RemoteList};
pub(super) use reset::ResetFlow;
#[cfg(test)]
pub(super) use reset::ResetStep;
pub(super) use target::TargetPicker;

mod catalog;
mod commit;
mod rebase;
mod remote;
mod reset;
mod target;

#[derive(Debug, Default)]
pub(super) struct PushControls {
    pub(super) force: Rect,
    pub(super) remote: Rect,
}

pub(super) const RESULT_VISIBLE_DURATION: Duration = Duration::from_secs(2);
const COMMAND_COLUMN_WIDTH: usize = 42;
const COMMAND_PALETTE_CHROME: u16 = 10;
const NAME_PROMPT_HEIGHT: u16 = 9;
const CONFIRMATION_HEIGHT: u16 = 12;
const ACTION_DIALOG_HEIGHT: u16 = 8;
const ACTION_PREFIX_LABEL_GAP: &str = "  ";
const RESULT_CARD_CHROME: u16 = 6;
pub(super) const STASH_COMMANDS: [CommandId; 2] = [CommandId::StashChanges, CommandId::StashPop];

pub(super) struct ActionCell {
    pub(super) label: &'static str,
    pub(super) shortcut: char,
    pub(super) icon: &'static str,
    pub(super) enabled: bool,
    pub(super) selected: bool,
    pub(super) running: Option<(Duration, &'static str)>,
}

#[derive(Debug)]
pub(super) struct ActionDialog {
    pub(super) title: &'static str,
    pub(super) commands: &'static [CommandId],
    pub(super) status: String,
    pub(super) cursor: ListCursor,
    pub(super) list_area: Rect,
    pub(super) buttons: ConfirmButtons,
}

impl ActionDialog {
    pub(super) fn new(title: &'static str, commands: &'static [CommandId], status: String) -> Self {
        Self {
            title,
            commands,
            status,
            cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
        }
    }
}

pub(super) struct OperationsState {
    pub(super) command_context: CommandContext,
    pub(super) quick_action_areas: Vec<(CommandId, Rect)>,
}

impl OperationsState {
    pub(super) fn new(command_context: CommandContext) -> Self {
        Self {
            command_context,
            quick_action_areas: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(super) struct NamePrompt {
    pub(super) command: CommandId,
    pub(super) input: TextField,
    pub(super) buttons: ConfirmButtons,
}

impl NamePrompt {
    pub(super) fn new(command: CommandId) -> Self {
        Self {
            command,
            input: TextField::new(),
            buttons: ConfirmButtons::default(),
        }
    }
}

#[derive(Debug)]
pub(super) struct CommandPalette {
    pub(super) query: TextField,
    pub(super) rows: Vec<CommandId>,
    pub(super) cursor: ListCursor,
    pub(super) list_area: Rect,
    pub(super) buttons: ConfirmButtons,
}

impl CommandPalette {
    pub(super) fn new() -> Self {
        Self {
            query: TextField::new(),
            rows: COMMANDS.to_vec(),
            cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
        }
    }

    pub(super) fn filter(&mut self) {
        self.rows = COMMANDS
            .iter()
            .copied()
            .filter(|command| command.matches(&self.query.text))
            .collect();
        self.cursor.clamp(self.rows.len());
    }
}

#[derive(Clone, Debug)]
pub(super) struct OperationResultView {
    pub(super) command: Option<CommandId>,
    result_name: Option<&'static str>,
    pub(super) success: bool,
    pub(super) lines: Vec<Line<'static>>,
    pub(super) shown_at: Instant,
}

impl OperationResultView {
    pub(super) fn operation(operation: &GitOperation, output: &str, success: bool) -> Self {
        let preview = operation.preview();
        let lines = if matches!(operation, GitOperation::PullFastForward) {
            pull_result_lines(&preview, output, success)
        } else {
            operation_result_lines(&preview, output, success)
        };
        Self {
            command: operation.command(),
            result_name: None,
            success,
            lines,
            shown_at: Instant::now(),
        }
    }

    pub(super) fn message(command: Option<CommandId>, success: bool, text: &str) -> Self {
        let style = if success {
            Style::default()
        } else {
            theme::error_text()
        };
        Self {
            command,
            result_name: None,
            success,
            lines: text
                .lines()
                .map(|line| Line::styled(line.to_owned(), style))
                .collect(),
            shown_at: Instant::now(),
        }
    }

    pub(super) fn named_message(result_name: &'static str, success: bool, text: &str) -> Self {
        let mut view = Self::message(None, success, text);
        view.result_name = Some(result_name);
        view
    }

    pub(super) fn expired(&self) -> bool {
        self.success && self.shown_at.elapsed() >= RESULT_VISIBLE_DURATION
    }

    pub(super) fn title(&self) -> String {
        if self.success && self.command == Some(CommandId::Commit) {
            return "✓ Committed".into();
        }
        format!(
            "{} {}",
            self.result_name.unwrap_or_else(|| {
                self.command
                    .map_or("Git operation", |command| command.result_name())
            }),
            if self.success { "complete" } else { "failed" }
        )
    }
}

fn operation_result_lines(preview: &str, output: &str, success: bool) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(preview.to_owned(), theme::accent_bold())),
        Line::default(),
    ];
    lines.extend(output.lines().map(|line| {
        Line::from(Span::styled(
            line.to_owned(),
            if success {
                Style::default()
            } else {
                theme::error_text()
            },
        ))
    }));
    lines
}

fn pull_result_lines(preview: &str, output: &str, success: bool) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(preview.to_owned(), theme::accent_bold())),
        Line::default(),
    ];
    lines.extend(output.lines().map(|line| {
        if !success {
            return Line::from(Span::styled(line.to_owned(), theme::error_text()));
        }
        if line.starts_with("Updating ") || line == "Fast-forward" || line == "Already up to date."
        {
            return Line::from(Span::styled(line.to_owned(), theme::success_title()));
        }
        let Some((path, changes)) = line.split_once('|') else {
            return Line::raw(line.to_owned());
        };
        let mut spans = vec![Span::raw(format!("{path}|"))];
        let mut run = String::new();
        let mut run_kind = None;
        for character in changes.chars() {
            let kind = match character {
                '+' => Some(theme::SUCCESS),
                '-' => Some(theme::ERROR),
                _ => None,
            };
            if kind != run_kind && !run.is_empty() {
                spans.push(Span::styled(
                    mem::take(&mut run),
                    run_kind.map_or_else(Style::default, |color| Style::default().fg(color)),
                ));
            }
            run_kind = kind;
            run.push(character);
        }
        if !run.is_empty() {
            spans.push(Span::styled(
                run,
                run_kind.map_or_else(Style::default, |color| Style::default().fg(color)),
            ));
        }
        Line::from(spans)
    }));
    lines
}

fn operation_output(operation: &GitOperation, result: Result<String, String>) -> (bool, String) {
    match result {
        Ok(output) => (true, output),
        Err(error) => {
            let recovery = operation
                .recovery_commands()
                .map(|commands| format!("\n\nResolve conflicts, then use: {commands}"))
                .unwrap_or_default();
            (false, format!("{error}{recovery}"))
        }
    }
}

fn command_status_label(reason: Option<&str>) -> &'static str {
    match reason {
        None => "",
        Some("no remotes configured") => "No remote",
        Some("detached HEAD") => "Detached HEAD",
        Some("no upstream configured") => "No upstream",
        Some("working tree is clean") => "Clean",
        Some("stash is empty") => "No stash",
        Some("no commit selected") => "No commit",
        Some("working tree has changes; stash or commit first") => "Changes present",
        Some(_) => "Unavailable",
    }
}

fn target_icon(target: &ResetTarget) -> &'static str {
    if target.reference.starts_with("refs/remotes/") {
        theme::REMOTE_GLYPH
    } else {
        theme::BRANCH_GLYPH
    }
}

fn list_status(query: &str, count: usize, singular: &str, plural: &str) -> String {
    match (query.is_empty(), count) {
        (true, _) => String::new(),
        (false, 0) => format!("No matching {plural}"),
        (false, 1) => format!("1 matching {singular}"),
        (false, count) => format!("{count} matching {plural}"),
    }
}

fn quick_action_icon(command: CommandId) -> &'static str {
    match command {
        CommandId::Fetch => "↧",
        CommandId::Pull => "↓",
        CommandId::Commit => "●",
        CommandId::Push => "↑",
        CommandId::CreateBranchAtHead => theme::BRANCH_GLYPH,
        CommandId::StashChanges => "≡",
        _ => "",
    }
}

pub(super) fn draw_action_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    actions: &[ActionCell],
    shortcut_hints: bool,
    mouse_position: Option<(u16, u16)>,
) -> Vec<Rect> {
    if actions.is_empty() {
        return Vec::new();
    }
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 6); 6])
        .split(area);
    actions
        .iter()
        .zip(cells.iter().copied())
        .enumerate()
        .map(|(index, (action, cell))| {
            let button = Rect::new(
                cell.x,
                cell.y,
                if index + 1 == cells.len() {
                    cell.width
                } else {
                    cell.width.saturating_sub(1)
                },
                cell.height,
            );
            let base = if action.selected {
                theme::accent_bold()
            } else if action.enabled {
                Style::default().fg(theme::TEXT)
            } else {
                theme::disabled()
            };
            let style = theme::hover(
                base.bg(theme::SURFACE_PANEL),
                action.enabled && area_hovered(mouse_position, button),
            );
            let content = if let Some((elapsed, progress)) = action.running {
                Line::from(vec![
                    theme::spinner_span(elapsed),
                    Span::styled(format!("{ACTION_PREFIX_LABEL_GAP}{progress}"), style),
                ])
            } else if shortcut_hints
                && usize::from(button.width) >= action.label.len().saturating_add(3)
            {
                Line::from(shortcut_spans(
                    action.shortcut.to_ascii_uppercase(),
                    action.label,
                    ACTION_PREFIX_LABEL_GAP,
                    "",
                    style,
                    action.enabled,
                ))
            } else if usize::from(cell.width) >= action.label.len().saturating_add(4) {
                Line::styled(
                    format!("{}{ACTION_PREFIX_LABEL_GAP}{}", action.icon, action.label),
                    style,
                )
            } else {
                Line::styled(action.label, style)
            };
            frame.render_widget(
                Paragraph::new(content)
                    .alignment(Alignment::Center)
                    .style(style),
                button,
            );
            button
        })
        .collect()
}

fn search_box(field: &TextField) -> Paragraph<'static> {
    let input = Line::from(vec![
        Span::raw(field.text.clone()),
        theme::cursor_span(field.cursor_started.elapsed()),
    ]);
    Paragraph::new(input).block(Block::default().borders(Borders::ALL).title("Search"))
}

impl App {
    pub(super) fn open_commands(&mut self) {
        self.refresh_command_selection_context();
        self.overlay = Overlay::Commands(CommandPalette::new());
    }

    fn run_selected_command(&mut self) {
        let Overlay::Commands(palette) = &self.overlay else {
            return;
        };
        let Some(command) = palette.rows.get(palette.cursor.selected).copied() else {
            return;
        };
        self.dispatch_command(command);
    }

    pub(super) fn run_quick_action(&mut self, command: CommandId) {
        if command == CommandId::StashChanges {
            self.open_stash_dialog();
        } else {
            self.dispatch_command(command);
        }
    }

    pub(super) fn open_stash_dialog(&mut self) {
        self.refresh_command_selection_context();
        if !self.ops.command_context.has_repository {
            self.show_result(OperationResultView::message(
                Some(CommandId::StashChanges),
                false,
                "not a Git repository",
            ));
            return;
        }
        self.overlay = Overlay::Action(ActionDialog::new(
            "Stash",
            &STASH_COMMANDS,
            "Choose a stash operation".to_owned(),
        ));
    }

    pub(super) fn handle_quick_actions_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        let Some(command) = self
            .ops
            .quick_action_areas
            .iter()
            .find_map(|(command, area)| {
                area.contains((mouse.column, mouse.row).into())
                    .then_some(*command)
            })
        else {
            return false;
        };
        self.run_quick_action(command);
        true
    }

    pub(super) fn dispatch_command(&mut self, command: CommandId) {
        self.refresh_command_selection_context();
        if matches!(self.overlay, Overlay::Commands(_) | Overlay::Action(_)) {
            self.overlay = Overlay::None;
        }
        if self.foreground.action.is_some() && !matches!(command, CommandId::SwitchRepository) {
            self.show_action_error("Wait for the current Git operation.");
            return;
        }
        let availability = command.availability(&self.ops.command_context);
        if !availability.enabled {
            if let Some(reason) = availability.reason {
                self.show_result(OperationResultView::message(Some(command), false, reason));
            }
            return;
        }
        match command {
            CommandId::Update => self.open_update_confirmation(),
            CommandId::Refresh => {
                self.request_background_refresh(
                    "Refreshing repositories",
                    RefreshScope::Repository,
                );
            }
            CommandId::SwitchRepository => {
                self.select_active_repository_row();
                self.refresh_command_selection_context();
            }
            CommandId::AddProject => self.add_project_from_picker(),
            CommandId::RemoveProject => self.confirm_remove_selected_project(),
            CommandId::ResetCurrentBranch => self.open_reset_flow(),
            CommandId::CopySha => self.run_graph_action(super::graph::GraphAction::CopySha),
            CommandId::Commit => self.open_commit_dialog(),
            CommandId::Stash => self.open_stash_dialog(),
            CommandId::AddRemote => self.open_add_remote(),
            CommandId::ShowRemotes => self.open_remotes(),
            CommandId::CheckoutCommit | CommandId::RebaseHere | CommandId::InteractiveRebase => {
                self.open_target_picker(command)
            }
            CommandId::CreateBranchAtHead | CommandId::CreateBranch | CommandId::CreateTag => {
                self.overlay = Overlay::Name(NamePrompt::new(command));
            }
            _ => match command.operation(&self.ops.command_context) {
                Ok(operation) if command.requires_confirmation() => {
                    self.overlay = Overlay::Confirm {
                        command,
                        operation,
                        buttons: ConfirmButtons::default(),
                        push_controls: Default::default(),
                    };
                }
                Ok(operation) => self.start_operation(command, operation),
                Err(reason) => self.show_result(OperationResultView::message(
                    Some(command),
                    false,
                    &format!("{}: {reason}", command.title()),
                )),
            },
        }
    }

    pub(super) fn submit_name(&mut self) {
        let Overlay::Name(prompt) = &mut self.overlay else {
            return;
        };
        if self.foreground.action.is_some() {
            return;
        }
        let Some(repository) = self.repository.as_ref() else {
            prompt.input.set_error("Not a Git repository");
            return;
        };
        let command = prompt.command;
        let path = repository.root().to_owned();
        let name = prompt.input.text.trim().to_owned();
        let id = self.foreground.next_id;
        let request = ForegroundRequest::ValidateName {
            id,
            path: path.clone(),
            command,
            name: name.clone(),
        };
        prompt.input.error = None;
        if let Err(error) = self.foreground.request_foreground(
            request,
            ForegroundKind::ValidateName {
                path,
                command,
                name,
            },
        ) {
            prompt
                .input
                .set_error(format!("Git worker stopped: {error}"));
        }
    }

    fn execute_pending_operation(&mut self) {
        if !matches!(self.overlay, Overlay::Confirm { .. }) {
            return;
        }
        let Overlay::Confirm {
            command, operation, ..
        } = mem::replace(&mut self.overlay, Overlay::None)
        else {
            return;
        };
        self.execute_operation(command, operation);
    }

    fn execute_operation(&mut self, command: CommandId, operation: GitOperation) {
        if matches!(
            operation,
            GitOperation::InteractiveRebase(_) | GitOperation::InteractiveRebaseOnto(_)
        ) {
            self.open_rebase_editor(operation);
            return;
        }
        self.start_operation(command, operation);
    }

    fn start_operation(&mut self, command: CommandId, operation: GitOperation) {
        let Some(repository) = self.repository.as_ref() else {
            self.show_result(OperationResultView::message(
                Some(command),
                false,
                "Not a Git repository",
            ));
            return;
        };
        if self.foreground.action.is_some() {
            self.show_action_error("Wait for the current Git operation.");
            return;
        }
        let path = repository.root().to_owned();
        let id = self.foreground.next_id;
        let request = ForegroundRequest::Operation {
            id,
            path: path.clone(),
            command,
            operation: operation.clone(),
        };
        self.advance_refresh_generation();
        match self.foreground.request_foreground(
            request,
            ForegroundKind::Operation {
                path,
                command,
                operation,
            },
        ) {
            Ok(_) => {}
            Err(error) => self.show_action_error(&format!("Git worker stopped: {error}")),
        }
    }

    pub(super) fn refresh_command_selection_context(&mut self) {
        let selected_commit = self
            .graph
            .selected_commit()
            .map(|commit| commit.sha.clone());
        self.ops.command_context.selected_commit = selected_commit;
        self.ops.command_context.has_repository = self.repository.is_some();
        self.ops.command_context.can_remove_project = self.workspaces.selected_is_project();
    }

    pub(super) fn apply_operation(
        &mut self,
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        operation: GitOperation,
        result: Result<String, String>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::Operation {
                        path: active,
                        command: active_command,
                        operation: active_operation,
                    } if active == &path
                        && *active_command == command
                        && active_operation == &operation
                )
            })
            .is_none()
        {
            return;
        }
        let (success, output) = operation_output(&operation, result);
        if success
            && matches!(
                operation,
                GitOperation::CheckoutCommit(_)
                    | GitOperation::CheckoutBranch(_)
                    | GitOperation::RebaseHere(_)
            )
        {
            self.history.follow_head = true;
        }
        let view = OperationResultView::operation(&operation, &output, success);
        match &operation {
            GitOperation::AddRemote { name, url } if success => {
                self.ops.command_context.remotes.push(crate::git::Remote {
                    name: name.clone(),
                    url: url.clone(),
                    push_url: url.clone(),
                });
                self.ops.command_context.has_remote = true;
                self.open_remotes();
            }
            GitOperation::RemoveRemote(name) if success => {
                self.ops
                    .command_context
                    .remotes
                    .retain(|remote| remote.name != *name);
                self.ops.command_context.has_remote = !self.ops.command_context.remotes.is_empty();
                self.open_remotes();
            }
            GitOperation::AddRemote { .. } if !success => {
                if let Overlay::AddRemote(dialog) = &mut self.overlay {
                    dialog.name.set_error(output);
                } else {
                    self.show_result(view);
                }
            }
            _ => match &operation {
                GitOperation::Commit { message, amend } if !success => {
                    self.overlay = Overlay::Commit(CommitDialog::failed(message, *amend, output));
                }
                _ => self.show_result(view),
            },
        }
        self.request_operation_refresh("Refreshing repository", RefreshScope::Repository);
    }

    pub(super) fn apply_name_validation(
        &mut self,
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        name: String,
        result: Result<(), String>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::ValidateName {
                        path: active,
                        command: active_command,
                        name: active_name,
                    } if active == &path
                        && *active_command == command
                        && active_name == &name
                )
            })
            .is_none()
        {
            return;
        }
        let Overlay::Name(prompt) = &mut self.overlay else {
            return;
        };
        if prompt.command != command || prompt.input.text.trim() != name {
            return;
        }
        match result {
            Ok(()) => match command.named_operation(&self.ops.command_context, name) {
                Ok(operation) => {
                    self.overlay = Overlay::Confirm {
                        command,
                        operation,
                        buttons: ConfirmButtons::default(),
                        push_controls: Default::default(),
                    };
                }
                Err(error) => prompt.input.set_error(error),
            },
            Err(error) => prompt.input.set_error(format!("Invalid name: {error}")),
        }
    }

    pub(super) fn expire_result(&mut self) -> bool {
        if let Overlay::Result(view, _) = &self.overlay
            && view.expired()
        {
            self.dismiss_result();
            return true;
        }
        false
    }

    pub(super) fn show_result(&mut self, view: OperationResultView) {
        self.overlay = Overlay::Result(view, None);
    }

    fn dismiss_result(&mut self) {
        if let Overlay::Result(_, previous) = std::mem::replace(&mut self.overlay, Overlay::None) {
            self.overlay = previous.map_or(Overlay::None, |previous| *previous);
        }
    }

    pub(super) fn show_action_error(&mut self, message: &str) {
        self.show_result(OperationResultView::named_message("Action", false, message));
    }

    pub(super) fn handle_result(&mut self, input: &Event) {
        let dismiss = match input {
            Event::Key(key) => {
                key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Esc | KeyCode::Enter)
            }
            Event::Mouse(mouse) => matches!(mouse.kind, MouseEventKind::Down(_)),
            _ => false,
        };
        if dismiss {
            self.dismiss_result();
        }
    }

    pub(super) fn handle_action_dialog(&mut self, input: &Event) {
        let Overlay::Action(dialog) = &mut self.overlay else {
            return;
        };
        if let Some(button) = left_click(input).and_then(|pointer| dialog.buttons.hit(pointer)) {
            match button {
                ConfirmButton::Primary => self.run_action_dialog_command(),
                ConfirmButton::Secondary => self.overlay = Overlay::None,
            }
            return;
        }
        let Some(edit) = picker_edit(input, dialog.list_area, dialog.cursor.scroll, false) else {
            return;
        };
        let page = usize::from(dialog.list_area.height);
        match update_picker(
            PickerState {
                query: None,
                cursor: &mut dialog.cursor,
                len: dialog.commands.len(),
                page,
            },
            edit,
        ) {
            PickerOutcome::Activate => self.run_action_dialog_command(),
            PickerOutcome::Cancel => self.overlay = Overlay::None,
            PickerOutcome::Moved | PickerOutcome::Filtered | PickerOutcome::Unchanged => {}
        }
    }

    fn run_action_dialog_command(&mut self) {
        let Overlay::Action(dialog) = &self.overlay else {
            return;
        };
        let Some(command) = dialog.commands.get(dialog.cursor.selected).copied() else {
            return;
        };
        self.dispatch_command(command);
    }

    pub(super) fn handle_name_prompt(&mut self, input: &Event) {
        let Overlay::Name(prompt) = &mut self.overlay else {
            return;
        };
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => self.submit_name(),
                KeyCode::Esc => self.overlay = Overlay::None,
                KeyCode::Backspace => {
                    prompt.input.edit(TextEdit::Backspace);
                }
                KeyCode::Char(character) if !chord(key.modifiers) => {
                    prompt.input.edit(TextEdit::Insert(character));
                }
                _ => {}
            },
            Event::Paste(text) => {
                prompt.input.edit(TextEdit::Paste(text.clone()));
            }
            _ => match left_click(input).and_then(|pointer| prompt.buttons.hit(pointer)) {
                Some(ConfirmButton::Primary) => self.submit_name(),
                Some(ConfirmButton::Secondary) => self.overlay = Overlay::None,
                None => {}
            },
        }
    }

    fn cancel_confirmation(&mut self) {
        if matches!(
            self.overlay,
            Overlay::Confirm {
                operation: GitOperation::RemoveRemote(_),
                ..
            }
        ) {
            self.open_remotes();
        } else {
            self.overlay = Overlay::None;
        }
    }

    pub(super) fn handle_confirmation(&mut self, input: &Event) {
        let Overlay::Confirm {
            buttons,
            operation,
            push_controls,
            ..
        } = &mut self.overlay
        else {
            return;
        };
        let buttons = *buttons;
        if let GitOperation::Push { force, remote, .. } = operation {
            let pressed = matches!(input, Event::Key(key)
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Char(' '));
            if pressed
                || left_click(input).is_some_and(|point| push_controls.force.contains(point.into()))
            {
                *force = !*force;
                return;
            }
            let direction = match input {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Left => Some(-1),
                    KeyCode::Right => Some(1),
                    _ => None,
                },
                _ if left_click(input)
                    .is_some_and(|point| push_controls.remote.contains(point.into())) =>
                {
                    Some(1)
                }
                _ => None,
            };
            if let Some(direction) = direction {
                let remotes = &self.ops.command_context.remotes;
                if !remotes.is_empty() {
                    let index = remotes
                        .iter()
                        .position(|item| item.name == *remote)
                        .unwrap_or(0);
                    let next = if direction < 0 {
                        (index + remotes.len() - 1) % remotes.len()
                    } else {
                        (index + 1) % remotes.len()
                    };
                    *remote = remotes[next].name.clone();
                }
                return;
            }
        }
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => self.execute_pending_operation(),
                KeyCode::Esc => self.cancel_confirmation(),
                _ => {}
            },
            _ => match left_click(input).and_then(|pointer| buttons.hit(pointer)) {
                Some(ConfirmButton::Primary) => self.execute_pending_operation(),
                Some(ConfirmButton::Secondary) => self.cancel_confirmation(),
                None => {}
            },
        }
    }

    pub(super) fn handle_commands(&mut self, input: &Event) {
        let Overlay::Commands(palette) = &mut self.overlay else {
            return;
        };
        if let Some(button) = left_click(input).and_then(|pointer| palette.buttons.hit(pointer)) {
            match button {
                ConfirmButton::Primary => self.run_selected_command(),
                ConfirmButton::Secondary => self.overlay = Overlay::None,
            }
            return;
        }
        let Some(edit) = picker_edit(input, palette.list_area, palette.cursor.scroll, true) else {
            return;
        };
        let len = palette.rows.len();
        let page = usize::from(palette.list_area.height);
        match update_picker(
            PickerState {
                query: Some(&mut palette.query),
                cursor: &mut palette.cursor,
                len,
                page,
            },
            edit,
        ) {
            PickerOutcome::Activate => self.run_selected_command(),
            PickerOutcome::Cancel => self.overlay = Overlay::None,
            PickerOutcome::Filtered => palette.filter(),
            PickerOutcome::Moved | PickerOutcome::Unchanged => {}
        }
    }

    pub(super) fn draw_result(&self, frame: &mut Frame<'_>, view: &OperationResultView) {
        let height = (view.lines.len() as u16)
            .saturating_add(RESULT_CARD_CHROME)
            .clamp(9, 20);
        let title_style = if view.success {
            theme::success_title()
        } else {
            theme::error_title()
        };
        let inner = widgets::dialog_frame_titled(
            frame,
            Span::styled(view.title(), title_style),
            theme::DIALOG_CARD,
            height,
        );
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        frame.render_widget(
            Paragraph::new(view.lines.clone()).wrap(Wrap { trim: false }),
            regions[0],
        );
        frame.render_widget(
            widgets::dialog_button(
                "[Esc] Close",
                false,
                area_hovered(self.shell.mouse_position, regions[2]),
            ),
            regions[2],
        );
    }

    pub(super) fn draw_quick_actions(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let cells = QUICK_ACTIONS.map(|command| {
            let enabled = if command == CommandId::StashChanges {
                self.ops.command_context.has_repository
            } else {
                command.availability(&self.ops.command_context).enabled
            };
            let running = self
                .foreground
                .action
                .as_ref()
                .filter(|action| {
                    let active = action.kind.command();
                    active == Some(command)
                        || (command == CommandId::StashChanges
                            && active == Some(CommandId::StashPop))
                })
                .map(|action| {
                    let active_command = action.kind.command().unwrap_or(command);
                    let progress = match &action.kind {
                        ForegroundKind::ResetContext { .. } => "Loading Reset targets",
                        _ => active_command.progress_label(),
                    };
                    (action.started.elapsed(), progress)
                });
            ActionCell {
                label: command.quick_action_label(),
                shortcut: command.quick_action_shortcut(),
                icon: quick_action_icon(command),
                enabled,
                selected: false,
                running,
            }
        });
        let areas = draw_action_bar(
            frame,
            area,
            &cells,
            self.shell.shortcut_hints,
            self.shell.mouse_position,
        );
        self.ops.quick_action_areas = QUICK_ACTIONS.into_iter().zip(areas).collect();
    }

    pub(super) fn draw_action_dialog(&self, frame: &mut Frame<'_>, dialog: &mut ActionDialog) {
        let inner = widgets::dialog_frame(
            frame,
            dialog.title,
            theme::DIALOG_MEDIUM,
            ACTION_DIALOG_HEIGHT,
        );
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        dialog.list_area = regions[0];
        let hovered = self.shell.mouse_position.and_then(|pointer| {
            dialog
                .cursor
                .row_at(regions[0], pointer, dialog.commands.len(), 0)
        });
        let items = dialog.commands.iter().enumerate().map(|(index, command)| {
            let availability = command.availability(&self.ops.command_context);
            let suffix = availability
                .reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            ListItem::new(format!("{}{}", command.title(), suffix)).style(theme::hover(
                if availability.enabled {
                    Style::default()
                } else {
                    theme::disabled()
                },
                hovered == Some(index),
            ))
        });
        let mut state = ListState::default().with_selected(Some(dialog.cursor.selected));
        *state.offset_mut() = dialog.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[0],
            &mut state,
        );
        dialog.cursor.scroll = state.offset();
        frame.render_widget(widgets::footer_hint(&dialog.status), regions[1]);
        dialog.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Enter] Run", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
    }

    pub(super) fn draw_name_prompt(&self, frame: &mut Frame<'_>, prompt: &mut NamePrompt) {
        let command = prompt.command;
        let inner = widgets::dialog_frame(
            frame,
            command.title(),
            theme::DIALOG_MEDIUM,
            NAME_PROMPT_HEIGHT,
        );
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let input = Line::from(vec![
            Span::raw(prompt.input.text.clone()),
            theme::cursor_span(prompt.input.cursor_started.elapsed()),
        ]);
        frame.render_widget(
            Paragraph::new(input).block(Block::default().borders(Borders::ALL).title("Name")),
            regions[0],
        );
        let validating = self.foreground.action.as_ref().filter(|action| {
            matches!(
                &action.kind,
                ForegroundKind::ValidateName {
                    command: active, ..
                } if *active == command
            )
        });
        let status = if let Some(action) = validating {
            Line::from(vec![
                theme::spinner_span(action.started.elapsed()),
                Span::raw(" Validating name"),
            ])
        } else if let Some(error) = prompt.input.error.as_deref() {
            Line::styled(format!("Error: {error}"), theme::error_text())
        } else {
            Line::default()
        };
        frame.render_widget(Paragraph::new(status), regions[1]);
        prompt.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Enter] Continue", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }

    pub(super) fn draw_confirmation(
        &self,
        frame: &mut Frame<'_>,
        command: CommandId,
        operation: &GitOperation,
        buttons: &mut ConfirmButtons,
        push_controls: &mut PushControls,
    ) {
        let force = match operation {
            GitOperation::Push { force, .. } => Some(*force),
            _ => None,
        };
        let commit_label = |target: &str| {
            self.graph
                .commits
                .iter()
                .find(|commit| commit.sha == target)
                .map(|commit| {
                    format!(
                        "{} · {}",
                        super::graph::short_commit(target),
                        commit.subject
                    )
                })
                .unwrap_or_else(|| target.to_owned())
        };
        let has_commit_context = matches!(
            operation,
            GitOperation::CheckoutCommit(_)
                | GitOperation::CheckoutBranch(_)
                | GitOperation::CherryPick(_)
                | GitOperation::Revert(_)
                | GitOperation::RebaseHere(_)
                | GitOperation::InteractiveRebaseOnto(_)
                | GitOperation::InteractiveRebase(_)
        );
        let inner = widgets::dialog_frame(
            frame,
            "Confirm Git operation",
            theme::DIALOG_MEDIUM,
            CONFIRMATION_HEIGHT
                + 2 * u16::from(force.is_some())
                + 3 * u16::from(has_commit_context)
                + 6 * u16::from(matches!(
                    operation,
                    GitOperation::DiscardTrackedChanges { .. }
                        | GitOperation::DiscardUnstagedPaths(_)
                )),
        );
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(u16::from(force.is_some())),
                Constraint::Length(u16::from(force.is_some())),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let description = match operation {
            GitOperation::DiscardUnstagedPaths(paths) => format!(
                "Discard unstaged changes in {} tracked files?\nRestore from: index (staged content)\nStaged changes and untracked files are kept.\n\nUnstaged edits will be permanently lost.\n\n{}{}",
                paths.len(),
                paths.iter().take(5).cloned().collect::<Vec<_>>().join("\n"),
                if paths.len() > 5 {
                    format!("\n... and {} more", paths.len() - 5)
                } else {
                    String::new()
                }
            ),
            GitOperation::DiscardTrackedChanges { head } => format!(
                "Repository: {}\nRestore to HEAD: {}\n\nDiscard ALL staged and unstaged changes to tracked files?\nUntracked files are kept. Submodule working trees are not restored.\n\nThis permanently deletes uncommitted tracked-file changes.\nFilename case collisions may prevent a clean working tree.\n\n{}",
                self.repository
                    .as_ref()
                    .map(|repo| repo.root().display().to_string())
                    .unwrap_or_default(),
                head,
                operation.preview()
            ),
            GitOperation::PullFastForward | GitOperation::Push { .. } => format!(
                "Branch: {}\n\n{}\n\n{}",
                self.ops
                    .command_context
                    .current_branch
                    .as_deref()
                    .unwrap_or("detached HEAD"),
                operation.preview(),
                if force == Some(true) {
                    "Overwrites the remote branch. The lease refuses the push when the remote moved since your last fetch."
                } else {
                    "The repository may change. Conflicts are left intact for recovery."
                }
            ),

            GitOperation::CheckoutCommit(target) => format!(
                "Checkout target: {}\n\n{}\n\nChecks out this commit with a detached HEAD.",
                commit_label(target),
                operation.preview()
            ),
            GitOperation::CheckoutBranch(branch) => format!(
                "Checkout branch: {branch}\n\n{}\n\nSwitches the working tree to this branch.",
                operation.preview()
            ),
            GitOperation::CherryPick(target) | GitOperation::Revert(target) => {
                let branch = self
                    .ops
                    .command_context
                    .current_branch
                    .as_deref()
                    .unwrap_or("detached HEAD");
                let label = if matches!(operation, GitOperation::CherryPick(_)) {
                    "Commit to apply"
                } else {
                    "Commit to revert"
                };
                format!(
                    "Current branch: {branch}\n{label}: {}\n\n{}\n\nConflicts are left intact for recovery.",
                    commit_label(target),
                    operation.preview()
                )
            }
            GitOperation::RebaseHere(target)
            | GitOperation::InteractiveRebaseOnto(target)
            | GitOperation::InteractiveRebase(target) => {
                let branch = self
                    .ops
                    .command_context
                    .current_branch
                    .as_deref()
                    .unwrap_or("detached HEAD");
                let target = target
                    .strip_prefix("refs/heads/")
                    .or_else(|| target.strip_prefix("refs/remotes/"))
                    .unwrap_or(target);
                let label = if matches!(operation, GitOperation::InteractiveRebase(_)) {
                    "Rewrite from (inclusive)"
                } else {
                    "Rebase onto"
                };
                format!(
                    "Branch: {branch}\n{label}: {}\n\n{}\n\nRewrites commit history.",
                    commit_label(target),
                    operation.preview()
                )
            }
            GitOperation::RemoveRemote(name) => format!(
                "Remove remote {name}?\n\n{}\n\nRemoves its local configuration and remote-tracking branches.",
                operation.preview()
            ),
            _ => format!(
                "Run this Git operation?\n\n{}\n\nThe repository may change. Conflicts are left intact for recovery.",
                operation.preview()
            ),
        };
        frame.render_widget(
            Paragraph::new(description).wrap(Wrap { trim: true }),
            regions[0],
        );
        *push_controls = PushControls::default();
        if let GitOperation::Push { remote, .. } = operation {
            push_controls.remote = regions[1];
            let default = self
                .ops
                .command_context
                .remotes
                .first()
                .is_some_and(|item| item.name == *remote);
            frame.render_widget(
                Paragraph::new(format!(
                    "Remote: {remote}{}  [←/→] Change",
                    if default { " (default)" } else { "" }
                ))
                .style(theme::hint()),
                regions[1],
            );
        }
        if let Some(force) = force {
            push_controls.force = regions[2];
            frame.render_widget(
                Paragraph::new(format!(
                    "[{}] Force with lease  [Space]",
                    if force { 'x' } else { ' ' }
                ))
                .style(if force {
                    theme::warning_text()
                } else {
                    theme::hint()
                }),
                regions[2],
            );
        }
        frame.render_widget(widgets::footer_hint(command.title()), regions[3]);
        *buttons = widgets::dialog_footer(
            frame,
            regions[4],
            ["[Enter] Run", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }

    pub(super) fn draw_command_palette(&self, frame: &mut Frame<'_>, palette: &mut CommandPalette) {
        let height = (COMMANDS.len() as u16).saturating_add(COMMAND_PALETTE_CHROME);
        let inner = widgets::dialog_frame(frame, "Commands", theme::DIALOG_WIDE, height);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        frame.render_widget(search_box(&palette.query), regions[0]);
        frame.render_widget(
            Paragraph::new(format!("  {:<COMMAND_COLUMN_WIDTH$}Status", "Command"))
                .style(theme::section_header().bg(theme::SURFACE_HEADER)),
            regions[1],
        );
        palette.list_area = regions[2];
        let hovered = self.shell.mouse_position.and_then(|pointer| {
            palette
                .cursor
                .row_at(regions[2], pointer, palette.rows.len(), 0)
        });
        let items = palette.rows.iter().enumerate().map(|(index, command)| {
            let availability = command.availability(&self.ops.command_context);
            let update_available = *command == CommandId::Update
                && matches!(
                    self.update.status,
                    super::update::UpdateStatus::Available(_)
                );
            let (command_style, status_style) = if availability.enabled {
                (Style::default(), Style::default())
            } else {
                (theme::disabled(), theme::warning_text())
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<COMMAND_COLUMN_WIDTH$}", command.title()),
                    command_style,
                ),
                Span::styled(
                    if update_available {
                        "Update available"
                    } else {
                        command_status_label(availability.reason)
                    },
                    status_style,
                ),
            ]))
            .style(theme::hover(Style::default(), hovered == Some(index)))
        });
        let mut state = ListState::default()
            .with_selected((!palette.rows.is_empty()).then_some(palette.cursor.selected));
        *state.offset_mut() = palette.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[2],
            &mut state,
        );
        palette.cursor.scroll = state.offset();
        frame.render_widget(
            widgets::footer_hint(&list_status(
                &palette.query.text,
                palette.rows.len(),
                "command",
                "commands",
            )),
            regions[3],
        );
        palette.buttons = widgets::dialog_footer(
            frame,
            regions[4],
            ["[Enter] Run", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;

    use crate::git::{GitOperation, Repository};
    use crate::ui::effect::{ForegroundRequest, ForegroundResult, KnownFingerprint, RequestId};
    use crate::ui::lanes::ForegroundKind;
    use crate::ui::overlay::Overlay;
    use crate::ui::shell::{ActiveTab, DEFAULT_ACTIVE_TAB, PaneFocus};
    use crate::ui::test_support::{
        buffer_text, click, find_text, find_text_in_row, git, intercept_foreground, offline_app,
        press, render, temp_repo, wait_for_foreground,
    };
    use crate::ui::{App, theme};

    use super::{
        CommandId, CommandPalette, NamePrompt, OperationResultView, QUICK_ACTIONS,
        RESULT_VISIBLE_DURATION, quick_action_icon,
    };

    #[test]
    fn successful_commit_result_shows_committed_with_a_check() {
        let operation = GitOperation::Commit {
            message: "Done".into(),
            amend: false,
        };
        let view = OperationResultView::operation(&operation, "Created commit", true);
        assert_eq!(view.title(), "✓ Committed");
        let failed = OperationResultView::operation(&operation, "Hook failed", false);
        assert_eq!(failed.title(), "Commit failed");
    }

    #[test]
    fn command_palette_search_ignores_case_for_typing_and_paste() {
        let mut app = offline_app();
        for query in ["rebase", "REBASE", "ReBaSe"] {
            for paste in [false, true] {
                app.handle(Event::Key(KeyEvent::new(
                    KeyCode::Char('p'),
                    KeyModifiers::ALT,
                )))
                .unwrap();
                if paste {
                    app.handle(Event::Paste(query.into())).unwrap();
                } else {
                    for character in query.chars() {
                        let modifiers = if character.is_uppercase() {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        };
                        app.handle(Event::Key(KeyEvent::new(
                            KeyCode::Char(character),
                            modifiers,
                        )))
                        .unwrap();
                    }
                }
                let Overlay::Commands(palette) = &app.overlay else {
                    panic!("Commands")
                };
                assert_eq!(palette.query.text, query);
                assert_eq!(
                    palette.rows,
                    [CommandId::RebaseHere, CommandId::InteractiveRebase]
                );
                press(&mut app, KeyCode::Esc);
            }
        }
    }

    #[test]
    fn palette_header_actions_open_existing_dialogs() {
        let root = temp_repo("palette-header-actions");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(ActiveTab::History);
        crate::ui::test_support::wait_for_refresh(&mut app);
        for (query, command) in [
            ("RESET", CommandId::ResetCurrentBranch),
            ("cherry-pick", CommandId::CherryPick),
            ("Revert", CommandId::Revert),
            ("COPY SHA", CommandId::CopySha),
        ] {
            app.open_commands();
            app.handle(Event::Paste(query.into())).unwrap();
            let Overlay::Commands(palette) = &app.overlay else {
                panic!("Commands")
            };
            assert_eq!(palette.rows, [command]);
            if command == CommandId::CopySha {
                let buffer = render(&mut app, 100, 32);
                let (x, y) = find_text(&buffer, "Copy SHA…").expect("Copy SHA row");
                click(&mut app, x, y);
            } else {
                press(&mut app, KeyCode::Enter);
            }
            wait_for_foreground(&mut app);
            match command {
                CommandId::ResetCurrentBranch => assert!(matches!(app.overlay, Overlay::Reset(_))),
                CommandId::CopySha => assert!(matches!(app.overlay, Overlay::CopySha(_))),
                _ => assert!(
                    matches!(app.overlay, Overlay::Confirm { command: selected, .. } if selected == command)
                ),
            }
            app.overlay = Overlay::None;
        }
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn palette_stash_opens_both_actions_even_on_a_clean_worktree() {
        let root = temp_repo("palette-stash");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.open_commands();
        app.handle(Event::Paste("stash".into())).unwrap();
        press(&mut app, KeyCode::Enter);
        let Overlay::Action(dialog) = &app.overlay else {
            panic!("Stash actions")
        };
        assert_eq!(
            dialog.commands,
            &[CommandId::StashChanges, CommandId::StashPop]
        );
        assert!(app.foreground.action.is_none());
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fetch_dispatch_enqueues_the_shared_foreground_operation_without_waiting() {
        let root = temp_repo("fetch-async");
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, _result_tx) = intercept_foreground(&mut app);

        app.run_quick_action(CommandId::Fetch);
        let request = request_rx.try_recv().expect("foreground Fetch request");
        assert!(matches!(
            request,
            ForegroundRequest::Operation {
                path,
                command: CommandId::Fetch,
                operation: GitOperation::Fetch,
                ..
            } if path == fs::canonicalize(&root).unwrap()
        ));
        assert!(matches!(
            app.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Operation {
                command: CommandId::Fetch,
                ..
            })
        ));
        let buffer = render(&mut app, 100, 20);
        let screen = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Fetching"));

        app.handle(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.shell.active_tab, ActiveTab::History);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commands_alt_and_mouse_dispatch_the_identical_fetch_request() {
        let root = temp_repo("fetch-routes");
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, _result_tx) = intercept_foreground(&mut app);

        let mut palette = CommandPalette::new();
        palette.rows = vec![CommandId::Fetch];
        app.overlay = Overlay::Commands(palette);
        app.run_selected_command();
        let from_commands = request_rx.try_recv().unwrap();
        app.foreground.action = None;

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        let from_alt = request_rx.try_recv().unwrap();
        app.foreground.action = None;
        render(&mut app, 100, 20);
        let fetch_area = app
            .ops
            .quick_action_areas
            .iter()
            .find_map(|(command, area)| (*command == CommandId::Fetch).then_some(*area))
            .unwrap();
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: fetch_area.x.saturating_add(1),
            row: fetch_area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        let from_mouse = request_rx.try_recv().unwrap();

        let signature = |request| match request {
            ForegroundRequest::Operation {
                path,
                command,
                operation,
                ..
            } => (path, command, operation),
            other => panic!("unexpected request: {other:?}"),
        };
        let expected = signature(from_commands);
        assert_eq!(signature(from_alt), expected);
        assert_eq!(signature(from_mouse), expected);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn noninteractive_command_operations_all_enqueue_the_same_worker() {
        let root = temp_repo("operation-routes");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        let operations = [
            (CommandId::Fetch, GitOperation::Fetch),
            (CommandId::Pull, GitOperation::PullFastForward),
            (
                CommandId::Push,
                GitOperation::Push {
                    force: false,
                    remote: "origin".into(),
                    branch: "main".into(),
                },
            ),
            (CommandId::StashChanges, GitOperation::StashChanges),
            (CommandId::StashPop, GitOperation::StashPop),
            (
                CommandId::CreateBranch,
                GitOperation::CreateBranch {
                    name: "feature".to_owned(),
                    commit: "abc123".to_owned(),
                },
            ),
            (
                CommandId::CreateTag,
                GitOperation::CreateTag {
                    name: "v1".to_owned(),
                    commit: "abc123".to_owned(),
                },
            ),
            (
                CommandId::CheckoutCommit,
                GitOperation::CheckoutCommit("abc123".to_owned()),
            ),
            (
                CommandId::RebaseHere,
                GitOperation::RebaseHere("abc123".to_owned()),
            ),
            (
                CommandId::CherryPick,
                GitOperation::CherryPick("abc123".to_owned()),
            ),
            (CommandId::Revert, GitOperation::Revert("abc123".to_owned())),
            (
                CommandId::ResetCurrentBranch,
                GitOperation::Reset {
                    mode: crate::git::ResetMode::Mixed,
                    target: "abc123".to_owned(),
                    target_name: "main".to_owned(),
                    expected_branch: "main".to_owned(),
                    expected_head: "def456".to_owned(),
                },
            ),
        ];
        for (command, operation) in operations {
            app.execute_operation(command, operation.clone());
            assert!(matches!(
                request_rx.try_recv().unwrap(),
                ForegroundRequest::Operation {
                    command: actual_command,
                    operation: actual_operation,
                    ..
                } if actual_command == command && actual_operation == operation
            ));
            app.foreground.action = None;
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_refresh_enqueues_the_existing_refresh_worker_without_waiting() {
        let root = temp_repo("explicit-refresh");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;

        app.dispatch_command(CommandId::Refresh);
        let request = request_rx.try_recv().expect("background refresh request");
        assert_eq!(
            request.context.active_repository,
            Some(app.active_path.clone())
        );
        assert_eq!(request.previous_fingerprint, KnownFingerprint::default());
        assert!(app.refresh.in_flight);
        assert!(app.refresh.workspaces_pending);
        assert!(app.refresh.active_progress().is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fetch_success_expires_and_failure_owns_the_first_escape() {
        let root = temp_repo("fetch-result");
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);

        app.run_quick_action(CommandId::Fetch);
        let (id, path, operation) = match request_rx.try_recv().unwrap() {
            ForegroundRequest::Operation {
                id,
                path,
                operation,
                ..
            } => (id, path, operation),
            other => panic!("unexpected request: {other:?}"),
        };
        result_tx
            .send(ForegroundResult::Operation {
                id: RequestId::new(id.get().wrapping_add(1)),
                path: path.clone(),
                command: CommandId::Fetch,
                operation: operation.clone(),
                result: Err("stale".to_owned()),
            })
            .unwrap();
        app.receive_foreground_results();
        assert!(app.foreground.action.is_some());
        assert!(app.overlay.result().is_none());
        result_tx
            .send(ForegroundResult::Operation {
                id,
                path: path.clone(),
                command: CommandId::Fetch,
                operation: operation.clone(),
                result: Ok("Git completed without output.".to_owned()),
            })
            .unwrap();
        app.receive_foreground_results();
        assert!(app.refresh.in_flight);
        let success = app.overlay.result_mut().expect("Fetch result");
        assert!(success.success);
        success.shown_at = Instant::now() - RESULT_VISIBLE_DURATION;
        app.maybe_auto_refresh();
        assert!(app.overlay.result().is_none());

        app.run_quick_action(CommandId::Fetch);
        let (id, path, operation) = match request_rx.try_recv().unwrap() {
            ForegroundRequest::Operation {
                id,
                path,
                operation,
                ..
            } => (id, path, operation),
            other => panic!("unexpected request: {other:?}"),
        };
        result_tx
            .send(ForegroundResult::Operation {
                id,
                path,
                command: CommandId::Fetch,
                operation,
                result: Err("fatal: unavailable".to_owned()),
            })
            .unwrap();
        app.receive_foreground_results();
        app.overlay.result_mut().unwrap().shown_at = Instant::now() - Duration::from_secs(60);
        app.expire_result();
        assert!(app.overlay.result().is_some());
        app.focus = PaneFocus::Workspaces;
        press(&mut app, KeyCode::Char('q'));
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay.result().is_none());
        assert_eq!(app.focus, PaneFocus::Workspaces);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pull_confirmation_enqueues_foreground_job_and_keeps_input_responsive() {
        let root = temp_repo("pull-async");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        app.overlay = Overlay::Confirm {
            command: CommandId::Pull,
            operation: GitOperation::PullFastForward,
            buttons: Default::default(),
            push_controls: Default::default(),
        };

        app.execute_pending_operation();
        let request = request_rx.try_recv().expect("foreground Pull request");
        assert!(matches!(
            request,
            ForegroundRequest::Operation {
                path,
                command: CommandId::Pull,
                operation: GitOperation::PullFastForward,
                ..
            } if path == fs::canonicalize(&root).unwrap()
        ));
        assert!(matches!(
            app.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Operation {
                command: CommandId::Pull,
                operation: GitOperation::PullFastForward,
                ..
            })
        ));
        assert_ne!(
            theme::spinner_frame(Duration::ZERO),
            theme::spinner_frame(Duration::from_millis(100))
        );
        let buffer = render(&mut app, 80, 20);
        let quick_actions = (0..80)
            .map(|column| buffer[(column, 3)].symbol())
            .collect::<String>();
        assert!(quick_actions.contains("Pulling"));
        app.handle(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.shell.active_tab, ActiveTab::History);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pull_result_colors_diffstat_and_success_expires_while_failure_waits_for_escape() {
        let mut success = OperationResultView::operation(
            &GitOperation::PullFastForward,
            "Updating abc..def\nFast-forward\nsrc/app.rs | 6 +++---",
            true,
        );
        assert_eq!(success.title(), "Pull complete");
        let lines = &success.lines;
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("git pull --ff-only") && span.style.fg == Some(theme::ACCENT)
        }));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("Fast-forward") && span.style.fg == Some(theme::SUCCESS)
        }));
        assert!(
            lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.content.contains("+++") && span.style.fg == Some(theme::SUCCESS)
            })
        );
        assert!(
            lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.content.contains("---") && span.style.fg == Some(theme::ERROR)
            })
        );

        success.shown_at = Instant::now() - RESULT_VISIBLE_DURATION;
        assert!(success.expired());

        let root = temp_repo("pull-error");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let mut failure =
            OperationResultView::operation(&GitOperation::PullFastForward, "fatal: failed", false);
        failure.shown_at = Instant::now() - Duration::from_secs(60);
        app.show_result(failure);
        app.expire_result();
        assert!(app.overlay.result().is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay.result().is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pull_worker_updates_a_local_clone_and_success_card_dismisses_independently() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-pull-worker-{unique}"));
        let remote = base.join("remote.git");
        let source = base.join("source");
        let clone = base.join("clone");
        fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "--bare", remote.to_str().unwrap()]);
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Test Author"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        fs::write(source.join("tracked.txt"), "base\n").unwrap();
        git(&source, &["add", "tracked.txt"]);
        git(&source, &["commit", "-m", "Base"]);
        git(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&source, &["push", "-u", "origin", "main"]);
        git(
            &base,
            &[
                "clone",
                "-b",
                "main",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        fs::write(source.join("tracked.txt"), "base\nremote\n").unwrap();
        git(&source, &["add", "tracked.txt"]);
        git(&source, &["commit", "-m", "Remote change"]);
        git(&source, &["push"]);

        let repository = Repository::discover(&clone).unwrap();
        let mut app = App::load(repository).unwrap();
        app.overlay = Overlay::Confirm {
            command: CommandId::Pull,
            operation: GitOperation::PullFastForward,
            buttons: Default::default(),
            push_controls: Default::default(),
        };
        app.execute_pending_operation();
        assert!(app.foreground.action.is_some());
        wait_for_foreground(&mut app);

        let result = app.overlay.result().expect("Pull result card");
        assert!(result.success);
        assert!(
            result
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content.contains("Fast-forward"))
        );
        assert!(app.refresh.in_flight);
        assert_eq!(
            fs::read_to_string(clone.join("tracked.txt")).unwrap(),
            "base\nremote\n"
        );
        let refresh_deadline = Instant::now() + Duration::from_secs(5);
        while app.refresh.deferred_result.is_none() && Instant::now() < refresh_deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.receive_refresh_result();
        }
        assert!(
            app.refresh.deferred_result.is_some(),
            "post-Pull refresh did not finish"
        );
        app.overlay.result_mut().unwrap().shown_at = Instant::now();
        app.maybe_auto_refresh();
        assert!(!app.refresh.in_flight);
        assert!(app.overlay.result().is_some());
        let branch = app.workspaces.repository_rows[0]
            .branch_status
            .as_ref()
            .expect("refreshed branch status");
        assert_eq!((branch.ahead, branch.behind), (0, 0));
        app.overlay.result_mut().unwrap().shown_at = Instant::now() - RESULT_VISIBLE_DURATION;
        app.expire_result();
        assert!(app.overlay.result().is_none());

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn quick_action_bar_keeps_one_geometry_in_changes_graph_and_alt_layers() {
        let mut app = offline_app();
        let row_text = |buffer: &Buffer, row, width| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
        };
        let buffer = render(&mut app, 80, 16);
        assert!(row_text(&buffer, 0, 80).trim().is_empty());
        assert!(row_text(&buffer, 1, 80).starts_with(" Changes    Graph    Files    Commands "));
        assert!(row_text(&buffer, 2, 80).trim().is_empty());
        let normal = row_text(&buffer, 3, 80);
        assert!(row_text(&buffer, 4, 80).trim().is_empty());
        assert!(row_text(&buffer, 5, 80).contains("Diff Last"));
        let mut last = 0;
        for command in QUICK_ACTIONS {
            let position = normal[last..]
                .find(command.quick_action_label())
                .map(|offset| last + offset)
                .expect("quick action label");
            assert!(position >= last);
            last = position + command.quick_action_label().len();
        }
        let changes_areas = app.ops.quick_action_areas.clone();
        assert_eq!(app.shell.tab_area.y, 1);
        assert_eq!(app.diff.changes_body_area.y, 9);
        assert_eq!(changes_areas.len(), QUICK_ACTIONS.len());
        assert!(
            changes_areas
                .iter()
                .all(|(_, area)| area.y == 3 && area.height == 1)
        );
        let first_button = changes_areas[0].1;
        assert_eq!(
            buffer[(first_button.x, first_button.y)].bg,
            theme::SURFACE_PANEL
        );
        assert_ne!(
            buffer[(first_button.right(), first_button.y)].bg,
            theme::SURFACE_PANEL,
            "the gap must remain visible around the button surface"
        );

        app.set_tab(ActiveTab::History);
        let buffer = render(&mut app, 80, 16);
        assert_eq!(app.ops.quick_action_areas, changes_areas);
        assert!(row_text(&buffer, 0, 80).trim().is_empty());
        assert!(row_text(&buffer, 2, 80).trim().is_empty());
        assert!(row_text(&buffer, 3, 80).contains("Stash"));
        assert!(row_text(&buffer, 4, 80).trim().is_empty());
        assert!(row_text(&buffer, 5, 80).contains("Copy SHA"));
        assert!(row_text(&buffer, 6, 80).trim().is_empty());

        app.shell.shortcut_hints = true;
        let buffer = render(&mut app, 80, 16);
        assert_eq!(app.ops.quick_action_areas, changes_areas);
        let navigation = row_text(&buffer, 1, 80);
        assert!(navigation.starts_with("1 Changes  2 Graph"));
        assert!(!navigation.contains("A Actions"));
        let hints = row_text(&buffer, 3, 80);
        for hint in [
            "F  Fetch",
            "L  Pull",
            "C  Commit",
            "U  Push",
            "B  Branch",
            "S  Stash",
        ] {
            assert!(hints.contains(hint), "missing {hint:?} in {hints:?}");
        }
        app.shell.shortcut_hints = false;
        app.set_tab(ActiveTab::Changes);
        let buffer = render(&mut app, 40, 12);
        assert!(row_text(&buffer, 0, 40).starts_with(" Changes "));
        assert!(row_text(&buffer, 1, 40).trim().is_empty());
        assert_eq!(app.ops.quick_action_areas.len(), QUICK_ACTIONS.len());
        assert_eq!(app.ops.quick_action_areas.last().unwrap().1.right(), 40);
        assert!(row_text(&buffer, 2, 40).contains("Stash"));
        assert_eq!(app.shell.tab_area.y, 0);
        assert_eq!(app.diff.changes_body_area.y, 5);
    }

    #[test]
    fn command_palette_aligns_command_and_status_columns() {
        let root = temp_repo("command-grid");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_commands();
        let buffer = render(&mut app, 100, 30);

        let rows = buffer
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let header = rows
            .iter()
            .find(|row| row.contains("Command") && row.contains("Status"));
        assert!(header.is_some());
        let refresh = rows
            .iter()
            .find(|row| row.contains("Refresh"))
            .expect("Refresh row");
        assert!(!refresh.contains("Ready"));
        let fetch = rows
            .iter()
            .find(|row| row.contains("Fetch") && row.contains("No remote"));
        assert!(fetch.is_some());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn result_card_dismisses_on_enter_or_click_and_swallows_other_keys() {
        let mut app = offline_app();
        app.show_result(OperationResultView::operation(
            &GitOperation::Fetch,
            "fatal: offline",
            false,
        ));
        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('/'),
            KeyCode::Tab,
            KeyCode::Down,
        ] {
            assert!(
                !app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
                    .unwrap()
            );
            assert!(app.overlay.result().is_some(), "{code:?} is swallowed");
        }
        assert_eq!(app.shell.active_tab, DEFAULT_ACTIVE_TAB);
        press(&mut app, KeyCode::Enter);
        assert!(app.overlay.result().is_none());

        app.show_result(OperationResultView::message(None, false, "boom"));
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert!(app.overlay.result().is_some());
        click(&mut app, 1, 1);
        assert!(app.overlay.result().is_none());

        app.show_result(OperationResultView::message(
            Some(CommandId::Refresh),
            true,
            "Repository refreshed.",
        ));
        app.maybe_auto_refresh();
        assert!(app.overlay.result().is_some());
        app.overlay.result_mut().unwrap().shown_at = Instant::now() - RESULT_VISIBLE_DURATION;
        app.maybe_auto_refresh();
        assert!(app.overlay.result().is_none());
    }

    #[test]
    fn result_cards_expire_only_on_success_and_title_falls_back() {
        let mut success = OperationResultView::operation(&GitOperation::Fetch, "done", true);
        assert_eq!(success.title(), "Fetch complete");
        assert_eq!(success.command, Some(CommandId::Fetch));
        assert!(!success.expired());
        success.shown_at = Instant::now() - RESULT_VISIBLE_DURATION;
        assert!(success.expired());

        let mut failure = OperationResultView::message(None, false, "fatal: unavailable");
        assert_eq!(failure.title(), "Git operation failed");
        failure.shown_at = Instant::now() - Duration::from_secs(60);
        assert!(!failure.expired());
        assert_eq!(failure.lines.len(), 1);
    }

    #[test]
    fn command_palette_has_a_frame_cursor_status_row_and_footer_buttons() {
        let mut app = offline_app();
        app.open_commands();
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(text.contains("Commands"));
        assert!(text.contains(theme::CURSOR_GLYPH));
        assert!(!text.contains("Type to filter commands"));
        assert!(text.contains("[Enter] Run"));
        assert!(text.contains("[Esc] Close"));
        let (x, y) = find_text(&buffer, "Command ").expect("column header");
        assert_eq!(buffer[(x, y)].fg, theme::ACCENT);
        assert_eq!(buffer[(x, y)].bg, theme::SURFACE_HEADER);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        let (x, y) = find_text(&buffer, "Refresh").expect("first row");
        assert_eq!(buffer[(x, y)].bg, theme::SURFACE_FOCUS);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x - 2, y)].symbol(), "▶");
        let (x, y) = find_text(&buffer, "Fetch").expect("disabled row");
        assert_eq!(buffer[(x, y)].fg, theme::MUTED);
        let status = find_text_in_row(&buffer, y, "Unavailable").expect("status column");
        assert_eq!(buffer[(status, y)].fg, theme::WARNING);

        for character in "fetch".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        let buffer = render(&mut app, 100, 32);
        assert!(buffer_text(&buffer).contains("1 matching command"));
        let Overlay::Commands(palette) = &app.overlay else {
            panic!("palette stays open while filtering");
        };
        assert_eq!(palette.rows, vec![CommandId::Fetch]);
        let run = palette.buttons.primary;
        click(&mut app, run.x + 2, run.y + 1);
        let result = app
            .overlay
            .result()
            .expect("running a disabled command explains why");
        assert!(!result.success);
        assert!(
            result
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content.contains("not a Git repository"))
        );
        press(&mut app, KeyCode::Esc);

        app.open_commands();
        render(&mut app, 100, 32);
        let Overlay::Commands(palette) = &app.overlay else {
            panic!("palette reopened");
        };
        let close = palette.buttons.secondary;
        click(&mut app, close.x + 2, close.y + 1);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn name_prompt_shows_cursor_hint_error_and_footer_buttons() {
        let mut app = offline_app();
        app.overlay = Overlay::Name(NamePrompt::new(CommandId::CreateTag));
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(text.contains("Tag…"));
        assert!(text.contains(theme::CURSOR_GLYPH));
        assert!(!text.contains("Enter a Git branch or tag name."));
        assert!(text.contains("[Enter] Continue"));
        assert!(text.contains("[Esc] Cancel"));

        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Enter);
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Error: Not a Git repository").expect("error row");
        assert_eq!(buffer[(x, y)].fg, theme::ERROR);
        assert!(!buffer_text(&buffer).contains("Enter a Git branch or tag name."));
        let Overlay::Name(prompt) = &app.overlay else {
            panic!("prompt stays open on error");
        };
        let cancel = prompt.buttons.secondary;
        click(&mut app, cancel.x + 2, cancel.y + 1);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn push_confirmation_toggles_force_with_lease_by_space_and_by_click() {
        let mut app = offline_app();
        app.overlay = Overlay::Confirm {
            command: CommandId::Push,
            operation: GitOperation::Push {
                force: false,
                remote: "origin".into(),
                branch: "main".into(),
            },
            buttons: Default::default(),
            push_controls: Default::default(),
        };
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("[ ] Force with lease  [Space]"), "{text}");
        assert!(!text.contains("--force-with-lease"), "{text}");

        press(&mut app, KeyCode::Char(' '));
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("[x] Force with lease"), "{text}");
        assert!(text.contains("git push --force-with-lease"), "{text}");
        assert!(text.contains("Overwrites the remote branch"), "{text}");

        let Overlay::Confirm { push_controls, .. } = &app.overlay else {
            unreachable!("confirmation was just opened");
        };
        let (x, y) = (push_controls.force.x + 1, push_controls.force.y);
        click(&mut app, x, y);
        let Overlay::Confirm { operation, .. } = &app.overlay else {
            unreachable!("the checkbox does not close the dialog");
        };
        assert_eq!(
            *operation,
            GitOperation::Push {
                force: false,
                remote: "origin".into(),
                branch: "main".into()
            }
        );

        app.overlay = Overlay::Confirm {
            command: CommandId::Pull,
            operation: GitOperation::PullFastForward,
            buttons: Default::default(),
            push_controls: Default::default(),
        };
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(!text.contains("Force with lease"), "{text}");
        press(&mut app, KeyCode::Char(' '));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
    }

    #[test]
    fn discard_requires_confirmation_and_escape_preserves_changes() {
        let (root, mut app) = crate::ui::test_support::committed_change("discard-dialog");
        app.dispatch_command(CommandId::DiscardTrackedChanges);
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        assert!(app.foreground.action.is_none());
        let screen = buffer_text(&render(&mut app, 120, 32));
        assert!(screen.contains("Restore to HEAD:"));
        assert!(screen.contains("ALL staged and unstaged"));
        assert!(screen.contains("Untracked files are kept"));
        assert!(screen.contains("permanently deletes"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "one\ntwo changed\nthree\n"
        );
        app.dispatch_command(CommandId::DiscardTrackedChanges);
        let (requests, _results) = crate::ui::test_support::intercept_foreground(&mut app);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            requests.try_recv().unwrap(),
            ForegroundRequest::Operation {
                operation: GitOperation::DiscardTrackedChanges { .. },
                ..
            }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmation_names_the_command_and_accepts_enter_escape_and_buttons() {
        let confirm = |app: &mut App| {
            app.overlay = Overlay::Confirm {
                command: CommandId::Pull,
                operation: GitOperation::PullFastForward,
                buttons: Default::default(),
                push_controls: Default::default(),
            };
        };
        let mut app = offline_app();
        confirm(&mut app);
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(text.contains("Confirm Git operation"));
        assert!(text.contains("git pull --ff-only"));
        assert!(text.contains("[Enter] Run"));
        assert!(text.contains("[Esc] Cancel"));
        let (x, y) = find_text(&buffer, "Pull").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::MUTED);
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));

        confirm(&mut app);
        render(&mut app, 100, 32);
        let Overlay::Confirm { buttons, .. } = &app.overlay else {
            unreachable!("confirmation was just opened");
        };
        let cancel = buttons.secondary;
        click(&mut app, cancel.x + 2, cancel.y + 1);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn result_card_shows_a_colored_title_status_row_and_close_button() {
        let mut app = offline_app();
        app.show_result(OperationResultView::operation(
            &GitOperation::Fetch,
            "fatal: offline",
            false,
        ));
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Fetch failed").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::ERROR);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        let (x, y) = find_text(&buffer, "fatal: offline").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::ERROR);
        let text = buffer_text(&buffer);
        assert!(!text.contains("Esc, Enter, or click closes"));
        assert!(text.contains("[Esc] Close"));

        app.show_result(OperationResultView::operation(
            &GitOperation::Fetch,
            "done",
            true,
        ));
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Fetch complete").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::SUCCESS);
        assert!(!buffer_text(&buffer).contains("Closes after 2 s"));
    }

    #[test]
    fn quick_action_icons_use_one_shared_two_cell_gap_and_branch_shares_the_badge_glyph() {
        for command in QUICK_ACTIONS {
            assert!(!quick_action_icon(command).is_empty());
        }
        assert_eq!(
            quick_action_icon(CommandId::CreateBranchAtHead),
            theme::BRANCH_GLYPH
        );
        assert_eq!(quick_action_icon(CommandId::Refresh), "");

        let mut app = offline_app();
        let buffer = render(&mut app, 120, 20);
        let text = buffer_text(&buffer);
        let normal_positions = QUICK_ACTIONS.map(|command| {
            let area = app
                .ops
                .quick_action_areas
                .iter()
                .find_map(|(owner, area)| (*owner == command).then_some(*area))
                .expect("quick action area");
            let label_x = find_text_in_row(&buffer, area.y, command.quick_action_label())
                .expect("quick action label");
            let icon_x = (area.x..area.right())
                .find(|x| buffer[(*x, area.y)].symbol() == quick_action_icon(command))
                .expect("quick action icon");
            assert_eq!(label_x, icon_x + 3, "{command:?} icon-label gap");
            (command, icon_x, label_x, area.y)
        });
        for command in QUICK_ACTIONS {
            let action = format!(
                "{}  {}",
                quick_action_icon(command),
                command.quick_action_label()
            );
            assert!(text.contains(&action), "missing {action:?} in {text:?}");
        }
        let (x, y) = find_text(&buffer, "Branch").expect("Branch quick action");
        assert_eq!(buffer[(x - 3, y)].symbol(), theme::BRANCH_GLYPH);
        assert!(!buffer_text(&buffer).contains('⑂'));

        app.shell.shortcut_hints = true;
        let hinted = render(&mut app, 120, 20);
        for (command, icon_x, label_x, row) in normal_positions {
            assert_eq!(
                find_text_in_row(&hinted, row, command.quick_action_label()),
                Some(label_x),
                "{command:?} label moved in shortcut mode"
            );
            assert_eq!(
                hinted[(icon_x, row)].symbol(),
                command
                    .quick_action_shortcut()
                    .to_ascii_uppercase()
                    .to_string(),
                "{command:?} shortcut moved from the icon column"
            );
        }
    }

    #[test]
    fn stash_quick_action_opens_the_shared_action_dialog() {
        let root = temp_repo("stash-action-dialog");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();

        app.run_quick_action(CommandId::StashChanges);
        let buffer = render(&mut app, 100, 24);
        let text = buffer_text(&buffer);
        assert!(matches!(app.overlay, Overlay::Action(_)));
        assert!(text.contains("Stash changes"));
        assert!(text.contains("Pop stash: stash is empty"));
        assert!(text.contains("Choose a stash operation"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::StashChanges)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stash_cell_owns_progress_for_push_and_pop() {
        let mut app = offline_app();
        app.ops.command_context.has_repository = true;
        app.foreground.action = Some(crate::ui::lanes::ForegroundAction {
            id: RequestId::FIRST,
            kind: ForegroundKind::Operation {
                path: app.active_path.clone(),
                command: CommandId::StashPop,
                operation: GitOperation::StashPop,
            },
            started: Instant::now(),
        });

        let buffer = render(&mut app, 100, 20);
        let (x, y) = find_text(&buffer, "Running").expect("Stash pop progress in Stash cell");
        let stash_area = app.ops.quick_action_areas.last().unwrap().1;
        assert!(stash_area.contains((x, y).into()));
        assert!(!app.status_bar_text().contains("Running"));
    }

    #[test]
    fn quick_actions_use_the_panel_surface_muted_disabled_labels_and_an_accent_spinner() {
        let mut app = offline_app();
        let buffer = render(&mut app, 100, 20);
        let (x, y) = find_text(&buffer, "Fetch").expect("Fetch quick action");
        assert_eq!(buffer[(x, y)].fg, theme::MUTED);
        assert_eq!(buffer[(x, y)].bg, theme::SURFACE_PANEL);

        let root = temp_repo("quick-action-style");
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let buffer = render(&mut app, 100, 20);
        let (x, y) = find_text(&buffer, "Fetch").expect("Fetch quick action");
        assert_eq!(buffer[(x, y)].fg, theme::TEXT);
        assert_eq!(buffer[(x, y)].bg, theme::SURFACE_PANEL);

        let (_request_rx, _result_tx) = intercept_foreground(&mut app);
        app.run_quick_action(CommandId::Fetch);
        let buffer = render(&mut app, 100, 20);
        let (x, y) = find_text(&buffer, "Fetching").expect("running label");
        let spinner = &buffer[(x - 3, y)];
        assert!(theme::SPINNER_FRAMES.contains(&spinner.symbol()));
        assert_eq!(spinner.fg, theme::ACCENT);
        assert_eq!(spinner.bg, theme::SURFACE_PANEL);

        fs::remove_dir_all(root).unwrap();
    }
}
