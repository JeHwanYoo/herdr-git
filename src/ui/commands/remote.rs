use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::git::GitOperation;
use crate::ui::overlay::Overlay;
use crate::ui::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState, TextEdit,
    TextField, chord, left_click, picker_edit, update_picker,
};
use crate::ui::{App, theme};

use super::CommandId;

#[derive(Debug)]
pub(in crate::ui) struct AddRemoteDialog {
    pub(super) name: TextField,
    pub(super) url: TextField,
    focus: usize,
    areas: [Rect; 2],
    buttons: ConfirmButtons,
}

#[derive(Debug, Default)]
pub(in crate::ui) struct RemoteList {
    cursor: ListCursor,
    list_area: Rect,
    buttons: ConfirmButtons,
}

impl App {
    pub(in crate::ui) fn open_add_remote(&mut self) {
        let mut name = TextField::new();
        let focus = if self.ops.command_context.remotes.is_empty() {
            name.text = "origin".into();
            1
        } else {
            0
        };
        self.overlay = Overlay::AddRemote(AddRemoteDialog {
            name,
            url: TextField::new(),
            focus,
            areas: [Rect::default(); 2],
            buttons: ConfirmButtons::default(),
        });
    }

    pub(in crate::ui) fn open_remotes(&mut self) {
        self.overlay = Overlay::Remotes(RemoteList::default());
    }

    fn submit_remote(&mut self) {
        if self.foreground.action.is_some() {
            return;
        }
        let Overlay::AddRemote(dialog) = &mut self.overlay else {
            return;
        };
        let name = dialog.name.text.trim().to_owned();
        let url = dialog.url.text.trim().to_owned();
        if name.is_empty() || name.starts_with('-') || name.chars().any(char::is_whitespace) {
            dialog.name.set_error("Enter a remote name without spaces.");
            dialog.focus = 0;
            return;
        }
        if self
            .ops
            .command_context
            .remotes
            .iter()
            .any(|remote| remote.name == name)
        {
            dialog
                .name
                .set_error("A remote with this name already exists.");
            dialog.focus = 0;
            return;
        }
        if url.is_empty() || url.chars().any(char::is_control) {
            dialog.url.set_error("Enter a repository URL or path.");
            dialog.focus = 1;
            return;
        }
        self.start_operation(CommandId::AddRemote, GitOperation::AddRemote { name, url });
    }

