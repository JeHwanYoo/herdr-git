use std::time::{SystemTime, UNIX_EPOCH};

use super::parse::{parse_iso_time, relative_time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitRefKind {
    Head,
    LocalBranch,
    RemoteBranch,
    RemoteHead,
    Tag,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRef {
    pub name: String,
    pub kind: CommitRefKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: String,
    pub refs: Vec<CommitRef>,
    pub subject: String,
    pub body: String,
}

impl Commit {
    pub fn uncommitted(parents: Vec<String>) -> Self {
        Self {
            sha: String::new(),
            parents,
            author_name: String::new(),
            author_email: String::new(),
            author_time: String::new(),
            refs: Vec::new(),
            subject: "Uncommitted".to_owned(),
            body: String::new(),
        }
    }

    pub fn is_uncommitted(&self) -> bool {
        self.sha.is_empty()
    }

    pub fn author_relative(&self, now: SystemTime) -> String {
        let Some(author_time) = parse_iso_time(&self.author_time) else {
            return String::new();
        };
        let now = now.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
        relative_time(now - author_time)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineHistoryCommit {
    pub sha: String,
    pub author: String,
    pub author_time: i64,
    pub summary: String,
    pub patch: String,
}

impl LineHistoryCommit {
    pub fn author_relative(&self, now: SystemTime) -> String {
        let now = now.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
        relative_time(now - self.author_time)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    pub status: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeSection {
    Working,
    Commit,
    Staged,
    Unstaged,
}

impl ChangeSection {
    pub(crate) fn diff_target(self) -> Option<DiffTarget> {
        match self {
            Self::Working | Self::Commit => None,
            Self::Staged => Some(DiffTarget::IndexAgainstHead),
            Self::Unstaged => Some(DiffTarget::WorkingTreeAgainstIndex),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingChange {
    pub section: ChangeSection,
    pub status: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffSummary {
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeOverview {
    pub changed_paths: usize,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchStatus {
    pub checked_out: String,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeReport {
    pub branch_status: BranchStatus,
    pub changed_paths: usize,
    pub has_untracked: bool,
    pub fingerprint: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepositoryFingerprint {
    pub refs: u64,
    pub worktree: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetTarget {
    pub name: String,
    pub reference: String,
    pub commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetContext {
    pub current_branch: String,
    pub current_head: String,
    pub targets: Vec<ResetTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIdentity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ChangesComparison {
    #[default]
    Last,
    Between {
        base: String,
        compare: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffTarget {
    WorkingTreeAgainstRevision {
        base: String,
    },
    WorkingTreeAgainstIndex,
    IndexAgainstHead,
    CommitAgainstParent {
        commit: String,
        parent: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetails {
    pub commit: Commit,
    pub changes: Vec<ChangedPath>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameInfo {
    pub sha: String,
    pub author: String,
    pub author_email: String,
    pub author_time: i64,
    pub summary: String,
    pub line: usize,
}

#[cfg(test)]
mod tests {
    #[test]
    fn changed_file_section_selects_its_git_comparison() {
        assert_eq!(
            crate::git::ChangeSection::Staged.diff_target(),
            Some(crate::git::DiffTarget::IndexAgainstHead)
        );
        assert_eq!(
            crate::git::ChangeSection::Unstaged.diff_target(),
            Some(crate::git::DiffTarget::WorkingTreeAgainstIndex)
        );
    }
}
