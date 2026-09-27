use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::git::GitOperation;
use crate::herdr::HerdrAgent;
use crate::ui::effect::{ForegroundRequest, RequestId};
use crate::ui::lanes::ForegroundKind;
use crate::ui::overlay::Overlay;
use crate::ui::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState, TextEdit,
    TextField, area_hovered, chord, left_click, picker_edit, update_picker,
};
use crate::ui::{App, theme};

use super::{CommandId, list_status, search_box};

const COMMIT_DIALOG_HEIGHT: u16 = 16;
const AGENT_PICKER_HEIGHT: u16 = 16;

#[derive(Debug)]
pub(in crate::ui) struct AgentPicker {
    pub(in crate::ui) title: &'static str,
    pub(in crate::ui) busy: bool,
    pub(in crate::ui) notice: Option<String>,
    pub(in crate::ui) query: TextField,
    pub(in crate::ui) agents: Vec<HerdrAgent>,
    pub(in crate::ui) cursor: ListCursor,
    pub(in crate::ui) list_area: Rect,
    pub(in crate::ui) buttons: ConfirmButtons,
}

impl AgentPicker {
    pub(in crate::ui) fn new(agents: Vec<HerdrAgent>) -> Self {
        Self {
            title: "Send Agent",
            busy: false,
            notice: None,
            query: TextField::new(),
            agents,
            cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
        }
    }

