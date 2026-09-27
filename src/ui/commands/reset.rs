use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::git::{Commit, GitOperation, ResetContext, ResetMode, ResetTarget};
use crate::ui::effect::{ForegroundRequest, RequestId};
use crate::ui::graph::short_commit;
use crate::ui::lanes::ForegroundKind;
use crate::ui::overlay::Overlay;
use crate::ui::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerEdit, PickerOutcome, PickerState,
    TextField, left_click, picker_edit, update_picker,
};
use crate::ui::{App, theme};

use super::{CommandId, OperationResultView, search_box, target_icon};

pub(in crate::ui) const RESET_MODES: [ResetMode; 3] =
    [ResetMode::Soft, ResetMode::Mixed, ResetMode::Hard];
const RESET_FLOW_HEIGHT: u16 = 22;
const RESET_CONFIRM_HEIGHT: u16 = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum ResetStep {
    Target,
    Mode,
    Confirm,
}

#[derive(Debug)]
pub(in crate::ui) struct ResetFlow {
    pub(in crate::ui) step: ResetStep,
    pub(in crate::ui) fixed_target: bool,
    pub(in crate::ui) current_branch: String,
    pub(in crate::ui) current_head: String,
    pub(in crate::ui) targets: Vec<ResetTarget>,
    pub(in crate::ui) query: TextField,
    pub(in crate::ui) target_cursor: ListCursor,
    pub(in crate::ui) mode_cursor: ListCursor,
    pub(in crate::ui) list_area: Rect,
    pub(in crate::ui) buttons: ConfirmButtons,
}

impl ResetFlow {
    pub(in crate::ui) fn new(context: ResetContext) -> Self {
        let current_ref = format!("refs/heads/{}", context.current_branch);
        let selected = context
            .targets
            .iter()
            .position(|target| target.reference == current_ref)
            .unwrap_or_default();
        Self {
            step: ResetStep::Target,
            fixed_target: false,
            current_branch: context.current_branch,
            current_head: context.current_head,
            targets: context.targets,
            query: TextField::new(),
            target_cursor: ListCursor {
                selected,
                scroll: 0,
            },
            mode_cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
        }
    }

    pub(in crate::ui) fn for_commit(
        current_branch: String,
        current_head: String,
        commit: Commit,
    ) -> Self {
        Self {
            step: ResetStep::Mode,
            fixed_target: true,
            current_branch,
            current_head,
            targets: vec![ResetTarget {
                name: commit.subject,
                reference: commit.sha.clone(),
                commit: commit.sha,
            }],
            query: TextField::new(),
            target_cursor: ListCursor::default(),
            mode_cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
        }
    }

    pub(in crate::ui) fn filtered_targets(&self) -> Vec<(usize, &ResetTarget)> {
        let query = self.query.text.to_lowercase();
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, target)| query.is_empty() || target.name.to_lowercase().contains(&query))
            .collect()
    }

    pub(in crate::ui) fn selected_target(&self) -> Option<&ResetTarget> {
        self.filtered_targets()
            .get(self.target_cursor.selected)
            .map(|(_, target)| *target)
    }

    pub(in crate::ui) fn mode(&self) -> ResetMode {
        RESET_MODES[self.mode_cursor.selected.min(RESET_MODES.len() - 1)]
    }
}

impl App {
    pub(in crate::ui) fn open_reset_flow(&mut self) {
        let Some(repository) = self.repository.as_ref() else {
            self.show_result(OperationResultView::message(
                Some(CommandId::ResetCurrentBranch),
                false,
                "Not a Git repository",
            ));
            return;
        };
        let path = repository.root().to_owned();
        let id = self.foreground.next_id;
        let request = ForegroundRequest::ResetContext {
            id,
            path: path.clone(),
        };
        if let Err(error) = self
            .foreground
            .request_foreground(request, ForegroundKind::ResetContext { path })
        {
            self.show_action_error(&format!("Git worker stopped: {error}"));
        }
    }

    pub(in crate::ui) fn open_reset_flow_to_commit(&mut self, commit: Commit) {
        let Some(current_branch) = self.ops.command_context.current_branch.clone() else {
            return;
        };
        let Some(current_head) = self.ops.command_context.head_commit.clone() else {
            return;
        };
        self.overlay = Overlay::Reset(ResetFlow::for_commit(current_branch, current_head, commit));
    }

