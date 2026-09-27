use std::fs;
use std::path::PathBuf;

use super::{
    Repository,
    process::{git, git_rebase},
};
use crate::git::GitOperation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Edit,
    Reword,
    Squash,
    Fixup,
    Drop,
}
impl RebaseAction {
    pub const ALL: [Self; 6] = [
        Self::Pick,
        Self::Edit,
        Self::Reword,
        Self::Squash,
        Self::Fixup,
        Self::Drop,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Pick => "Pick",
            Self::Edit => "Edit",
            Self::Reword => "Reword",
            Self::Squash => "Squash",
            Self::Fixup => "Fixup",
            Self::Drop => "Drop",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Pick => "Use commit",
            Self::Edit => "Stop for amending",
            Self::Reword => "Edit commit message",
            Self::Squash => "Combine into the commit below; keep both messages",
            Self::Fixup => "Combine into the commit below; drop this message",
            Self::Drop => "Remove commit",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Pick | Self::Reword => "pick",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseEntry {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub message: String,
    pub action: RebaseAction,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebasePlan {
    pub branch: String,
    pub head: String,
    pub upstream: Option<String>,
    pub target: String,
    pub entries: Vec<RebaseEntry>,
    pub update_refs: bool,
    pub active: bool,
}
impl RebasePlan {
    pub fn validate(&self) -> Result<(), String> {
        if self.entries.is_empty() {
            return Err("No commits to rebase.".into());
        }
        let mut previous = false;
        let mut seen = std::collections::HashSet::new();
        for entry in self.entries.iter().rev() {
            if !matches!(entry.sha.len(), 40 | 64)
                || !entry.sha.bytes().all(|c| c.is_ascii_hexdigit())
                || !seen.insert(&entry.sha)
            {
                return Err("Invalid or duplicate commit in rebase plan.".into());
            }
            if matches!(entry.action, RebaseAction::Squash | RebaseAction::Fixup) && !previous {
                return Err("Squash and Fixup need a retained commit below them.".into());
            }
            if entry.action == RebaseAction::Reword && entry.message.trim().is_empty() {
                return Err("Reword needs a non-empty message.".into());
            }
            previous |= entry.action != RebaseAction::Drop;
        }
        Ok(())
    }
    fn todo(&self) -> String {
        let mut todo = String::new();
        let mut markers = String::new();
        for (index, entry) in self.entries.iter().enumerate().rev() {
            if !matches!(
                entry.action,
                RebaseAction::Squash | RebaseAction::Fixup | RebaseAction::Drop
            ) {
                todo.push_str(&markers);
                markers.clear();
            }
            todo.push_str(&format!("{} {}\n", entry.action.verb(), entry.sha));
            if entry.action == RebaseAction::Reword {
                todo.push_str(&format!("exec git commit --amend --allow-empty -F \"$HERDR_REBASE_DIR/message-{index}\"\n"));
            }
            markers.push_str(&format!("# herdr-end {}\n", entry.sha));
        }
        todo.push_str(&markers);
        todo
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseTask {
    Start(RebasePlan),
    Continue,
    Skip,
    Abort,
    Amend(String),
}
#[derive(Clone, Debug)]
pub struct RebaseResult {
    pub active: bool,
    pub success: bool,
    pub output: String,
    pub message: String,
}

const SEQUENCE_EDITOR: &str = r##"#!/bin/sh
set -eu
awk 'NR == FNR { if ($1 == "pick") key=$2; if ($1 == "update-ref") refs[key]=refs[key] $0 "\n"; next } { print } $1 == "#" && $2 == "herdr-end" { printf "%s", refs[$3] }' "$1" "$HERDR_REBASE_DIR/todo" > "$HERDR_REBASE_DIR/combined"
cat "$HERDR_REBASE_DIR/combined" > "$1"
"##;

impl Repository {
    fn rebase_dir(&self) -> Result<PathBuf, String> {
        Ok(PathBuf::from(
            git(self.root(), &["rev-parse", "--absolute-git-dir"])?.trim(),
        ))
    }
    pub fn rebase_plan(&self, operation: &GitOperation) -> Result<RebasePlan, String> {
        let dir = self.rebase_dir()?;
        let active = dir.join("rebase-merge").is_dir() || dir.join("rebase-apply").is_dir();
        let head = git(self.root(), &["rev-parse", "HEAD"])?.trim().to_owned();
        if active {
            let branch = fs::read_to_string(dir.join("rebase-merge/head-name"))
                .unwrap_or_else(|_| "Rebase in progress".into())
                .trim()
                .trim_start_matches("refs/heads/")
                .to_owned();
            return Ok(RebasePlan {
                branch,
                head: head.clone(),
                upstream: None,
                target: "Rebase in progress".into(),
                entries: vec![RebaseEntry {
                    sha: head.clone(),
                    author: String::new(),
                    date: String::new(),
                    message: git(self.root(), &["log", "-1", "--format=%B"])?
                        .trim_end()
                        .into(),
                    action: RebaseAction::Edit,
                }],
                update_refs: false,
                active,
            });
        }
        let branch = git(self.root(), &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map_err(|_| "Checkout a branch before rebasing.".to_owned())?
            .trim()
            .to_owned();
        let (target, inclusive) = match operation {
            GitOperation::InteractiveRebase(value) => (value, true),
            GitOperation::InteractiveRebaseOnto(value) => (value, false),
            _ => return Err("Choose an interactive rebase target.".into()),
        };
        let target_sha = git(
            self.root(),
            &["rev-parse", "--verify", &format!("{target}^{{commit}}")],
        )?
        .trim()
        .to_owned();
        let upstream = if inclusive {
            git(
                self.root(),
                &["rev-parse", "--verify", &format!("{target_sha}^")],
            )
            .ok()
            .map(|s| s.trim().to_owned())
        } else {
            Some(target_sha)
        };
        let range = upstream
            .as_ref()
            .map(|sha| format!("{sha}..{head}"))
            .unwrap_or_else(|| head.clone());
        let output = git(
            self.root(),
            &[
                "log",
                "--topo-order",
                "--date-order",
                "--no-merges",
                "--format=%H%x00%an%x00%aI%x00%B%x00",
                &range,
            ],
        )?;
        let fields: Vec<_> = output.split('\0').collect();
        let mut entries = Vec::new();
        for row in fields.as_chunks::<4>().0 {
            entries.push(RebaseEntry {
                sha: row[0].trim().into(),
                author: row[1].into(),
                date: row[2].chars().take(10).collect(),
                message: row[3].trim_end().into(),
                action: RebaseAction::Pick,
            });
        }
        Ok(RebasePlan {
            branch,
            head,
            upstream,
            target: target.clone(),
            entries,
            update_refs: false,
            active: false,
        })
    }
    pub fn run_rebase(&self, task: &RebaseTask) -> Result<RebaseResult, String> {
        let dir = self.rebase_dir()?;
        let state = dir.join("herdr-rebase");
        let active = || dir.join("rebase-merge").is_dir() || dir.join("rebase-apply").is_dir();
        let args: Vec<String> = match task {
            RebaseTask::Start(plan) => {
                plan.validate()?;
                if active() {
                    return Err(
                        "A rebase is already in progress. Reopen Interactive rebase.".into(),
                    );
                }
                let head = git(self.root(), &["rev-parse", "HEAD"])?;
                let branch = git(self.root(), &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
                if head.trim() != plan.head || branch.trim() != plan.branch {
                    return Err(
                        "Branch or HEAD changed. Close and reopen Interactive rebase.".into(),
                    );
                }
                if !git(self.root(), &["status", "--porcelain"])?.is_empty() {
                    return Err("Commit or stash working changes before rebasing.".into());
                }
                if state.exists() {
                    fs::remove_dir_all(&state).map_err(|e| e.to_string())?;
                }
                fs::create_dir(&state).map_err(|e| e.to_string())?;
                fs::write(state.join("todo"), plan.todo()).map_err(|e| e.to_string())?;
                fs::write(state.join("sequence-editor"), SEQUENCE_EDITOR)
                    .map_err(|e| e.to_string())?;
                for (i, entry) in plan.entries.iter().enumerate() {
                    if entry.action == RebaseAction::Reword {
                        fs::write(state.join(format!("message-{i}")), &entry.message)
                            .map_err(|e| e.to_string())?;
                    }
                }
                let mut args = vec![
                    "-c".into(),
                    "core.abbrev=false".into(),
                    "rebase".into(),
                    "-i".into(),
                    "--no-autosquash".into(),
                    "--no-autostash".into(),
                    "--no-rebase-merges".into(),
                    "--reschedule-failed-exec".into(),
                    "--empty=keep".into(),
                    if plan.update_refs {
                        "--update-refs".into()
                    } else {
                        "--no-update-refs".into()
                    },
                ];
                match &plan.upstream {
                    Some(sha) => args.push(sha.clone()),
                    None => args.push("--root".into()),
                }
                args
            }
            _ => {
                if !active() {
                    return Err("No rebase is in progress. Close and refresh.".into());
                }
                match task {
                    RebaseTask::Continue => vec!["rebase".into(), "--continue".into()],
                    RebaseTask::Skip => vec!["rebase".into(), "--skip".into()],
                    RebaseTask::Abort => vec!["rebase".into(), "--abort".into()],
                    RebaseTask::Amend(message) => {
                        if !dir.join("rebase-merge/amend").exists() {
                            return Err("Amend is available only at an Edit stop. Resolve conflicts and use Continue.".into());
                        }
                        if message.trim().is_empty() {
                            return Err("Enter a commit message.".into());
                        }
                        vec![
                            "commit".into(),
                            "--amend".into(),
                            "--allow-empty".into(),
                            "-m".into(),
                            message.clone(),
                        ]
                    }
                    RebaseTask::Start(_) => unreachable!(),
                }
            }
        };
        let result = git_rebase(self.root(), &args, &state)?;
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_owned();
        let still_active = active();
        if !still_active && state.exists() {
            let _ = fs::remove_dir_all(&state);
        }
        Ok(RebaseResult {
            active: still_active,
            success: result.status.success(),
            output,
            message: git(self.root(), &["log", "-1", "--format=%B"])
                .unwrap_or_default()
                .trim_end()
                .into(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    pub(super) fn git(root: &std::path::Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    pub(super) fn temp_repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        root
    }

    #[test]
    fn native_rebase_rewords_unicode_and_drops_a_commit() {
        let root = temp_repo("native-rebase");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        for name in ["one", "two", "three"] {
            std::fs::write(root.join(name), name).unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-m", name]);
        }
        let repo = Repository::discover(&root).unwrap();
        let mut plan = repo
            .rebase_plan(&GitOperation::InteractiveRebaseOnto("HEAD~3".into()))
            .unwrap();
        plan.entries[2].action = RebaseAction::Reword;
        plan.entries[2].message = "한글 제목\n\n한글 본문".into();
        plan.entries[1].action = RebaseAction::Drop;
        let result = repo.run_rebase(&RebaseTask::Start(plan)).unwrap();
        assert!(!result.active, "{}", result.output);
        assert!(result.success, "{}", result.output);
        assert!(!root.join("two").exists());
        let message =
            super::super::process::git(&root, &["log", "-1", "--format=%B", "HEAD~1"]).unwrap();
        assert!(message.contains("한글 제목\n\n한글 본문"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::super::process::git as output;
    use super::tests::{git, temp_repo};
    use super::*;

    fn fixture(name: &str) -> (PathBuf, Repository, RebasePlan) {
        let root = temp_repo(name);
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        for name in ["one", "two", "three"] {
            fs::write(root.join(name), name).unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-m", name]);
        }
        let repo = Repository::discover(&root).unwrap();
        let plan = repo
            .rebase_plan(&GitOperation::InteractiveRebaseOnto("HEAD~3".into()))
            .unwrap();
        (root, repo, plan)
    }
    #[test]
    fn native_rebase_edit_amend_continue_and_reopen() {
        let (root, repo, mut plan) = fixture("rebase-edit");
        plan.entries[0].action = RebaseAction::Edit;
        let result = repo.run_rebase(&RebaseTask::Start(plan)).unwrap();
        assert!(result.active, "{}", result.output);
        assert!(
            !result
                .output
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{:?}",
            result.output
        );
        assert!(
            repo.rebase_plan(&GitOperation::InteractiveRebaseOnto("HEAD".into()))
                .unwrap()
                .active
        );
        let result = repo
            .run_rebase(&RebaseTask::Amend("수정한 커밋".into()))
            .unwrap();
        assert!(result.success && result.active, "{}", result.output);
        let result = repo.run_rebase(&RebaseTask::Continue).unwrap();
        assert!(result.success && !result.active, "{}", result.output);
        assert!(
            output(&root, &["log", "--format=%s"])
                .unwrap()
                .contains("수정한 커밋")
        );
        assert!(!root.join(".git/herdr-rebase").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_rebase_rejects_stale_dirty_and_invalid_plans() {
        let (root, repo, mut plan) = fixture("rebase-guards");
        plan.entries.last_mut().unwrap().action = RebaseAction::Fixup;
        assert!(
            repo.run_rebase(&RebaseTask::Start(plan.clone()))
                .unwrap_err()
                .contains("below them")
        );
        plan.entries.last_mut().unwrap().action = RebaseAction::Pick;
        fs::write(root.join("dirty"), "work").unwrap();
        assert!(
            repo.run_rebase(&RebaseTask::Start(plan.clone()))
                .unwrap_err()
                .contains("working changes")
        );
        fs::remove_file(root.join("dirty")).unwrap();
        git(&root, &["commit", "--allow-empty", "-m", "New HEAD"]);
        assert!(
            repo.run_rebase(&RebaseTask::Start(plan))
                .unwrap_err()
                .contains("HEAD changed")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_rebase_squash_fixup_reorder_and_update_refs() {
        let (root, repo, mut plan) = fixture("rebase-combine");
        git(&root, &["branch", "dependent", "HEAD~2"]);
        plan.entries.swap(1, 2);
        plan.entries[1].action = RebaseAction::Squash;
        plan.entries[0].action = RebaseAction::Fixup;
        plan.update_refs = true;
        let result = repo.run_rebase(&RebaseTask::Start(plan)).unwrap();
        assert!(result.success && !result.active, "{}", result.output);
        assert_eq!(
            output(&root, &["rev-list", "--count", "HEAD"])
                .unwrap()
                .trim(),
            "2"
        );
        let message = output(&root, &["log", "-1", "--format=%B"]).unwrap();
        assert!(
            message.starts_with("two\n") && message.contains("one") && !message.contains("three"),
            "{message}"
        );
        assert_eq!(
            output(&root, &["rev-parse", "dependent"]).unwrap(),
            output(&root, &["rev-parse", "HEAD"]).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_rebase_conflict_abort_restores_original_branch() {
        let (root, repo, mut plan) = fixture("rebase-conflict");
        let original = plan.head.clone();
        git(&root, &["branch", "topic"]);
        git(&root, &["switch", "--detach", "HEAD~3"]);
        fs::write(root.join("one"), "conflict").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Upstream"]);
        let upstream = output(&root, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        git(&root, &["switch", "main"]);
        plan.upstream = Some(upstream);
        let result = repo.run_rebase(&RebaseTask::Start(plan)).unwrap();
        assert!(result.active && !result.success, "{}", result.output);
        let result = repo.run_rebase(&RebaseTask::Abort).unwrap();
        assert!(result.success && !result.active, "{}", result.output);
        assert_eq!(
            output(&root, &["rev-parse", "HEAD"]).unwrap().trim(),
            original
        );
        assert_eq!(fs::read_to_string(root.join("one")).unwrap(), "one");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_rebase_can_reword_root_commit() {
        let (root, repo, _) = fixture("rebase-root");
        let mut plan = repo
            .rebase_plan(&GitOperation::InteractiveRebase("HEAD~3".into()))
            .unwrap();
        assert!(plan.upstream.is_none());
        assert_eq!(plan.entries.len(), 4);
        plan.entries[3].action = RebaseAction::Reword;
        plan.entries[3].message = "새 루트".into();
        let result = repo.run_rebase(&RebaseTask::Start(plan)).unwrap();
        assert!(result.success && !result.active, "{}", result.output);
        assert_eq!(
            output(&root, &["log", "-1", "--format=%s", "HEAD~3"])
                .unwrap()
                .trim(),
            "새 루트"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