    pub(in crate::ui) fn filtered(&self) -> Vec<&HerdrAgent> {
        let query = self.query.text.to_lowercase();
        self.agents
            .iter()
            .filter(|agent| {
                query.is_empty()
                    || format!(
                        "{} {} {} {}",
                        agent.workspace, agent.agent_type, agent.label, agent.pane_id
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .collect()
    }

    pub(in crate::ui) fn selected(&self) -> Option<&HerdrAgent> {
        self.filtered().get(self.cursor.selected).copied()
    }

    pub(in crate::ui) fn prioritize_current_workspace(&mut self, current_path: Option<&Path>) {
        let Some(current_path) = current_path else {
            return;
        };
        let current_path = normalize_path(current_path);
        let mut workspace = Vec::new();
        let mut others = Vec::new();
        for agent in self.agents.drain(..) {
            let cwd = Path::new(&agent.cwd);
            if !agent.cwd.is_empty() && normalize_path(cwd) == current_path {
                workspace.push(agent);
            } else {
                others.push(agent);
            }
        }
        self.agents.extend(workspace);
        self.agents.extend(others);

        if !self.agents.is_empty() {
            self.cursor.selected = 0;
            self.cursor.scroll = 0;
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum CommitFocus {
    Subject,
    Description,
    Amend,
    Submit,
    Cancel,
}

impl CommitFocus {
    const CYCLE: [Self; 5] = [
        Self::Subject,
        Self::Description,
        Self::Amend,
        Self::Submit,
        Self::Cancel,
    ];

    pub(in crate::ui) fn next(self) -> Self {
        let index = Self::CYCLE
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0);
        Self::CYCLE[(index + 1) % Self::CYCLE.len()]
    }

    pub(in crate::ui) fn previous(self) -> Self {
        let index = Self::CYCLE
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0);
        Self::CYCLE[(index + Self::CYCLE.len() - 1) % Self::CYCLE.len()]
    }
}

#[derive(Debug)]
pub(in crate::ui) struct CommitDialog {
    pub(in crate::ui) subject: TextField,
    pub(in crate::ui) description: TextField,
    pub(in crate::ui) subject_area: Rect,
    pub(in crate::ui) description_area: Rect,
    pub(in crate::ui) amend: bool,
    pub(in crate::ui) agent: bool,
    pub(in crate::ui) focus: CommitFocus,
    pub(in crate::ui) amend_area: Rect,
    pub(in crate::ui) buttons: ConfirmButtons,
    pub(in crate::ui) agent_picker: Option<AgentPicker>,
}

impl Default for CommitDialog {
    fn default() -> Self {
        Self {
            subject: TextField::new(),
            description: TextField::new(),
            subject_area: Rect::default(),
            description_area: Rect::default(),
            amend: false,
            agent: false,
            focus: CommitFocus::Subject,
            amend_area: Rect::default(),
            buttons: ConfirmButtons::default(),
            agent_picker: None,
        }
    }
}

impl CommitDialog {
    pub(in crate::ui) fn failed(message: &str, amend: bool, error: String) -> Self {
        let mut dialog = Self {
            amend,
            ..Self::default()
        };
        dialog.load_message(message);
        dialog.subject.set_error(error);
        dialog
    }

    fn load_message(&mut self, message: &str) {
        let (subject, description) = message.split_once('\n').unwrap_or((message, ""));
        self.subject.text = subject.trim_end_matches('\r').to_owned();
        self.description.text = description.trim_matches(['\r', '\n']).to_owned();
    }

    fn message(&self) -> String {
        let subject = self.subject.text.trim();
        let description = self.description.text.trim();
        if description.is_empty() {
            subject.to_owned()
        } else {
            format!("{subject}\n\n{description}")
        }
    }

    fn editing(&self) -> bool {
        matches!(self.focus, CommitFocus::Subject | CommitFocus::Description)
    }

    fn edit_input(&mut self, edit: TextEdit) {
        self.subject.error = None;
        if self.focus == CommitFocus::Description {
            self.description.edit(edit);
        } else if let TextEdit::Paste(text) = edit {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let (subject, description) = text.split_once('\n').unwrap_or((&text, ""));
            self.subject.edit(TextEdit::Paste(subject.to_owned()));
            if !description.is_empty() {
                self.description.edit(TextEdit::Paste(
                    description.trim_start_matches('\n').to_owned(),
                ));
                self.focus = CommitFocus::Description;
            }
        } else {
            self.subject.edit(edit);
        }
    }

    pub(in crate::ui) fn focus_input(&mut self) {
        self.focus = CommitFocus::Subject;
        self.subject.cursor_started = Instant::now();
    }
}

impl App {
    pub(in crate::ui) fn open_commit_dialog(&mut self) {
        self.overlay = Overlay::Commit(CommitDialog::default());
    }

    pub(in crate::ui) fn close_commit_dialog(&mut self) {
        if self.foreground.action.as_ref().is_some_and(|action| {
            matches!(
                &action.kind,
                ForegroundKind::ListAgentPanes { .. } | ForegroundKind::SendAgentRequest { .. }
            )
        }) {
            self.foreground.action = None;
        }
        self.overlay = Overlay::None;
    }

    pub(in crate::ui) fn activate_commit_focus(&mut self) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        match dialog.focus {
            CommitFocus::Subject
            | CommitFocus::Description
            | CommitFocus::Amend
            | CommitFocus::Submit => self.submit_commit_or_agent(),
            CommitFocus::Cancel => self.close_commit_dialog(),
        }
    }

    pub(in crate::ui) fn toggle_commit_amend(&mut self) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        dialog.amend = !dialog.amend;
        dialog.focus_input();
        dialog.subject.error = None;
        if !dialog.amend {
            return;
        }
        let Some(repository) = self.repository.as_ref() else {
            dialog.subject.set_error("Not a Git repository");
            dialog.amend = false;
            return;
        };
        if self.ops.command_context.head_commit.is_none() {
            dialog.subject.set_error("No HEAD commit to amend.");
            dialog.amend = false;
            return;
        }
        let path = repository.root().to_owned();
        let id = self.foreground.next_id;
        let request = ForegroundRequest::LoadAmendMessage {
            id,
            path: path.clone(),
        };
        let _ = self
            .foreground
            .request_foreground(request, ForegroundKind::LoadAmendMessage { path });
    }

    fn submit_commit_or_agent(&mut self) {
        let Overlay::Commit(dialog) = &self.overlay else {
            return;
        };
        if dialog.agent {
            self.request_agent_picker();
        } else {
            self.submit_commit();
        }
    }

    fn submit_commit(&mut self) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        let message = dialog.message();
        if dialog.subject.text.trim().is_empty() {
            dialog.subject.set_error("Enter a commit subject.");
            return;
        }
        if dialog.amend && self.ops.command_context.head_commit.is_none() {
            dialog.subject.set_error("No HEAD commit to amend.");
            return;
        }
        if !dialog.amend && !self.ops.command_context.has_staged_changes {
            dialog.subject.set_error("Stage changes before committing.");
            return;
        }
        let operation = GitOperation::Commit {
            message,
            amend: dialog.amend,
        };
        self.overlay = Overlay::None;
        self.start_operation(CommandId::Commit, operation);
    }

    pub(in crate::ui) fn request_agent_picker(&mut self) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        let Some(repository) = self.repository.as_ref() else {
            dialog.subject.set_error("Not a Git repository");
            return;
        };
        let path = repository.root().to_owned();
        let id = self.foreground.next_id;
        let request = ForegroundRequest::ListAgentPanes {
            id,
            path: path.clone(),
        };
        if self
            .foreground
            .request_foreground(request, ForegroundKind::ListAgentPanes { path })
            .is_ok()
        {
            dialog.subject.error = None;
        }
    }

