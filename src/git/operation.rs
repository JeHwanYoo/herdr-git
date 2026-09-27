use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitOperation {
    StageAll,
    StagePath(String),
    StagePaths(Vec<String>),
    UnstageAll,
    UnstagePath(String),
    UnstagePaths(Vec<String>),
    Fetch,
    PullFastForward,
    Commit {
        message: String,
        amend: bool,
    },
    Push {
        force: bool,
        remote: String,
        branch: String,
    },
    AddRemote {
        name: String,
        url: String,
    },
    RemoveRemote(String),
    StashChanges,
    StashPop,
    CreateBranch {
        name: String,
        commit: String,
    },
    CreateTag {
        name: String,
        commit: String,
    },
    CheckoutCommit(String),
    CheckoutBranch(String),
    RebaseHere(String),
    InteractiveRebase(String),
    InteractiveRebaseOnto(String),
    CherryPick(String),
    Revert(String),
    Reset {
        mode: ResetMode,
        target: String,
        target_name: String,
        expected_branch: String,
        expected_head: String,
    },
}

impl GitOperation {
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::StageAll => strings(&["add", "--all"]),
            Self::StagePath(path) => {
                vec!["add".into(), "--all".into(), "--".into(), path.clone()]
            }
            Self::StagePaths(paths) => {
                let mut args = strings(&["add", "--all", "--"]);
                args.extend(paths.iter().cloned());
                args
            }
            Self::UnstageAll => strings(&["restore", "--staged", "--", "."]),
            Self::UnstagePath(path) => vec![
                "restore".into(),
                "--staged".into(),
                "--".into(),
                path.clone(),
            ],
            Self::UnstagePaths(paths) => {
                let mut args = strings(&["restore", "--staged", "--"]);
                args.extend(paths.iter().cloned());
                args
            }
            Self::Fetch => strings(&["fetch", "--all", "--prune"]),
            Self::PullFastForward => strings(&["pull", "--ff-only"]),
            Self::Commit { message, amend } => {
                let mut args = strings(&["commit"]);
                if *amend {
                    args.push("--amend".into());
                }
                args.extend(["-m".into(), message.clone()]);
                args
            }
            Self::Push {
                force,
                remote,
                branch,
            } => {
                let mut args = strings(&["push"]);
                if *force {
                    args.push("--force-with-lease".into());
                }
                args.extend([
                    "--set-upstream".into(),
                    "--".into(),
                    remote.clone(),
                    format!("HEAD:refs/heads/{branch}"),
                ]);
                args
            }
            Self::AddRemote { name, url } => vec![
                "remote".into(),
                "add".into(),
                "--".into(),
                name.clone(),
                url.clone(),
            ],
            Self::RemoveRemote(name) => {
                vec!["remote".into(), "remove".into(), "--".into(), name.clone()]
            }
            Self::StashChanges => strings(&["stash", "push", "--include-untracked"]),
            Self::StashPop => strings(&["stash", "pop"]),
            Self::CreateBranch { name, commit } => {
                vec!["branch".into(), "--".into(), name.clone(), commit.clone()]
            }
            Self::CreateTag { name, commit } => {
                vec!["tag".into(), "--".into(), name.clone(), commit.clone()]
            }
            Self::CheckoutCommit(sha) => vec!["checkout".into(), "--detach".into(), sha.clone()],
            Self::CheckoutBranch(name) => vec!["switch".into(), "--".into(), name.clone()],
            Self::RebaseHere(sha) => vec!["rebase".into(), sha.clone()],
            Self::InteractiveRebaseOnto(reference) => {
                vec!["rebase".into(), "-i".into(), reference.clone()]
            }
            Self::InteractiveRebase(sha) => vec!["rebase".into(), "-i".into(), format!("{sha}^")],
            Self::CherryPick(sha) => vec!["cherry-pick".into(), sha.clone()],
            Self::Revert(sha) => vec!["revert".into(), "--no-edit".into(), sha.clone()],
            Self::Reset { mode, target, .. } => {
                vec!["reset".into(), mode.flag().into(), target.clone()]
            }
        }
    }

    pub fn preview(&self) -> String {
        format!("git {}", self.args().join(" "))
    }

    pub fn recovery_commands(&self) -> Option<&'static str> {
        match self {
            Self::RebaseHere(_) | Self::InteractiveRebase(_) | Self::InteractiveRebaseOnto(_) => {
                Some("git rebase --continue | --skip | --abort")
            }
            Self::CherryPick(_) => Some("git cherry-pick --continue | --skip | --abort"),
            Self::Revert(_) => Some("git revert --continue | --skip | --abort"),
            _ => None,
        }
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

