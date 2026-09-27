use std::path::{Path, PathBuf};

use crate::git::{BranchStatus, ChangeOverview, Repository, WorktreeReport, worktree_report};

mod picker;
mod store;

pub use picker::pick_project_directory;
pub use store::ProjectRegistry;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub path: PathBuf,
    pub overview: ChangeOverview,
    pub branch_status: BranchStatus,
    pub fingerprint: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectStatus {
    pub root: PathBuf,
    pub primary: WorktreeStatus,
    pub worktrees: Vec<WorktreeStatus>,
}

pub fn inspect_project(
    root: &Path,
    previous: &[WorktreeStatus],
    fresh: Option<&WorktreeStatus>,
) -> Result<ProjectStatus, String> {
    let paths = Repository::at_root(root.to_owned()).worktree_paths()?;
    let mut statuses = paths
        .into_iter()
        .map(|path| {
            if let Some(fresh) = fresh.filter(|fresh| fresh.path == path) {
                return Ok(fresh.clone());
            }
            let previous = previous.iter().find(|status| status.path == path);
            inspect_root(path, previous)
        })
        .collect::<Result<Vec<_>, String>>()?;
    if statuses.is_empty() {
        return Err("Git returned no worktrees".to_owned());
    }
    let primary = statuses.remove(0);
    Ok(ProjectStatus {
        root: primary.path.clone(),
        primary,
        worktrees: statuses,
    })
}

pub fn inspect_worktree(
    path: &Path,
    previous: Option<&WorktreeStatus>,
) -> Result<WorktreeStatus, String> {
    let report = worktree_report(path)?;
    match previous {
        Some(previous) if previous.fingerprint == report.fingerprint => {
            inspect_unchanged(previous, report)
        }
        _ => inspect_report(Repository::discover(path)?, report),
    }
}

fn inspect_root(
    root: PathBuf,
    previous: Option<&WorktreeStatus>,
) -> Result<WorktreeStatus, String> {
    let report = worktree_report(&root)?;
    match previous {
        Some(previous) if previous.fingerprint == report.fingerprint => {
            inspect_unchanged(previous, report)
        }
        _ => inspect_report(Repository::at_root(root), report),
    }
}

fn inspect_unchanged(
    previous: &WorktreeStatus,
    report: WorktreeReport,
) -> Result<WorktreeStatus, String> {
    if report.changed_paths == 0 {
        return Ok(previous.clone());
    }
    inspect_report(Repository::at_root(previous.path.clone()), report)
}

fn inspect_report(
    repository: Repository,
    report: WorktreeReport,
) -> Result<WorktreeStatus, String> {
    let overview = repository.change_overview(&report)?;
    Ok(WorktreeStatus {
        path: repository.root().to_owned(),
        overview,
        branch_status: report.branch_status,
        fingerprint: report.fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ProjectRegistry, inspect_project, inspect_worktree};
    use crate::git::{ChangeOverview, git_processes_started};

    fn git(cwd: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn persists_normalized_projects_and_removes_only_registration() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-registry-{unique}"));
        let root = base.join("project");
        let linked = base.join("linked");
        let state_file = base.join("state/projects.json");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        git(
            &root,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );

        let mut registry = ProjectRegistry::load(Some(state_file.clone())).unwrap();
        assert!(registry.add(&linked).unwrap());
        assert!(!registry.add(&root).unwrap());
        let registered_root = fs::canonicalize(&root).unwrap();
        assert_eq!(registry.roots(), [registered_root.as_path()]);
        let status = inspect_project(&root, &[], None).unwrap();
        assert_eq!(status.primary.branch_status.checked_out, "main");
        assert_eq!(status.worktrees.len(), 1);
        assert_eq!(status.worktrees[0].branch_status.checked_out, "feature");
        assert_ne!(status.primary.fingerprint, status.worktrees[0].fingerprint);

        let reloaded = ProjectRegistry::load(Some(state_file)).unwrap();
        assert_eq!(reloaded.roots(), [registered_root.as_path()]);
        assert!(registry.remove(&registered_root).unwrap());
        assert!(root.exists());
        assert!(linked.exists());

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_dirty_worktree_re_reads_its_totals_and_a_clean_one_is_reused() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-dirty-{unique}"));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        let overview = |additions: usize| ChangeOverview {
            changed_paths: 1,
            additions,
            deletions: 0,
        };

        let first = inspect_project(&root, &[], None).unwrap();
        assert_eq!(first.primary.overview, ChangeOverview::default());

        let before = git_processes_started();
        let second = inspect_project(&root, std::slice::from_ref(&first.primary), None).unwrap();
        assert_eq!(
            git_processes_started() - before,
            2,
            "a clean worktree costs one worktree list and one status read"
        );
        assert_eq!(second, first);

        fs::write(root.join("tracked.txt"), "base\nmore\n").unwrap();
        let third = inspect_project(&root, std::slice::from_ref(&second.primary), None).unwrap();
        assert_ne!(third.primary.fingerprint, second.primary.fingerprint);
        assert_eq!(third.primary.overview, overview(1));

        fs::write(root.join("tracked.txt"), "base\nmore\nagain\n").unwrap();
        let before = git_processes_started();
        let fourth = inspect_project(&root, std::slice::from_ref(&third.primary), None).unwrap();
        assert_eq!(
            git_processes_started() - before,
            4,
            "a dirty worktree without untracked files adds only two numstat reads"
        );
        assert_eq!(fourth.primary.fingerprint, third.primary.fingerprint);
        assert_eq!(fourth.primary.overview, overview(2));

        fs::write(root.join("tracked.txt"), "base\nmore\nagain\nonce more\n").unwrap();
        let before = git_processes_started();
        let fifth = inspect_worktree(&root, Some(&fourth.primary)).unwrap();
        assert_eq!(
            git_processes_started() - before,
            3,
            "an unchanged status hash skips resolving the root"
        );
        assert_eq!(fifth.path, fourth.primary.path);
        assert_eq!(fifth.overview, overview(3));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_plain_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-git-plain-{unique}"));
        fs::create_dir(&root).unwrap();
        let mut registry = ProjectRegistry::load(None).unwrap();

        assert_eq!(
            registry.add(&root).unwrap_err(),
            "Choose a directory inside a Git repository."
        );
        assert!(registry.roots().is_empty());

        fs::remove_dir_all(root).unwrap();
    }
}
