use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

use crate::git::GitOperation;
use crate::herdr::HerdrAgent;

use super::commands::CommandId;
use super::effect::{
    ActiveRefresh, ChangedRefresh, ChangesRefresh, ForegroundRequest, ForegroundResult,
    ForegroundWorkers, KnownFingerprint, Lane, ReadCancellations, ReadGeneration, RefreshContext,
    RefreshIntent, RefreshProgress, RefreshRequest, RefreshResult, RefreshScope, RefreshWorker,
    RequestId, SharedSyntax, WorkspaceRefresh, start_foreground_workers, start_refresh_worker,
};
use super::files::{StagingOwner, TreeSelectionKey};
use super::shell::ActiveTab;
use super::workspaces::RepositoryRowKind;
use super::{AUTO_FETCH_INTERVAL, App};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ForegroundKind {
    Rebase {
        path: PathBuf,
    },
    Operation {
        path: PathBuf,
        command: CommandId,
        operation: GitOperation,
    },
    BranchTargets {
        command: CommandId,
        path: PathBuf,
    },
    ResetContext {
        path: PathBuf,
    },
    LoadAmendMessage {
        path: PathBuf,
    },
    ListAgentPanes {
        path: PathBuf,
    },
    SendAgentRequest {
        path: PathBuf,
        agent: HerdrAgent,
    },
    ValidateName {
        path: PathBuf,
        command: CommandId,
        name: String,
    },
    Staging {
        path: PathBuf,
        operation: GitOperation,
        owner: Option<StagingOwner>,
    },
    AddProject,
    RemoveProject {
        root: PathBuf,
    },
    Switch {
        generation: ReadGeneration,
        path: PathBuf,
        row_kind: RepositoryRowKind,
    },
}

