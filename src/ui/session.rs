use std::path::PathBuf;
use std::time::Instant;

use crate::git::ReadError;

use super::commands::CommandContext;
use super::effect::{
    ForegroundRequest, KnownFingerprint, ReadGeneration, RefreshIntent, RepositorySnapshot,
    RequestId, SwitchTarget,
};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::shell::{ActiveTab, PaneFocus};
use super::workspaces::RepositoryRowKind;
use super::{App, ScrollbarOwner};

impl App {
    pub(super) fn apply_repository_snapshot(&mut self, snapshot: RepositorySnapshot) {
        self.advance_refresh_generation();
        self.reset_commit_inspection();
        self.inspect.clear_caches();
        let changes = snapshot.changes;
        self.repository = Some(snapshot.repository);
        self.active_path = snapshot.root;
        self.comparison = super::comparison::ComparisonState::default();
        self.comparison.commit_titles = changes.commit_titles;
        self.local_identity = snapshot.local_identity;
        self.github_origin = snapshot.github_origin;
        self.history.clear();
        self.graph.clear();
        self.end_scrollbar_drag(ScrollbarOwner::History);
        self.inspect.details = None;
        if matches!(self.overlay, Overlay::GraphFilter | Overlay::FileFilter) {
            self.overlay = Overlay::None;
        }
        self.shell.error = None;
        self.files
            .replace_changes(changes.changes, changes.selected, changes.summaries);
        self.diff.replace_diff(changes.target, changes.diff_text);
        self.unfocus_diff();
        self.clear_pending_diff_focus();
        self.repository_fingerprint = snapshot
            .fingerprint
            .map(KnownFingerprint::of)
            .unwrap_or_default();
        self.ops.command_context = snapshot.command_context;
        self.rebuild_tree();
        self.clear_selection();
        if let Some(highlighted) = changes.highlighted {
            self.apply_highlighted_diff(highlighted);
        }
        self.refresh_remove_project_capability();
        self.refresh.last_check = Instant::now();
        if self.shell.active_tab == ActiveTab::History {
            self.request_history();
            self.refresh.pending = true;
            self.refresh.pending_intent = RefreshIntent::Interaction;
        }
        if self.shell.active_tab == ActiveTab::Files {
            self.request_repository_files();
        }
    }

    pub(super) fn load_non_repository_context(&mut self) {
        self.comparison = super::comparison::ComparisonState::default();
        self.advance_refresh_generation();
        self.reset_commit_inspection();
        self.inspect.clear_caches();
        self.repository = None;
        self.active_path.clone_from(&self.shell.invoking_path);
        self.local_identity = None;
        self.github_origin = false;
        self.history.clear();
        self.graph.clear();
        self.end_scrollbar_drag(ScrollbarOwner::History);
        self.inspect.details = None;
        if matches!(self.overlay, Overlay::GraphFilter | Overlay::FileFilter) {
            self.overlay = Overlay::None;
        }
        self.files.clear();
        self.show_non_repository_diff();
        self.clear_pending_diff_focus();
        self.repository_fingerprint = KnownFingerprint::default();
        self.ops.command_context = CommandContext::default();
        self.refresh_remove_project_capability();
        self.request_repository_files();
    }

    pub(super) fn switch_repository_row(&mut self, index: usize) {
        let Some(row) = self.workspaces.repository_rows.get(index).cloned() else {
            return;
        };
        self.workspaces.repository_selected = index;
        self.focus = PaneFocus::Workspaces;
        self.refresh_remove_project_capability();
        let switching = matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        );
        if self.foreground.action.is_some() && !switching {
            self.show_action_error("Wait for the current Git operation.");
            return;
        }
        if matches!(row.kind, RepositoryRowKind::Current) && row.overview.is_none() {
            if switching || self.diff.blame_read.is_some() {
                self.foreground.reads.advance_all();
            }
            self.diff.pending_blame = None;
            self.workspaces.pending_switch = None;
            self.load_non_repository_context();
            self.show_action_error("Current is not a Git repository");
            return;
        }
        if self.repository.is_some() && self.active_path == row.path {
            if matches!(
                self.foreground.action.as_ref().map(|action| &action.kind),
                Some(ForegroundKind::Switch { .. })
            ) {
                self.foreground.reads.advance_all();
                self.workspaces.pending_switch = None;
                self.diff.pending_blame = None;
            }
            return;
        }
        self.advance_refresh_generation();
        self.foreground.reads.advance_all();
        self.inspect.pending_commit_details = None;
        self.diff.pending_diff = None;
        self.diff.pending_blame = None;
        self.workspaces.pending_switch = Some(SwitchTarget {
            path: row.path,
            row_kind: row.kind,
            started: Instant::now(),
        });
        self.start_pending_switch();
    }

    pub(super) fn start_pending_switch(&mut self) {
        if self.foreground.action.is_some() {
            return;
        }
        let Some(target) = self.workspaces.pending_switch.take() else {
            return;
        };
        if !self
            .workspaces
            .selected_row()
            .is_some_and(|row| row.path == target.path && row.kind == target.row_kind)
        {
            return;
        }
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.switch.generation;
        let request = ForegroundRequest::Switch {
            id,
            generation,
            path: target.path.clone(),
        };
        match self.foreground.request_foreground(
            request,
            ForegroundKind::Switch {
                generation,
                path: target.path,
                row_kind: target.row_kind,
            },
        ) {
            Ok(_) => {
                if let Some(action) = self.foreground.action.as_mut() {
                    action.started = target.started;
                }
            }
            Err(error) => {
                self.show_action_error(&format!("Git worker stopped: {error}"));
            }
        }
    }

    pub(super) fn apply_switch(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        path: PathBuf,
        result: Result<Box<RepositorySnapshot>, ReadError>,
    ) {
        if self
            .foreground
            .take_matching_action(id, |kind| {
                matches!(
                    kind,
                    ForegroundKind::Switch {
                        generation: active_generation,
                        path: active,
                        ..
                    } if *active_generation == generation && active == &path
                )
            })
            .is_none()
        {
            return;
        }
        if !self.foreground.reads.switch.is_current(generation) {
            return;
        }
        match result {
            Ok(snapshot) => {
                self.apply_repository_snapshot(*snapshot);
            }
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => {
                self.show_result(super::commands::OperationResultView::named_message(
                    "Repository switch",
                    false,
                    &error,
                ));
            }
        }
    }
}
