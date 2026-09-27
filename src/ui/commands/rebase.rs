use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};

use crate::git::{GitOperation, RebaseAction, RebasePlan, RebaseResult, RebaseTask};
use crate::ui::effect::{ForegroundRequest, RefreshScope, RequestId};
use crate::ui::lanes::ForegroundKind;
use crate::ui::overlay::Overlay;
use crate::ui::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, TextEdit, TextField, left_click,
};
use crate::ui::{App, theme};

use super::OperationResultView;

#[derive(Debug, Default)]
pub(in crate::ui) struct RebaseEditor {
    plan: Option<RebasePlan>,
    cursor: ListCursor,
    list: Rect,
    menu: Option<ListCursor>,
    menu_area: Rect,
    anchor: Option<usize>,
    message: Option<(Option<usize>, TextField)>,
    confirmation: Option<RebaseTask>,
    status: String,
    amend_message: String,
    busy: bool,
    buttons: ConfirmButtons,
    update_refs: Rect,
    recovery: Vec<(Rect, char)>,
}

impl RebaseEditor {
    fn range(&self) -> (usize, usize) {
        let cursor = self.cursor.selected;
        match self.anchor {
            Some(anchor) => (anchor.min(cursor), anchor.max(cursor)),
            None => (cursor, cursor),
        }
    }
    fn set_action(&mut self, action: RebaseAction) {
        let (start, end) = self.range();
        if action == RebaseAction::Reword {
            if start != end {
                self.status = "Reword edits one commit at a time.".into();
                return;
            }
            if let Some(entry) = self.plan.as_ref().and_then(|plan| plan.entries.get(start)) {
                let mut field = TextField::new();
                field.text = entry.message.clone();
                self.message = Some((Some(start), field));
            }
            return;
        }
        let Some(plan) = &mut self.plan else { return };
        let Some(rows) = plan.entries.get_mut(start..=end) else {
            return;
        };
        for entry in rows {
            entry.action = action;
        }
        self.anchor = None;
        self.status.clear();
    }
    fn move_selection(&mut self, delta: isize) {
        let (start, end) = self.range();
        let Some(plan) = &mut self.plan else { return };
        if delta < 0 {
            if start == 0 {
                return;
            }
            plan.entries[start - 1..=end].rotate_left(1);
        } else {
            if end + 1 >= plan.entries.len() {
                return;
            }
            plan.entries[start..=end + 1].rotate_right(1);
        }
        self.cursor.selected = self.cursor.selected.saturating_add_signed(delta);
        self.anchor = self
            .anchor
            .map(|anchor| anchor.saturating_add_signed(delta));
    }
    fn choose(&mut self, index: usize) {
        self.menu = None;
        match index {
            6 => self.move_selection(-1),
            7 => self.move_selection(1),
            _ => self.set_action(RebaseAction::ALL[index]),
        }
    }
    fn prepare_start(&mut self) {
        if let Some(plan) = &self.plan {
            match plan.validate() {
                Ok(()) => self.confirmation = Some(RebaseTask::Start(plan.clone())),
                Err(error) => self.status = error,
            }
        }
    }
}