    pub(in crate::ui) fn send_selected_agent_request(&mut self) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        let Some(agent) = dialog
            .agent_picker
            .as_ref()
            .and_then(AgentPicker::selected)
            .cloned()
        else {
            return;
        };
        let Some(repository) = self.repository.as_ref() else {
            return;
        };
        let path = repository.root().to_owned();
        let id = self.foreground.next_id;
        let request = ForegroundRequest::SendAgentRequest {
            id,
            path: path.clone(),
            agent: agent.clone(),
            prompt: dialog.message(),
            amend: dialog.amend,
        };
        if self
            .foreground
            .request_foreground(request, ForegroundKind::SendAgentRequest { path, agent })
            .is_ok()
        {
            dialog.agent_picker = None;
        }
    }

    pub(in crate::ui) fn apply_amend_message(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<String, String>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::LoadAmendMessage { path: active }
                    if active == &path
                )
            })
            .is_none()
        {
            return;
        }
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        if !dialog.amend {
            return;
        }
        match result {
            Ok(message) => {
                dialog.load_message(&message);
                dialog.subject.error = None;
            }
            Err(error) => {
                dialog.amend = false;
                dialog.subject.set_error(error);
            }
        }
        dialog.focus_input();
    }

    pub(in crate::ui) fn apply_agent_panes(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<Vec<HerdrAgent>, String>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::ListAgentPanes { path: active }
                    if active == &path
                )
            })
            .is_none()
        {
            return;
        }
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        match result {
            Ok(agents) if agents.is_empty() => {
                dialog.subject.set_error("No open Agent pane.");
            }
            Ok(agents) => {
                let mut picker = AgentPicker::new(agents);
                let current_path = self
                    .repository
                    .as_ref()
                    .map(|repository| Path::new(repository.root()));
                picker.prioritize_current_workspace(current_path);
                dialog.agent_picker = Some(picker)
            }
            Err(error) => dialog.subject.set_error(error),
        }
    }

    pub(in crate::ui) fn apply_agent_request(
        &mut self,
        id: RequestId,
        path: PathBuf,
        agent: HerdrAgent,
        result: Result<(), String>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::SendAgentRequest {
                        path: active,
                        agent: active_agent,
                    } if active == &path && active_agent == &agent
                )
            })
            .is_none()
        {
            return;
        }
        match result {
            Ok(_) => {
                if matches!(self.overlay, Overlay::Commit(_)) {
                    self.overlay = Overlay::None;
                }
                self.show_result(super::OperationResultView::named_message(
                    "Agent request",
                    true,
                    &format!("Request sent to {}", agent.label),
                ));
            }
            Err(error) => {
                if let Overlay::Commit(dialog) = &mut self.overlay {
                    dialog.subject.set_error(error);
                }
            }
        }
    }

    pub(in crate::ui) fn handle_agent_picker(&mut self, input: &Event) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        let Some(picker) = dialog.agent_picker.as_mut() else {
            return;
        };
        if let Some(button) = left_click(input).and_then(|pointer| picker.buttons.hit(pointer)) {
            match button {
                ConfirmButton::Primary => self.send_selected_agent_request(),
                ConfirmButton::Secondary => dialog.agent_picker = None,
            }
            return;
        }
        let Some(edit) = picker_edit(input, picker.list_area, picker.cursor.scroll, true) else {
            return;
        };
        let len = picker.filtered().len();
        let page = usize::from(picker.list_area.height);
        match update_picker(
            PickerState {
                query: Some(&mut picker.query),
                cursor: &mut picker.cursor,
                len,
                page,
            },
            edit,
        ) {
            PickerOutcome::Activate => self.send_selected_agent_request(),
            PickerOutcome::Cancel => dialog.agent_picker = None,
            PickerOutcome::Moved | PickerOutcome::Filtered | PickerOutcome::Unchanged => {}
        }
    }

    pub(in crate::ui) fn handle_commit_dialog(&mut self, input: &Event) {
        let Overlay::Commit(dialog) = &mut self.overlay else {
            return;
        };
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => self.close_commit_dialog(),
                KeyCode::BackTab => dialog.focus = dialog.focus.previous(),
                KeyCode::Tab => dialog.focus = dialog.focus.next(),
                KeyCode::Enter
                    if key.modifiers.contains(KeyModifiers::SHIFT) && dialog.editing() =>
                {
                    if dialog.focus == CommitFocus::Subject {
                        dialog.focus = CommitFocus::Description;
                    } else {
                        dialog.edit_input(TextEdit::Newline);
                    }
                }
                KeyCode::Enter => self.activate_commit_focus(),
                KeyCode::Char(' ') if dialog.focus == CommitFocus::Amend => {
                    self.toggle_commit_amend()
                }
                KeyCode::Backspace if dialog.editing() => {
                    dialog.edit_input(TextEdit::Backspace);
                }
                KeyCode::Char(character) if dialog.editing() && !chord(key.modifiers) => {
                    dialog.edit_input(TextEdit::Insert(character));
                }
                _ => {}
            },
            Event::Paste(text) if dialog.editing() => {
                dialog.edit_input(TextEdit::Paste(text.clone()));
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
            {
                let point = (mouse.column, mouse.row);
                if dialog.subject_area.contains(point.into()) {
                    dialog.focus_input();
                } else if dialog.description_area.contains(point.into()) {
                    dialog.focus = CommitFocus::Description;
                    dialog.description.cursor_started = Instant::now();
                } else if dialog.amend_area.contains(point.into()) {
                    self.toggle_commit_amend();
                } else {
                    match dialog.buttons.hit(point) {
                        Some(ConfirmButton::Primary) => self.submit_commit_or_agent(),
                        Some(ConfirmButton::Secondary) => self.close_commit_dialog(),
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn commit_progress(&self) -> Option<(&'static str, Duration)> {
        let action = self.foreground.action.as_ref()?;
        let label = match &action.kind {
            ForegroundKind::LoadAmendMessage { .. } => "Loading HEAD message",
            ForegroundKind::ListAgentPanes { .. } => "Finding Agent panes",
            ForegroundKind::SendAgentRequest { .. } => "Sending",
            _ => return None,
        };
        Some((label, action.started.elapsed()))
    }

    pub(in crate::ui) fn draw_commit_dialog(
        &self,
        frame: &mut Frame<'_>,
        dialog: &mut CommitDialog,
    ) {
        let inner =
            widgets::dialog_frame(frame, "Commit", theme::DIALOG_WIDE, COMMIT_DIALOG_HEIGHT);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(9),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let fields = Layout::vertical([
            Constraint::Length(if regions[0].height < 6 { 1 } else { 3 }),
            Constraint::Min(1),
        ])
        .split(regions[0]);
        dialog.subject_area = fields[0];
        dialog.description_area = fields[1];
        for (field, title, focus, area) in [
            (&dialog.subject, "Subject", CommitFocus::Subject, fields[0]),
            (
                &dialog.description,
                "Description",
                CommitFocus::Description,
                fields[1],
            ),
        ] {
            let mut lines: Vec<Line<'_>> = if field.is_empty() {
                vec![Line::from(Span::styled(title, theme::hint()))]
            } else {
                field.text.split('\n').map(Line::raw).collect()
            };
            if dialog.focus == focus {
                let cursor = theme::cursor_span(field.cursor_started.elapsed());
                let last = lines.last_mut().expect("split returns one line");
                if field.is_empty() {
                    last.spans.insert(0, cursor);
                } else {
                    last.spans.push(cursor);
                }
            }
            let border = if area.height >= 3 { 2 } else { 0 };
            let vertical = lines
                .len()
                .saturating_sub(usize::from(area.height.saturating_sub(border)));
            let horizontal = lines
                .last()
                .map_or(0, Line::width)
                .saturating_sub(usize::from(area.width.saturating_sub(border)));
            frame.render_widget(
                Paragraph::new(lines)
                    .scroll((
                        vertical.min(u16::MAX as usize) as u16,
                        horizontal.min(u16::MAX as usize) as u16,
                    ))
                    .block(
                        Block::default()
                            .borders(if border == 0 {
                                Borders::NONE
                            } else {
                                Borders::ALL
                            })
                            .border_style(if dialog.focus == focus {
                                theme::accent()
                            } else {
                                Style::default()
                            }),
                    ),
                area,
            );
        }
        let control = |focused: bool, area: Rect| {
            theme::hover(
                if focused {
                    theme::focus_row()
                } else {
                    Style::default()
                },
                area_hovered(self.shell.mouse_position, area),
            )
        };
        dialog.amend_area = regions[1];
        frame.render_widget(
            Paragraph::new(format!("[{}] Amend", if dialog.amend { "x" } else { " " }))
                .style(control(dialog.focus == CommitFocus::Amend, regions[1])),
            regions[1],
        );
        let status = if let Some(error) = dialog.subject.error.as_deref() {
            Line::styled(format!("Error: {error}"), theme::error_text())
        } else if let Some((label, elapsed)) = self.commit_progress() {
            Line::from(vec![
                theme::spinner_span(elapsed),
                Span::raw(format!(" {label}")),
            ])
        } else {
            Line::default()
        };
        frame.render_widget(Paragraph::new(status), regions[2]);
        let confirm = "[Enter]";
        let verb = if dialog.agent { "Send" } else { "Commit" };
        let focused = match dialog.focus {
            CommitFocus::Submit => Some(ConfirmButton::Primary),
            CommitFocus::Cancel => Some(ConfirmButton::Secondary),
            CommitFocus::Subject | CommitFocus::Description | CommitFocus::Amend => None,
        };
        dialog.buttons = widgets::dialog_footer(
            frame,
            regions[3],
            [&format!("{confirm} {verb}"), "[Esc] Cancel"],
            focused,
            self.shell.mouse_position,
        );
    }

    pub(in crate::ui) fn draw_agent_picker(&self, frame: &mut Frame<'_>, picker: &mut AgentPicker) {
        let inner =
            widgets::dialog_frame(frame, picker.title, theme::DIALOG_WIDE, AGENT_PICKER_HEIGHT);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        frame.render_widget(search_box(&picker.query), regions[0]);
        let labels = picker
            .filtered()
            .into_iter()
            .map(|agent| agent_row(agent, regions[1].width))
            .collect::<Vec<_>>();
        picker.list_area = regions[1];
        let hovered = self
            .shell
            .mouse_position
            .and_then(|pointer| picker.cursor.row_at(regions[1], pointer, labels.len(), 0));
        let items = labels.iter().enumerate().map(|(index, label)| {
            ListItem::new(label.clone())
                .style(theme::hover(Style::default(), hovered == Some(index)))
        });
        let mut state = ListState::default().with_selected(
            (!labels.is_empty()).then_some(picker.cursor.selected.min(labels.len() - 1)),
        );
        *state.offset_mut() = picker.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[1],
            &mut state,
        );
        picker.cursor.scroll = state.offset();
        let status = if picker.busy {
            Line::from(vec![
                theme::spinner_span(
                    self.foreground
                        .action
                        .as_ref()
                        .map(|a| a.started.elapsed())
                        .unwrap_or_default(),
                ),
                Span::raw(
                    picker
                        .notice
                        .clone()
                        .unwrap_or_else(|| "Loading agents".into()),
                ),
            ])
        } else if let Some(error) = picker.query.error.as_ref() {
            Line::styled(error.clone(), theme::error_text())
        } else {
            Line::styled(
                picker.notice.clone().unwrap_or_else(|| {
                    list_status(
                        &picker.query.text,
                        labels.len(),
                        "Agent pane",
                        "Agent panes",
                    )
                }),
                theme::hint(),
            )
        };
        frame.render_widget(Paragraph::new(status), regions[2]);
        picker.buttons = widgets::dialog_footer(
            frame,
            regions[3],
            ["[Enter] Send", "[Esc] Back"],
            None,
            self.shell.mouse_position,
        );
    }
}

fn agent_row(agent: &HerdrAgent, width: u16) -> Line<'static> {
    use crate::ui::widgets::truncate_to_width;
    let workspace = if agent.workspace.is_empty() {
        "Space"
    } else {
        &agent.workspace
    };
    let (icon, kind) = match agent.agent_type.as_str() {
        "codex" => ("", "Codex"),
        "claude" => ("✳", "Claude"),
        "" => ("", "Agent"),
        other => ("", other),
    };
    let space = truncate_to_width(workspace, usize::from(width / 3).min(24));
    let prefix = format!("▣ {space} › {icon} {kind} · ");
    let name = truncate_to_width(
        &agent.label,
        usize::from(width).saturating_sub(Line::from(prefix.as_str()).width() + 2),
    );
    Line::from(vec![
        Span::styled(format!("▣ {space}"), theme::accent()),
        Span::styled(" › ", theme::hint()),
        Span::styled(format!("{icon} {kind}"), theme::accent_bold()),
        Span::styled(" · ", theme::hint()),
        Span::raw(name),
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Instant;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Modifier;

    use crate::git::{GitOperation, Repository};
    use crate::herdr::HerdrAgent;
    use crate::ui::commands::CommandId;
    use crate::ui::effect::{ForegroundRequest, ForegroundResult, RefreshIntent, RequestId};
    use crate::ui::lanes::{ForegroundAction, ForegroundKind};
    use crate::ui::overlay::Overlay;
    use crate::ui::test_support::{
        buffer_text, click, find_text, intercept_foreground, offline_app, press, render, temp_repo,
    };
    use crate::ui::{App, theme};

    use super::{AgentPicker, CommitFocus};

    #[test]
    fn commit_subject_and_description_submit_from_keyboard_and_mouse() {
        for mouse in [false, true] {
            let root = temp_repo("commit-fields");
            let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
            app.open_commit_dialog();
            let (requests, _) = intercept_foreground(&mut app);
            app.ops.command_context.has_staged_changes = true;
            app.handle(Event::Paste("Fix parser".into())).unwrap();
            if mouse {
                render(&mut app, 90, 24);
                let area = app.overlay.commit().unwrap().description_area;
                click(&mut app, area.x + 1, area.y + 1);
            } else {
                press(&mut app, KeyCode::Tab);
            }
            app.handle(Event::Paste(
                "Preserve blank lines.\n\nHandle Unicode.".into(),
            ))
            .unwrap();
            let dialog = app.overlay.commit().unwrap();
            assert_eq!(dialog.subject.text, "Fix parser");
            assert_eq!(
                dialog.description.text,
                "Preserve blank lines.\n\nHandle Unicode."
            );
            assert_eq!(dialog.focus, CommitFocus::Description);
            if mouse {
                render(&mut app, 90, 24);
                let area = app.overlay.commit().unwrap().buttons.primary;
                click(&mut app, area.x + 1, area.y + 1);
            } else {
                press(&mut app, KeyCode::Enter);
            }
            let ForegroundRequest::Operation {
                operation: GitOperation::Commit { message, .. },
                ..
            } = requests.try_recv().unwrap()
            else {
                panic!("expected commit");
            };
            assert_eq!(
                message,
                "Fix parser\n\nPreserve blank lines.\n\nHandle Unicode."
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn failed_commit_restores_subject_and_description() {
        let dialog =
            super::CommitDialog::failed("Subject\n\nBody\n\nDetails", true, "failed".into());
        assert_eq!(dialog.subject.text, "Subject");
        assert_eq!(dialog.description.text, "Body\n\nDetails");
        assert!(dialog.amend);
    }

    #[test]
    fn commit_fields_paste_validate_and_fit_small_terminals() {
        let mut app = offline_app();
        app.open_commit_dialog();
        let text = buffer_text(&render(&mut app, 40, 12));
        assert!(text.contains("Subject"));
        assert!(text.contains("Description"));
        press(&mut app, KeyCode::Tab);
        app.handle(Event::Paste("Body only".into())).unwrap();
        press(&mut app, KeyCode::Char('y'));
        press(&mut app, KeyCode::Enter);
        let dialog = app.overlay.commit().unwrap();
        assert_eq!(dialog.description.text, "Body onlyy");
        assert_eq!(
            dialog.subject.error.as_deref(),
            Some("Enter a commit subject.")
        );
        app.open_commit_dialog();
        app.handle(Event::Paste("Subject\r\n\r\nBody\r\nDetails".into()))
            .unwrap();
        let dialog = app.overlay.commit().unwrap();
        assert_eq!(dialog.subject.text, "Subject");
        assert_eq!(dialog.description.text, "Body\nDetails");
        assert_eq!(dialog.message(), "Subject\n\nBody\nDetails");
        let mut dialog = super::CommitDialog::default();
        dialog.subject.text = "Subject only".into();
        assert_eq!(dialog.message(), "Subject only");
    }

    fn review_agent() -> HerdrAgent {
        HerdrAgent {
            pane_id: "pane-1".to_owned(),
            label: "Review agent".to_owned(),
            cwd: "/tmp/project".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn failed_commit_starts_its_refresh_even_while_the_dialog_reopens() {
        let root = temp_repo("commit-refresh");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (operation_rx, operation_result_tx) = intercept_foreground(&mut app);
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        let operation = GitOperation::Commit {
            message: "test".to_owned(),
            amend: false,
        };

        app.execute_operation(CommandId::Commit, operation.clone());
        let (id, path) = match operation_rx.try_recv().unwrap() {
            ForegroundRequest::Operation { id, path, .. } => (id, path),
            other => panic!("unexpected request: {other:?}"),
        };
        operation_result_tx
            .send(ForegroundResult::Operation {
                id,
                path,
                command: CommandId::Commit,
                operation,
                result: Err("commit failed".to_owned()),
            })
            .unwrap();

        app.receive_foreground_results();

        let dialog = app
            .overlay
            .commit()
            .expect("failed Commit reopens its dialog");
        assert_eq!(dialog.subject.text, "test");
        assert_eq!(dialog.subject.error.as_deref(), Some("commit failed"));
        let refresh = refresh_rx
            .try_recv()
            .expect("failed Commit must still start its operation refresh");
        assert_eq!(refresh.intent, RefreshIntent::Operation);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn description_shift_enter_inserts_a_line_and_cursor_follows_the_empty_final_row() {
        let mut app = offline_app();
        app.open_commit_dialog();
        press(&mut app, KeyCode::Tab);
        for character in "first line".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )))
        .unwrap();
        assert_eq!(
            app.overlay.commit().unwrap().description.text,
            "first line\n"
        );
        let buffer = render(&mut app, 90, 24);
        let (x, y) = find_text(&buffer, "first line").unwrap();
        assert_eq!(buffer[(x, y + 1)].symbol(), theme::CURSOR_GLYPH);

        press(&mut app, KeyCode::Char('s'));
        let buffer = render(&mut app, 90, 24);
        assert_eq!(buffer[(x, y + 1)].symbol(), "s");
        assert_eq!(buffer[(x + 1, y + 1)].symbol(), theme::CURSOR_GLYPH);
        assert_eq!(buffer[(x + 1, y + 1)].fg, theme::ACCENT);
    }

    #[test]
    fn commit_dialog_hides_agent_option_and_preserves_agent_submission() {
        let root = temp_repo("commit-ui");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_commit_dialog();
        let buffer = render(&mut app, 90, 24);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Subject"));
        assert!(rendered.contains("[ ] Amend"));
        assert!(!rendered.contains("Amend HEAD"));
        assert!(!rendered.contains("[ ] Send Agent"));
        assert!(rendered.contains("[Enter] Commit"));

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('R'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        {
            let dialog = app.overlay.commit_mut().unwrap();
            dialog.amend = true;
            dialog.agent = true;
            dialog.subject.text.clear();
        }
        let buffer = render(&mut app, 90, 24);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Description"));
        assert!(!rendered.contains("[x] Send Agent"));
        assert!(rendered.contains("[Enter] Send"));

        {
            let dialog = app.overlay.commit_mut().unwrap();
            dialog.subject.text.push('R');
            dialog.focus = CommitFocus::Submit;
        }
        let buffer = render(&mut app, 90, 24);
        assert!(buffer_text(&buffer).contains("[Enter] Send"));
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        app.activate_commit_focus();
        let ForegroundRequest::ListAgentPanes { path, .. } = request_rx.try_recv().unwrap() else {
            panic!("expected open agent pane listing");
        };
        assert_eq!(path, app.repository.as_ref().unwrap().root());
        let dialog = app.overlay.commit().unwrap();
        assert_eq!(dialog.subject.text, "R");
        assert!(dialog.amend);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_pane_listing_does_not_reopen_picker_after_commit_cancel() {
        let root = temp_repo("agent-list-stale");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        app.open_commit_dialog();
        app.overlay.commit_mut().unwrap().agent = true;

        app.request_agent_picker();
        let ForegroundRequest::ListAgentPanes { id, path } = request_rx.try_recv().unwrap() else {
            panic!("expected open agent pane listing");
        };
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(app.overlay.commit().is_none());

        result_tx
            .send(ForegroundResult::AgentPanesListed {
                id,
                path,
                result: Ok(vec![HerdrAgent {
                    pane_id: "pane-1".to_owned(),
                    label: "Review agent".to_owned(),
                    cwd: root.display().to_string(),
                    ..Default::default()
                }]),
            })
            .unwrap();
        app.receive_foreground_results();

        assert!(app.overlay.commit().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_request_failure_does_not_reopen_commit_after_cancel() {
        let root = temp_repo("agent-send-stale");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        app.open_commit_dialog();
        {
            let dialog = app.overlay.commit_mut().unwrap();
            dialog.agent = true;
            dialog.agent_picker = Some(AgentPicker::new(vec![HerdrAgent {
                pane_id: "pane-1".to_owned(),
                label: "Review agent".to_owned(),
                cwd: root.display().to_string(),
                ..Default::default()
            }]));
        }

        app.send_selected_agent_request();
        assert!(app.overlay.commit().unwrap().agent_picker.is_none());
        let ForegroundRequest::SendAgentRequest {
            id, path, agent, ..
        } = request_rx.try_recv().unwrap()
        else {
            panic!("expected agent request");
        };
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(app.overlay.commit().is_none());

        result_tx
            .send(ForegroundResult::AgentRequestSent {
                id,
                path,
                agent,
                result: Err("Agent pane closed".to_owned()),
            })
            .unwrap();
        app.receive_foreground_results();

        assert!(app.overlay.commit().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_amend_loads_head_message() {
        let root = temp_repo("amend-ui");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_commit_dialog();

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.overlay.commit().unwrap().subject.text, "p");

        app.ops.command_context.head_commit = Some("abc123".to_owned());
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        app.toggle_commit_amend();
        let ForegroundRequest::LoadAmendMessage { id, path } = request_rx.try_recv().unwrap()
        else {
            panic!("expected amend message read");
        };
        result_tx
            .send(ForegroundResult::AmendMessageLoaded {
                id,
                path,
                result: Ok("Previous subject\n\nPrevious body".to_owned()),
            })
            .unwrap();
        app.receive_foreground_results();
        let dialog = app.overlay.commit().unwrap();
        assert_eq!(dialog.subject.text, "Previous subject");
        assert_eq!(dialog.description.text, "Previous body");
        assert_eq!(dialog.focus, CommitFocus::Subject);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_dialog_shift_tab_cycles_backwards_and_enter_submits_from_controls() {
        let root = temp_repo("commit-grammar");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.open_commit_dialog();
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.overlay.commit().unwrap().focus, CommitFocus::Cancel);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.overlay.commit().unwrap().focus, CommitFocus::Submit);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.overlay.commit().unwrap().focus, CommitFocus::Subject);
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.overlay.commit().unwrap().subject.text, "y");
        assert!(buffer_text(&render(&mut app, 90, 24)).contains("[Enter] Commit"));

        press(&mut app, KeyCode::Tab);
        assert_eq!(
            app.overlay.commit().unwrap().focus,
            CommitFocus::Description
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.overlay.commit().unwrap().focus, CommitFocus::Amend);
        assert!(buffer_text(&render(&mut app, 90, 24)).contains("[Enter] Commit"));
        press(&mut app, KeyCode::Enter);
        let dialog = app.overlay.commit().unwrap();
        assert_eq!(dialog.subject.text, "y");
        assert_eq!(
            dialog.subject.error.as_deref(),
            Some("Stage changes before committing.")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_focus_cycles_in_both_directions() {
        assert_eq!(CommitFocus::Subject.next(), CommitFocus::Description);
        assert_eq!(CommitFocus::Description.next(), CommitFocus::Amend);
        assert_eq!(CommitFocus::Cancel.next(), CommitFocus::Subject);
        assert_eq!(CommitFocus::Subject.previous(), CommitFocus::Cancel);
        assert_eq!(CommitFocus::Amend.next(), CommitFocus::Submit);
        assert_eq!(CommitFocus::Submit.previous(), CommitFocus::Amend);
    }

    #[test]
    fn commit_dialog_status_row_shows_hint_error_and_spinner_and_n_never_cancels() {
        let mut app = offline_app();
        app.open_commit_dialog();
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(!text.contains("Tab/Shift+Tab Move focus"));
        assert!(text.contains("[Enter] Commit"));
        assert!(text.contains("[Esc] Cancel"));
        let (x, y) = find_text(&buffer, "Subject").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::HINT);
        assert_eq!(buffer[(x - 1, y)].symbol(), theme::CURSOR_GLYPH);
        assert_eq!(buffer[(x - 1, y)].fg, theme::ACCENT);

        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.overlay.commit().unwrap().subject.text, "n");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "[ ] Amend").unwrap();
        assert_eq!(buffer[(x, y)].bg, theme::SURFACE_FOCUS);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        let text = buffer_text(&buffer);
        assert!(text.contains("[Enter] Commit"));
        assert!(text.contains("[Esc] Cancel"));

        press(&mut app, KeyCode::Char(' '));
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Error: Not a Git repository").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::ERROR);
        assert!(!buffer_text(&buffer).contains("Tab/Shift+Tab"));

        app.overlay.commit_mut().unwrap().subject.error = None;
        app.foreground.action = Some(ForegroundAction {
            id: RequestId::FIRST,
            kind: ForegroundKind::LoadAmendMessage {
                path: PathBuf::from("/tmp/project"),
            },
            started: Instant::now(),
        });
        let buffer = render(&mut app, 100, 32);
        let (x, y) = find_text(&buffer, "Loading HEAD message").unwrap();
        assert!(theme::SPINNER_FRAMES.contains(&buffer[(x - 2, y)].symbol()));
        assert_eq!(buffer[(x - 2, y)].fg, theme::ACCENT);
        app.foreground.action = None;

        press(&mut app, KeyCode::Tab);
        assert_eq!(
            app.overlay.commit().unwrap().focus,
            CommitFocus::Description
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.overlay.commit().unwrap().focus, CommitFocus::Amend);
        press(&mut app, KeyCode::Char('n'));
        let dialog = app
            .overlay
            .commit()
            .expect("n is not a cancel key in the Commit dialog");
        assert_eq!(dialog.focus, CommitFocus::Amend);
        assert_eq!(dialog.subject.text, "n");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn agent_picker_has_a_status_row_and_footer_buttons() {
        let mut app = offline_app();
        app.open_commit_dialog();
        app.overlay.commit_mut().unwrap().agent_picker = Some(AgentPicker::new(vec![
            review_agent(),
            HerdrAgent {
                pane_id: "pane-2".to_owned(),
                label: "Build agent".to_owned(),
                cwd: "/tmp/project".to_owned(),
                ..Default::default()
            },
        ]));
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(text.contains("Send Agent"));
        assert!(!text.contains("Type to filter Agent panes"));
        assert!(text.contains("[Enter] Send"));
        assert!(text.contains("[Esc] Back"));
        assert!(text.contains(theme::CURSOR_GLYPH));
        let (x, y) = find_text(&buffer, "Review agent").unwrap();
        assert_eq!(buffer[(x, y)].bg, theme::SURFACE_FOCUS);
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        assert!(text.contains("▣ Space ›"));
        assert_eq!(buffer[(x - 2, y)].symbol(), "·");

        for character in "rev".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        let buffer = render(&mut app, 100, 32);
        assert!(buffer_text(&buffer).contains("1 matching Agent pane"));
        let back = app
            .overlay
            .commit()
            .unwrap()
            .agent_picker
            .as_ref()
            .unwrap()
            .buttons
            .secondary;
        click(&mut app, back.x + 2, back.y + 1);
        let dialog = app
            .overlay
            .commit()
            .expect("Back returns to the Commit dialog");
        assert!(dialog.agent_picker.is_none());
    }
}
