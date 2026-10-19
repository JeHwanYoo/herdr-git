use std::io::Read;
use std::path::{Path, PathBuf};
use std::{collections::hash_map::DefaultHasher, hash::Hash, hash::Hasher};
use std::{fs, io};

mod changes;
mod history;
mod model;
mod operation;
mod parse;
mod process;
mod rebase;
mod refs;
pub use history::{HISTORY_PAGE_SIZE, HistoryPage, HistorySession};
pub use model::{
    BlameInfo, BranchStatus, ChangeOverview, ChangeSection, ChangedPath, ChangesComparison, Commit,
    CommitDetails, CommitRef, CommitRefKind, DiffSummary, DiffTarget, LineChange,
    LineHistoryCommit, LocalIdentity, RepositoryFingerprint, ResetContext, ResetTarget,
    WorkingChange, WorktreeReport,
};
pub use operation::{GitOperation, ResetMode};
use parse::{
    parse_blame, parse_blame_range, parse_changes, parse_line_changes, parse_line_history,
    parse_numstat, parse_working_changes, parse_worktree_report,
};
#[cfg(test)]
pub(crate) use process::git_processes_started;
pub use process::{
    GIT_READ_CANCELLED, ReadError, with_read_cancellation, with_read_cancellation_result,
};
use process::{git, git_difference, git_owned_output, read_cancelled};
pub use rebase::{RebaseAction, RebasePlan, RebaseResult, RebaseTask};
pub use refs::{GitCommandFacts, Remote};

#[derive(Debug)]
pub struct Repository {
    root: PathBuf,
}

impl Repository {
    pub fn discover(path: &Path) -> Result<Self, String> {
        let output = git(path, &["rev-parse", "--show-toplevel"])?;
        Ok(Self {
            root: PathBuf::from(output.trim_end()),
        })
    }

    pub fn at_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn project_root(&self) -> Result<PathBuf, String> {
        self.worktree_paths()?
            .into_iter()
            .next()
            .ok_or_else(|| "Git returned no worktrees".to_owned())
    }