impl App {
    pub(in crate::ui) fn open_rebase_editor(&mut self, operation: GitOperation) {
        let path = self.active_path.clone();
        self.overlay = Overlay::Rebase(RebaseEditor {
            busy: true,
            status: "Loading commits…".into(),
            ..Default::default()
        });
        let id = self.foreground.next_id;
        if let Err(error) = self.foreground.request_foreground(
            ForegroundRequest::RebasePlan {
                id,
                path: path.clone(),
                operation,
            },
            ForegroundKind::Rebase { path },
        ) && let Overlay::Rebase(editor) = &mut self.overlay
        {
            editor.busy = false;
            editor.status = error;
        }
    }
    pub(in crate::ui) fn apply_rebase_plan(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<RebasePlan, String>,
    ) {
        if self
            .foreground
            .take_matching_action(
                id,
                |kind| matches!(kind, ForegroundKind::Rebase { path: active } if active == &path),
            )
            .is_none()
            || self.active_path != path
        {
            return;
        }
        if let Overlay::Rebase(editor) = &mut self.overlay {
            editor.busy = false;
            match result {
                Ok(plan) => {
                    editor.status = if plan.active {
                        "Rebase paused. Resolve and stage files in Changes, then Continue.".into()
                    } else {
                        String::new()
                    };
                    editor.amend_message = plan
                        .entries
                        .first()
                        .map(|e| e.message.clone())
                        .unwrap_or_default();
                    editor.plan = Some(plan);
                }
                Err(error) => editor.status = error,
            }
        }
    }
    fn submit_rebase(&mut self, task: RebaseTask) {
        let path = self.active_path.clone();
        let id = self.foreground.next_id;
        if let Overlay::Rebase(editor) = &mut self.overlay {
            editor.busy = true;
            editor.confirmation = None;
            editor.status = "Running interactive rebase…".into();
        }
        self.advance_refresh_generation();
        if let Err(error) = self.foreground.request_foreground(
            ForegroundRequest::RebaseRun {
                id,
                path: path.clone(),
                task,
            },
            ForegroundKind::Rebase { path },
        ) && let Overlay::Rebase(editor) = &mut self.overlay
        {
            editor.busy = false;
            editor.status = error;
        }
    }
    pub(in crate::ui) fn apply_rebase_run(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<RebaseResult, String>,
    ) {
        if self
            .foreground
            .take_matching_action(
                id,
                |kind| matches!(kind, ForegroundKind::Rebase { path: active } if active == &path),
            )
            .is_none()
            || self.active_path != path
        {
            return;
        }
        match result {
            Ok(result) if !result.active => {
                self.history.follow_head = result.success;
                self.show_result(OperationResultView::named_message(
                    "Interactive rebase",
                    result.success,
                    &result.output,
                ));
            }
            Ok(result) => {
                if let Overlay::Rebase(editor) = &mut self.overlay {
                    editor.busy = false;
                    editor.status = result.output;
                    editor.amend_message = result.message;
                    if let Some(plan) = &mut editor.plan {
                        plan.active = true;
                    }
                }
            }
            Err(error) => {
                if let Overlay::Rebase(editor) = &mut self.overlay {
                    editor.busy = false;
                    editor.status = error;
                }
            }
        }
        self.request_operation_refresh("Refreshing repository", RefreshScope::Repository);
    }
    pub(in crate::ui) fn handle_rebase_editor(&mut self, input: &Event) {
        let Overlay::Rebase(editor) = &mut self.overlay else {
            return;
        };
        let key = match input {
            Event::Key(key) if key.kind != KeyEventKind::Release => Some(*key),
            _ => None,
        };
        let code = key.map(|key| key.code);
        let ctrl = key.is_some_and(|key| key.modifiers.contains(KeyModifiers::CONTROL));
        let shift = key.is_some_and(|key| key.modifiers.contains(KeyModifiers::SHIFT));
        let clicked = left_click(input);
        let button = clicked.and_then(|point| editor.buttons.hit(point));
        let cancel = code == Some(KeyCode::Esc) || button == Some(ConfirmButton::Secondary);
        let accept = code == Some(KeyCode::Enter) || button == Some(ConfirmButton::Primary);
        if editor.busy {
            if editor.plan.is_none() && cancel {
                self.overlay = Overlay::None;
            }
            return;
        }
        if editor.confirmation.is_some() {
            if cancel {
                editor.confirmation = None;
            } else if accept {
                let task = editor.confirmation.take().unwrap();
                self.submit_rebase(task);
            }
            return;
        }
        if let Some((index, field)) = &mut editor.message {
            if cancel {
                editor.message = None;
                return;
            }
            if (accept && !shift) || button == Some(ConfirmButton::Primary) {
                if field.text.trim().is_empty() {
                    field.set_error("Enter a commit message.");
                    return;
                }
                let text = field.text.clone();
                let index = *index;
                editor.message = None;
                if let Some(index) = index {
                    if let Some(entry) = editor.plan.as_mut().and_then(|p| p.entries.get_mut(index))
                    {
                        entry.message = text;
                        entry.action = RebaseAction::Reword;
                    }
                } else {
                    self.submit_rebase(RebaseTask::Amend(text));
                }
            } else {
                match input {
                    Event::Paste(text) => {
                        field.edit(TextEdit::Paste(
                            text.replace("\r\n", "\n").replace('\r', "\n"),
                        ));
                    }
                    _ => match code {
                        Some(KeyCode::Char('u')) if ctrl => field.text.clear(),
                        Some(KeyCode::Char(c)) if !ctrl => {
                            field.edit(TextEdit::Insert(c));
                        }
                        Some(KeyCode::Backspace) => {
                            field.edit(TextEdit::Backspace);
                        }
                        Some(KeyCode::Enter) if shift => {
                            field.edit(TextEdit::Newline);
                        }
                        _ => {}
                    },
                }
            }
            return;
        }
        if let Some(menu) = &mut editor.menu {
            if cancel {
                editor.menu = None;
                return;
            }
            if let Some(row) = clicked.and_then(|p| menu.row_at(editor.menu_area, p, 8, 0)) {
                editor.choose(row);
                return;
            }
            match code {
                Some(KeyCode::Up) => menu.move_by(-1, 8),
                Some(KeyCode::Down) => menu.move_by(1, 8),
                Some(KeyCode::Enter) => {
                    let row = menu.selected;
                    editor.choose(row);
                }
                Some(KeyCode::Char(c)) => {
                    if let Some(index) = "persfd".chars().position(|key| key == c) {
                        editor.choose(index);
                    }
                }
                _ => {}
            }
            return;
        }
        if code == Some(KeyCode::Esc) && editor.anchor.is_some() {
            editor.anchor = None;
            editor.status.clear();
            return;
        }
        if cancel {
            self.overlay = Overlay::None;
            return;
        }
        let Some(plan) = &mut editor.plan else {
            return;
        };
        if plan.active {
            let action = clicked
                .and_then(|p| {
                    editor
                        .recovery
                        .iter()
                        .find(|(area, _)| area.contains(p.into()))
                        .map(|(_, c)| *c)
                })
                .or(match code {
                    Some(KeyCode::Char(c)) => Some(c),
                    _ => None,
                });
            match action {
                Some('c') => self.submit_rebase(RebaseTask::Continue),
                Some('s') => editor.confirmation = Some(RebaseTask::Skip),
                Some('a') => editor.confirmation = Some(RebaseTask::Abort),
                Some('m') => {
                    let mut field = TextField::new();
                    field.text = editor.amend_message.clone();
                    editor.message = Some((None, field));
                }
                _ => {}
            }
            return;
        }
        if clicked.is_some_and(|p| editor.update_refs.contains(p.into()))
            || code == Some(KeyCode::Char(' '))
        {
            plan.update_refs = !plan.update_refs;
            return;
        }
        if accept {
            editor.prepare_start();
            return;
        }
        if let Some(row) =
            clicked.and_then(|p| editor.cursor.row_at(editor.list, p, plan.entries.len(), 0))
        {
            editor.cursor.selected = row;
            editor.anchor = None;
            editor.menu = Some(ListCursor::default());
            return;
        }
        match code {
            Some(KeyCode::Up) if ctrl => editor.choose(6),
            Some(KeyCode::Down) if ctrl => editor.choose(7),
            Some(KeyCode::Up) => editor.cursor.move_by(-1, plan.entries.len()),
            Some(KeyCode::Down) => editor.cursor.move_by(1, plan.entries.len()),
            Some(KeyCode::PageUp) => {
                editor
                    .cursor
                    .page(-1, plan.entries.len(), editor.list.height.into())
            }
            Some(KeyCode::PageDown) => {
                editor
                    .cursor
                    .page(1, plan.entries.len(), editor.list.height.into())
            }
            Some(KeyCode::Char('?')) => editor.menu = Some(ListCursor::default()),
            Some(KeyCode::Char('v')) if editor.anchor.is_none() && !plan.entries.is_empty() => {
                editor.anchor = Some(editor.cursor.selected);
                editor.status.clear();
            }
            Some(KeyCode::Char(c)) => {
                if let Some(index) = "persfd".chars().position(|key| key == c) {
                    editor.choose(index);
                }
            }
            _ => {
                if let Event::Mouse(mouse) = input {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => editor.cursor.move_by(-3, plan.entries.len()),
                        MouseEventKind::ScrollDown => editor.cursor.move_by(3, plan.entries.len()),
                        _ => {}
                    }
                }
            }
        }
    }

    pub(in crate::ui) fn draw_rebase_editor(
        &self,
        frame: &mut Frame<'_>,
        editor: &mut RebaseEditor,
    ) {
        let inner = widgets::dialog_frame(frame, "Interactive rebase", 140, 30);
        let (first, last) = editor.range();
        let visual = editor.anchor.is_some();
        let areas = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(3),
        ])
        .split(inner);
        editor.buttons = widgets::dialog_footer(
            frame,
            areas[4],
            ["[Enter] Rebase", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
        if let Some(plan) = &editor.plan {
            frame.render_widget(
                Paragraph::new(format!("Rebase: {}\nOn: {}", plan.branch, plan.target)),
                areas[0],
            );
            editor.update_refs = areas[1];
            frame.render_widget(
                Paragraph::new(format!(
                    "[{}] Update dependent branches  [Space]",
                    if plan.update_refs { 'x' } else { ' ' }
                ))
                .style(theme::hint()),
                areas[1],
            );
            if !plan.active {
                editor.list = Rect {
                    y: areas[2].y.saturating_add(1),
                    height: areas[2].height.saturating_sub(1),
                    ..areas[2]
                };
                let wide = areas[2].width >= 100;
                let widths = [
                    Constraint::Length(9),
                    Constraint::Min(12),
                    Constraint::Length(if wide { 18 } else { 0 }),
                    Constraint::Length(8),
                    Constraint::Length(if wide { 10 } else { 0 }),
                ];
                let rows = plan.entries.iter().enumerate().map(|(index, entry)| {
                    let row = Row::new(vec![
                        Cell::from(entry.action.label())
                            .style(Style::default().fg(action_color(entry.action))),
                        Cell::from(entry.message.lines().next().unwrap_or("")),
                        Cell::from(if wide { entry.author.as_str() } else { "" }),
                        Cell::from(&entry.sha[..8]),
                        Cell::from(if wide { entry.date.as_str() } else { "" }),
                    ]);
                    if visual && (first..=last).contains(&index) {
                        row.style(theme::selection_row())
                    } else {
                        row
                    }
                });
                let mut state = TableState::default()
                    .with_selected((!plan.entries.is_empty()).then_some(editor.cursor.selected));
                *state.offset_mut() = editor.cursor.scroll;
                frame.render_stateful_widget(
                    Table::new(rows, widths)
                        .header(
                            Row::new([
                                "Action",
                                "Commit",
                                if wide { "Author" } else { "" },
                                "SHA",
                                if wide { "Date" } else { "" },
                            ])
                            .style(theme::section_header()),
                        )
                        .row_highlight_style(if visual {
                            theme::selection_row()
                        } else {
                            theme::focus_row()
                        })
                        .highlight_symbol(theme::LIST_MARKER),
                    areas[2],
                    &mut state,
                );
                editor.cursor.scroll = state.offset();
            } else {
                frame.render_widget(
                    Paragraph::new(&*editor.status).wrap(Wrap { trim: false }),
                    areas[2],
                );
                let rows = Layout::horizontal([Constraint::Percentage(25); 4]).split(areas[3]);
                editor.recovery.clear();
                for ((area, label), key) in rows
                    .iter()
                    .zip(["[c] Continue", "[s] Skip", "[a] Abort", "[m] Amend message"])
                    .zip(['c', 's', 'a', 'm'])
                {
                    frame.render_widget(Paragraph::new(label).style(theme::accent()), *area);
                    editor.recovery.push((*area, key));
                }
                editor.buttons = widgets::dialog_footer(
                    frame,
                    areas[4],
                    ["Resolve / stage in Changes", "[Esc] Close"],
                    None,
                    self.shell.mouse_position,
                );
            }
        }
        if editor.busy {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    theme::spinner_span(
                        self.foreground
                            .action
                            .as_ref()
                            .map(|a| a.started.elapsed())
                            .unwrap_or_default(),
                    ),
                    Span::raw(format!(" {}", editor.status)),
                ])),
                areas[3],
            );
        } else if !editor.plan.as_ref().is_some_and(|p| p.active) {
            let hints = Layout::vertical([Constraint::Length(1); 2]).split(areas[3]);
            frame.render_widget(Paragraph::new(action_keys()), hints[0]);
            let guidance = if !editor.status.is_empty() {
                Line::from(Span::styled(editor.status.clone(), theme::warning_text()))
            } else if visual {
                Line::from(Span::styled(
                    format!(
                        "Visual: {} commits · p/e/s/f/d set all · Ctrl+↑/↓ move the block · Esc clear",
                        last - first + 1
                    ),
                    theme::accent(),
                ))
            } else {
                Line::from(Span::styled(
                    "v select lines · Ctrl+↑/↓ move commit · ? what the keys do",
                    theme::hint(),
                ))
            };
            frame.render_widget(Paragraph::new(guidance), hints[1]);
        }
        if let Some(task) = &editor.confirmation {
            let inner = widgets::dialog_frame(frame, "Confirm interactive rebase", 76, 10);
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
            let text = match task {
                RebaseTask::Start(plan) => format!(
                    "Rewrite {} commits on {}?\nTarget: {}\nUpdate dependent branches: {}",
                    plan.entries.len(),
                    plan.branch,
                    plan.target,
                    plan.update_refs
                ),
                RebaseTask::Skip => "Skip the current commit and continue?".into(),
                _ => "Abort rebase and restore the original branch?".into(),
            };
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), rows[0]);
            editor.buttons = widgets::dialog_footer(
                frame,
                rows[1],
                ["[Enter] Confirm", "[Esc] Back"],
                None,
                self.shell.mouse_position,
            );
        } else if let Some((_, field)) = &editor.message {
            let inner = widgets::dialog_frame(frame, "Commit message", 100, 20);
            let rows = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(2),
                Constraint::Length(3),
            ])
            .split(inner);
            let text = format!(
                "{}{}",
                field.text,
                if theme::cursor_blink_visible(field.cursor_started.elapsed()) {
                    theme::CURSOR_GLYPH
                } else {
                    ""
                }
            );
            let mut wrapped = vec![String::new()];
            let mut width = 0;
            for character in text.chars() {
                let next = Line::from(character.to_string()).width();
                if character == '\n' || width + next > usize::from(rows[0].width.max(1)) {
                    wrapped.push(String::new());
                    width = 0;
                }
                if character != '\n' {
                    wrapped.last_mut().unwrap().push(character);
                    width += next;
                }
            }
            let scroll = wrapped.len().saturating_sub(usize::from(rows[0].height));
            frame.render_widget(
                Paragraph::new(wrapped.join("\n"))
                    .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
                rows[0],
            );
            frame.render_widget(
                Paragraph::new(field.error.as_deref().unwrap_or(
                    "Enter saves · Shift+Enter new line · Ctrl+U clears · paste supported",
                )),
                rows[1],
            );
            editor.buttons = widgets::dialog_footer(
                frame,
                rows[2],
                ["[Enter] Save", "[Esc] Cancel"],
                None,
                self.shell.mouse_position,
            );
        } else if let Some(menu) = &mut editor.menu {
            let title = if last > first {
                format!("Actions for {} commits", last - first + 1)
            } else {
                "Actions".to_owned()
            };
            let inner = widgets::dialog_frame(frame, &title, 68, 12);
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
            editor.menu_area = rows[0];
            let items = RebaseAction::ALL
                .iter()
                .zip(['p', 'e', 'r', 's', 'f', 'd'])
                .map(|(action, key)| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{key}  "), theme::accent()),
                        Span::styled(
                            format!("{:<8}", action.label()),
                            Style::default().fg(action_color(*action)),
                        ),
                        Span::raw(action.description()),
                    ]))
                })
                .chain([
                    ListItem::new(Line::from(vec![
                        Span::styled("Ctrl+↑  ", theme::accent()),
                        Span::raw("Move up"),
                    ])),
                    ListItem::new(Line::from(vec![
                        Span::styled("Ctrl+↓  ", theme::accent()),
                        Span::raw("Move down"),
                    ])),
                ]);
            let mut state = ListState::default().with_selected(Some(menu.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(theme::focus_row())
                    .highlight_symbol(theme::LIST_MARKER),
                rows[0],
                &mut state,
            );
            menu.scroll = state.offset();
            frame.render_widget(
                Paragraph::new("Press the key itself on the list, or pick a row here · Esc closes")
                    .style(theme::hint()),
                rows[1],
            );
        }
    }
}

