use std::path::PathBuf;

use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::git::{GitOperation, ResetTarget};
use crate::ui::effect::{ForegroundRequest, RequestId};
use crate::ui::lanes::ForegroundKind;
use crate::ui::overlay::Overlay;
use crate::ui::widgets::{
    self, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState, TextField,
    left_click, picker_edit, update_picker,
};
use crate::ui::{App, theme};

use super::{CommandId, OperationResultView, search_box};

#[derive(Debug)]
pub(in crate::ui) struct TargetPicker {
    command: CommandId,
    commit: String,
    subject: String,
    targets: Vec<ResetTarget>,
    query: TextField,
    cursor: ListCursor,
    list_area: Rect,
    buttons: ConfirmButtons,
    loading: bool,
}

impl TargetPicker {
    fn commit_operation(&self, commit: String) -> GitOperation {
        match self.command {
            CommandId::RebaseHere => GitOperation::RebaseHere(commit),
            CommandId::InteractiveRebase => GitOperation::InteractiveRebase(commit),
            _ => GitOperation::CheckoutCommit(commit),
        }
    }

    fn rows(&self) -> Vec<(String, GitOperation)> {
        let query = self.query.text.trim();
        let effect = if self.command == CommandId::CheckoutCommit {
            "detached HEAD"
        } else {
            "commit"
        };
        let mut rows = Vec::new();
        if query.is_empty() {
            rows.push((
                format!(
                    "{} {} · {effect}",
                    &self.commit[..self.commit.len().min(8)],
                    self.subject
                ),
                self.commit_operation(self.commit.clone()),
            ));
        }
        rows.extend(
            self.targets
                .iter()
                .filter(|target| target.name.to_lowercase().contains(&query.to_lowercase()))
                .map(|target| {
                    let local = target.reference.strip_prefix("refs/heads/");
                    let (kind, operation) = match self.command {
                        CommandId::RebaseHere => (
                            if local.is_some() {
                                "branch"
                            } else {
                                "remote branch"
                            },
                            GitOperation::RebaseHere(target.reference.clone()),
                        ),
                        CommandId::InteractiveRebase => (
                            if local.is_some() {
                                "branch"
                            } else {
                                "remote branch"
                            },
                            GitOperation::InteractiveRebaseOnto(target.reference.clone()),
                        ),
                        _ => match local {
                            Some(name) => ("branch", GitOperation::CheckoutBranch(name.to_owned())),
                            None => (
                                "remote, detached HEAD",
                                GitOperation::CheckoutCommit(target.commit.clone()),
                            ),
                        },
                    };
                    (format!("{} · {kind}", target.name), operation)
                }),
        );
        if !query.is_empty()
            && query.len() >= 4
            && query.len() <= 64
            && query.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            rows.insert(
                0,
                (
                    format!("{query} · {effect}"),
                    self.commit_operation(query.to_owned()),
                ),
            );
        }
        rows
    }
}

impl App {
    pub(in crate::ui) fn open_target_picker(&mut self, command: CommandId) {
        let selected = self
            .graph
            .selected_commit()
            .map(|commit| (commit.sha.clone(), commit.subject.clone()));
        let fallback = if command == CommandId::InteractiveRebase {
            self.ops
                .command_context
                .head_commit
                .clone()
                .map(|sha| (sha, "HEAD".into()))
        } else {
            None
        };
        let Some((commit, subject)) = selected.or(fallback) else {
            return;
        };
        let Some(repository) = self.repository.as_ref() else {
            return;
        };
        let path = repository.root().to_owned();
        self.overlay = Overlay::Target(TargetPicker {
            command,
            commit,
            subject,
            targets: Vec::new(),
            query: TextField::new(),
            cursor: ListCursor::default(),
            list_area: Rect::default(),
            buttons: ConfirmButtons::default(),
            loading: true,
        });
        let id = self.foreground.next_id;
        if let Err(error) = self.foreground.request_foreground(
            ForegroundRequest::BranchTargets {
                id,
                command,
                path: path.clone(),
            },
            ForegroundKind::BranchTargets { path, command },
        ) {
            self.show_result(OperationResultView::message(
                Some(command),
                false,
                &format!("Git worker stopped: {error}"),
            ));
        }
    }