impl ForegroundKind {
    pub(super) fn command(&self) -> Option<CommandId> {
        match self {
            Self::Operation { command, .. } | Self::ValidateName { command, .. } => Some(*command),
            Self::BranchTargets { command, .. } => Some(*command),
            Self::Rebase { .. } => Some(CommandId::InteractiveRebase),
            Self::ResetContext { .. } => Some(CommandId::ResetCurrentBranch),
            Self::LoadAmendMessage { .. }
            | Self::ListAgentPanes { .. }
            | Self::SendAgentRequest { .. } => None,
            Self::Staging { .. }
            | Self::AddProject
            | Self::RemoveProject { .. }
            | Self::Switch { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct ForegroundAction {
    pub(super) id: RequestId,
    pub(super) kind: ForegroundKind,
    pub(super) started: Instant,
}

pub(super) struct RefreshLane {
    pub(super) request_tx: Sender<RefreshRequest>,
    pub(super) result_rx: Receiver<RefreshResult>,
    pub(super) workspace_rx: Receiver<WorkspaceRefresh>,
    pub(super) deferred_workspaces: Option<WorkspaceRefresh>,
    pub(super) workspaces_pending: bool,
    pub(super) inspect_workspaces: bool,
    pub(super) scope: RefreshScope,
    pub(super) running_scope: RefreshScope,
    pub(super) in_flight: bool,
    pub(super) pending: bool,
    pub(super) pending_intent: RefreshIntent,
    pub(super) deferred_result: Option<RefreshResult>,
    pub(super) next_id: RequestId,
    pub(super) latest_id: RequestId,
    pub(super) generation: ReadGeneration,
    pub(super) cancellation_generation: Arc<AtomicU64>,
    pub(super) progress: Option<RefreshProgress>,
    pub(super) last_check: Instant,
    pub(super) last_remote_fetch: Instant,
    pub(super) fetch_in_flight: Option<RequestId>,
}

impl RefreshLane {
    pub(super) fn running_progress(&self) -> Option<&RefreshProgress> {
        self.progress.as_ref().filter(|_| self.in_flight)
    }

    pub(super) fn active_progress(&self) -> Option<&RefreshProgress> {
        self.running_progress()
            .filter(|_| !self.workspaces_pending && self.running_scope != RefreshScope::Workspaces)
    }

    pub(super) fn start(syntax: SharedSyntax) -> Result<Self, String> {
        let cancellation_generation = Arc::new(AtomicU64::new(0));
        let RefreshWorker {
            request_tx,
            result_rx,
            workspace_rx,
        } = start_refresh_worker(Arc::clone(&cancellation_generation), syntax)?;
        Ok(Self {
            request_tx,
            result_rx,
            workspace_rx,
            deferred_workspaces: None,
            workspaces_pending: false,
            inspect_workspaces: true,
            scope: RefreshScope::Repository,
            running_scope: RefreshScope::Repository,
            in_flight: false,
            pending: false,
            pending_intent: RefreshIntent::Polling,
            deferred_result: None,
            next_id: RequestId::FIRST,
            latest_id: RequestId::ZERO,
            generation: ReadGeneration::ZERO,
            cancellation_generation,
            progress: None,
            last_check: Instant::now(),
            last_remote_fetch: Instant::now() - AUTO_FETCH_INTERVAL,
            fetch_in_flight: None,
        })
    }
}

pub(super) struct ReadGate {
    pub(super) generation: ReadGeneration,
    pub(super) cancellation: Arc<AtomicU64>,
}

impl ReadGate {
    fn new() -> Self {
        Self {
            generation: ReadGeneration::ZERO,
            cancellation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn advance(&mut self) {
        self.generation.advance();
        self.cancellation
            .store(self.generation.get(), Ordering::Release);
    }

    pub(super) fn is_current(&self, generation: ReadGeneration) -> bool {
        generation == self.generation
    }
}

pub(super) struct ReadGates {
    pub(super) diff: ReadGate,
    pub(super) details: ReadGate,
    pub(super) blame: ReadGate,
    pub(super) selection_blame: ReadGate,
    pub(super) line_history: ReadGate,
    pub(super) switch: ReadGate,
}

impl ReadGates {
    fn new() -> Self {
        Self {
            diff: ReadGate::new(),
            details: ReadGate::new(),
            blame: ReadGate::new(),
            selection_blame: ReadGate::new(),
            line_history: ReadGate::new(),
            switch: ReadGate::new(),
        }
    }

    fn cancellations(&self) -> ReadCancellations {
        ReadCancellations {
            diff: Arc::clone(&self.diff.cancellation),
            details: Arc::clone(&self.details.cancellation),
            blame: Arc::clone(&self.blame.cancellation),
            selection_blame: Arc::clone(&self.selection_blame.cancellation),
            line_history: Arc::clone(&self.line_history.cancellation),
            switch: Arc::clone(&self.switch.cancellation),
        }
    }

    pub(super) fn advance_all(&mut self) {
        self.diff.advance();
        self.details.advance();
        self.blame.advance();
        self.selection_blame.advance();
        self.line_history.advance();
        self.switch.advance();
    }
}

pub(super) struct ForegroundLane {
    pub(super) read_tx: Sender<ForegroundRequest>,
    pub(super) mutation_tx: Sender<ForegroundRequest>,
    pub(super) result_rx: Receiver<ForegroundResult>,
    pub(super) action: Option<ForegroundAction>,
    pub(super) next_id: RequestId,
    pub(super) reads: ReadGates,
}

impl ForegroundLane {
    pub(super) fn start(syntax: SharedSyntax) -> Result<Self, String> {
        let reads = ReadGates::new();
        let ForegroundWorkers {
            read_tx,
            mutation_tx,
            result_rx,
        } = start_foreground_workers(reads.cancellations(), syntax)?;
        Ok(Self {
            read_tx,
            mutation_tx,
            result_rx,
            action: None,
            next_id: RequestId::FIRST,
            reads,
        })
    }

    pub(super) fn request_foreground(
        &mut self,
        request: ForegroundRequest,
        kind: ForegroundKind,
    ) -> Result<RequestId, String> {
        let id = self.request(request)?;
        self.action = Some(ForegroundAction {
            id,
            kind,
            started: Instant::now(),
        });
        Ok(id)
    }

    pub(super) fn request(&mut self, request: ForegroundRequest) -> Result<RequestId, String> {
        let id = self.next_id;
        self.send(request)?;
        self.next_id.advance();
        Ok(id)
    }

    pub(super) fn send(&self, request: ForegroundRequest) -> Result<(), String> {
        let sender = match request.lane() {
            Lane::Read => &self.read_tx,
            Lane::Mutation => &self.mutation_tx,
        };
        sender.send(request).map_err(|error| error.to_string())
    }

    pub(super) fn take_matching_action(
        &mut self,
        id: RequestId,
        matches: impl FnOnce(&ForegroundKind) -> bool,
    ) -> Option<ForegroundKind> {
        let owned = self
            .action
            .as_ref()
            .is_some_and(|action| action.id == id && matches(&action.kind));
        if !owned {
            return None;
        }
        self.action.take().map(|action| action.kind)
    }
}

impl App {
    pub(super) fn request_background_refresh(&mut self, running: &str, scope: RefreshScope) {
        self.request_refresh(running, RefreshIntent::Interaction, scope);
    }

    pub(super) fn request_operation_refresh(&mut self, running: &str, scope: RefreshScope) {
        self.request_refresh(running, RefreshIntent::Operation, scope);
    }

    pub(super) fn request_refresh(
        &mut self,
        running: &str,
        intent: RefreshIntent,
        scope: RefreshScope,
    ) {
        self.advance_refresh_generation();
        self.refresh.scope = scope;
        self.refresh.inspect_workspaces =
            !(intent == RefreshIntent::Operation && scope == RefreshScope::Changes);
        if matches!(scope, RefreshScope::History | RefreshScope::Repository)
            && (self.graph.history_loaded || self.shell.active_tab == ActiveTab::History)
        {
            self.request_history_for(intent);
        }
        match scope {
            RefreshScope::Workspaces => {}
            RefreshScope::History => self.repository_fingerprint.refs = None,
            RefreshScope::Changes => self.repository_fingerprint.worktree = None,
            RefreshScope::Repository => self.repository_fingerprint = KnownFingerprint::default(),
        }
        self.refresh.pending = true;
        self.refresh.pending_intent = intent;
        self.refresh.progress = Some(RefreshProgress {
            started: Instant::now(),
            running: running.to_owned(),
        });
        if intent == RefreshIntent::Operation
            || (!self.refresh.in_flight
                && self.refresh.deferred_result.is_none()
                && !self.refresh_is_transient(intent))
        {
            self.start_auto_refresh();
        }
    }

    pub(super) fn advance_refresh_generation(&mut self) {
        self.refresh.generation.advance();
        self.refresh
            .cancellation_generation
            .store(self.refresh.generation.get(), Ordering::Release);
        self.refresh.deferred_result = None;
        self.refresh.deferred_workspaces = None;
        self.refresh.workspaces_pending = false;
        self.refresh.inspect_workspaces = true;
        self.refresh.progress = None;
    }

    pub(super) fn refresh_context(&self) -> RefreshContext {
        RefreshContext {
            generation: self.refresh.generation,
            invoking_path: self.shell.invoking_path.clone(),
            project_roots: self.workspaces.project_registry.roots().to_vec(),
            expanded_projects: self.workspaces.expanded_projects(),
            active_repository: self
                .repository
                .as_ref()
                .map(|repository| repository.root().to_owned()),
            history_loaded: self.graph.history_loaded,
            wants_history: self.graph.history_loaded || self.shell.active_tab == ActiveTab::History,
            query: self.graph.query.text.clone(),
            selected_commit_sha: self
                .graph
                .selected_commit()
                .map(|commit| commit.sha.clone()),
            selected_change: self
                .files
                .changes
                .get(self.files.change_selected)
                .map(|change| TreeSelectionKey {
                    section: change.section,
                    path: change.path.clone(),
                }),
            comparison: self.comparison.mode.clone(),
            diff_target: self.diff.diff_target.clone(),
            fold_toggles: self.diff.fold_toggles.clone(),
        }
    }

    pub(super) fn receive_workspace_results(&mut self) -> bool {
        let mut received = false;
        while let Ok(result) = self.refresh.workspace_rx.try_recv() {
            if result.id == self.refresh.latest_id
                && result.context.generation == self.refresh.generation
            {
                self.refresh.workspaces_pending = false;
                self.refresh.deferred_workspaces = Some(result);
                received = true;
            }
        }
        received
    }

    pub(super) fn apply_workspace_refresh(&mut self) -> bool {
        let Some(result) = self.refresh.deferred_workspaces.as_ref() else {
            return false;
        };
        if self.refresh_is_transient(result.intent) {
            return false;
        }
        let result = self
            .refresh
            .deferred_workspaces
            .take()
            .expect("workspace completion");
        if result.id != self.refresh.latest_id
            || result.context.generation != self.refresh.generation
            || result.context.invoking_path != self.shell.invoking_path
            || result.context.project_roots != self.workspaces.project_registry.roots()
        {
            return true;
        }
        if let Some(current) = result.repository_rows.first() {
            let rows = crate::ui::workspaces::repository_rows_from_statuses(
                current,
                self.workspaces.project_registry.roots(),
                &result.project_statuses,
                &self.workspaces.expanded_projects(),
            );
            self.apply_repository_rows(result.project_statuses, rows);
        }
        self.sync_uncommitted_row();
        true
    }

    pub(super) fn receive_refresh_result(&mut self) -> bool {
        let mut received = false;
        while let Ok(result) = self.refresh.result_rx.try_recv() {
            received = true;
            if result.id != self.refresh.latest_id {
                continue;
            }
            self.refresh.in_flight = false;
            self.refresh.workspaces_pending = false;
            if result.cancelled {
                if !self.refresh.pending {
                    self.refresh.pending_intent = RefreshIntent::Interaction;
                }
                self.refresh.pending = true;
                continue;
            }
            self.refresh.deferred_result = Some(result);
        }
        received
    }

    pub(super) fn refresh_is_transient(&self, intent: RefreshIntent) -> bool {
        self.overlay.is_transient(intent)
            || self.files.is_selecting()
            || self.review.is_selecting()
            || self.diff.is_resizing()
            || self.foreground.action.is_some()
    }

    pub(super) fn apply_deferred_refresh(&mut self) -> bool {
        let Some(result) = self.refresh.deferred_result.take() else {
            return false;
        };
        let current = self.refresh_context();
        if result.id != self.refresh.latest_id
            || result.context.generation != current.generation
            || result.context.active_repository != current.active_repository
        {
            self.refresh.pending = true;
            self.refresh.pending_intent = result.intent;
            return true;
        }
        let diff_matches = result.context.selected_change == current.selected_change
            && result.context.comparison == current.comparison
            && result.context.diff_target == current.diff_target
            && result.context.fold_toggles == current.fold_toggles;
        let mut retry_changes = false;
        match result.active {
            None => {}
            Some(Err(error)) => {
                self.shell.error = Some(error);
            }
            Some(Ok(ActiveRefresh::Unchanged { fingerprint })) => {
                self.repository_fingerprint = KnownFingerprint::of(fingerprint);
                self.shell.error = None;
            }
            Some(Ok(ActiveRefresh::Changed(mut changed))) => {
                if !diff_matches && changed.changes.is_some() {
                    changed.changes = None;
                    retry_changes = true;
                }
                self.apply_changed_refresh(*changed, result.intent);
            }
        }
        self.refresh.pending = retry_changes;
        if retry_changes {
            self.repository_fingerprint.worktree = None;
            self.refresh.pending_intent = result.intent;
            self.refresh.inspect_workspaces = false;
        }
        if !retry_changes {
            self.refresh.progress.take();
        }
        true
    }

    fn apply_changed_refresh(&mut self, changed: ChangedRefresh, intent: RefreshIntent) -> bool {
        let mut known = KnownFingerprint::of(changed.fingerprint);
        if changed.history {
            self.request_history_for(intent);
        }
        if let Some(changes) = changed.changes
            && !self.apply_changes_refresh(changes)
        {
            known.worktree = None;
        }
        self.ops.command_context = changed.command_context;
        self.sync_uncommitted_row();
        self.refresh_command_selection_context();
        self.refresh_remove_project_capability();
        self.repository_fingerprint = known;
        known.refs.is_some() && known.worktree.is_some()
    }

    fn apply_changes_refresh(&mut self, changes: Result<ChangesRefresh, String>) -> bool {
        let changes = match changes {
            Ok(changes) => changes,
            Err(error) => {
                self.clear_pending_diff_focus();
                self.shell.error = Some(error);
                return false;
            }
        };
        self.shell.error = None;
        let saved_selection = self.selection_lines_on(super::review::SelectionSurface::Changes);
        self.files.changes = changes.changes;
        self.files.change_selected = changes.selected;
        self.diff.diff_target = changes.target;
        self.comparison.commit_titles = changes.commit_titles;
        self.files.change_summaries = changes.summaries;
        self.diff.diff_text = changes.diff_text;
        self.rebuild_tree();
        self.clear_selection();
        if let Some(highlighted) = changes.highlighted {
            self.apply_highlighted_diff(highlighted);
        }
        self.restore_selection(saved_selection);
        self.settle_diff_after_refresh();
        true
    }

    pub(super) fn start_auto_refresh(&mut self) {
        let id = self.refresh.next_id.take_and_advance();
        self.refresh.latest_id = id;
        let intent = self.refresh.pending_intent;
        if intent == RefreshIntent::Polling
            && self.refresh.fetch_in_flight.is_none()
            && self.refresh.last_remote_fetch.elapsed() >= AUTO_FETCH_INTERVAL
        {
            self.start_remote_fetch();
        }
        let request = RefreshRequest {
            scope: self.refresh.scope,
            inspect_workspaces: self.refresh.inspect_workspaces,
            id,
            intent,
            context: self.refresh_context(),
            previous_fingerprint: self.repository_fingerprint,
            previous_highlight: self.diff.highlight_key,
            previous_worktrees: self.workspaces.inspected_worktrees(),
            changes: self.files.changes.clone(),
        };
        match self.refresh.request_tx.send(request) {
            Ok(()) => {
                if let Some(progress) = self.refresh.progress.as_mut() {
                    progress.started = Instant::now();
                }
                self.refresh.in_flight = true;
                self.refresh.running_scope = self.refresh.scope;
                self.refresh.workspaces_pending = self.refresh.inspect_workspaces;
                self.refresh.inspect_workspaces = true;
                self.refresh.pending = false;
                self.refresh.last_check = Instant::now();
            }
            Err(error) => {
                self.shell.error = Some(format!("Refresh worker stopped: {error}"));
                self.refresh.progress = None;
                self.refresh.in_flight = false;
                self.refresh.pending = false;
            }
        }
    }

    fn start_remote_fetch(&mut self) {
        let Some(repository) = self.repository.as_ref() else {
            return;
        };
        let request = ForegroundRequest::Fetch {
            id: self.foreground.next_id,
            roots: vec![repository.root().to_owned()],
        };
        if let Ok(id) = self.foreground.request(request) {
            self.refresh.fetch_in_flight = Some(id);
            self.refresh.last_remote_fetch = Instant::now();
        }
    }

    pub(super) fn apply_fetch(&mut self, id: RequestId) {
        if self.refresh.fetch_in_flight != Some(id) {
            return;
        }
        self.refresh.fetch_in_flight = None;
        if !self.refresh.pending {
            self.refresh.pending = true;
            self.refresh.pending_intent = RefreshIntent::Polling;
        }
    }
}

#[cfg(test)]
mod workspace_tests {
    use crate::ui::effect::WorkspaceRefresh;
    use crate::ui::test_support::{buffer_text, committed_change, render};

    use super::*;

    #[test]
    fn staging_refresh_skips_workspace_inspection_and_polling_restores_it() {
        let (root, mut app) = committed_change("stage-refresh-scope");
        let (tx, rx) = std::sync::mpsc::channel();
        app.refresh.request_tx = tx;
        app.request_operation_refresh("Refreshing changes", RefreshScope::Changes);
        let request = rx.try_recv().unwrap();
        assert!(!request.inspect_workspaces);
        assert_eq!(request.scope, RefreshScope::Changes);
        assert_eq!(request.intent, RefreshIntent::Operation);
        app.refresh.pending_intent = RefreshIntent::Polling;
        app.start_auto_refresh();
        assert!(rx.try_recv().unwrap().inspect_workspaces);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_diff_retries_without_rescanning_workspaces_or_reporting_success_early() {
        let (root, mut app) = committed_change("workspace-diff-retry");
        let (tx, rx) = std::sync::mpsc::channel();
        app.refresh.request_tx = tx;
        let context = app.refresh_context();
        let rows = app.workspaces.repository_rows.clone();
        let diff_text = app.diff.diff_text.clone();
        app.refresh.deferred_result = Some(RefreshResult {
            id: app.refresh.latest_id,
            intent: RefreshIntent::Interaction,
            context,
            cancelled: false,
            active: Some(Ok(ActiveRefresh::Changed(Box::new(ChangedRefresh {
                fingerprint: crate::git::RepositoryFingerprint {
                    refs: 1,
                    worktree: 2,
                },
                history: false,
                changes: Some(Err("stale diff error".into())),
                command_context: app.ops.command_context.clone(),
            })))),
        });
        app.refresh.progress = Some(RefreshProgress {
            started: Instant::now(),
            running: "Loading changes".into(),
        });
        app.files.change_selected = usize::MAX;
        assert!(app.apply_deferred_refresh());
        assert_eq!(app.workspaces.repository_rows, rows);
        assert_eq!(app.diff.diff_text, diff_text);
        assert!(app.shell.error.is_none());
        assert!(!app.status_bar_text().contains("Changes loaded"));
        assert!(app.refresh.progress.is_some());
        assert!(app.refresh.pending);
        assert!(app.repository_fingerprint.worktree.is_none());
        app.start_auto_refresh();
        assert!(!rx.try_recv().unwrap().inspect_workspaces);
        assert!(!app.refresh.workspaces_pending);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_completion_defers_under_overlays_and_rejects_changed_registry() {
        let (root, mut app) = committed_change("workspace-deferred");
        let result = WorkspaceRefresh {
            id: app.refresh.latest_id,
            intent: RefreshIntent::Interaction,
            context: app.refresh_context(),
            project_statuses: app.workspaces.project_statuses.clone(),
            repository_rows: std::mem::take(&mut app.workspaces.repository_rows),
        };
        app.refresh.deferred_workspaces = Some(result);
        app.overlay = crate::ui::overlay::Overlay::GraphFilter;
        assert!(!app.apply_workspace_refresh());
        assert!(app.workspaces.repository_rows.is_empty());
        app.overlay = crate::ui::overlay::Overlay::None;
        app.refresh
            .deferred_workspaces
            .as_mut()
            .unwrap()
            .context
            .project_roots
            .push(root.clone());
        assert!(app.apply_workspace_refresh());
        assert!(app.workspaces.repository_rows.is_empty());
        assert!(app.refresh.deferred_workspaces.is_none());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_results_apply_before_active_refresh_despite_file_and_filter_changes() {
        let (root, mut app) = committed_change("workspace-early");
        let rows = std::mem::take(&mut app.workspaces.repository_rows);
        let context = app.refresh_context();
        let (tx, rx) = std::sync::mpsc::channel();
        app.refresh.workspace_rx = rx;
        app.refresh.latest_id = RequestId::FIRST;
        app.refresh.in_flight = true;
        app.refresh.workspaces_pending = true;
        app.refresh.progress = Some(RefreshProgress {
            started: Instant::now(),
            running: "Refreshing repository".into(),
        });
        let text = buffer_text(&render(&mut app, 120, 40));
        assert_eq!(text.matches("Loading Workspaces").count(), 1);
        let pane = app.workspaces.repository_area;
        let buffer = render(&mut app, 120, 40);
        let title: String = (pane.x..pane.right())
            .map(|x| buffer[(x, pane.y)].symbol())
            .collect();
        assert!(title.contains("Loading Workspaces"));
        app.graph.query.text = "new filter".into();
        app.files.change_selected = usize::MAX;
        tx.send(WorkspaceRefresh {
            id: RequestId::FIRST,
            intent: RefreshIntent::Interaction,
            context,
            project_statuses: app.workspaces.project_statuses.clone(),
            repository_rows: rows,
        })
        .unwrap();
        assert!(app.receive_workspace_results());
        assert!(app.apply_workspace_refresh());
        assert!(!app.workspaces.repository_rows.is_empty());
        assert!(app.refresh.in_flight, "active refresh is still pending");
        assert!(!app.refresh.workspaces_pending);
        assert!(!buffer_text(&render(&mut app, 120, 40)).contains("Loading Workspaces"));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
