use super::*;

impl Repository {
    pub fn change_overview(&self, report: &WorktreeReport) -> Result<ChangeOverview, String> {
        let changed_paths = report.changed_paths;
        if changed_paths == 0 {
            return Ok(ChangeOverview::default());
        }
        let staged = self.diff_summary(&DiffTarget::IndexAgainstHead)?;
        let unstaged = self.diff_summary_including_untracked(
            &DiffTarget::WorkingTreeAgainstIndex,
            report.has_untracked,
        )?;
        Ok(ChangeOverview {
            changed_paths,
            additions: staged.additions.saturating_add(unstaged.additions),
            deletions: staged.deletions.saturating_add(unstaged.deletions),
        })
    }

    pub fn details(&self, commit: &Commit) -> Result<CommitDetails, String> {
        let output = git(
            &self.root,
            &[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-status",
                "-r",
                "-M",
                "-C",
                &commit.sha,
            ],
        )?;
        Ok(CommitDetails {
            commit: commit.clone(),
            changes: parse_changes(&output),
        })
    }

    pub fn changes_comparison(&self, comparison: &ChangesComparison) -> Result<DiffTarget, String> {
        if let ChangesComparison::Between { base, compare } = comparison {
            return self.comparison_between(base, compare);
        }
        let head = self.resolve_comparison_commit("HEAD").ok();
        let dirty = !self.working_changes()?.is_empty();
        let Some(head) = head else {
            return Ok(DiffTarget::WorkingTreeAgainstRevision {
                base: empty_tree(self)?,
            });
        };
        if dirty {
            return Ok(DiffTarget::WorkingTreeAgainstRevision { base: head });
        }
        let previous = self.resolve_comparison_commit("HEAD^1").ok();
        let Ok(base) = self.branch_start() else {
            return Ok(DiffTarget::CommitAgainstParent {
                commit: head,
                parent: previous,
            });
        };
        let parent = if let Some(previous) = previous {
            if git(
                &self.root,
                &["merge-base", "--is-ancestor", &base, &previous],
            )
            .is_ok()
            {
                previous
            } else {
                head.clone()
            }
        } else {
            head.clone()
        };
        Ok(DiffTarget::CommitAgainstParent {
            commit: head,
            parent: Some(parent),
        })
    }