    pub fn worktree_paths(&self) -> Result<Vec<PathBuf>, String> {
        let output = git(&self.root, &["worktree", "list", "--porcelain", "-z"])?;
        Ok(output
            .split('\0')
            .filter_map(|field| field.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect())
    }

    pub fn local_identity(&self) -> Option<LocalIdentity> {
        let name = git(&self.root, &["config", "--get", "user.name"]).ok()?;
        let email = git(&self.root, &["config", "--get", "user.email"]).ok()?;
        Some(LocalIdentity {
            name: name.trim().to_owned(),
            email: email.trim().to_owned(),
        })
        .filter(|identity| !identity.name.is_empty() && !identity.email.is_empty())
    }

    pub fn head_commit_message(&self) -> Result<String, String> {
        git(&self.root, &["log", "-1", "--format=%B", "HEAD"])
            .map(|message| message.trim_end_matches(['\r', '\n']).to_owned())
    }

    #[cfg(test)]
    pub fn history(&self) -> Result<Vec<Commit>, String> {
        self.history_session()?
            .next_page(HISTORY_PAGE_SIZE)
            .map(|page| page.commits)
    }
}

pub use changes::worktree_report;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        ChangeSection, DiffTarget, GitOperation, LineChange, Repository, ResetMode, worktree_report,
    };

    #[test]
    fn main_last_keeps_its_branch_boundary_after_clone_and_reflog_expiry() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-main-parent-{unique}"));
        let cloned = root.with_extension("clone");
        fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("file.txt"), "base\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "First"]);
        let repo = Repository::discover(&root).unwrap();
        let first = repo.resolve_comparison_commit("HEAD").unwrap();
        fs::write(root.join("file.txt"), "base\none\ntwo\n").unwrap();
        run(&root, &["commit", "-am", "Latest"]);
        run(
            &root,
            &["clone", root.to_str().unwrap(), cloned.to_str().unwrap()],
        );
        let repo = Repository::discover(&cloned).unwrap();
        let last = super::ChangesComparison::Last;
        for expire in [false, true] {
            if expire {
                run(&cloned, &["reflog", "expire", "--expire=all", "--all"]);
            }
            let target = repo.changes_comparison(&last).unwrap();
            assert!(
                matches!(&target, DiffTarget::CommitAgainstParent { parent: Some(base), .. } if base == &first)
            );
            assert_eq!(repo.diff_summary(&target).unwrap().additions, 2);
            let titles = repo.comparison_titles(&target).unwrap();
            assert!(titles.contains(&(first.clone(), "First".into())));
            assert!(titles.contains(&(
                repo.resolve_comparison_commit("HEAD").unwrap(),
                "Latest".into()
            )));
        }
        fs::write(cloned.join("file.txt"), "base\none\ntwo\nworking\n").unwrap();
        let target = repo.changes_comparison(&last).unwrap();
        assert_eq!(
            target,
            DiffTarget::WorkingTreeAgainstRevision {
                base: repo.resolve_comparison_commit("HEAD").unwrap()
            }
        );
        assert_eq!(repo.diff_summary(&target).unwrap().additions, 1);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(cloned).unwrap();
    }

    #[test]
    fn files_lists_tracked_and_untracked_paths_without_ignored_ones() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-repository-files-{unique}"));
        fs::create_dir_all(root.join("src")).unwrap();
        run(&root, &["init", "-b", "main"]);
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        run(&root, &["add", "."]);
        fs::write(root.join("notes.md"), "draft\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/output"), "build\n").unwrap();
        let files = Repository::discover(&root).unwrap().files().unwrap();
        assert_eq!(files, [".gitignore", "notes.md", "src/main.rs"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn line_changes_place_removed_lines_and_count_added_ones_against_head() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-line-changes-{unique}"));
        fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("file.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "base"]);
        fs::write(root.join("file.txt"), "one\nTWO\nthree\nextra\n").unwrap();
        let changes = Repository::discover(&root)
            .unwrap()
            .line_changes("file.txt")
            .unwrap();
        assert_eq!(
            changes,
            [
                LineChange {
                    old_start: 2,
                    removed: vec!["two".to_owned()],
                    new_start: 2,
                    added: 1,
                },
                LineChange {
                    old_start: 4,
                    removed: vec!["four".to_owned()],
                    new_start: 4,
                    added: 1,
                },
            ]
        );
        fs::write(root.join("file.txt"), "one\nthree\nfour\nfive\n").unwrap();
        let changes = Repository::discover(&root)
            .unwrap()
            .line_changes("file.txt")
            .unwrap();
        assert_eq!(
            changes,
            [
                LineChange {
                    old_start: 2,
                    removed: vec!["two".to_owned()],
                    new_start: 2,
                    added: 0,
                },
                LineChange {
                    old_start: 5,
                    removed: Vec::new(),
                    new_start: 4,
                    added: 1,
                },
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_last_branch_boundary_and_working_tip() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-comparison-modes-{unique}"));
        fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("file.txt"), "base\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "Base"]);
        run(&root, &["commit", "--allow-empty", "-m", "Main advances"]);
        run(&root, &["checkout", "-b", "feature"]);
        let repo = Repository::discover(&root).unwrap();
        let last = super::ChangesComparison::Last;
        let target = repo.changes_comparison(&last).unwrap();
        assert!(repo.comparison_changes(&target).unwrap().is_empty());
        fs::write(root.join("file.txt"), "base\none\n").unwrap();
        run(&root, &["commit", "-am", "One"]);
        fs::write(root.join("file.txt"), "base\none\ntwo\n").unwrap();
        run(&root, &["commit", "-am", "Two"]);
        assert_eq!(
            repo.diff_summary(&repo.changes_comparison(&last).unwrap())
                .unwrap()
                .additions,
            1
        );
        fs::write(root.join("file.txt"), "base\none\ntwo\nstaged\n").unwrap();
        run(&root, &["add", "."]);
        fs::write(root.join("file.txt"), "base\none\ntwo\nstaged\nunstaged\n").unwrap();
        fs::write(root.join("new.txt"), "untracked\n").unwrap();
        let before = repo.refresh_fingerprint().unwrap();
        let target = repo.changes_comparison(&last).unwrap();
        let patch = repo
            .diff_with_context(&target, Some("file.txt"), None)
            .unwrap();
        assert!(patch.contains("+staged") && patch.contains("+unstaged"));
        assert_eq!(repo.comparison_changes(&target).unwrap().len(), 2);
        assert_eq!(repo.diff_summary(&target).unwrap().additions, 3);
        assert_eq!(before, repo.refresh_fingerprint().unwrap());
        run(&root, &["reflog", "expire", "--expire=all", "--all"]);
        assert_eq!(
            repo.diff_summary(&repo.changes_comparison(&last).unwrap())
                .unwrap()
                .additions,
            3,
            "a missing reflog leaves Last on the first parent"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comparison_between_resolves_branches_and_rejects_invalid_revisions() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-comparison-{unique}"));
        fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "before\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "Base"]);
        run(&root, &["branch", "base"]);
        fs::write(root.join("tracked.txt"), "after\n").unwrap();
        run(&root, &["commit", "-am", "Compare"]);
        let repository = Repository::discover(&root).unwrap();
        let target = repository.comparison_between("base", "HEAD").unwrap();
        let changes = repository.comparison_changes(&target).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "tracked.txt");
        let patch = repository
            .diff_with_context(&target, Some("tracked.txt"), None)
            .unwrap();
        assert!(patch.contains("-before") && patch.contains("+after"));
        assert_eq!(repository.diff_summary(&target).unwrap().additions, 1);
        assert!(repository.comparison_between("--help", "HEAD").is_err());
        assert!(repository.comparison_between("base", "missing").is_err());
        assert!(
            repository
                .comparison_changes(&repository.comparison_between("HEAD", "HEAD").unwrap())
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_history_and_details_from_git() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("README.md"), "first\n").unwrap();
        run(&root, &["add", "README.md"]);
        run(&root, &["commit", "-m", "Initial commit"]);
        fs::write(root.join("README.md"), "first\nsecond\n").unwrap();
        run(&root, &["commit", "-am", "Add second line"]);

        let repository = Repository::discover(&root).unwrap();
        let identity = repository.local_identity().unwrap();
        let history = repository.history().unwrap();
        let details = repository.details(&history[0]).unwrap();

        assert_eq!(history.len(), 2);
        assert_eq!(identity.name, "Test Author");
        assert_eq!(identity.email, "test@example.com");
        assert_eq!(history[0].subject, "Add second line");
        assert_eq!(history[0].author_name, "Test Author");
        assert_eq!(history[0].parents.len(), 1);
        assert_eq!(details.changes[0].path, "README.md");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_fingerprint_changes_with_worktree_index_and_refs() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-refresh-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        let repository = Repository::discover(&root).unwrap();

        let clean = repository.refresh_fingerprint().unwrap();
        fs::write(root.join("tracked.txt"), "base\nchanged\n").unwrap();
        let dirty = repository.refresh_fingerprint().unwrap();
        assert_ne!(clean.worktree, dirty.worktree);
        assert_eq!(clean.refs, dirty.refs);

        run(&root, &["add", "tracked.txt"]);
        let staged = repository.refresh_fingerprint().unwrap();
        assert_ne!(dirty.worktree, staged.worktree);
        assert_eq!(dirty.refs, staged.refs);

        run(&root, &["commit", "-m", "Change"]);
        let committed = repository.refresh_fingerprint().unwrap();
        assert_ne!(staged.refs, committed.refs);
        assert_ne!(staged.worktree, committed.worktree);

        run(&root, &["tag", "v1"]);
        let tagged = repository.refresh_fingerprint().unwrap();
        assert_ne!(committed.refs, tagged.refs);
        assert_eq!(committed.worktree, tagged.worktree);

        run(&root, &["branch", "twin"]);
        let branched = repository.refresh_fingerprint().unwrap();
        run(&root, &["checkout", "twin"]);
        let switched = repository.refresh_fingerprint().unwrap();
        assert_ne!(
            branched.refs, switched.refs,
            "moving HEAD to a branch at the same commit is a refs change"
        );
        assert_eq!(branched.worktree, switched.worktree);

        fs::create_dir(root.join("fresh")).unwrap();
        fs::write(root.join("fresh/one.txt"), "one\n").unwrap();
        let one_untracked = repository.refresh_fingerprint().unwrap();
        fs::write(root.join("fresh/two.txt"), "two\n").unwrap();
        let two_untracked = repository.refresh_fingerprint().unwrap();
        assert_ne!(one_untracked.worktree, two_untracked.worktree);
        assert_eq!(one_untracked.refs, two_untracked.refs);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stages_and_unstages_paths_and_all_without_changing_file_contents() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-stage-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nchanged\n").unwrap();
        fs::write(root.join("new file.txt"), "new\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let changes = repository.working_changes().unwrap();
        assert!(changes.iter().any(|change| {
            change.section == ChangeSection::Unstaged
                && change.status == "??"
                && change.path == "new file.txt"
        }));

        repository
            .execute(&GitOperation::StagePath("new file.txt".to_owned()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("new file.txt")).unwrap(),
            "new\n"
        );
        assert!(repository.working_changes().unwrap().iter().any(|change| {
            change.section == ChangeSection::Staged && change.path == "new file.txt"
        }));

        repository.execute(&GitOperation::StageAll).unwrap();
        assert!(
            repository
                .working_changes()
                .unwrap()
                .iter()
                .all(|change| change.section == ChangeSection::Staged)
        );
        repository
            .execute(&GitOperation::UnstagePath("tracked.txt".to_owned()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\nchanged\n"
        );
        assert!(repository.working_changes().unwrap().iter().any(|change| {
            change.section == ChangeSection::Unstaged && change.path == "tracked.txt"
        }));

        repository.execute(&GitOperation::UnstageAll).unwrap();
        let changes = repository.working_changes().unwrap();
        assert!(
            !changes
                .iter()
                .any(|change| change.section == ChangeSection::Staged)
        );
        assert!(changes.iter().any(|change| change.path == "new file.txt"));
        assert_eq!(
            fs::read_to_string(root.join("new file.txt")).unwrap(),
            "new\n"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unstages_an_unborn_index_without_removing_worktree_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-unborn-stage-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        fs::write(root.join("first.txt"), "first\n").unwrap();
        fs::write(root.join("second.txt"), "second\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        repository
            .execute(&GitOperation::StagePaths(vec![
                "first.txt".to_owned(),
                "second.txt".to_owned(),
            ]))
            .unwrap();
        repository
            .execute(&GitOperation::UnstagePaths(vec![
                "first.txt".to_owned(),
                "second.txt".to_owned(),
            ]))
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("first.txt")).unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("second.txt")).unwrap(),
            "second\n"
        );
        assert!(repository.working_changes().unwrap().iter().any(|change| {
            change.section == ChangeSection::Unstaged
                && change.status == "??"
                && change.path == "first.txt"
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stages_and_unstages_selected_paths_in_one_operation() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-batch-stage-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        for path in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(path), "base\n").unwrap();
        }
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "Base"]);
        for path in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(path), format!("base\n{path}\n")).unwrap();
        }

        let repository = Repository::discover(&root).unwrap();
        repository
            .execute(&GitOperation::StagePaths(vec![
                "a.txt".to_owned(),
                "c.txt".to_owned(),
            ]))
            .unwrap();
        let changes = repository.working_changes().unwrap();
        for path in ["a.txt", "c.txt"] {
            assert!(
                changes.iter().any(|change| {
                    change.path == path && change.section == ChangeSection::Staged
                })
            );
        }
        assert!(
            changes.iter().any(|change| {
                change.path == "b.txt" && change.section == ChangeSection::Unstaged
            })
        );

        repository
            .execute(&GitOperation::UnstagePaths(vec![
                "a.txt".to_owned(),
                "c.txt".to_owned(),
            ]))
            .unwrap();
        assert!(
            repository
                .working_changes()
                .unwrap()
                .iter()
                .all(|change| change.section == ChangeSection::Unstaged)
        );
        for path in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                fs::read_to_string(root.join(path)).unwrap(),
                format!("base\n{path}\n")
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diff_targets_do_not_change_repository_state() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-diff-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nstaged\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        fs::write(root.join("tracked.txt"), "base\nstaged\nunstaged\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let before = repository.status_and_refs().unwrap();
        let changes = repository.working_changes().unwrap();
        let unstaged = repository
            .diff_with_context(
                &DiffTarget::WorkingTreeAgainstIndex,
                Some("tracked.txt"),
                None,
            )
            .unwrap();
        let staged = repository
            .diff_with_context(&DiffTarget::IndexAgainstHead, Some("tracked.txt"), None)
            .unwrap();
        let after = repository.status_and_refs().unwrap();
        let committed_blame = repository.blame("tracked.txt", 1, Some("HEAD")).unwrap();
        let working_blame = repository.blame("tracked.txt", 3, None).unwrap();
        let range_blame = repository.blame_range("tracked.txt", 1, 3, None).unwrap();
        let history = repository
            .line_history("tracked.txt", &[(1, 1)], Some("HEAD"))
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].summary, "Base");
        assert!(history[0].patch.contains("+base"));
        assert_eq!(
            range_blame
                .iter()
                .map(|blame| blame.line)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(range_blame[0].summary, "Base");
        assert!(range_blame[2].sha.chars().all(|character| character == '0'));

        assert!(
            changes
                .iter()
                .any(|item| item.section == ChangeSection::Staged)
        );
        assert!(
            changes
                .iter()
                .any(|item| item.section == ChangeSection::Unstaged)
        );
        assert!(unstaged.contains("+unstaged"));
        assert!(staged.contains("+staged"));
        assert_eq!(before, after);
        assert_eq!(committed_blame.author, "Test Author");
        assert_eq!(working_blame.line, 3);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovers_linked_worktrees_and_normalizes_the_project_root() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-project-{unique}"));
        let linked = std::env::temp_dir().join(format!("herdr-git-worktree-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        run(
            &root,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );

        let project = Repository::discover(&root).unwrap();
        let worktree = Repository::discover(&linked).unwrap();
        assert_eq!(worktree.project_root().unwrap(), project.root());
        assert_eq!(
            worktree.worktree_paths().unwrap(),
            [project.root(), worktree.root()]
        );

        fs::remove_dir_all(linked).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compares_the_checked_out_branch_with_its_configured_upstream() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-branch-status-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        run(&root, &["add", "base.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        run(&root, &["remote", "add", "upstream", "."]);
        run(&root, &["checkout", "-b", "feature"]);
        fs::write(root.join("feature.txt"), "feature\n").unwrap();
        run(&root, &["add", "feature.txt"]);
        run(&root, &["commit", "-m", "Feature"]);
        let feature = git_output(&root, &["rev-parse", "HEAD"]);
        run(&root, &["checkout", "main"]);
        fs::write(root.join("main.txt"), "main\n").unwrap();
        run(&root, &["add", "main.txt"]);
        run(&root, &["commit", "-m", "Main"]);
        let main = git_output(&root, &["rev-parse", "HEAD"]);
        run(&root, &["config", "branch.feature.remote", "upstream"]);
        run(
            &root,
            &["config", "branch.feature.merge", "refs/heads/main"],
        );
        run(&root, &["update-ref", "refs/remotes/upstream/main", &main]);
        run(
            &root,
            &["update-ref", "refs/remotes/upstream/staging", &feature],
        );
        run(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/upstream/HEAD",
                "refs/remotes/upstream/staging",
            ],
        );
        run(&root, &["checkout", "feature"]);

        let status = worktree_report(&root).unwrap().branch_status;

        assert_eq!(status.checked_out, "feature");
        assert_eq!(status.upstream.as_deref(), Some("upstream/main"));
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 1);

        run(&root, &["checkout", "--detach"]);
        let detached = worktree_report(&root).unwrap().branch_status;
        assert_eq!(detached.checked_out, format!("Detached {}", &feature[..8]));
        assert_eq!(detached.upstream, None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn summarizes_unique_changed_paths_and_both_index_groups() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-overview-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nstaged\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        fs::write(root.join("tracked.txt"), "base\nstaged\nunstaged\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();

        fs::create_dir(root.join("sub")).unwrap();
        let report = worktree_report(&root.join("sub")).unwrap();
        assert_eq!(report, worktree_report(&root).unwrap());
        let overview = Repository::discover(&root)
            .unwrap()
            .change_overview(&report)
            .unwrap();
        assert_eq!(overview.changed_paths, 2);
        assert_eq!(overview.additions, 3);
        assert_eq!(overview.deletions, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renders_an_untracked_file_as_an_empty_file_diff_without_mutation() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-untracked-diff-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("new file.txt"), "first\nsecond\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let before = repository.status_and_refs().unwrap();
        let diff = repository
            .diff_with_context(
                &DiffTarget::WorkingTreeAgainstIndex,
                Some("new file.txt"),
                None,
            )
            .unwrap();
        let after = repository.status_and_refs().unwrap();

        assert!(diff.contains("new file mode"));
        assert!(diff.contains("--- /dev/null"));
        assert!(diff.contains("+first\n+second"));
        assert_eq!(before, after);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renders_an_untracked_symlink_to_a_directory_without_following_it() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-untracked-symlink-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        fs::create_dir_all(root.join("skills/spec")).unwrap();
        fs::write(root.join("skills/spec/SKILL.md"), "content\n").unwrap();
        std::os::unix::fs::symlink("../skills/spec", root.join("skills/link")).unwrap();

        let repository = Repository::discover(&root).unwrap();
        let diff = repository
            .diff_with_context(
                &DiffTarget::WorkingTreeAgainstIndex,
                Some("skills/link"),
                None,
            )
            .unwrap();
        assert!(diff.contains("new file mode 120000"), "{diff}");
        assert!(diff.contains("--- /dev/null"), "{diff}");
        assert!(diff.contains("+../skills/spec"), "{diff}");

        let summary = repository
            .diff_summary(&DiffTarget::WorkingTreeAgainstIndex)
            .unwrap();
        assert_eq!(summary.additions, 2);
        assert_eq!(summary.deletions, 0);
        assert_eq!(summary.files, 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn includes_untracked_files_in_the_working_tree_summary() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-untracked-summary-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        fs::write(root.join("notes.txt"), "first\nsecond\n").unwrap();
        fs::write(root.join("image.bin"), [0, 1, 2, 3]).unwrap();

        let repository = Repository::discover(&root).unwrap();
        let before = repository.status_and_refs().unwrap();
        let summary = repository
            .diff_summary(&DiffTarget::WorkingTreeAgainstIndex)
            .unwrap();
        let after = repository.status_and_refs().unwrap();

        assert_eq!(summary.files, 2);
        assert_eq!(summary.additions, 2);
        assert_eq!(summary.deletions, 0);
        assert_eq!(before, after);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fetches_from_a_local_remote() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("herdr-git-fetch-test-{unique}"));
        let remote = parent.join("remote.git");
        let source = parent.join("source");
        let clone = parent.join("clone");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir(&remote).unwrap();
        run(&remote, &["init", "--bare"]);
        fs::create_dir(&source).unwrap();
        run(&source, &["init", "-b", "main"]);
        run(&source, &["config", "user.name", "Test Author"]);
        run(&source, &["config", "user.email", "test@example.com"]);
        fs::write(source.join("README.md"), "remote\n").unwrap();
        run(&source, &["add", "README.md"]);
        run(&source, &["commit", "-m", "Remote commit"]);
        run(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&source, &["push", "-u", "origin", "main"]);
        let status = Command::new("git")
            .args(["clone", remote.to_str().unwrap(), clone.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());

        let repository = Repository::discover(&clone).unwrap();
        let context = repository.command_facts();
        assert!(context.has_remote);
        assert!(repository.execute(&GitOperation::Fetch).is_ok());
        assert!(repository.fetch_remote_refs().is_ok());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn executes_stash_cherry_pick_and_revert_without_a_shell() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-operation-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        run(&root, &["checkout", "-b", "feature"]);
        fs::write(root.join("feature.txt"), "feature\n").unwrap();
        run(&root, &["add", "feature.txt"]);
        run(&root, &["commit", "-m", "Feature"]);
        let feature_sha = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let feature_sha = String::from_utf8(feature_sha.stdout)
            .unwrap()
            .trim()
            .to_owned();
        run(&root, &["checkout", "main"]);

        let repository = Repository::discover(&root).unwrap();
        repository
            .execute(&GitOperation::CherryPick(feature_sha.clone()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("feature.txt")).unwrap(),
            "feature\n"
        );
        repository
            .execute(&GitOperation::Revert(feature_sha))
            .unwrap();
        assert!(!root.join("feature.txt").exists());

        fs::write(root.join("tracked.txt"), "base\ndirty\n").unwrap();
        repository.execute(&GitOperation::StashChanges).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\n"
        );
        repository.execute(&GitOperation::StashPop).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\ndirty\n"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_and_creates_named_refs_at_the_selected_commit() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-ref-test-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        run(&root, &["add", "README.md"]);
        run(&root, &["commit", "-m", "Base"]);
        let repository = Repository::discover(&root).unwrap();
        let sha = repository.history().unwrap()[0].sha.clone();

        assert!(repository.validate_branch_name("feature/topic").is_ok());
        assert!(repository.validate_branch_name("bad name").is_err());
        assert!(repository.validate_tag_name("v0.1.0").is_ok());
        assert!(repository.validate_tag_name("bad..tag").is_err());
        repository
            .execute(&GitOperation::CreateBranch {
                name: "feature/topic".to_owned(),
                commit: sha.clone(),
            })
            .unwrap();
        repository
            .execute(&GitOperation::CreateTag {
                name: "v0.1.0".to_owned(),
                commit: sha,
            })
            .unwrap();
        let (_, refs) = repository.status_and_refs().unwrap();
        assert!(refs.contains("refs/heads/feature/topic"));
        assert!(refs.contains("refs/tags/v0.1.0"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reset_context_lists_branch_targets_and_stale_guards_stop_execution() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-reset-context-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        run(&root, &["branch", "target", &base]);
        run(&root, &["update-ref", "refs/remotes/origin/main", &base]);
        run(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        fs::write(root.join("tracked.txt"), "next\n").unwrap();
        run(&root, &["commit", "-am", "Next"]);

        let repository = Repository::discover(&root).unwrap();
        let context = repository.reset_context().unwrap();
        assert_eq!(context.current_branch, "main");
        assert_eq!(
            context.current_head,
            git_output(&root, &["rev-parse", "HEAD"])
        );
        assert!(context.targets.iter().any(|target| target.name == "target"));
        assert!(
            context
                .targets
                .iter()
                .any(|target| target.name == "origin/main")
        );
        assert!(
            context
                .targets
                .iter()
                .all(|target| target.name != "origin/HEAD")
        );

        let captured_head = context.current_head.clone();
        fs::write(root.join("tracked.txt"), "later\n").unwrap();
        run(&root, &["commit", "-am", "Later"]);
        let later = git_output(&root, &["rev-parse", "HEAD"]);
        let error = repository
            .execute(&GitOperation::Reset {
                mode: ResetMode::Hard,
                target: base,
                target_name: "target".to_owned(),
                expected_branch: context.current_branch,
                expected_head: captured_head,
            })
            .unwrap_err();
        assert!(error.contains("branch or HEAD changed"));
        assert_eq!(git_output(&root, &["rev-parse", "HEAD"]), later);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn soft_mixed_and_hard_reset_have_exact_index_and_worktree_effects() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-reset-modes-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "Base"]);
        let base = git_output(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("tracked.txt"), "next\n").unwrap();
        run(&root, &["commit", "-am", "Next"]);
        let next = git_output(&root, &["rev-parse", "HEAD"]);
        run(&root, &["branch", "untouched", &next]);
        run(&root, &["update-ref", "refs/remotes/origin/main", &next]);
        let repository = Repository::discover(&root).unwrap();
        let operation = |mode, expected_head: &str| GitOperation::Reset {
            mode,
            target: base.clone(),
            target_name: "target".to_owned(),
            expected_branch: "main".to_owned(),
            expected_head: expected_head.to_owned(),
        };

        fs::write(root.join("tracked.txt"), "soft\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        repository
            .execute(&operation(ResetMode::Soft, &next))
            .unwrap();
        assert_eq!(git_output(&root, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "soft\n"
        );
        assert_eq!(git_output(&root, &["show", ":tracked.txt"]), "soft");
        assert_eq!(git_output(&root, &["rev-parse", "ORIG_HEAD"]), next);

        run(&root, &["reset", "--hard", &next]);
        fs::write(root.join("tracked.txt"), "mixed\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        repository
            .execute(&operation(ResetMode::Mixed, &next))
            .unwrap();
        assert_eq!(git_output(&root, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "mixed\n"
        );
        assert_eq!(git_output(&root, &["show", ":tracked.txt"]), "base");

        run(&root, &["reset", "--hard", &next]);
        fs::write(root.join("tracked.txt"), "dirty\n").unwrap();
        fs::write(root.join("untracked.txt"), "keep\n").unwrap();
        repository
            .execute(&operation(ResetMode::Hard, &next))
            .unwrap();
        assert_eq!(git_output(&root, &["rev-parse", "HEAD"]), base);
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "base\n"
        );
        assert_eq!(git_output(&root, &["show", ":tracked.txt"]), "base");
        assert_eq!(
            fs::read_to_string(root.join("untracked.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(git_output(&root, &["rev-parse", "untouched"]), next);
        assert_eq!(
            git_output(&root, &["rev-parse", "refs/remotes/origin/main"]),
            next
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_and_amend_execute_with_the_exact_message() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-commit-{unique}"));
        fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(&root, &["config", "user.name", "Test Author"]);
        run(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "first\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        let repository = Repository::discover(&root).unwrap();
        repository
            .execute(&GitOperation::Commit {
                message: "Initial subject\n\nInitial body".to_owned(),
                amend: false,
            })
            .unwrap();
        assert_eq!(
            repository.head_commit_message().unwrap(),
            "Initial subject\n\nInitial body"
        );
        assert_eq!(
            git_output(&root, &["log", "-1", "--pretty=%B"]),
            "Initial subject\n\nInitial body"
        );
        fs::write(root.join("tracked.txt"), "second\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        repository
            .execute(&GitOperation::Commit {
                message: "Replacement".to_owned(),
                amend: true,
            })
            .unwrap();
        assert_eq!(git_output(&root, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(
            git_output(&root, &["log", "-1", "--pretty=%B"]),
            "Replacement"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn run(root: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_output(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
