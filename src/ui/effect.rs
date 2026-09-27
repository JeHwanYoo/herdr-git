use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

use crate::git::{
    BlameInfo, ChangeSection, Commit, CommitDetails, DiffSummary, DiffTarget, GIT_READ_CANCELLED,
    GitOperation, LineHistoryCommit, LocalIdentity, ReadError, Repository, RepositoryFingerprint,
    ResetContext, WorkingChange, with_read_cancellation, with_read_cancellation_result,
};
use crate::herdr::{HerdrAgent, list_agents, send_agent_request};
use crate::project::{
    ProjectRegistry, ProjectStatus, WorktreeStatus, inspect_project, inspect_worktree,
    pick_project_directory,
};

use super::commands::{CommandContext, CommandId};
use super::files::TreeSelectionKey;
use super::syntax::{DiffDocument, FoldKey, SyntaxHighlighter};
use super::workspaces::{RepositoryRow, RepositoryRowKind, repository_rows_from_statuses};
pub(super) use foreground::start_foreground_workers;
#[cfg(test)]
pub(super) use foreground::{load_repository_snapshot, run_foreground_job};
#[cfg(test)]
pub(super) use refresh::run_refresh_job;
pub(super) use refresh::start_refresh_worker;
#[cfg(test)]
use refresh::{inspect_repository_rows_cancellable, refresh_active_repository, refresh_changes};

mod foreground;
mod refresh;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct RequestId(u64);

impl RequestId {
    pub(super) const ZERO: Self = Self(0);
    pub(super) const FIRST: Self = Self(1);