    pub fn comparison_titles(&self, target: &DiffTarget) -> Result<Vec<(String, String)>, String> {
        let revisions: Vec<&str> = match target {
            DiffTarget::CommitAgainstParent { commit, parent } => parent
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(commit.as_str()))
                .collect(),
            DiffTarget::WorkingTreeAgainstRevision { base } => vec![base],
            DiffTarget::IndexAgainstHead => vec!["HEAD"],
            DiffTarget::WorkingTreeAgainstIndex => return Ok(Vec::new()),
        };
        let mut args = vec!["log", "--no-walk", "--format=%H%x00%s", "--end-of-options"];
        args.extend(revisions);
        args.push("--");
        Ok(git(&self.root, &args)?
            .lines()
            .filter_map(|line| {
                let (sha, subject) = line.split_once('\0')?;
                Some((sha.to_owned(), subject.to_owned()))
            })
            .collect())
    }

    pub fn comparison_between(&self, base: &str, compare: &str) -> Result<DiffTarget, String> {
        Ok(DiffTarget::CommitAgainstParent {
            parent: Some(self.resolve_comparison_commit(base)?),
            commit: self.resolve_comparison_commit(compare)?,
        })
    }

    pub(super) fn resolve_comparison_commit(&self, revision: &str) -> Result<String, String> {
        let revision = revision.trim();
        if revision.is_empty() {
            return Err("Choose a branch or commit.".into());
        }
        git(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ],
        )
        .map(|value| value.trim().to_owned())
    }

    pub fn comparison_changes(&self, target: &DiffTarget) -> Result<Vec<WorkingChange>, String> {
        if let DiffTarget::WorkingTreeAgainstRevision { base } = target {
            let text = git(
                &self.root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--name-status",
                    "-M",
                    "-C",
                    base,
                    "--",
                ],
            )?;
            let mut changes = parse_working_changes(&text, ChangeSection::Working);
            for path in self.untracked_paths()? {
                if !changes.iter().any(|change| change.path == path) {
                    changes.push(WorkingChange {
                        section: ChangeSection::Working,
                        status: "??".into(),
                        path,
                    });
                }
            }
            changes.sort_by(|a, b| a.path.cmp(&b.path));
            return Ok(changes);
        }
        let DiffTarget::CommitAgainstParent { commit, parent } = target else {
            return self.working_changes();
        };
        let base = parent.clone().map(Ok).unwrap_or_else(|| empty_tree(self))?;
        let text = git(
            &self.root,
            &[
                "diff",
                "--no-ext-diff",
                "--name-status",
                "-M",
                "-C",
                &base,
                commit,
                "--",
            ],
        )?;
        Ok(parse_working_changes(&text, ChangeSection::Commit))
    }

    pub fn working_changes(&self) -> Result<Vec<WorkingChange>, String> {
        let mut changes = parse_working_changes(
            &git(&self.root, &["diff", "--name-status", "-M", "-C"])?,
            ChangeSection::Unstaged,
        );
        changes.extend(
            self.untracked_paths()?
                .into_iter()
                .map(|path| WorkingChange {
                    section: ChangeSection::Unstaged,
                    status: "??".to_owned(),
                    path,
                }),
        );
        changes.extend(parse_working_changes(
            &git(
                &self.root,
                &["diff", "--cached", "--name-status", "-M", "-C"],
            )?,
            ChangeSection::Staged,
        ));
        changes.sort_by(|left, right| {
            section_order(left.section)
                .cmp(&section_order(right.section))
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(changes)
    }

    pub fn diff_with_context(
        &self,
        target: &DiffTarget,
        path: Option<&str>,
        context: Option<usize>,
    ) -> Result<String, String> {
        if matches!(
            target,
            DiffTarget::WorkingTreeAgainstIndex | DiffTarget::WorkingTreeAgainstRevision { .. }
        ) && let Some(path) = path
            && self.is_untracked(path)?
        {
            return self.untracked_diff(path, context);
        }
        let mut args = vec![
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--find-copies",
        ];
        let context_arg = context.map(|lines| format!("--unified={lines}"));
        if let Some(context_arg) = &context_arg {
            args.push(context_arg);
        }
        let parent;
        match target {
            DiffTarget::WorkingTreeAgainstRevision { base } => args.push(base),
            DiffTarget::WorkingTreeAgainstIndex => {}
            DiffTarget::IndexAgainstHead => args.push("--cached"),
            DiffTarget::CommitAgainstParent {
                commit,
                parent: selected,
            } => {
                parent = selected
                    .clone()
                    .unwrap_or_else(|| empty_tree(self).unwrap_or_default());
                args.push(&parent);
                args.push(commit);
            }
        }
        if let Some(path) = path {
            args.push("--");
            args.push(path);
        }
        git(&self.root, &args)
    }

    fn is_untracked(&self, path: &str) -> Result<bool, String> {
        git(
            &self.root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                path,
            ],
        )
        .map(|output| output.split('\0').any(|candidate| candidate == path))
    }

    pub fn files(&self) -> Result<Vec<String>, String> {
        git(
            &self.root,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
        )
        .map(|output| {
            let mut paths = output
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            paths
        })
    }

    fn untracked_paths(&self) -> Result<Vec<String>, String> {
        git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )
        .map(|output| {
            output
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect()
        })
    }

    fn untracked_diff(&self, path: &str, context: Option<usize>) -> Result<String, String> {
        if let Some(target) = self.untracked_symlink_target(path)? {
            return Ok(symlink_addition_diff(path, &target));
        }
        let mut args = vec!["diff", "--no-index", "--no-ext-diff", "--no-color"];
        let context_arg = context.map(|lines| format!("--unified={lines}"));
        if let Some(context_arg) = &context_arg {
            args.push(context_arg);
        }
        args.extend(["--", "/dev/null", path]);
        git_difference(&self.root, &args)
    }

    pub fn diff_summary(&self, target: &DiffTarget) -> Result<DiffSummary, String> {
        self.diff_summary_including_untracked(target, true)
    }

    fn diff_summary_including_untracked(
        &self,
        target: &DiffTarget,
        include_untracked: bool,
    ) -> Result<DiffSummary, String> {
        let mut args = vec!["diff", "--numstat", "--find-renames", "--find-copies"];
        let parent;
        match target {
            DiffTarget::WorkingTreeAgainstRevision { base } => args.push(base),
            DiffTarget::WorkingTreeAgainstIndex => {}
            DiffTarget::IndexAgainstHead => args.push("--cached"),
            DiffTarget::CommitAgainstParent {
                commit,
                parent: selected,
            } => {
                parent = selected
                    .clone()
                    .unwrap_or_else(|| empty_tree(self).unwrap_or_default());
                args.push(&parent);
                args.push(commit);
            }
        }
        let mut summary = parse_numstat(&git(&self.root, &args)?);
        if include_untracked
            && matches!(
                target,
                DiffTarget::WorkingTreeAgainstIndex | DiffTarget::WorkingTreeAgainstRevision { .. }
            )
        {
            for path in self.untracked_paths()? {
                let untracked = self.untracked_summary(&path)?;
                summary.files += untracked.files;
                summary.additions += untracked.additions;
                summary.deletions += untracked.deletions;
            }
        }
        Ok(summary)
    }

    fn untracked_symlink_target(&self, path: &str) -> Result<Option<String>, String> {
        let absolute = self.root.join(path);
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_symlink() => fs::read_link(&absolute)
                .map(|target| Some(target.to_string_lossy().into_owned()))
                .map_err(|error| file_read_error(&absolute, error)),
            _ => Ok(None),
        }
    }

    fn untracked_summary(&self, path: &str) -> Result<DiffSummary, String> {
        if self.untracked_symlink_target(path)?.is_some() {
            return Ok(DiffSummary {
                files: 1,
                additions: 1,
                deletions: 0,
            });
        }
        let absolute = self.root.join(path);
        match fs::symlink_metadata(&absolute) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if read_cancelled() {
                    return Err(GIT_READ_CANCELLED.to_owned());
                }
                let file =
                    fs::File::open(&absolute).map_err(|error| file_read_error(&absolute, error))?;
                summarize_untracked_reader(file, &absolute)
            }
            _ => {
                let output = git_difference(
                    &self.root,
                    &["diff", "--no-index", "--numstat", "--", "/dev/null", path],
                )?;
                Ok(parse_numstat(&output))
            }
        }
    }

    pub fn blame(
        &self,
        path: &str,
        line: usize,
        revision: Option<&str>,
    ) -> Result<BlameInfo, String> {
        let range = format!("{line},{line}");
        let mut args = vec!["blame", "--line-porcelain", "-L", &range];
        if let Some(revision) = revision {
            args.push(revision);
        }
        args.push("--");
        args.push(path);
        parse_blame(&git(&self.root, &args)?)
    }

    pub fn line_history(
        &self,
        path: &str,
        ranges: &[(usize, usize)],
        revision: Option<&str>,
    ) -> Result<Vec<LineHistoryCommit>, String> {
        let ranges = ranges
            .iter()
            .map(|(start, end)| format!("-L{start},{end}:{path}"))
            .collect::<Vec<_>>();
        let mut args = vec![
            "log",
            "--no-color",
            "--max-count=100",
            "--format=%x1e%H%x1f%an%x1f%at%x1f%s",
        ];
        args.extend(ranges.iter().map(String::as_str));
        if let Some(revision) = revision {
            args.push(revision);
        }
        parse_line_history(&git(&self.root, &args)?)
    }

    pub fn blame_range(
        &self,
        path: &str,
        start: usize,
        end: usize,
        revision: Option<&str>,
    ) -> Result<Vec<BlameInfo>, String> {
        let range = format!("{start},{end}");
        let mut args = vec!["blame", "--line-porcelain", "-L", &range];
        if let Some(revision) = revision {
            args.push(revision);
        }
        args.push("--");
        args.push(path);
        parse_blame_range(&git(&self.root, &args)?)
    }
}