    pub(in crate::ui) fn apply_branch_targets(
        &mut self,
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        result: Result<Vec<ResetTarget>, String>,
    ) {
        if self.foreground.take_matching_action(id, |kind| matches!(kind, ForegroundKind::BranchTargets { path: active, command: active_command } if active == &path && *active_command == command)).is_none() {
            return;
        }
        if self.active_path != path {
            return;
        }
        let Overlay::Target(picker) = &mut self.overlay else {
            return;
        };
        if picker.command != command {
            return;
        }
        picker.loading = false;
        match result {
            Ok(targets) => picker.targets = targets,
            Err(error) => picker.query.set_error(format!(
                "{error}. Close and retry {}.",
                picker.command.result_name()
            )),
        }
    }

    fn confirm_target(&mut self) {
        let Overlay::Target(picker) = &self.overlay else {
            return;
        };
        if picker.loading {
            return;
        }
        let Some((_, operation)) = picker.rows().get(picker.cursor.selected).cloned() else {
            return;
        };
        if picker.command == CommandId::InteractiveRebase {
            self.open_rebase_editor(operation);
            return;
        }
        self.overlay = Overlay::Confirm {
            command: picker.command,
            operation,
            buttons: ConfirmButtons::default(),
            push_controls: Default::default(),
        };
    }

    pub(in crate::ui) fn handle_target_picker(&mut self, input: &Event) {
        let Overlay::Target(picker) = &mut self.overlay else {
            return;
        };
        if let Some(button) = left_click(input).and_then(|pointer| picker.buttons.hit(pointer)) {
            match button {
                ConfirmButton::Primary => self.confirm_target(),
                ConfirmButton::Secondary => self.overlay = Overlay::None,
            }
            return;
        }
        let Some(edit) = picker_edit(input, picker.list_area, picker.cursor.scroll, true) else {
            return;
        };
        let len = picker.rows().len();
        match update_picker(
            PickerState {
                query: Some(&mut picker.query),
                cursor: &mut picker.cursor,
                len,
                page: usize::from(picker.list_area.height),
            },
            edit,
        ) {
            PickerOutcome::Activate => self.confirm_target(),
            PickerOutcome::Cancel => self.overlay = Overlay::None,
            _ => {}
        }
    }

