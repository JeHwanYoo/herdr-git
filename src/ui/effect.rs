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
use super::path_tree::PathTree;
use super::syntax::{DiffDocument, FoldKey, SyntaxHighlighter};
use super::workspaces::{RepositoryRow, RepositoryRowKind, repository_rows_from_statuses};
pub(super) use foreground::start_foreground_workers;
#[cfg(test)]
pub(super) use foreground::{load_repository_snapshot, run_foreground_job};
#[cfg(test)]
pub(super) use refresh::run_refresh_job;
pub(super) use refresh::start_refresh_worker;

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
    pub(super) repository_files: Arc<AtomicU64>,
    pub(super) file_preview: Arc<AtomicU64>,
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
pub(super) struct RepositoryFiles {
    pub(super) paths: Vec<String>,
    pub(super) tree: PathTree,
}

#[derive(Debug, Default)]
pub(super) struct FilePreview {
    pub(super) document: DiffDocument,
    pub(super) notice: Option<String>,
}

impl FilePreview {
    pub(super) fn notice(text: impl Into<String>) -> Self {
        Self {
            document: DiffDocument::default(),
            notice: Some(text.into()),
        }
    }
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
    RepositoryFiles {
        id: RequestId,
        generation: ReadGeneration,
        root: PathBuf,
    },
    FilePreview {
        id: RequestId,
        generation: ReadGeneration,
        root: PathBuf,
        file: String,
        new_file: bool,
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
            | Self::RepositoryFiles { .. }
            | Self::FilePreview { .. }
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
    RepositoryFiles {
        id: RequestId,
        generation: ReadGeneration,
        root: PathBuf,
        result: Result<RepositoryFiles, ReadError>,
    },
    FilePreview {
        id: RequestId,
        generation: ReadGeneration,
        root: PathBuf,
        file: String,
        result: Result<FilePreview, ReadError>,
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
    use std::path::PathBuf;

    use super::{
        CommitDetailsReadTarget, DiffOwner, ForegroundRequest, Lane, ReadGeneration, RequestId,
    };
    use crate::git::{Commit, DiffTarget, GitOperation};
    use crate::herdr::HerdrAgent;
    use crate::project::ProjectRegistry;
    use crate::ui::commands::CommandId;

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