fn action_color(action: RebaseAction) -> ratatui::style::Color {
    match action {
        RebaseAction::Drop => theme::ERROR,
        RebaseAction::Pick => theme::SUCCESS,
        _ => theme::WARNING,
    }
}

fn action_keys() -> Line<'static> {
    let mut spans = Vec::new();
    for (action, key) in RebaseAction::ALL.iter().zip(['p', 'e', 'r', 's', 'f', 'd']) {
        if !spans.is_empty() {
            spans.push(Span::styled("  ", theme::hint()));
        }
        spans.push(Span::styled(
            format!("{key} "),
            Style::default().fg(theme::HINT),
        ));
        spans.push(Span::styled(
            action.label(),
            Style::default().fg(action_color(*action)),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;

    use crate::git::Repository;
    use crate::ui::commands::CommandId;
    use crate::ui::test_support::{
        buffer_text, click, git, press, render, temp_repo, wait_for_foreground, wait_for_refresh,
    };

    use super::*;

    fn ctrl(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
            .unwrap();
    }
    fn shift(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::SHIFT)))
            .unwrap();
    }
    #[test]
    fn native_rebase_modal_keyboard_mouse_and_unicode_execution() {
        let root = temp_repo("rebase-modal");
        for message in ["Base", "Second", "Third", "유니코드 커밋"] {
            git(&root, &["commit", "--allow-empty", "-m", message]);
        }
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.open_rebase_editor(GitOperation::InteractiveRebaseOnto("HEAD~3".into()));
        wait_for_foreground(&mut app);
        let screen = buffer_text(&render(&mut app, 150, 40));
        assert!(screen.contains("Update dependent branches"), "{screen}");
        assert!(screen.replace(' ', "").contains("유니코드커밋"), "{screen}");
        let compact = buffer_text(&render(&mut app, 40, 12));
        assert!(
            compact.replace(' ', "").contains("유니코드커밋"),
            "{compact}"
        );
        press(&mut app, KeyCode::Char('r'));
        ctrl(&mut app, KeyCode::Char('u'));
        app.handle(Event::Paste("한글 제목\n\n본문".into()))
            .unwrap();
        press(&mut app, KeyCode::Enter);
        ctrl(&mut app, KeyCode::Down);
        render(&mut app, 150, 40);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("rebase modal")
        };
        assert_eq!(
            editor.plan.as_ref().unwrap().entries[1].message,
            "한글 제목\n\n본문"
        );
        let list = editor.list;
        click(&mut app, list.x + 2, list.y + 2);
        render(&mut app, 150, 40);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("rebase modal")
        };
        let menu = editor.menu_area;
        click(&mut app, menu.x + 3, menu.y + 5);
        render(&mut app, 150, 40);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("rebase modal")
        };
        assert_eq!(
            editor.plan.as_ref().unwrap().entries[2].action,
            RebaseAction::Drop
        );
        let checkbox = editor.update_refs;
        click(&mut app, checkbox.x + 1, checkbox.y);
        press(&mut app, KeyCode::Enter);
        assert!(buffer_text(&render(&mut app, 150, 40)).contains("Rewrite 3 commits"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        assert!(app.overlay.result().unwrap().success);
        wait_for_refresh(&mut app);
        assert_eq!(
            app.ops.command_context.current_branch.as_deref(),
            Some("main")
        );
        let history = Repository::discover(&root)
            .unwrap()
            .rebase_plan(&GitOperation::InteractiveRebaseOnto("HEAD~2".into()))
            .unwrap();
        assert_eq!(history.entries[1].message, "한글 제목\n\n본문");
        assert!(crate::ui::commands::COMMANDS.contains(&CommandId::InteractiveRebase));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rebase_editor_lists_newest_first_and_selects_a_visual_range() {
        let root = temp_repo("rebase-visual");
        for message in ["Base", "One", "Two", "Three"] {
            git(&root, &["commit", "--allow-empty", "-m", message]);
        }
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.open_rebase_editor(GitOperation::InteractiveRebaseOnto("HEAD~3".into()));
        wait_for_foreground(&mut app);
        assert_eq!(subjects(&app), ["Three", "Two", "One"]);
        let screen = buffer_text(&render(&mut app, 150, 40));
        assert!(!screen.contains("Oldest first"), "{screen}");
        assert!(screen.contains("d Drop"), "{screen}");

        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Down);
        assert!(buffer_text(&render(&mut app, 150, 40)).contains("Visual: 2 commits"));
        press(&mut app, KeyCode::Char('d'));
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("rebase modal")
        };
        assert!(editor.anchor.is_none());
        assert_eq!(
            editor
                .plan
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .map(|entry| entry.action)
                .collect::<Vec<_>>(),
            [RebaseAction::Drop, RebaseAction::Drop, RebaseAction::Pick]
        );

        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Up);
        ctrl(&mut app, KeyCode::Down);
        assert_eq!(subjects(&app), ["One", "Three", "Two"]);
        press(&mut app, KeyCode::Esc);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("Esc clears the visual range before it closes the dialog")
        };
        assert!(editor.anchor.is_none());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rebase_editor_accepts_with_enter_and_explains_its_keys_on_question_mark() {
        let root = temp_repo("rebase-enter");
        for message in ["Base", "One", "Two"] {
            git(&root, &["commit", "--allow-empty", "-m", message]);
        }
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.open_rebase_editor(GitOperation::InteractiveRebaseOnto("HEAD~2".into()));
        wait_for_foreground(&mut app);
        let screen = buffer_text(&render(&mut app, 150, 40));
        assert!(screen.contains("[Enter] Rebase"), "{screen}");
        assert!(!screen.contains("Ctrl+Enter"), "{screen}");

        press(&mut app, KeyCode::Char('?'));
        let help = buffer_text(&render(&mut app, 150, 40));
        assert!(help.contains("Use commit"), "{help}");
        assert!(help.contains("Remove commit"), "{help}");
        press(&mut app, KeyCode::Esc);

        press(&mut app, KeyCode::Char('r'));
        ctrl(&mut app, KeyCode::Char('u'));
        for character in "제목".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        shift(&mut app, KeyCode::Enter);
        for character in "본문".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        press(&mut app, KeyCode::Enter);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("rebase modal")
        };
        let entry = &editor.plan.as_ref().unwrap().entries[0];
        assert_eq!(entry.message, "제목\n본문");
        assert_eq!(entry.action, RebaseAction::Reword);

        press(&mut app, KeyCode::Enter);
        assert!(buffer_text(&render(&mut app, 150, 40)).contains("Rewrite 2 commits"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn subjects(app: &App) -> Vec<String> {
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("rebase modal")
        };
        editor
            .plan
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }
    #[test]
    fn native_rebase_cancel_loading_and_edit_recovery() {
        let root = temp_repo("rebase-recovery-modal");
        for message in ["Base", "Edit me"] {
            git(&root, &["commit", "--allow-empty", "-m", message]);
        }
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.graph.commits.clear();
        app.dispatch_command(CommandId::InteractiveRebase);
        wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Target(_)));
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("native plan")
        };
        assert_eq!(editor.plan.as_ref().unwrap().entries.len(), 1);
        press(&mut app, KeyCode::Esc);
        app.open_rebase_editor(GitOperation::InteractiveRebase("HEAD".into()));
        press(&mut app, KeyCode::Esc);
        wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::None));
        app.open_rebase_editor(GitOperation::InteractiveRebase("HEAD".into()));
        wait_for_foreground(&mut app);
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        assert!(buffer_text(&render(&mut app, 100, 35)).contains("[c] Continue"));
        press(&mut app, KeyCode::Esc);
        app.open_rebase_editor(GitOperation::InteractiveRebase("HEAD".into()));
        wait_for_foreground(&mut app);
        render(&mut app, 100, 35);
        let Overlay::Rebase(editor) = &app.overlay else {
            panic!("recovery")
        };
        let area = editor.recovery[2].0;
        click(&mut app, area.x + 1, area.y);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        assert!(app.overlay.result().unwrap().success);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
