use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
    pub push_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitCommandFacts {
    pub has_remote: bool,
    pub remotes: Vec<Remote>,
    pub current_branch: Option<String>,
    pub upstream: Option<String>,
    pub has_changes: bool,
    pub has_staged_changes: bool,
    pub has_stash: bool,
    pub head_commit: Option<String>,
}

impl Repository {
    pub fn remotes(&self) -> Result<Vec<Remote>, String> {
        let args = ["config", "--null", "--get-regexp", "^remote\\..*\\.url$"].map(str::to_owned);
        let output = git_owned_output(&self.root, &args)?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        let config = String::from_utf8_lossy(&output.stdout);
        let mut remotes: Vec<Remote> = Vec::new();
        for entry in config.split('\0') {
            let Some((key, _)) = entry.split_once('\n') else {
                continue;
            };
            let Some(name) = key
                .strip_prefix("remote.")
                .and_then(|key| key.strip_suffix(".url"))
            else {
                continue;
            };
            if remotes.iter().any(|remote| remote.name == name) {
                continue;
            }
            remotes.push(Remote {
                name: name.to_owned(),
                url: git(&self.root, &["remote", "get-url", "--", name])?
                    .trim()
                    .to_owned(),
                push_url: git(&self.root, &["remote", "get-url", "--push", "--", name])?
                    .trim()
                    .to_owned(),
            });
        }
        Ok(remotes)
    }

    pub fn branch_start(&self) -> Result<String, String> {
        let branch = git(&self.root, &["symbolic-ref", "--quiet", "HEAD"]).map_err(|_| {
            "Branch start unavailable for detached HEAD. Choose a base branch.".to_owned()
        })?;
        if branch.trim() == "refs/heads/main" {
            return git(
                &self.root,
                &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
            )?
            .lines()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| "Branch start unavailable. Choose a base branch.".into());
        }
        let log = git(
            &self.root,
            &["reflog", "show", "--format=%H%x00%gs", branch.trim()],
        )?;
        log.lines()
            .rev()
            .find_map(|line| {
                let (sha, subject) = line.split_once('\0')?;
                (subject.starts_with("branch: Created from ")
                    || subject.starts_with("commit (initial):")
                    || subject.starts_with("clone: from "))
                .then(|| sha.to_owned())
            })
            .ok_or_else(|| "Branch start unavailable. Choose a base branch.".into())
    }

    pub fn command_facts(&self) -> GitCommandFacts {
        let remotes = self.remotes().unwrap_or_default();
        let current_branch = git(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let upstream = git(&self.root, &["rev-parse", "--abbrev-ref", "@{upstream}"])
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let status = git(&self.root, &["status", "--porcelain=v2"]).unwrap_or_default();
        let has_changes = !status.is_empty();
        let has_staged_changes = status.lines().any(|line| {
            (line.starts_with("1 ") || line.starts_with("2 "))
                && line.as_bytes().get(2).is_some_and(|state| *state != b'.')
        });
        let has_stash =
            git(&self.root, &["stash", "list", "-1"]).is_ok_and(|value| !value.is_empty());
        let head_commit = git(&self.root, &["rev-parse", "--verify", "HEAD"])
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        GitCommandFacts {
            has_remote: !remotes.is_empty(),
            remotes,
            current_branch,
            upstream,
            has_changes,
            has_staged_changes,
            has_stash,
            head_commit,
        }
    }

    pub fn origin_is_github(&self) -> bool {
        self.origin_url().is_some_and(|url| is_github_remote(&url))
    }

    pub fn origin_url(&self) -> Option<String> {
        git(&self.root, &["remote", "get-url", "origin"])
            .ok()
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
    }

    pub fn refresh_fingerprint(&self) -> Result<RepositoryFingerprint, String> {
        let mut refs = DefaultHasher::new();
        let mut worktree = DefaultHasher::new();
        let status = git(
            &self.root,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
            ],
        )?;
        for line in status.lines() {
            if line.starts_with("# ") {
                line.hash(&mut refs);
            } else {
                line.hash(&mut worktree);
            }
        }
        for args in [
            &["diff", "--no-ext-diff", "--no-color"][..],
            &["diff", "--cached", "--no-ext-diff", "--no-color"][..],
        ] {
            git(&self.root, args)?.hash(&mut worktree);
        }
        git(&self.root, &["show-ref", "--head"])
            .unwrap_or_default()
            .hash(&mut refs);
        git(&self.root, &["config", "--get-regexp", "^remote\\."])
            .unwrap_or_default()
            .hash(&mut refs);
        Ok(RepositoryFingerprint {
            refs: refs.finish(),
            worktree: worktree.finish(),
        })
    }

    pub fn branches(&self) -> Result<Vec<BranchInfo>, String> {
        self.branches_matching(&["refs/heads/", "refs/remotes/"])
    }

    pub(super) fn branches_matching(&self, patterns: &[&str]) -> Result<Vec<BranchInfo>, String> {
        let mut args = vec![
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname)%00%(objectname)%00%(HEAD)%00%(symref)%00%(worktreepath)%00",
        ];
        args.extend_from_slice(patterns);
        parse_branches(&git(&self.root, &args)?)
    }

    pub fn branch_targets(&self) -> Result<Vec<ResetTarget>, String> {
        let output = git(
            &self.root,
            &[
                "for-each-ref",
                "--sort=refname",
                "--format=%(refname)%00%(refname:short)%00%(objectname)",
                "refs/heads",
                "refs/remotes",
            ],
        )?;
        let mut targets = Vec::new();
        for record in output.lines().filter(|record| !record.is_empty()) {
            let mut fields = record.split('\0');
            let reference = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Git returned a reset target without a ref".to_owned())?;
            let name = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Git returned a reset target without a name".to_owned())?;
            let commit = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "Git returned a reset target without a commit".to_owned())?;
            if reference.starts_with("refs/remotes/") && name.ends_with("/HEAD") {
                continue;
            }
            targets.push(ResetTarget {
                name: name.to_owned(),
                reference: reference.to_owned(),
                commit: commit.to_owned(),
            });
        }
        Ok(targets)
    }

    pub fn reset_context(&self) -> Result<ResetContext, String> {
        let current_branch = git(&self.root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map_err(|_| "Checkout a branch before resetting".to_owned())?
            .trim()
            .to_owned();
        let current_head = git(&self.root, &["rev-parse", "--verify", "HEAD"])
            .map_err(|_| "No commits to reset".to_owned())?
            .trim()
            .to_owned();
        let mut targets = self.branch_targets()?;
        if targets.is_empty() {
            return Err("No branch targets available".to_owned());
        }
        let current_ref = format!("refs/heads/{current_branch}");
        targets.sort_by(|left, right| {
            let left_key = (
                left.reference != current_ref,
                left.reference.starts_with("refs/remotes/"),
                left.name.as_str(),
            );
            let right_key = (
                right.reference != current_ref,
                right.reference.starts_with("refs/remotes/"),
                right.name.as_str(),
            );
            left_key.cmp(&right_key)
        });
        Ok(ResetContext {
            current_branch,
            current_head,
            targets,
        })
    }

    pub fn fetch_remote_refs(&self) -> Result<(), String> {
        git(&self.root, &["fetch", "--all", "--prune"]).map(|_| ())
    }

    #[cfg(test)]
    pub fn status_and_refs(&self) -> Result<(String, String), String> {
        Ok((
            git(&self.root, &["status", "--porcelain=v2", "--branch"])?,
            git(&self.root, &["show-ref", "--head"]).unwrap_or_default(),
        ))
    }
}

