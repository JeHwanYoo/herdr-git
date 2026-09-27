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
            Ok(message)
        } else {
            Err(message)
        }
    }

    fn has_head(&self) -> bool {
        git(&self.root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
    }
}