    #[cfg(test)]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(super) const fn get(self) -> u64 {
        self.0
    }

    pub(super) fn take_and_advance(&mut self) -> Self {
        let current = *self;
        self.0 = self.0.wrapping_add(1);
        current
    }

    pub(super) fn advance(&mut self) -> Self {
        self.0 = self.0.wrapping_add(1);
        *self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct ReadGeneration(u64);

impl ReadGeneration {
    pub(super) const ZERO: Self = Self(0);

    #[cfg(test)]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(super) const fn get(self) -> u64 {
        self.0
    }

    pub(super) fn advance(&mut self) -> Self {
        self.0 = self.0.wrapping_add(1);
        *self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct KnownFingerprint {
    pub(super) refs: Option<u64>,
    pub(super) worktree: Option<u64>,
}

impl KnownFingerprint {
    pub(super) fn of(fingerprint: RepositoryFingerprint) -> Self {
        Self {
            refs: Some(fingerprint.refs),
            worktree: Some(fingerprint.worktree),
        }
    }

    pub(super) fn refs_changed(self, current: RepositoryFingerprint) -> bool {
        self.refs != Some(current.refs)
    }

    pub(super) fn worktree_changed(self, current: RepositoryFingerprint) -> bool {
        self.worktree != Some(current.worktree)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefreshScope {
    Workspaces,
    History,
    Changes,
    Repository,
}

pub(super) type RepositoryInspection = (Vec<Result<ProjectStatus, String>>, Vec<RepositoryRow>);

pub(super) type SharedSyntax = Arc<OnceLock<SyntaxHighlighter>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lane {
    Read,
    Mutation,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ReadCancellations {
    pub(super) diff: Arc<AtomicU64>,
    pub(super) details: Arc<AtomicU64>,
    pub(super) blame: Arc<AtomicU64>,
    pub(super) selection_blame: Arc<AtomicU64>,
    pub(super) line_history: Arc<AtomicU64>,
    pub(super) switch: Arc<AtomicU64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RefreshContext {
    pub(super) generation: ReadGeneration,
    pub(super) invoking_path: PathBuf,
    pub(super) project_roots: Vec<PathBuf>,
    pub(super) expanded_projects: Vec<PathBuf>,
    pub(super) active_repository: Option<PathBuf>,
    pub(super) history_loaded: bool,
    pub(super) wants_history: bool,
    pub(super) query: String,
    pub(super) selected_commit_sha: Option<String>,
    pub(super) selected_change: Option<TreeSelectionKey>,
    pub(super) diff_target: DiffTarget,
    pub(super) comparison: Option<crate::git::ChangesComparison>,
    pub(super) fold_toggles: HashSet<FoldKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RefreshIntent {
    Polling,
    Interaction,
    Operation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RefreshRequest {
    pub(super) scope: RefreshScope,
    pub(super) inspect_workspaces: bool,
    pub(super) id: RequestId,
    pub(super) intent: RefreshIntent,
    pub(super) context: RefreshContext,
    pub(super) previous_fingerprint: KnownFingerprint,
    pub(super) previous_highlight: Option<u64>,
    pub(super) previous_worktrees: InspectedWorktrees,
    pub(super) changes: Vec<WorkingChange>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct InspectedWorktrees {
    pub(super) current: Option<WorktreeStatus>,
    pub(super) projects: Vec<WorktreeStatus>,
}

#[derive(Debug)]
pub(super) struct WorkspaceRefresh {
    pub(super) id: RequestId,
    pub(super) intent: RefreshIntent,
    pub(super) context: RefreshContext,
    pub(super) project_statuses: Vec<Result<ProjectStatus, String>>,
    pub(super) repository_rows: Vec<RepositoryRow>,
}

#[derive(Debug)]
pub(super) struct RefreshResult {
    pub(super) id: RequestId,
    pub(super) intent: RefreshIntent,
    pub(super) context: RefreshContext,
    pub(super) cancelled: bool,
    pub(super) active: Option<Result<ActiveRefresh, String>>,
}

pub(super) struct RefreshWorker {
    pub(super) request_tx: Sender<RefreshRequest>,
    pub(super) result_rx: Receiver<RefreshResult>,
    pub(super) workspace_rx: Receiver<WorkspaceRefresh>,
}

#[derive(Debug)]
pub(super) enum ActiveRefresh {
    Unchanged { fingerprint: RepositoryFingerprint },
    Changed(Box<ChangedRefresh>),
}

#[derive(Debug)]
pub(super) struct ChangedRefresh {
    pub(super) fingerprint: RepositoryFingerprint,
    pub(super) history: bool,
    pub(super) changes: Option<Result<ChangesRefresh, String>>,
    pub(super) command_context: CommandContext,
}

#[derive(Debug)]
pub(super) struct ChangesRefresh {
    pub(super) changes: Vec<WorkingChange>,
    pub(super) selected: usize,
    pub(super) target: DiffTarget,
    pub(super) commit_titles: Vec<(String, String)>,
    pub(super) summaries: Vec<(ChangeSection, DiffSummary)>,
    pub(super) diff_text: String,
    pub(super) highlighted: Option<HighlightedDiff>,
}

#[derive(Debug)]
pub(super) struct HighlightedDiff {
    pub(super) language: String,
    pub(super) split: DiffDocument,
    pub(super) key: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiffOwner {
    Changes,
    CommitPreview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DiffReadTarget {
    pub(super) owner: DiffOwner,
    pub(super) path: PathBuf,
    pub(super) file: String,
    pub(super) target: DiffTarget,
    pub(super) fold_toggles: HashSet<FoldKey>,
}

#[derive(Debug)]
pub(super) struct PendingDiff {
    pub(super) id: RequestId,
    pub(super) generation: ReadGeneration,
    pub(super) target: DiffReadTarget,
    pub(super) started: Instant,
}

#[derive(Debug)]
pub(super) struct BlameRead {
    pub(super) id: RequestId,
    pub(super) generation: ReadGeneration,
    pub(super) target: BlameTarget,
    pub(super) started: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommitDetailsReadTarget {
    pub(super) path: PathBuf,
    pub(super) commit: Commit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingCommitDetails {
    pub(super) id: RequestId,
    pub(super) generation: ReadGeneration,
    pub(super) target: CommitDetailsReadTarget,
    pub(super) started: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BlameTarget {
    pub(super) path: PathBuf,
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) revision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SelectionBlameTarget {
    pub(super) path: PathBuf,
    pub(super) file: String,
    pub(super) lines: Vec<usize>,
    pub(super) revision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LineHistoryTarget {
    pub(super) path: PathBuf,
    pub(super) file: String,
    pub(super) ranges: Vec<(usize, usize)>,
    pub(super) revision: Option<String>,
}

#[derive(Debug)]
pub(super) struct LineHistoryEntry {
    pub(super) commit: LineHistoryCommit,
    pub(super) document: DiffDocument,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SwitchTarget {
    pub(super) path: PathBuf,
    pub(super) row_kind: RepositoryRowKind,
    pub(super) started: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ForegroundRequest {
    RebasePlan {
        id: RequestId,
        path: PathBuf,
        operation: GitOperation,
    },
    RebaseRun {
        id: RequestId,
        path: PathBuf,
        task: crate::git::RebaseTask,
    },
    Operation {
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        operation: GitOperation,
    },
    ComparisonTargets {
        id: RequestId,
        path: PathBuf,
    },
    BranchTargets {
        command: CommandId,
        id: RequestId,
        path: PathBuf,
    },
    ResetContext {
        id: RequestId,
        path: PathBuf,
    },
    LoadAmendMessage {
        id: RequestId,
        path: PathBuf,
    },
    ListAgentPanes {
        id: RequestId,
        path: PathBuf,
    },
    SendAgentRequest {
        id: RequestId,
        path: PathBuf,
        agent: HerdrAgent,
        prompt: String,
        amend: bool,
    },
    ValidateName {
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        name: String,
    },
    Staging {
        id: RequestId,
        path: PathBuf,
        operation: GitOperation,
    },
    Diff {
        id: RequestId,
        generation: ReadGeneration,
        owner: DiffOwner,
        path: PathBuf,
        file: String,
        target: DiffTarget,
        fold_toggles: HashSet<FoldKey>,
    },
    Refold {
        id: RequestId,
        generation: ReadGeneration,
        target: DiffReadTarget,
        text: String,
    },
    CommitDetails {
        id: RequestId,
        generation: ReadGeneration,
        target: CommitDetailsReadTarget,
    },
    Blame {
        id: RequestId,
        generation: ReadGeneration,
        path: PathBuf,
        file: String,
        line: usize,
        revision: Option<String>,
    },
    SelectionBlame {
        id: RequestId,
        generation: ReadGeneration,
        target: SelectionBlameTarget,
    },
    LineHistory {
        id: RequestId,
        generation: ReadGeneration,
        target: LineHistoryTarget,
    },
    AddProject {
        id: RequestId,
        registry: ProjectRegistry,
    },
    RemoveProject {
        id: RequestId,
        registry: ProjectRegistry,
        root: PathBuf,
    },
    Switch {
        id: RequestId,
        generation: ReadGeneration,
        path: PathBuf,
    },
    Fetch {
        id: RequestId,
        roots: Vec<PathBuf>,
    },
}

impl ForegroundRequest {
    pub(super) fn lane(&self) -> Lane {
        match self {
            Self::RebasePlan { .. }
            | Self::Diff { .. }
            | Self::Refold { .. }
            | Self::CommitDetails { .. }
            | Self::Blame { .. }
            | Self::SelectionBlame { .. }
            | Self::LineHistory { .. }
            | Self::Switch { .. }
            | Self::ComparisonTargets { .. }
            | Self::BranchTargets { .. } => Lane::Read,
            Self::RebaseRun { .. }
            | Self::Operation { .. }
            | Self::ResetContext { .. }
            | Self::LoadAmendMessage { .. }
            | Self::ListAgentPanes { .. }
            | Self::SendAgentRequest { .. }
            | Self::ValidateName { .. }
            | Self::Staging { .. }
            | Self::AddProject { .. }
            | Self::RemoveProject { .. }
            | Self::Fetch { .. } => Lane::Mutation,
        }
    }
}

#[derive(Debug)]
pub(super) enum ForegroundResult {
    RebasePlan {
        id: RequestId,
        path: PathBuf,
        result: Result<crate::git::RebasePlan, String>,
    },
    RebaseRun {
        id: RequestId,
        path: PathBuf,
        result: Result<crate::git::RebaseResult, String>,
    },
    Operation {
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        operation: GitOperation,
        result: Result<String, String>,
    },
    ComparisonTargets {
        id: RequestId,
        path: PathBuf,
        result: Result<Vec<crate::git::ResetTarget>, String>,
    },
    BranchTargets {
        command: CommandId,
        id: RequestId,
        path: PathBuf,
        result: Result<Vec<crate::git::ResetTarget>, String>,
    },
    ResetContext {
        id: RequestId,
        path: PathBuf,
        result: Result<ResetContext, String>,
    },
    AmendMessageLoaded {
        id: RequestId,
        path: PathBuf,
        result: Result<String, String>,
    },
    AgentPanesListed {
        id: RequestId,
        path: PathBuf,
        result: Result<Vec<HerdrAgent>, String>,
    },
    AgentRequestSent {
        id: RequestId,
        path: PathBuf,
        agent: HerdrAgent,
        result: Result<(), String>,
    },
    ValidateName {
        id: RequestId,
        path: PathBuf,
        command: CommandId,
        name: String,
        result: Result<(), String>,
    },
    Staging {
        id: RequestId,
        path: PathBuf,
        operation: GitOperation,
        result: Result<String, String>,
    },
    Diff {
        id: RequestId,
        generation: ReadGeneration,
        owner: DiffOwner,
        path: PathBuf,
        file: String,
        target: DiffTarget,
        fold_toggles: HashSet<FoldKey>,
        result: Result<(String, HighlightedDiff), ReadError>,
    },
    CommitDetails {
        id: RequestId,
        generation: ReadGeneration,
        target: CommitDetailsReadTarget,
        result: Box<Result<CommitDetails, ReadError>>,
    },
    Blame {
        id: RequestId,
        generation: ReadGeneration,
        path: PathBuf,
        file: String,
        line: usize,
        revision: Option<String>,
        result: Result<BlameInfo, ReadError>,
    },
    SelectionBlame {
        id: RequestId,
        generation: ReadGeneration,
        target: SelectionBlameTarget,
        result: Result<Vec<BlameInfo>, ReadError>,
    },
    LineHistory {
        id: RequestId,
        generation: ReadGeneration,
        result: Result<Vec<LineHistoryEntry>, ReadError>,
    },
    ProjectMutation {
        id: RequestId,
        result: Result<(ProjectRegistry, ProjectMutationOutcome), String>,
    },
    Switch {
        id: RequestId,
        generation: ReadGeneration,
        path: PathBuf,
        result: Result<Box<RepositorySnapshot>, ReadError>,
    },
    Fetch {
        id: RequestId,
    },
}

#[derive(Debug)]
pub(super) struct RefreshProgress {
    pub(super) started: Instant,
    pub(super) running: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProjectMutationOutcome {
    Added,
    AlreadyRegistered,
    Cancelled,
    Removed,
    NotRegistered,
}

#[derive(Debug)]
pub(super) struct RepositorySnapshot {
    pub(super) repository: Repository,
    pub(super) root: PathBuf,
    pub(super) local_identity: Option<LocalIdentity>,
    pub(super) github_origin: bool,
    pub(super) fingerprint: Option<RepositoryFingerprint>,
    pub(super) changes: ChangesRefresh,
    pub(super) command_context: CommandContext,
}

pub(super) struct ForegroundWorkers {
    pub(super) read_tx: Sender<ForegroundRequest>,
    pub(super) mutation_tx: Sender<ForegroundRequest>,
    pub(super) result_rx: Receiver<ForegroundResult>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::git::{
        ChangeOverview, ChangeSection, Commit, DiffTarget, GitOperation, Repository, WorkingChange,
        git_processes_started,
    };
    use crate::herdr::HerdrAgent;
    use crate::project::ProjectRegistry;
    use crate::ui::commands::CommandId;
    use crate::ui::files::TreeSelectionKey;
    use crate::ui::syntax::{FoldKey, FoldKind, SyntaxHighlighter};
    use crate::ui::test_support::{git, temp_repo};
    use crate::ui::workspaces::{RepositoryRowKind, WorkspacesState};

    use super::{
        ActiveRefresh, CommitDetailsReadTarget, DiffOwner, ForegroundRequest, InspectedWorktrees,
        KnownFingerprint, Lane, ReadGeneration, RefreshContext, RefreshIntent, RefreshRequest,
        RequestId, inspect_repository_rows_cancellable, refresh_active_repository, refresh_changes,
    };

    #[test]
    fn typed_request_sequences_wrap_without_mixing_with_generations() {
        let mut next = RequestId::FIRST;
        assert_eq!(next.take_and_advance().get(), 1);
        assert_eq!(next.take_and_advance().get(), 2);

        let mut generation = ReadGeneration::ZERO;
        assert_eq!(generation.advance().get(), 1);
        assert_eq!(generation.advance().get(), 2);
        assert_ne!(format!("{next:?}"), format!("{generation:?}"));
    }

    fn committed_repo(name: &str) -> PathBuf {
        let root = temp_repo(name);
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::canonicalize(root).unwrap()
    }

    fn context_for(root: &Path) -> RefreshContext {
        RefreshContext {
            generation: ReadGeneration::ZERO,
            invoking_path: root.to_owned(),
            project_roots: Vec::new(),
            expanded_projects: Vec::new(),
            active_repository: Some(root.to_owned()),
            history_loaded: true,
            wants_history: true,
            query: String::new(),
            selected_commit_sha: None,
            selected_change: None,
            comparison: None,
            diff_target: DiffTarget::WorkingTreeAgainstIndex,
            fold_toggles: HashSet::new(),
        }
    }

    fn request_for(context: RefreshContext, previous: KnownFingerprint) -> RefreshRequest {
        RefreshRequest {
            scope: crate::ui::effect::RefreshScope::Repository,
            inspect_workspaces: true,
            id: RequestId::FIRST,
            intent: RefreshIntent::Polling,
            context,
            previous_fingerprint: previous,
            previous_highlight: None,
            previous_worktrees: InspectedWorktrees::default(),
            changes: Vec::new(),
        }
    }

    #[test]
    fn staging_refresh_avoids_unrelated_repository_reads() {
        let root = committed_repo("stage-latency");
        let other = committed_repo("stage-other-project");
        let repository = Repository::discover(&root).unwrap();
        fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        let start = std::time::Instant::now();
        repository
            .execute(&GitOperation::StagePath("tracked.txt".into()))
            .unwrap();
        let add_elapsed = start.elapsed();
        let mut context = context_for(&root);
        context.project_roots = vec![root.clone(), other.clone()];
        context.expanded_projects = context.project_roots.clone();
        let mut request = request_for(context, KnownFingerprint::default());
        request.scope = crate::ui::effect::RefreshScope::Changes;
        request.intent = RefreshIntent::Operation;
        let syntax = SyntaxHighlighter::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let before = git_processes_started();
        let start = std::time::Instant::now();
        let baseline = super::run_refresh_job(request.clone(), &syntax, &0.into(), &tx);
        let baseline_elapsed = start.elapsed();
        let baseline_reads = git_processes_started() - before;
        assert!(rx.try_recv().is_ok());
        request.inspect_workspaces = false;
        let before = git_processes_started();
        let start = std::time::Instant::now();
        let fast = super::run_refresh_job(request, &syntax, &0.into(), &tx);
        let fast_elapsed = start.elapsed();
        let fast_reads = git_processes_started() - before;
        assert!(rx.try_recv().is_err());
        assert!(fast_reads < baseline_reads);
        let (Some(Ok(ActiveRefresh::Changed(baseline))), Some(Ok(ActiveRefresh::Changed(fast)))) =
            (baseline.active, fast.active)
        else {
            panic!("active refreshes")
        };
        assert_eq!(baseline.fingerprint, fast.fingerprint);
        assert!(fast.command_context.has_staged_changes);
        assert_eq!(
            baseline.changes.unwrap().unwrap().changes,
            fast.changes.unwrap().unwrap().changes
        );
        eprintln!(
            "stage timing: git add={add_elapsed:?}; refresh with workspaces={baseline_elapsed:?} ({baseline_reads} Git processes); active only={fast_elapsed:?} ({fast_reads} Git processes)"
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn workspaces_only_refresh_publishes_rows_without_reading_the_active_repository() {
        let root = committed_repo("workspace-scope");
        let mut context = context_for(&root);
        context.active_repository = Some(root.join("missing"));
        let mut request = request_for(context, KnownFingerprint::default());
        request.scope = crate::ui::effect::RefreshScope::Workspaces;
        let (tx, rx) = std::sync::mpsc::channel();
        let syntax = SyntaxHighlighter::new();
        let result = super::run_refresh_job(request, &syntax, &0.into(), &tx);
        assert!(!result.cancelled);
        assert!(result.active.is_none());
        let workspaces = rx.try_recv().unwrap();
        assert_eq!(workspaces.repository_rows[0].path, root);
        assert!(workspaces.repository_rows[0].error.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    fn changed(
        root: &Path,
        request: &RefreshRequest,
        syntax: &SyntaxHighlighter,
    ) -> super::ChangedRefresh {
        match refresh_active_repository(root, request, syntax, &|| false).unwrap() {
            ActiveRefresh::Changed(changed) => *changed,
            ActiveRefresh::Unchanged { .. } => panic!("expected a changed refresh"),
        }
    }

    #[test]
    fn worktree_edits_refresh_changes_and_leave_history_until_refs_move() {
        let root = committed_repo("refresh-halves");
        let syntax = SyntaxHighlighter::new();
        let context = context_for(&root);

        let first = changed(
            &root,
            &request_for(context.clone(), KnownFingerprint::default()),
            &syntax,
        );
        assert!(
            !first.history,
            "first history starts independently of refresh"
        );
        assert!(first.changes.as_ref().is_some_and(Result::is_ok));
        let known = KnownFingerprint::of(first.fingerprint);

        fs::write(root.join("tracked.txt"), "base\nedited\n").unwrap();
        let edited = changed(&root, &request_for(context.clone(), known), &syntax);
        assert!(!edited.history, "a worktree edit leaves history alone");
        let changes = edited.changes.expect("changes re-read").unwrap();
        assert_eq!(changes.changes[0].path, "tracked.txt");
        assert!(changes.diff_text.contains("+edited"));
        assert_eq!(edited.fingerprint.refs, first.fingerprint.refs);
        let known = KnownFingerprint::of(edited.fingerprint);

        git(&root, &["commit", "-am", "Edit"]);
        let committed = changed(&root, &request_for(context.clone(), known), &syntax);
        assert!(committed.history);
        let history = Repository::at_root(root.clone()).history().unwrap();
        assert_eq!(history[0].subject, "Edit");
        assert!(committed.changes.is_some());
        let known = KnownFingerprint::of(committed.fingerprint);

        git(&root, &["tag", "v1"]);
        let tagged = changed(&root, &request_for(context.clone(), known), &syntax);
        assert!(tagged.history, "a new ref refreshes history");
        assert!(
            tagged.changes.is_none(),
            "a new ref leaves local changes alone"
        );
        let known = KnownFingerprint::of(tagged.fingerprint);

        let mut commit_context = context;
        commit_context.diff_target = DiffTarget::CommitAgainstParent {
            commit: history[0].sha.clone(),
            parent: history[0].parents.first().cloned(),
        };
        git(&root, &["tag", "v2"]);
        let commit_diff = changed(&root, &request_for(commit_context, known), &syntax);
        assert!(
            commit_diff.changes.is_some(),
            "an open commit diff is re-read when refs move"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_repository_is_unchanged_unless_the_graph_awaits_its_first_history() {
        let root = committed_repo("refresh-first-load");
        let syntax = SyntaxHighlighter::new();
        let context = context_for(&root);
        let first = changed(
            &root,
            &request_for(context.clone(), KnownFingerprint::default()),
            &syntax,
        );
        let known = KnownFingerprint::of(first.fingerprint);

        let mut awaiting = context.clone();
        awaiting.history_loaded = false;
        let loaded = changed(&root, &request_for(awaiting, known), &syntax);
        assert!(!loaded.history, "opening Graph starts its own session");
        assert!(loaded.changes.is_none());

        let mut uninterested = context;
        uninterested.history_loaded = false;
        uninterested.wants_history = false;
        let result =
            refresh_active_repository(&root, &request_for(uninterested, known), &syntax, &|| false)
                .unwrap();
        assert!(matches!(
            result,
            ActiveRefresh::Unchanged { fingerprint } if fingerprint == first.fingerprint
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_diff_text_and_folds_skip_highlighting_but_a_new_fold_set_does_not() {
        let root = committed_repo("refresh-highlight");
        fs::write(root.join("tracked.txt"), "base\nedited\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let syntax = SyntaxHighlighter::new();
        let context = context_for(&root);

        let first = refresh_changes(&repository, &context, &[], None, &syntax, &|| false).unwrap();
        let key = first.highlighted.expect("first read highlights").key;

        let repeat =
            refresh_changes(&repository, &context, &[], Some(key), &syntax, &|| false).unwrap();
        assert!(repeat.highlighted.is_none());
        assert_eq!(repeat.diff_text, first.diff_text);

        let mut folded = context;
        folded.fold_toggles.insert(FoldKey {
            kind: FoldKind::Context,
            old_start: 1,
            old_end: 1,
            new_start: 1,
            new_end: 1,
        });
        let refolded =
            refresh_changes(&repository, &folded, &[], Some(key), &syntax, &|| false).unwrap();
        let highlighted = refolded.highlighted.expect("a new fold set re-highlights");
        assert_ne!(highlighted.key, key);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_same_placeholder_text_for_another_file_re_highlights() {
        let root = committed_repo("refresh-highlight-path");
        let repository = Repository::discover(&root).unwrap();
        let syntax = SyntaxHighlighter::new();
        let changes = ["a.txt", "b.txt"].map(|path| WorkingChange {
            section: ChangeSection::Commit,
            status: "M".to_owned(),
            path: path.to_owned(),
        });
        let select = |path: &str| {
            let mut context = context_for(&root);
            context.diff_target = DiffTarget::CommitAgainstParent {
                commit: "HEAD".to_owned(),
                parent: None,
            };
            context.selected_change = Some(TreeSelectionKey {
                section: ChangeSection::Commit,
                path: path.to_owned(),
            });
            context
        };

        let first = refresh_changes(
            &repository,
            &select("a.txt"),
            &changes,
            None,
            &syntax,
            &|| false,
        )
        .unwrap();
        assert_eq!(first.diff_text, "No diff for this file and target.");
        let key = first.highlighted.expect("first read highlights").key;

        let other = refresh_changes(
            &repository,
            &select("b.txt"),
            &changes,
            Some(key),
            &syntax,
            &|| false,
        )
        .unwrap();
        assert_eq!(other.diff_text, first.diff_text);
        let highlighted = other
            .highlighted
            .expect("the same text for another file re-highlights");
        assert_ne!(highlighted.key, key);

        let repeat = refresh_changes(
            &repository,
            &select("b.txt"),
            &changes,
            Some(highlighted.key),
            &syntax,
            &|| false,
        )
        .unwrap();
        assert!(repeat.highlighted.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_worktrees_are_reused_after_one_status_read_each() {
        let root = committed_repo("inspect-reuse");
        let linked = root.with_file_name(format!(
            "{}-linked",
            root.file_name().unwrap().to_string_lossy()
        ));
        git(
            &root,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        let linked = fs::canonicalize(&linked).unwrap();
        let mut context = context_for(&root);
        context.active_repository = None;
        context.project_roots = vec![root.clone()];
        context.expanded_projects = vec![root.clone()];
        let previous_of = |inspection: &super::RepositoryInspection| {
            let mut workspaces = WorkspacesState::new(ProjectRegistry::load(None).unwrap());
            workspaces.project_statuses = inspection.0.clone();
            workspaces.repository_rows = inspection.1.clone();
            workspaces.inspected_worktrees()
        };

        let first =
            inspect_repository_rows_cancellable(&context, &InspectedWorktrees::default(), &|| {
                false
            })
            .unwrap();
        assert_eq!(first.1.len(), 3);
        assert!(matches!(first.1[0].kind, RepositoryRowKind::Current));
        assert_eq!(first.1[0].path, root);
        assert_eq!(first.1[1].worktree_count, 1);
        assert_eq!(first.1[2].path, linked);
        assert_eq!(first.1[0].fingerprint, first.1[1].fingerprint);
        assert!(first.1[2].fingerprint.is_some());

        let previous = previous_of(&first);
        let before = git_processes_started();
        let second = inspect_repository_rows_cancellable(&context, &previous, &|| false).unwrap();
        assert_eq!(
            git_processes_started() - before,
            3,
            "one status read per worktree plus one worktree list per Project"
        );
        assert_eq!(second, first);

        fs::write(linked.join("tracked.txt"), "base\nfeature\n").unwrap();
        git(&linked, &["commit", "-am", "Feature"]);
        let previous = previous_of(&second);
        let before = git_processes_started();
        let third = inspect_repository_rows_cancellable(&context, &previous, &|| false).unwrap();
        assert_eq!(
            git_processes_started() - before,
            3,
            "a moved but clean worktree needs no numstat"
        );
        assert_eq!(third.1[..2], first.1[..2]);
        assert_ne!(third.1[2].fingerprint, first.1[2].fingerprint);
        assert_eq!(
            third.1[2].branch_status.as_ref().unwrap().checked_out,
            "feature"
        );

        fs::write(linked.join("tracked.txt"), "base\nfeature\nmore\n").unwrap();
        let previous = previous_of(&third);
        let before = git_processes_started();
        let fourth = inspect_repository_rows_cancellable(&context, &previous, &|| false).unwrap();
        assert_eq!(
            git_processes_started() - before,
            5,
            "a dirty worktree without untracked files adds only two numstat reads"
        );
        assert_eq!(fourth.1[..2], first.1[..2]);
        assert_eq!(
            fourth.1[2].overview,
            Some(ChangeOverview {
                changed_paths: 1,
                additions: 1,
                deletions: 0,
            })
        );

        fs::write(linked.join("tracked.txt"), "base\nfeature\nmore\nagain\n").unwrap();
        let previous = previous_of(&fourth);
        let before = git_processes_started();
        let fifth = inspect_repository_rows_cancellable(&context, &previous, &|| false).unwrap();
        assert_eq!(
            git_processes_started() - before,
            5,
            "a dirty worktree re-reads its totals while its status output stands still"
        );
        assert_eq!(fifth.1[..2], first.1[..2]);
        assert_eq!(fifth.1[2].fingerprint, fourth.1[2].fingerprint);
        assert_eq!(
            fifth.1[2].overview,
            Some(ChangeOverview {
                changed_paths: 1,
                additions: 2,
                deletions: 0,
            })
        );

        fs::remove_dir_all(linked).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_targets_use_one_git_read_at_the_known_root() {
        let root = committed_repo("branch-read-count");
        let before = git_processes_started();
        let result = super::run_foreground_job(
            ForegroundRequest::BranchTargets {
                id: RequestId::FIRST,
                path: root.clone(),
                command: CommandId::CheckoutCommit,
            },
            &super::SharedSyntax::default(),
            &super::ReadCancellations::default(),
        );
        assert!(
            matches!(result, super::ForegroundResult::BranchTargets { result: Ok(ref targets), .. } if !targets.is_empty())
        );
        assert_eq!(git_processes_started() - before, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_and_mutations_route_to_their_own_lane() {
        let id = RequestId::FIRST;
        let generation = ReadGeneration::ZERO;
        let path = PathBuf::from("/repo");
        let commit = Commit {
            sha: "0123456789abcdef".to_owned(),
            parents: Vec::new(),
            author_name: String::new(),
            author_email: String::new(),
            author_time: String::new(),
            refs: Vec::new(),
            subject: String::new(),
            body: String::new(),
            graph: crate::git::GraphPrefix::plain(""),
        };
        let agent = HerdrAgent {
            pane_id: "pane-1".to_owned(),
            label: "Agent".to_owned(),
            cwd: "/repo".to_owned(),
            ..Default::default()
        };
        let reads = [
            ForegroundRequest::BranchTargets {
                id,
                path: path.clone(),
                command: CommandId::CheckoutCommit,
            },
            ForegroundRequest::Diff {
                id,
                generation,
                owner: DiffOwner::Changes,
                path: path.clone(),
                file: "a.txt".to_owned(),
                target: DiffTarget::WorkingTreeAgainstIndex,
                fold_toggles: HashSet::new(),
            },
            ForegroundRequest::CommitDetails {
                id,
                generation,
                target: CommitDetailsReadTarget {
                    path: path.clone(),
                    commit,
                },
            },
            ForegroundRequest::Blame {
                id,
                generation,
                path: path.clone(),
                file: "a.txt".to_owned(),
                line: 1,
                revision: None,
            },
            ForegroundRequest::Switch {
                id,
                generation,
                path: path.clone(),
            },
        ];
        let mutations = [
            ForegroundRequest::Operation {
                id,
                path: path.clone(),
                command: CommandId::Fetch,
                operation: GitOperation::Fetch,
            },
            ForegroundRequest::ResetContext {
                id,
                path: path.clone(),
            },
            ForegroundRequest::LoadAmendMessage {
                id,
                path: path.clone(),
            },
            ForegroundRequest::ListAgentPanes {
                id,
                path: path.clone(),
            },
            ForegroundRequest::SendAgentRequest {
                id,
                path: path.clone(),
                agent,
                prompt: String::new(),
                amend: false,
            },
            ForegroundRequest::ValidateName {
                id,
                path: path.clone(),
                command: CommandId::CreateTag,
                name: "v1".to_owned(),
            },
            ForegroundRequest::Staging {
                id,
                path: path.clone(),
                operation: GitOperation::StageAll,
            },
            ForegroundRequest::AddProject {
                id,
                registry: ProjectRegistry::load(None).unwrap(),
            },
            ForegroundRequest::RemoveProject {
                id,
                registry: ProjectRegistry::load(None).unwrap(),
                root: path.clone(),
            },
            ForegroundRequest::Fetch {
                id,
                roots: vec![path],
            },
        ];

        for request in &reads {
            assert_eq!(request.lane(), Lane::Read, "{request:?}");
        }
        for request in &mutations {
            assert_eq!(request.lane(), Lane::Mutation, "{request:?}");
        }
    }
}