impl Repository {
    pub fn validate_branch_name(&self, name: &str) -> Result<(), String> {
        git(&self.root, &["check-ref-format", "--branch", name]).map(|_| ())
    }

    pub fn validate_tag_name(&self, name: &str) -> Result<(), String> {
        let reference = format!("refs/tags/{name}");
        git(&self.root, &["check-ref-format", &reference]).map(|_| ())
    }

    pub fn execute(&self, operation: &GitOperation) -> Result<String, String> {
        if let GitOperation::Push { remote, branch, .. } = operation {
            let current = git(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
            if current.trim() != branch {
                return Err("Push stopped because the current branch changed. Try again.".into());
            }
            if !self.remotes()?.iter().any(|item| item.name == *remote) {
                return Err("The selected remote no longer exists. Try again.".into());
            }
        }
        if let GitOperation::Reset {
            expected_branch,
            expected_head,
            ..
        } = operation
        {
            let current_branch = git(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .ok()
                .map(|value| value.trim().to_owned());
            let current_head = git(&self.root, &["rev-parse", "--verify", "HEAD"])
                .ok()
                .map(|value| value.trim().to_owned());
            if current_branch.as_deref() != Some(expected_branch.as_str())
                || current_head.as_deref() != Some(expected_head.as_str())
            {
                return Err(
                    "Reset stopped because the current branch or HEAD changed. Refresh and try again."
                        .to_owned(),
                );
            }
        }
        let args = match operation {
            GitOperation::UnstageAll if !self.has_head() => {
                ["rm", "--cached", "-r", "--ignore-unmatch", "--", "."]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            }
            GitOperation::UnstagePath(path) if !self.has_head() => vec![
                "rm".into(),
                "--cached".into(),
                "--ignore-unmatch".into(),
                "--".into(),
                path.clone(),
            ],
            GitOperation::UnstagePaths(paths) if !self.has_head() => {
                let mut args = vec![
                    "rm".into(),
                    "--cached".into(),
                    "--ignore-unmatch".into(),
                    "--".into(),
                ];
                args.extend(paths.iter().cloned());
                args
            }
            _ => operation.args(),
        };
        let output = git_owned_output(&self.root, &args)?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("{stdout}\n{stderr}"),
            (false, true) => stdout,
            (true, false) => stderr,
            (true, true) => "Git completed without output.".to_owned(),
        };
        if output.status.success() {
            self.verify_staging(operation)?;
            Ok(message)
        } else {
            Err(message)
        }
    }

    fn verify_staging(&self, operation: &GitOperation) -> Result<(), String> {
        let (stage, paths) = match operation {
            GitOperation::StageAll => (true, &[][..]),
            GitOperation::StagePath(path) => (true, std::slice::from_ref(path)),
            GitOperation::StagePaths(paths) => (true, paths.as_slice()),
            GitOperation::UnstageAll => (false, &[][..]),
            GitOperation::UnstagePath(path) => (false, std::slice::from_ref(path)),
            GitOperation::UnstagePaths(paths) => (false, paths.as_slice()),
            _ => return Ok(()),
        };
        let read_paths = |mut args: Vec<String>| -> Result<Vec<String>, String> {
            args.push("--".into());
            args.extend(paths.iter().cloned());
            let output = git_owned_output(&self.root, &args)?;
            if !output.status.success() {
                return Err(format!(
                    "Could not verify the index update: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(String::from_utf8_lossy(&output.stdout)
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect())
        };
        let mut args = strings(&["diff", "--name-only", "-z", "--ignore-submodules=dirty"]);
        if !stage {
            args.push("--cached".into());
        }
        let mut remaining = read_paths(args)?;
        if stage {
            remaining.extend(read_paths(strings(&[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
            ]))?);
        }
        remaining.sort();
        remaining.dedup();
        if remaining.is_empty() {
            return Ok(());
        }
        let state = if stage { "unstaged" } else { "staged" };
        let mut message = format!(
            "Git completed, but {} requested paths remain {state}:\n{}",
            remaining.len(),
            remaining
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        if remaining.len() > 20 {
            message.push_str(&format!("\n... and {} more", remaining.len() - 20));
        }
        message.push_str(
            "\nThe operation may have applied partially. Review the refreshed file list.",
        );
        if stage {
            message.push_str("\nCheck for concurrent file edits or filename case collisions. Case-colliding files require a case-sensitive filesystem.");
        }
        Err(message)
    }

    fn has_head(&self) -> bool {
        git(&self.root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn repository(name: &str) -> Repository {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-staging-{name}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]).unwrap();
        git(&root, &["config", "user.name", "Test Author"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        Repository::at_root(root)
    }

    #[test]
    fn staging_verification_checks_only_requested_paths_and_supports_unborn_head() {
        let repository = repository("verify");
        let root = repository.root();
        fs::write(root.join("selected.txt"), "selected\n").unwrap();
        fs::write(root.join("other.txt"), "other\n").unwrap();
        let stage = GitOperation::StagePath("selected.txt".into());
        let error = repository.verify_staging(&stage).unwrap_err();
        assert!(error.contains("selected.txt"));
        assert!(!error.contains("other.txt"));
        repository.execute(&stage).unwrap();
        repository.verify_staging(&stage).unwrap();
        assert!(repository.verify_staging(&GitOperation::StageAll).is_err());
        let unstage = GitOperation::UnstagePath("selected.txt".into());
        assert!(repository.verify_staging(&unstage).is_err());
        repository.execute(&unstage).unwrap();
        repository.verify_staging(&unstage).unwrap();
        repository.execute(&GitOperation::StageAll).unwrap();
        git(root, &["commit", "-m", "Base"]).unwrap();
        fs::write(root.join("selected.txt"), "changed\n").unwrap();
        fs::remove_file(root.join("other.txt")).unwrap();
        repository.execute(&stage).unwrap();
        assert!(repository.verify_staging(&GitOperation::StageAll).is_err());
        repository.execute(&GitOperation::StageAll).unwrap();
        repository.verify_staging(&GitOperation::StageAll).unwrap();
        repository.execute(&GitOperation::UnstageAll).unwrap();
        repository
            .verify_staging(&GitOperation::UnstageAll)
            .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn case_colliding_paths_do_not_report_false_staging_success() {
        let repository = repository("case-collision");
        let root = repository.root();
        fs::write(root.join("FILE.txt"), "upper\n").unwrap();
        let case_sensitive = !root.join("file.txt").exists();
        let upper = git(root, &["hash-object", "-w", "FILE.txt"]).unwrap();
        fs::write(root.join("file.txt"), "lower\n").unwrap();
        let lower = git(root, &["hash-object", "-w", "file.txt"]).unwrap();
        for (path, blob) in [("FILE.txt", upper.trim()), ("file.txt", lower.trim())] {
            git(
                root,
                &["update-index", "--add", "--cacheinfo", "100644", blob, path],
            )
            .unwrap();
        }
        git(root, &["commit", "-m", "Case-sensitive tree"]).unwrap();
        fs::write(root.join("independent.txt"), "new file\n").unwrap();
        if case_sensitive {
            fs::write(root.join("FILE.txt"), "changed upper\n").unwrap();
        }
        for operation in [
            GitOperation::StageAll,
            GitOperation::StagePath("FILE.txt".into()),
            GitOperation::StagePaths(vec!["FILE.txt".into(), "file.txt".into()]),
        ] {
            let result = repository.execute(&operation);
            if case_sensitive {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert!(error.contains("remain unstaged"), "{error}");
                assert!(error.contains("FILE.txt"), "{error}");
                assert!(error.contains("case-sensitive filesystem"), "{error}");
            }
        }
        let staged = git(root, &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("independent.txt"));
        fs::remove_dir_all(root).unwrap();
    }
}