    pub(in crate::ui) fn apply_reset_context(
        &mut self,
        id: RequestId,
        path: PathBuf,
        result: Result<ResetContext, String>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::ResetContext { path: active }
                    if active == &path
                )
            })
            .is_none()
        {
            return;
        }
        match result {
            Ok(context) => self.overlay = Overlay::Reset(ResetFlow::new(context)),
            Err(error) => self.show_result(OperationResultView::message(
                Some(CommandId::ResetCurrentBranch),
                false,
                &error,
            )),
        }
    }

    fn advance_reset_flow(&mut self) {
        let Overlay::Reset(flow) = &mut self.overlay else {
            return;
        };
        if flow.step == ResetStep::Target && flow.filtered_targets().is_empty() {
            return;
        }
        flow.step = match flow.step {
            ResetStep::Target => ResetStep::Mode,
            ResetStep::Mode | ResetStep::Confirm => ResetStep::Confirm,
        };
    }

    fn back_reset_flow(&mut self) {
        let Overlay::Reset(flow) = &mut self.overlay else {
            return;
        };
        match flow.step {
            ResetStep::Target => self.overlay = Overlay::None,
            ResetStep::Mode if flow.fixed_target => self.overlay = Overlay::None,
            ResetStep::Mode => flow.step = ResetStep::Target,
            ResetStep::Confirm => flow.step = ResetStep::Mode,
        }
    }

    fn execute_reset_flow(&mut self) {
        let Overlay::Reset(flow) = &self.overlay else {
            return;
        };
        let Some(target) = flow.selected_target().cloned() else {
            return;
        };
        let operation = GitOperation::Reset {
            mode: flow.mode(),
            target: target.commit,
            target_name: target.name,
            expected_branch: flow.current_branch.clone(),
            expected_head: flow.current_head.clone(),
        };
        self.overlay = Overlay::None;
        self.execute_operation(CommandId::ResetCurrentBranch, operation);
    }

    pub(in crate::ui) fn handle_reset_flow(&mut self, input: &Event) {
        let Overlay::Reset(flow) = &mut self.overlay else {
            return;
        };
        if let Some(point) = left_click(input) {
            match flow.buttons.hit(point) {
                Some(ConfirmButton::Primary) => {
                    if flow.step == ResetStep::Confirm {
                        self.execute_reset_flow();
                    } else {
                        self.advance_reset_flow();
                    }
                    return;
                }
                Some(ConfirmButton::Secondary) => {
                    self.back_reset_flow();
                    return;
                }
                None => {}
            }
        }
        match flow.step {
            ResetStep::Confirm => {
                if let Event::Key(key) = input
                    && key.kind == KeyEventKind::Press
                {
                    match key.code {
                        KeyCode::Enter => self.execute_reset_flow(),
                        KeyCode::Esc => self.back_reset_flow(),
                        _ => {}
                    }
                }
            }
            step => {
                let has_text_field = step == ResetStep::Target;
                let len = if has_text_field {
                    flow.filtered_targets().len()
                } else {
                    RESET_MODES.len()
                };
                let page = usize::from(flow.list_area.height);
                let cursor = if has_text_field {
                    &mut flow.target_cursor
                } else {
                    &mut flow.mode_cursor
                };
                let Some(edit) = picker_edit(input, flow.list_area, cursor.scroll, has_text_field)
                else {
                    return;
                };
                if let PickerEdit::ClickRow(row) = edit {
                    if row < len {
                        cursor.selected = row;
                    }
                    return;
                }
                let query = has_text_field.then_some(&mut flow.query);
                match update_picker(
                    PickerState {
                        query,
                        cursor,
                        len,
                        page,
                    },
                    edit,
                ) {
                    PickerOutcome::Activate => self.advance_reset_flow(),
                    PickerOutcome::Cancel => self.back_reset_flow(),
                    PickerOutcome::Moved | PickerOutcome::Filtered | PickerOutcome::Unchanged => {}
                }
            }
        }
    }

    pub(in crate::ui) fn draw_reset_flow(&self, frame: &mut Frame<'_>, flow: &mut ResetFlow) {
        let height = if flow.step == ResetStep::Confirm {
            RESET_CONFIRM_HEIGHT
        } else {
            RESET_FLOW_HEIGHT
        };
        let inner = widgets::dialog_frame(frame, "Reset", theme::DIALOG_WIDE, height);
        let mut context = vec![Line::from(vec![
            Span::styled("Current branch  ", theme::hint()),
            Span::styled(flow.current_branch.clone(), theme::accent_bold()),
            Span::styled("    HEAD  ", theme::hint()),
            Span::styled(short_commit(&flow.current_head), theme::accent_bold()),
        ])];
        if flow.step == ResetStep::Mode
            && let Some(target) = flow.selected_target()
        {
            context.push(Line::from(vec![
                Span::styled("Target  ", theme::hint()),
                Span::styled(short_commit(&target.commit), theme::accent_bold()),
                Span::raw(" · "),
                Span::styled(target.name.clone(), theme::accent_bold()),
            ]));
        }
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(context.len() as u16),
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        frame.render_widget(Paragraph::new(context), regions[0]);

        flow.list_area = Rect::default();
        match flow.step {
            ResetStep::Target => {
                let target_regions = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(1)])
                    .split(regions[1]);
                frame.render_widget(search_box(&flow.query), target_regions[0]);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title("Target branch");
                let inner = block.inner(target_regions[1]);
                flow.list_area = inner;
                let rows = flow
                    .filtered_targets()
                    .into_iter()
                    .map(|(_, target)| {
                        format!(
                            "{} {}  {}",
                            target_icon(target),
                            target.name,
                            short_commit(&target.commit)
                        )
                    })
                    .collect::<Vec<_>>();
                let hovered = self
                    .shell
                    .mouse_position
                    .and_then(|pointer| flow.target_cursor.row_at(inner, pointer, rows.len(), 0));
                let items = rows.into_iter().enumerate().map(|(row, label)| {
                    ListItem::new(label).style(theme::hover(Style::default(), hovered == Some(row)))
                });
                let mut state =
                    ListState::default().with_selected(Some(flow.target_cursor.selected));
                *state.offset_mut() = flow.target_cursor.scroll;
                frame.render_stateful_widget(
                    List::new(items)
                        .highlight_symbol(theme::LIST_MARKER)
                        .highlight_style(theme::focus_row())
                        .block(block),
                    target_regions[1],
                    &mut state,
                );
                flow.target_cursor.scroll = state.offset();
            }
            ResetStep::Mode => {
                let block = Block::default().borders(Borders::ALL).title("Reset mode");
                let inner = block.inner(regions[1]);
                flow.list_area = inner;
                let hovered = self.shell.mouse_position.and_then(|pointer| {
                    flow.mode_cursor
                        .row_at(inner, pointer, RESET_MODES.len(), 0)
                });
                let items = RESET_MODES.iter().enumerate().map(|(index, mode)| {
                    ListItem::new(format!("{:<7} {}", mode.label(), mode.summary()))
                        .style(theme::hover(Style::default(), hovered == Some(index)))
                });
                let mut state = ListState::default().with_selected(Some(flow.mode_cursor.selected));
                frame.render_stateful_widget(
                    List::new(items)
                        .highlight_symbol(theme::LIST_MARKER)
                        .highlight_style(theme::focus_row())
                        .block(block),
                    regions[1],
                    &mut state,
                );
            }
            ResetStep::Confirm => {
                let mode = flow.mode();
                let mut lines = vec![Line::from(vec![
                    Span::styled("From    ", theme::hint()),
                    Span::styled(flow.current_branch.clone(), theme::accent_bold()),
                    Span::raw(" · "),
                    Span::styled(short_commit(&flow.current_head), theme::accent_bold()),
                ])];
                if let Some(target) = flow.selected_target() {
                    lines.push(Line::from(vec![
                        Span::styled("Target  ", theme::hint()),
                        Span::styled(target.name.clone(), theme::accent_bold()),
                        Span::raw(" · "),
                        Span::styled(short_commit(&target.commit), theme::accent_bold()),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("Mode    ", theme::hint()),
                        Span::styled(mode.label(), theme::accent_bold()),
                    ]));
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        format!("git reset {} {}", mode.flag(), target.commit),
                        theme::accent(),
                    ));
                    lines.push(Line::raw(""));
                }
                match mode {
                    ResetMode::Soft => lines.push(Line::raw(mode.effect())),
                    ResetMode::Mixed => {
                        lines.push(Line::styled(mode.effect(), theme::warning_text()))
                    }
                    ResetMode::Hard => {
                        lines.push(Line::raw(
                            "The current branch, index, and tracked working tree move to the target.",
                        ));
                        lines.push(Line::styled(
                            "Tracked changes will be discarded. Obstructing untracked paths may be removed.",
                            theme::error_title(),
                        ));
                    }
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Remote refs are not changed. The target branch is not checked out.",
                    theme::hint(),
                ));
                frame.render_widget(
                    Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Confirm Reset"),
                    ),
                    regions[1],
                );
            }
        }

        let labels = match flow.step {
            ResetStep::Target => ["[Enter] Continue", "[Esc] Cancel"],
            ResetStep::Mode => ["[Enter] Continue", "[Esc] Back"],
            ResetStep::Confirm => ["[Enter] Reset", "[Esc] Back"],
        };
        flow.buttons =
            widgets::dialog_footer(frame, regions[3], labels, None, self.shell.mouse_position);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Modifier;

    use crate::git::Repository;
    use crate::ui::commands::CommandId;
    use crate::ui::test_support::{click, git, press, render, temp_repo, wait_for_foreground};
    use crate::ui::{App, theme};

    use super::ResetStep;

    #[test]
    fn reset_flow_supports_mouse_and_keyboard_confirmation_paths() {
        let root = temp_repo("reset-ui");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        let base = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let base = String::from_utf8(base.stdout).unwrap().trim().to_owned();
        git(&root, &["branch", "target", &base]);
        git(&root, &["update-ref", "refs/remotes/origin/main", &base]);
        fs::write(root.join("tracked.txt"), "next\n").unwrap();
        git(&root, &["commit", "-am", "Next"]);
        fs::write(root.join("tracked.txt"), "dirty\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.dispatch_command(CommandId::ResetCurrentBranch);
        let status = app.status_bar_text();
        assert!(status.contains("Loading Reset targets"));
        assert!(
            theme::SPINNER_FRAMES
                .iter()
                .any(|frame| status.contains(frame))
        );
        wait_for_foreground(&mut app);
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Target);

        let buffer = render(&mut app, 100, 32);
        let target_screen = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(target_screen.contains("Reset"));
        assert!(!target_screen.contains("Click selects · Enter continues"));
        assert!(target_screen.contains("[Enter] Continue"));
        assert!(target_screen.contains("[Esc] Cancel"));
        assert!(target_screen.contains("Search"));
        assert!(target_screen.contains('▏'));
        assert!(target_screen.contains(&format!("{} target", theme::BRANCH_GLYPH)));
        assert!(target_screen.contains(theme::REMOTE_GLYPH));
        assert!(target_screen.contains("origin/main"));
        for character in "TARGET".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        assert_eq!(
            app.overlay.reset().unwrap().selected_target().unwrap().name,
            "target"
        );
        for _ in 0.."TARGET".len() {
            press(&mut app, KeyCode::Backspace);
        }
        assert_eq!(app.overlay.reset().unwrap().target_cursor.selected, 0);
        let target_index = app
            .overlay
            .reset()
            .unwrap()
            .targets
            .iter()
            .position(|target| target.name == "target")
            .unwrap();
        render(&mut app, 100, 32);
        let list_area = app.overlay.reset().unwrap().list_area;
        let primary = app.overlay.reset().unwrap().buttons.primary;
        click(
            &mut app,
            list_area.x.saturating_add(2),
            list_area.y.saturating_add(target_index as u16),
        );
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Target);
        assert_eq!(
            app.overlay.reset().unwrap().selected_target().unwrap().name,
            "target"
        );
        click(
            &mut app,
            primary.x.saturating_add(2),
            primary.y.saturating_add(1),
        );
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Mode);

        render(&mut app, 100, 32);
        let list_area = app.overlay.reset().unwrap().list_area;
        click(
            &mut app,
            list_area.x.saturating_add(2),
            list_area.y.saturating_add(2),
        );
        assert_eq!(app.overlay.reset().unwrap().mode_cursor.selected, 2);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Confirm);

        let buffer = render(&mut app, 100, 32);
        let screen = buffer
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Current branch"));
        assert!(screen.contains("target"));
        assert!(screen.contains("Hard"));
        assert!(screen.contains("Tracked changes will be discarded"));
        assert!(screen.contains("[Enter] Reset"));
        assert!(screen.contains("[Esc] Back"));
        assert!(buffer.content().iter().any(|cell| cell.fg == theme::ACCENT));
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.fg == theme::ERROR && cell.modifier.contains(Modifier::BOLD))
        );

        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Confirm);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Mode);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Confirm);
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Mode);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        render(&mut app, 100, 32);
        let primary = app.overlay.reset().unwrap().buttons.primary;
        click(
            &mut app,
            primary.x.saturating_add(2),
            primary.y.saturating_add(1),
        );
        assert!(app.overlay.reset().is_none());
        wait_for_foreground(&mut app);
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\n"
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), base);

        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();

        app.dispatch_command(CommandId::ResetCurrentBranch);
        wait_for_foreground(&mut app);
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Target);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(app.overlay.reset().is_none());
        wait_for_foreground(&mut app);

        fs::remove_dir_all(root).unwrap();
    }
}