fn parse_branches(mut output: &str) -> Result<Vec<BranchInfo>, String> {
    let invalid_record = || "Git returned an invalid branch record.".to_owned();
    let mut branches = Vec::new();
    while !output.is_empty() {
        let mut fields = [""; 5];
        for field in &mut fields {
            let (value, remaining) = output.split_once('\0').ok_or_else(invalid_record)?;
            *field = value;
            output = remaining;
        }
        let [reference, commit, head, symref, worktree] = fields;
        if reference.is_empty() || commit.is_empty() {
            return Err(invalid_record());
        }
        if !output.is_empty() {
            output = output.strip_prefix('\n').ok_or_else(invalid_record)?;
        }
        if !symref.is_empty()
            || !(reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/"))
        {
            continue;
        }
        branches.push(BranchInfo {
            reference: reference.to_owned(),
            commit: commit.to_owned(),
            is_current: head == "*",
            worktree_path: (!worktree.is_empty()).then(|| PathBuf::from(worktree)),
        });
    }
    Ok(branches)
}

pub(super) fn is_github_remote(url: &str) -> bool {
    [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn recognizes_supported_github_remote_forms() {
        assert!(is_github_remote("git@github.com:JeHwanYoo/herdr-git.git"));
        assert!(is_github_remote("https://github.com/JeHwanYoo/herdr-git"));
        assert!(!is_github_remote("https://gitlab.com/a/b"));
    }

    #[test]
    fn branch_management_lists_full_refs_and_excludes_only_symbolic_aliases() {
        let repository = repository("namespaces");
        let root = repository.root();
        git(root, &["commit", "--allow-empty", "-m", "Base"]).unwrap();
        let commit = git(root, &["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        git(root, &["branch", "topic"]).unwrap();
        for reference in [
            "refs/remotes/origin/topic",
            "refs/remotes/backup/HEAD",
            "refs/other/topic",
        ] {
            git(root, &["update-ref", reference, &commit]).unwrap();
        }
        git(
            root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/topic",
            ],
        )
        .unwrap();
        git(root, &["tag", "topic"]).unwrap();
        let branches = repository.branches().unwrap();
        assert_eq!(
            branches
                .iter()
                .map(|branch| branch.reference.as_str())
                .collect::<Vec<_>>(),
            [
                "refs/heads/main",
                "refs/heads/topic",
                "refs/remotes/backup/HEAD",
                "refs/remotes/origin/topic",
            ]
        );
        assert_eq!(repository.branches_matching(&["refs/"]).unwrap(), branches);
        assert!(branches.iter().all(|branch| branch.commit == commit));
        assert!(branches[0].is_current);
        assert_eq!(branches[0].worktree_path.as_deref(), Some(root));
        assert!(branches[0].is_local());
        assert_eq!(branches[1].name(), "topic");
        assert!(!branches[2].is_local());
        assert_eq!(branches[2].name(), "backup/HEAD");
        assert_eq!(branches[3].name(), "origin/topic");
        assert!(
            branches[1..]
                .iter()
                .all(|branch| !branch.is_current && branch.worktree_path.is_none())
        );
        assert_eq!(
            repository.branches_matching(&["refs/heads/topic"]).unwrap(),
            [branches[1].clone()]
        );
        git(root, &["checkout", "--detach", &commit]).unwrap();
        let detached = repository.branches().unwrap();
        assert_eq!(
            detached
                .iter()
                .map(|branch| (&branch.reference, &branch.commit))
                .collect::<Vec<_>>(),
            branches
                .iter()
                .map(|branch| (&branch.reference, &branch.commit))
                .collect::<Vec<_>>()
        );
        assert!(
            detached
                .iter()
                .all(|branch| !branch.is_current && branch.worktree_path.is_none())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_unborn_repository_has_no_branches() {
        let repository = repository("unborn");
        assert!(repository.branches().unwrap().is_empty());
        fs::remove_dir_all(repository.root()).unwrap();
    }

    #[test]
    fn branch_management_preserves_linked_worktree_spaces_and_newlines() {
        let repository = repository("worktree");
        let root = repository.root();
        git(root, &["commit", "--allow-empty", "-m", "Base"]).unwrap();
        let linked = root.join(" linked \n checkout \n");
        git(
            root,
            &[
                "worktree",
                "add",
                "-b",
                "a-linked",
                linked.to_str().unwrap(),
            ],
        )
        .unwrap();
        let branches = repository.branches().unwrap();
        let linked_branch = branches
            .iter()
            .find(|branch| branch.reference == "refs/heads/a-linked")
            .unwrap();
        assert_eq!(linked_branch.worktree_path.as_ref(), Some(&linked));
        assert!(!linked_branch.is_current);
        let main = branches
            .iter()
            .find(|branch| branch.reference == "refs/heads/main")
            .unwrap();
        assert!(main.is_current);
        assert_eq!(main.worktree_path.as_deref(), Some(root));
        let linked_repository = Repository::at_root(linked.clone());
        let linked_branches = linked_repository.branches().unwrap();
        assert!(
            linked_branches
                .iter()
                .find(|branch| branch.reference == "refs/heads/a-linked")
                .unwrap()
                .is_current
        );
        assert!(
            !linked_branches
                .iter()
                .find(|branch| branch.reference == "refs/heads/main")
                .unwrap()
                .is_current
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn branch_management_rejects_incomplete_or_missing_required_fields() {
        for output in [
            "\n",
            "refs/heads/topic\0",
            "refs/heads/topic\0abc\0 \0\0",
            "\0abc\0 \0\0\0\n",
            "refs/heads/topic\0\0 \0\0\0\n",
            "refs/heads/topic\0abc\0 \0\0\0\n\n",
            "refs/heads/topic\0abc\0 \0\0\0trailing",
        ] {
            assert_eq!(
                parse_branches(output),
                Err("Git returned an invalid branch record.".to_owned()),
                "{output:?}"
            );
        }
        assert_eq!(parse_branches(""), Ok(Vec::new()));
    }

    #[test]
    fn branch_management_parses_nul_records_without_trimming_worktree_paths() {
        let output = "refs/heads/first\0abc\0*\0\0\n path \n\0\nrefs/heads/second\0def\0 \0\0\0\n";
        let branches = parse_branches(output).unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].worktree_path, Some(PathBuf::from("\n path \n")));
        assert_eq!(branches[1].reference, "refs/heads/second");
        assert_eq!(branches[1].worktree_path, None);
        assert_eq!(
            parse_branches(output.strip_suffix('\n').unwrap()).unwrap(),
            branches
        );
    }

    fn repository(name: &str) -> Repository {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("herdr-branches-{name}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        git(&root, &["init", "-b", "main"]).unwrap();
        git(&root, &["config", "user.name", "Test Author"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        Repository::at_root(root)
    }
}