pub fn worktree_report(path: &Path) -> Result<WorktreeReport, String> {
    parse_worktree_report(&git(
        path,
        &[
            "-c",
            "status.relativePaths=false",
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ],
    )?)
}

#[cfg(test)]
pub(super) fn summarize_untracked_bytes(bytes: &[u8]) -> DiffSummary {
    let binary = bytes.iter().take(8_000).any(|byte| *byte == 0);
    let additions = if bytes.is_empty() || binary {
        0
    } else {
        bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(!bytes.ends_with(b"\n"))
    };
    DiffSummary {
        files: 1,
        additions,
        deletions: 0,
    }
}

fn symlink_addition_diff(path: &str, target: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 120000\nindex 0000000..0000000\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+{target}\n\\ No newline at end of file\n"
    )
}

pub(super) fn summarize_untracked_reader(
    mut reader: impl Read,
    path: &Path,
) -> Result<DiffSummary, String> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_usize;
    let mut additions = 0_usize;
    let mut binary = false;
    let mut inspected_for_binary = 0_usize;
    let mut last_byte = None;
    loop {
        if read_cancelled() {
            return Err(GIT_READ_CANCELLED.to_owned());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| file_read_error(path, error))?;
        if read == 0 {
            break;
        }
        if read_cancelled() {
            return Err(GIT_READ_CANCELLED.to_owned());
        }
        let bytes = &buffer[..read];
        if inspected_for_binary < 8_000 {
            let remaining = 8_000 - inspected_for_binary;
            let inspected = bytes.len().min(remaining);
            binary |= bytes[..inspected].contains(&0);
            inspected_for_binary += inspected;
        }
        additions += bytes.iter().filter(|byte| **byte == b'\n').count();
        total += read;
        last_byte = bytes.last().copied();
    }
    if total == 0 || binary {
        additions = 0;
    } else if last_byte != Some(b'\n') {
        additions += 1;
    }
    Ok(DiffSummary {
        files: 1,
        additions,
        deletions: 0,
    })
}

fn file_read_error(path: &Path, error: io::Error) -> String {
    format!("could not read {}: {error}", path.display())
}

fn empty_tree(repository: &Repository) -> Result<String, String> {
    git(
        repository.root(),
        &["hash-object", "-t", "tree", "/dev/null"],
    )
    .map(|value| value.trim().to_owned())
}

fn section_order(section: ChangeSection) -> u8 {
    match section {
        ChangeSection::Working => 0,
        ChangeSection::Commit => 0,
        ChangeSection::Staged => 1,
        ChangeSection::Unstaged => 2,
    }
}