    pub(in crate::ui) fn draw_target_picker(
        &self,
        frame: &mut Frame<'_>,
        picker: &mut TargetPicker,
    ) {
        let inner =
            widgets::dialog_frame(frame, picker.command.result_name(), theme::DIALOG_WIDE, 20);
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
        picker.list_area = regions[1];
        let rows = picker.rows();
        let hovered = self
            .shell
            .mouse_position
            .and_then(|pointer| picker.cursor.row_at(regions[1], pointer, rows.len(), 0));
        let items = rows.iter().enumerate().map(|(index, (label, _))| {
            ListItem::new(label.clone())
                .style(theme::hover(Style::default(), hovered == Some(index)))
        });
        let mut state = ListState::default()
            .with_selected((!rows.is_empty()).then_some(picker.cursor.selected));
        *state.offset_mut() = picker.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[1],
            &mut state,
        );
        picker.cursor.scroll = state.offset();
        let status = if picker.loading {
            let elapsed = self
                .foreground
                .action
                .as_ref()
                .map(|action| action.started.elapsed())
                .unwrap_or_default();
            Line::from(vec![
                theme::spinner_span(elapsed),
                Span::raw(" Loading branches"),
            ])
        } else if let Some(error) = &picker.query.error {
            Line::styled(format!("Error: {error}"), theme::error_text())
        } else if rows.is_empty() {
            Line::styled("No matching branches or commit hash", theme::hint())
        } else {
            Line::styled("Choose a branch or type a commit hash", theme::hint())
        };
        frame.render_widget(Paragraph::new(status), regions[2]);
        picker.buttons = widgets::dialog_footer(
            frame,
            regions[3],
            ["[Enter] Continue", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use crate::git::Repository;
    use crate::ui::test_support::{
        click, git, press, render, temp_repo, wait_for_foreground, wait_for_refresh,
    };

    use super::*;

    #[test]
    fn branch_picker_loads_while_fetch_waits_on_mutation_lane() {
        let root = temp_repo("branch-read-lane");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        let (mutation_tx, mutation_rx) = std::sync::mpsc::channel();
        app.foreground.mutation_tx = mutation_tx;
        app.foreground
            .request(ForegroundRequest::Fetch {
                id: app.foreground.next_id,
                roots: vec![root.clone()],
            })
            .unwrap();
        assert_eq!(
            app.input_poll_timeout(),
            std::time::Duration::from_millis(100)
        );
        app.dispatch_command(CommandId::CheckoutCommit);
        assert_eq!(
            app.input_poll_timeout(),
            std::time::Duration::from_millis(16)
        );
        wait_for_foreground(&mut app);
        let Overlay::Target(picker) = &app.overlay else {
            panic!("target picker")
        };
        assert!(!picker.loading);
        assert!(picker.targets.iter().any(|target| target.name == "main"));
        assert_eq!(
            app.input_poll_timeout(),
            std::time::Duration::from_millis(100)
        );
        assert!(matches!(
            mutation_rx.try_recv(),
            Ok(ForegroundRequest::Fetch { .. })
        ));
        assert!(mutation_rx.try_recv().is_err());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_picker_defaults_to_commit_and_accepts_branches_and_hashes() {
        let root = temp_repo("rebase-picker");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(&root, &["branch", "topic"]);
        git(&root, &["commit", "--allow-empty", "-m", "Next"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.select(1);
        let sha = app.graph.selected_commit().unwrap().sha.clone();
        app.dispatch_command(CommandId::RebaseHere);
        wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Target(_)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::RebaseHere(sha.clone()))
        );
        press(&mut app, KeyCode::Esc);
        app.dispatch_command(CommandId::RebaseHere);
        wait_for_foreground(&mut app);
        app.handle(Event::Paste("topic".into())).unwrap();
        render(&mut app, 100, 30);
        let Overlay::Target(picker) = &app.overlay else {
            panic!("target picker")
        };
        let area = picker.list_area;
        click(&mut app, area.x + 2, area.y);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::RebaseHere("refs/heads/topic".into()))
        );
        press(&mut app, KeyCode::Esc);
        app.dispatch_command(CommandId::RebaseHere);
        wait_for_foreground(&mut app);
        app.handle(Event::Paste(sha[..8].into())).unwrap();
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::RebaseHere(sha[..8].into()))
        );
        press(&mut app, KeyCode::Esc);
        app.dispatch_command(CommandId::InteractiveRebase);
        wait_for_foreground(&mut app);
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Rebase(_)));
        assert_eq!(CommandId::RebaseHere.title(), "Rebase…");
        assert_eq!(CommandId::InteractiveRebase.title(), "Interactive rebase…");
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rebase_onto_branch_replays_work_and_follows_new_head() {
        let root = temp_repo("rebase-branch");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(&root, &["switch", "-c", "feature"]);
        std::fs::write(root.join("feature.txt"), "feature").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Feature"]);
        git(&root, &["switch", "main"]);
        std::fs::write(root.join("upstream.txt"), "upstream").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Upstream"]);
        git(&root, &["switch", "feature"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(crate::ui::shell::ActiveTab::History);
        wait_for_refresh(&mut app);
        let before = app.ops.command_context.head_commit.clone();
        app.dispatch_command(CommandId::RebaseHere);
        wait_for_foreground(&mut app);
        app.handle(Event::Paste("main".into())).unwrap();
        press(&mut app, KeyCode::Enter);
        let screen = crate::ui::test_support::buffer_text(&render(&mut app, 100, 30));
        assert!(screen.contains("Branch: feature"));
        assert!(screen.contains("Rebase onto: main"));
        press(&mut app, KeyCode::Enter);
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert_ne!(before, app.ops.command_context.head_commit);
        assert_eq!(
            app.ops.command_context.current_branch.as_deref(),
            Some("feature")
        );
        assert_eq!(
            Some(&app.graph.selected_commit().unwrap().sha),
            app.ops.command_context.head_commit.as_ref()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("feature.txt")).unwrap(),
            "feature"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("upstream.txt")).unwrap(),
            "upstream"
        );
        git(&root, &["merge-base", "--is-ancestor", "main", "HEAD"]);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_selects_commit_branch_and_typed_hash() {
        let root = temp_repo("checkout-picker");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(&root, &["branch", "topic"]);
        git(&root, &["commit", "--allow-empty", "-m", "Next"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.select(1);
        let sha = app.graph.selected_commit().unwrap().sha.clone();
        app.dispatch_command(CommandId::CheckoutCommit);
        wait_for_foreground(&mut app);
        let Overlay::Target(picker) = &app.overlay else {
            panic!("checkout picker")
        };
        assert_eq!(
            picker.rows()[0].1,
            GitOperation::CheckoutCommit(sha.clone())
        );
        assert!(
            picker
                .rows()
                .iter()
                .any(|(_, op)| op == &GitOperation::CheckoutBranch("topic".into()))
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::CheckoutCommit(sha.clone()))
        );
        press(&mut app, KeyCode::Esc);

        app.dispatch_command(CommandId::CheckoutCommit);
        wait_for_foreground(&mut app);
        app.handle(Event::Paste("topic".into())).unwrap();
        render(&mut app, 100, 30);
        let Overlay::Target(picker) = &app.overlay else {
            panic!("checkout picker")
        };
        let area = picker.list_area;
        click(&mut app, area.x + 2, area.y);
        let operation = app.overlay.confirm_operation().unwrap().clone();
        assert_eq!(operation, GitOperation::CheckoutBranch("topic".into()));
        repository.execute(&operation).unwrap();
        assert_eq!(
            repository.command_facts().current_branch.as_deref(),
            Some("topic")
        );
        press(&mut app, KeyCode::Esc);

        app.dispatch_command(CommandId::CheckoutCommit);
        wait_for_foreground(&mut app);
        app.handle(Event::Paste(sha[..8].to_owned())).unwrap();
        press(&mut app, KeyCode::Enter);
        let operation = app.overlay.confirm_operation().unwrap().clone();
        assert_eq!(operation, GitOperation::CheckoutCommit(sha[..8].to_owned()));
        repository.execute(&operation).unwrap();
        assert_eq!(repository.command_facts().current_branch, None);
        assert_eq!(repository.command_facts().head_commit, Some(sha));
        assert!(
            repository
                .branch_targets()
                .unwrap()
                .iter()
                .any(|target| target.name == "topic")
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_completion_reveals_head_beyond_loaded_history() {
        let root = temp_repo("checkout-follow-head");
        git(&root, &["commit", "--allow-empty", "-m", "Destination"]);
        git(&root, &["branch", "destination"]);
        for _ in 0..105 {
            git(&root, &["commit", "--allow-empty", "-m", "Newer"]);
        }
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(crate::ui::shell::ActiveTab::History);
        wait_for_refresh(&mut app);
        render(&mut app, 100, 30);
        assert!(app.graph.history_has_more);
        app.graph.query.text = "Newer".into();
        app.start_operation(
            CommandId::CheckoutCommit,
            GitOperation::CheckoutBranch("destination".into()),
        );
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        let head = app.ops.command_context.head_commit.as_ref().unwrap();
        assert_eq!(&app.graph.selected_commit().unwrap().sha, head);
        assert!(app.graph.selected >= 100);
        assert!(app.graph.query.text.is_empty());
        assert!(app.graph.selected >= app.graph.history_scroll);
        assert!(
            app.graph.selected
                < app.graph.history_scroll + app.graph.history_content_area.height as usize
        );
        app.overlay = Overlay::None;
        let newest = app.graph.commits[0].sha.clone();
        app.start_operation(
            CommandId::CheckoutCommit,
            GitOperation::CheckoutCommit(newest[..8].into()),
        );
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert_eq!(app.graph.selected_commit().unwrap().sha, newest);
        assert_eq!(app.graph.selected, 0);
        assert_eq!(app.graph.history_scroll, 0);
        app.overlay = Overlay::None;
        let selected = app.graph.selected;
        let scroll = app.graph.history_scroll;
        app.start_operation(
            CommandId::CheckoutCommit,
            GitOperation::CheckoutCommit("badbadbad".into()),
        );
        wait_for_foreground(&mut app);
        wait_for_refresh(&mut app);
        assert_eq!(app.graph.selected, selected);
        assert_eq!(app.graph.history_scroll, scroll);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkout_cancel_does_not_reopen_when_targets_arrive() {
        let root = temp_repo("checkout-cancel");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        wait_for_refresh(&mut app);
        app.dispatch_command(CommandId::CheckoutCommit);
        press(&mut app, KeyCode::Esc);
        wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::None));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