    pub(in crate::ui) fn handle_add_remote(&mut self, input: &Event) {
        if self.foreground.action.is_some() {
            return;
        }
        let Overlay::AddRemote(dialog) = &mut self.overlay else {
            return;
        };
        if let Some(point) = left_click(input) {
            if let Some(focus) = dialog
                .areas
                .iter()
                .position(|area| area.contains(point.into()))
            {
                dialog.focus = focus;
                return;
            }
            match dialog.buttons.hit(point) {
                Some(ConfirmButton::Primary) => self.submit_remote(),
                Some(ConfirmButton::Secondary) => self.overlay = Overlay::None,
                None => {}
            }
            return;
        }
        let field = if dialog.focus == 0 {
            &mut dialog.name
        } else {
            &mut dialog.url
        };
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => self.overlay = Overlay::None,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => dialog.focus ^= 1,
                KeyCode::Enter if dialog.focus == 0 => dialog.focus = 1,
                KeyCode::Enter => self.submit_remote(),
                KeyCode::Backspace => {
                    field.edit(TextEdit::Backspace);
                }
                KeyCode::Char(character) if !chord(key.modifiers) => {
                    field.edit(TextEdit::Insert(character));
                }
                _ => {}
            },
            Event::Paste(text) => {
                field.edit(TextEdit::Paste(text.clone()));
            }
            _ => {}
        }
    }

    pub(in crate::ui) fn draw_add_remote(
        &self,
        frame: &mut Frame<'_>,
        dialog: &mut AddRemoteDialog,
    ) {
        let inner = widgets::dialog_frame(frame, "Add Remotes", theme::DIALOG_MEDIUM, 12);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        for (index, (title, field)) in [("Name", &dialog.name), ("URL", &dialog.url)]
            .into_iter()
            .enumerate()
        {
            dialog.areas[index] = regions[index];
            let mut spans = vec![Span::raw(field.text.clone())];
            if dialog.focus == index {
                spans.push(theme::cursor_span(field.cursor_started.elapsed()));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(if dialog.focus == index {
                            theme::accent_bold()
                        } else {
                            theme::hint()
                        }),
                ),
                regions[index],
            );
        }
        let status = if self.foreground.action.is_some() {
            Line::styled("Adding remote…", theme::hint())
        } else if let Some(error) = dialog.name.error.as_deref().or(dialog.url.error.as_deref()) {
            Line::styled(error, theme::error_text())
        } else {
            Line::styled("[Tab] Switch field", theme::hint())
        };
        frame.render_widget(Paragraph::new(status), regions[2]);
        dialog.buttons = widgets::dialog_footer(
            frame,
            regions[3],
            ["[Enter] Add", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }

    fn confirm_remove_remote(&mut self) {
        let Overlay::Remotes(list) = &self.overlay else {
            return;
        };
        let Some(remote) = self.ops.command_context.remotes.get(list.cursor.selected) else {
            return;
        };
        self.overlay = Overlay::Confirm {
            command: CommandId::RemoveRemote,
            operation: GitOperation::RemoveRemote(remote.name.clone()),
            buttons: ConfirmButtons::default(),
            push_controls: Default::default(),
        };
    }

    pub(in crate::ui) fn handle_remotes(&mut self, input: &Event) {
        let Overlay::Remotes(list) = &mut self.overlay else {
            return;
        };
        let delete = matches!(input, Event::Key(key) if key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Delete | KeyCode::Char('d')));
        if delete
            || left_click(input).and_then(|point| list.buttons.hit(point))
                == Some(ConfirmButton::Primary)
        {
            self.confirm_remove_remote();
            return;
        }
        if left_click(input).and_then(|point| list.buttons.hit(point))
            == Some(ConfirmButton::Secondary)
        {
            self.overlay = Overlay::None;
            return;
        }
        let Some(edit) = picker_edit(input, list.list_area, list.cursor.scroll, false) else {
            return;
        };
        match update_picker(
            PickerState {
                query: None,
                cursor: &mut list.cursor,
                len: self.ops.command_context.remotes.len(),
                page: usize::from(list.list_area.height),
            },
            edit,
        ) {
            PickerOutcome::Cancel => self.overlay = Overlay::None,
            PickerOutcome::Activate
            | PickerOutcome::Moved
            | PickerOutcome::Filtered
            | PickerOutcome::Unchanged => {}
        }
    }

    pub(in crate::ui) fn draw_remotes(&self, frame: &mut Frame<'_>, list: &mut RemoteList) {
        let remotes = &self.ops.command_context.remotes;
        list.cursor.clamp(remotes.len());
        let height = u16::try_from(remotes.len())
            .unwrap_or(u16::MAX)
            .saturating_add(9)
            .max(12);
        let inner = widgets::dialog_frame(frame, "Show Remotes", theme::DIALOG_WIDE, height);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(2),
                Constraint::Length(3),
            ])
            .split(inner);
        list.list_area = regions[0];
        if remotes.is_empty() {
            frame.render_widget(Paragraph::new("No remotes configured."), regions[0]);
        } else {
            let items = remotes.iter().enumerate().map(|(index, remote)| {
                ListItem::new(format!(
                    "{}{}  {}",
                    remote.name,
                    if index == 0 { " (default)" } else { "" },
                    remote.url,
                ))
            });
            let mut state = ListState::default().with_selected(Some(list.cursor.selected));
            *state.offset_mut() = list.cursor.scroll;
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol(theme::LIST_MARKER)
                    .highlight_style(theme::focus_row()),
                regions[0],
                &mut state,
            );
            list.cursor.scroll = state.offset();
        }
        if let Some(remote) = remotes.get(list.cursor.selected) {
            frame.render_widget(
                Paragraph::new(format!("Push URL: {}\n[j/k] Select", remote.push_url))
                    .style(theme::hint()),
                regions[1],
            );
        }
        list.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Del] Remove", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use crossterm::event::{Event, KeyCode};

    use crate::git::Repository;
    use crate::ui::test_support::{
        buffer_text, click, git, press, render, temp_repo, wait_for_foreground, wait_for_refresh,
    };

    use super::*;

    fn settle(app: &mut App) {
        let overlay = std::mem::replace(&mut app.overlay, Overlay::None);
        wait_for_refresh(app);
        app.overlay = overlay;
    }

    fn add(app: &mut App, name: &str, url: &str) {
        app.dispatch_command(CommandId::AddRemote);
        let Overlay::AddRemote(dialog) = &mut app.overlay else {
            panic!("add remote");
        };
        dialog.name.text = name.into();
        dialog.url.text = url.into();
        dialog.focus = 1;
        press(app, KeyCode::Enter);
        wait_for_foreground(app);
        settle(app);
    }

    #[test]
    fn remote_order_survives_reload_and_removing_default_selects_the_next_remote() {
        let root = temp_repo("remotes-order");
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.dispatch_command(CommandId::ShowRemotes);
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("No remotes configured"));
        add(&mut app, "z-first", "../first repo.git");
        add(&mut app, "a-second", "../second.git");
        add(&mut app, "middle", "../third.git");
        let names = |app: &App| {
            app.ops
                .command_context
                .remotes
                .iter()
                .map(|remote| remote.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["z-first", "a-second", "middle"]);
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("z-first (default)"), "{text}");
        assert!(text.contains("../first repo.git"), "{text}");
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        assert_eq!(names(&app), ["z-first", "a-second", "middle"]);
        app.dispatch_command(CommandId::ShowRemotes);
        press(&mut app, KeyCode::Delete);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::RemoveRemote("z-first".into()))
        );
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::Remotes(_)));
        assert_eq!(names(&app), ["z-first", "a-second", "middle"]);
        press(&mut app, KeyCode::Delete);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        settle(&mut app);
        assert!(matches!(app.overlay, Overlay::Remotes(_)));
        assert_eq!(names(&app), ["a-second", "middle"]);
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("a-second (default)"));
        for _ in 0..2 {
            press(&mut app, KeyCode::Delete);
            press(&mut app, KeyCode::Enter);
            wait_for_foreground(&mut app);
            settle(&mut app);
        }
        assert!(!app.ops.command_context.has_remote);
        assert!(app.ops.command_context.remotes.is_empty());
        assert!(
            !CommandId::Push
                .availability(&app.ops.command_context)
                .enabled
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn add_remote_accepts_paste_and_keeps_fields_after_invalid_names_or_duplicates() {
        let root = temp_repo("remotes-validation");
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.dispatch_command(CommandId::AddRemote);
        app.handle(Event::Paste("../repo with spaces.git".into()))
            .unwrap();
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        settle(&mut app);
        assert_eq!(app.ops.command_context.remotes[0].name, "origin");
        assert_eq!(
            app.ops.command_context.remotes[0].url,
            "../repo with spaces.git"
        );
        add(&mut app, "origin", "../other.git");
        let Overlay::AddRemote(dialog) = &app.overlay else {
            panic!("duplicate stays open");
        };
        assert!(
            dialog
                .name
                .error
                .as_deref()
                .unwrap()
                .contains("already exists")
        );
        assert_eq!(dialog.url.text, "../other.git");
        add(&mut app, "invalid..name", "../other.git");
        let Overlay::AddRemote(dialog) = &app.overlay else {
            panic!("Git error stays open");
        };
        assert!(dialog.name.error.is_some());
        assert_eq!(dialog.name.text, "invalid..name");
        assert_eq!(app.ops.command_context.remotes.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn push_defaults_to_first_remote_and_can_change_destination_by_key_or_click() {
        let root = temp_repo("push-remotes");
        let first = root.join("first.git");
        let second = root.join("second.git");
        git(&root, &["init", "--bare", first.to_str().unwrap()]);
        git(&root, &["init", "--bare", second.to_str().unwrap()]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(
            &root,
            &["remote", "add", "z-first", first.to_str().unwrap()],
        );
        git(
            &root,
            &["remote", "add", "a-second", second.to_str().unwrap()],
        );
        git(
            &root,
            &[
                "remote",
                "set-url",
                "--push",
                "a-second",
                second.to_str().unwrap(),
            ],
        );
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        let fingerprint = app
            .repository
            .as_ref()
            .unwrap()
            .refresh_fingerprint()
            .unwrap();
        git(
            &root,
            &["config", "remote.z-first.pushurl", first.to_str().unwrap()],
        );
        assert_ne!(
            fingerprint.refs,
            app.repository
                .as_ref()
                .unwrap()
                .refresh_fingerprint()
                .unwrap()
                .refs
        );
        app.dispatch_command(CommandId::Push);
        let selected = |app: &App| match app.overlay.confirm_operation().unwrap() {
            GitOperation::Push { remote, .. } => remote.clone(),
            _ => panic!("push"),
        };
        assert_eq!(selected(&app), "z-first");
        let text = buffer_text(&render(&mut app, 110, 32));
        assert!(text.contains("Remote: z-first (default)"), "{text}");
        press(&mut app, KeyCode::Left);
        assert_eq!(selected(&app), "a-second");
        press(&mut app, KeyCode::Right);
        assert_eq!(selected(&app), "z-first");
        render(&mut app, 110, 32);
        let Overlay::Confirm { push_controls, .. } = &app.overlay else {
            panic!("confirmation");
        };
        let area = push_controls.remote;
        click(&mut app, area.x + 1, area.y);
        assert_eq!(selected(&app), "a-second");
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        settle(&mut app);
        assert!(app.overlay.result().unwrap().success);
        let head = |path: &std::path::Path| {
            Command::new("git")
                .current_dir(path)
                .args(["rev-parse", "--verify", "refs/heads/main"])
                .output()
                .unwrap()
        };
        assert!(head(&second).status.success());
        assert!(!head(&first).status.success());
        let context = app.repository.as_ref().unwrap().command_facts();
        assert_eq!(context.upstream.as_deref(), Some("a-second/main"));
        app.dispatch_command(CommandId::Push);
        assert_eq!(
            selected(&app),
            "z-first",
            "tracking or last choice must not replace the default"
        );
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        settle(&mut app);
        assert!(app.overlay.result().unwrap().success);
        assert!(head(&first).status.success());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn push_stops_if_the_selected_remote_or_branch_changed() {
        let root = temp_repo("push-stale");
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(&root, &["remote", "add", "origin", "../repo.git"]);
        let repo = Repository::discover(&root).unwrap();
        let operation = GitOperation::Push {
            force: false,
            remote: "origin".into(),
            branch: "main".into(),
        };
        git(&root, &["switch", "-c", "topic"]);
        assert!(
            repo.execute(&operation)
                .unwrap_err()
                .contains("branch changed")
        );
        git(&root, &["switch", "main"]);
        git(&root, &["remote", "remove", "origin"]);
        assert!(
            repo.execute(&operation)
                .unwrap_err()
                .contains("no longer exists")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
