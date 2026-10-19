use super::*;

pub(in crate::ui) fn start_refresh_worker(
    cancellation_generation: Arc<AtomicU64>,
    syntax: SharedSyntax,
) -> Result<RefreshWorker, String> {
    let (request_tx, request_rx) = mpsc::channel::<RefreshRequest>();
    let (result_tx, result_rx) = mpsc::channel::<RefreshResult>();
    let (workspace_tx, workspace_rx) = mpsc::channel();
    thread::Builder::new()
        .name("herdr-git-refresh".to_owned())
        .spawn(move || {
            let syntax = syntax.get_or_init(SyntaxHighlighter::new);
            while let Ok(request) = request_rx.recv() {
                let expected = request.context.generation.get();
                let result =
                    with_read_cancellation(Arc::clone(&cancellation_generation), expected, || {
                        run_refresh_job(request, syntax, &cancellation_generation, &workspace_tx)
                    });
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("Could not start refresh worker: {error}"))?;
    Ok(RefreshWorker {
        request_tx,
        result_rx,
        workspace_rx,
    })
}

pub(in crate::ui) fn run_refresh_job(
    request: RefreshRequest,
    syntax: &SyntaxHighlighter,
    cancellation_generation: &AtomicU64,
    workspace_tx: &Sender<WorkspaceRefresh>,
) -> RefreshResult {
    let expected = request.context.generation.get();
    let cancelled = || cancellation_generation.load(Ordering::Acquire) != expected;
    if cancelled() {
        return cancelled_refresh_result(request);
    }
    if request.inspect_workspaces {
        let Some((project_statuses, repository_rows)) = inspect_repository_rows_cancellable(
            &request.context,
            &request.previous_worktrees,
            &cancelled,
        ) else {
            return cancelled_refresh_result(request);
        };
        if workspace_tx
            .send(WorkspaceRefresh {
                id: request.id,
                intent: request.intent,
                context: request.context.clone(),
                project_statuses,
                repository_rows,
            })
            .is_err()
        {
            return cancelled_refresh_result(request);
        }
    }
    let active = if request.scope == RefreshScope::Workspaces {
        None
    } else {
        request
            .context
            .active_repository
            .as_deref()
            .map(|path| refresh_active_repository(path, &request, syntax, &cancelled))
    };
    if cancelled() {
        return cancelled_refresh_result(request);
    }
    RefreshResult {
        id: request.id,
        intent: request.intent,
        context: request.context,
        cancelled: false,
        active,
    }
}

fn cancelled_refresh_result(request: RefreshRequest) -> RefreshResult {
    RefreshResult {
        id: request.id,
        intent: request.intent,
        context: request.context,
        cancelled: true,
        active: None,
    }
}

fn inspect_repository_rows_cancellable(
    context: &RefreshContext,
    previous: &InspectedWorktrees,
    cancelled: &dyn Fn() -> bool,
) -> Option<RepositoryInspection> {
    if cancelled() {
        return None;
    }
    let current = inspect_worktree(&context.invoking_path, previous.current.as_ref()).ok();
    let current_row = match &current {
        Some(current) => RepositoryRow {
            kind: RepositoryRowKind::Current,
            path: current.path.clone(),
            overview: Some(current.overview),
            branch_status: Some(current.branch_status.clone()),
            worktree_count: 0,
            error: None,
            fingerprint: Some(current.fingerprint),
        },
        None => RepositoryRow {
            kind: RepositoryRowKind::Current,
            path: context.invoking_path.clone(),
            overview: None,
            branch_status: None,
            worktree_count: 0,
            error: Some("Not a Git repository".to_owned()),
            fingerprint: None,
        },
    };
    if cancelled() {
        return None;
    }
    let mut project_statuses = Vec::with_capacity(context.project_roots.len());
    for root in &context.project_roots {
        if cancelled() {
            return None;
        }
        project_statuses.push(inspect_project(root, &previous.projects, current.as_ref()));
    }
    if cancelled() {
        return None;
    }
    let rows = repository_rows_from_statuses(
        &current_row,
        &context.project_roots,
        &project_statuses,
        &context.expanded_projects,
    );
    Some((project_statuses, rows))
}

fn refresh_active_repository(
    path: &Path,
    request: &RefreshRequest,
    syntax: &SyntaxHighlighter,
    cancelled: &dyn Fn() -> bool,
) -> Result<ActiveRefresh, String> {
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let context = &request.context;
    let repository = Repository::discover(path)?;
    let fingerprint = repository.refresh_fingerprint()?;
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let refs_changed = request.previous_fingerprint.refs_changed(fingerprint);
    let worktree_changed = request.previous_fingerprint.worktree_changed(fingerprint);
    let first_history_load = context.wants_history && !context.history_loaded;
    if !refs_changed && !worktree_changed && !first_history_load {
        return Ok(ActiveRefresh::Unchanged { fingerprint });
    }

    let history =
        context.wants_history && refs_changed && request.previous_fingerprint.refs.is_some();
    let selected_commit = context.selected_commit_sha.clone();
    let command_context =
        CommandContext::from_git_facts(repository.command_facts(), selected_commit);
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let commit_target = context.comparison.is_some()
        || matches!(context.diff_target, DiffTarget::CommitAgainstParent { .. });
    let changes = (worktree_changed || (commit_target && refs_changed)).then(|| {
        refresh_changes(
            &repository,
            context,
            &request.changes,
            request.previous_highlight,
            syntax,
            cancelled,
        )
    });
    Ok(ActiveRefresh::Changed(Box::new(ChangedRefresh {
        fingerprint,
        history,
        changes,
        command_context,
    })))
}

pub(super) fn refresh_changes(
    repository: &Repository,
    context: &RefreshContext,
    current_changes: &[WorkingChange],
    previous_highlight: Option<u64>,
    syntax: &SyntaxHighlighter,
    cancelled: &dyn Fn() -> bool,
) -> Result<ChangesRefresh, String> {
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let local_target = matches!(
        context.diff_target,
        DiffTarget::WorkingTreeAgainstIndex | DiffTarget::IndexAgainstHead
    );
    let comparison_target = context
        .comparison
        .as_ref()
        .map(|comparison| repository.changes_comparison(comparison))
        .transpose()?;
    let local_sections = comparison_target.as_ref().is_some_and(|target| {
        matches!(
            (context.comparison.as_ref(), target),
            (
                Some(crate::git::ChangesComparison::Last),
                DiffTarget::WorkingTreeAgainstRevision { .. }
            )
        )
    });
    let changes = if local_sections {
        repository.working_changes()?
    } else if let Some(target) = &comparison_target {
        repository.comparison_changes(target)?
    } else if local_target {
        repository.working_changes()?
    } else {
        current_changes.to_vec()
    };
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let selected = context
        .selected_change
        .as_ref()
        .and_then(|selected| {
            changes.iter().position(|change| {
                change.section == selected.section && change.path == selected.path
            })
        })
        .unwrap_or_default()
        .min(changes.len().saturating_sub(1));
    let target = (!local_sections)
        .then(|| comparison_target.clone())
        .flatten()
        .or_else(|| {
            changes
                .get(selected)
                .and_then(|change| change.section.diff_target())
        })
        .unwrap_or_else(|| context.diff_target.clone());
    let mut summaries = Vec::new();
    for section in [
        ChangeSection::Working,
        ChangeSection::Commit,
        ChangeSection::Staged,
        ChangeSection::Unstaged,
    ] {
        if cancelled() {
            return Err(GIT_READ_CANCELLED.to_owned());
        }
        if !changes.iter().any(|change| change.section == section) {
            continue;
        }
        let section_target = (!local_sections)
            .then(|| comparison_target.clone())
            .flatten()
            .or_else(|| section.diff_target())
            .unwrap_or_else(|| target.clone());
        if let Ok(summary) = repository.diff_summary(&section_target) {
            summaries.push((section, summary));
        }
    }
    let path = changes.get(selected).map(|change| change.path.as_str());
    let diff_text = if changes.is_empty() {
        "No changes.".to_owned()
    } else {
        match repository.diff_with_context(&target, path, Some(1_000_000)) {
            Ok(diff) if diff.is_empty() => "No diff for this file and target.".to_owned(),
            Ok(diff) => diff,
            Err(error) => format!("Error: {error}"),
        }
    };
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let highlighted =
        if previous_highlight == Some(highlight_key(&diff_text, path, &context.fold_toggles)) {
            None
        } else {
            Some(highlight_diff(
                syntax,
                &diff_text,
                path,
                &context.fold_toggles,
                cancelled,
            )?)
        };
    let commit_titles = repository.comparison_titles(&target).unwrap_or_default();
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    Ok(ChangesRefresh {
        commit_titles,
        changes,
        selected,
        target,
        summaries,
        diff_text,
        highlighted,
    })
}

fn highlight_key(diff_text: &str, path: Option<&str>, fold_toggles: &HashSet<FoldKey>) -> u64 {
    let mut folds = fold_toggles
        .iter()
        .map(|key| {
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            hasher.finish()
        })
        .collect::<Vec<_>>();
    folds.sort_unstable();
    let mut hasher = DefaultHasher::new();
    diff_text.hash(&mut hasher);
    path.hash(&mut hasher);
    folds.hash(&mut hasher);
    hasher.finish()
}

pub(super) fn highlight_diff(
    syntax: &SyntaxHighlighter,
    diff_text: &str,
    path: Option<&str>,
    fold_toggles: &HashSet<FoldKey>,
    cancelled: &dyn Fn() -> bool,
) -> Result<HighlightedDiff, String> {
    let language = if diff_text.contains("Binary files ") || diff_text.contains("GIT binary patch")
    {
        "Binary".to_owned()
    } else {
        path.and_then(|path| syntax.language_name(path))
            .unwrap_or("Plain text")
            .to_owned()
    };
    let split = syntax
        .side_by_side_diff_with_folds_cancellable(diff_text, path, fold_toggles, cancelled)
        .ok_or_else(|| GIT_READ_CANCELLED.to_owned())?;
    Ok(HighlightedDiff {
        language,
        split,
        key: highlight_key(diff_text, path, fold_toggles),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::git::{ChangeOverview, git_processes_started};
    use crate::ui::syntax::FoldKind;
    use crate::ui::test_support::{committed_repo, git};
    use crate::ui::workspaces::WorkspacesState;

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
            scope: RefreshScope::Repository,
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
        repository
            .execute(&GitOperation::StagePath("tracked.txt".into()))
            .unwrap();
        let mut context = context_for(&root);
        context.project_roots = vec![root.clone(), other.clone()];
        context.expanded_projects = context.project_roots.clone();
        let mut request = request_for(context, KnownFingerprint::default());
        request.scope = RefreshScope::Changes;
        request.intent = RefreshIntent::Operation;
        let syntax = SyntaxHighlighter::new();
        let (tx, rx) = mpsc::channel();
        let before = git_processes_started();
        let baseline = run_refresh_job(request.clone(), &syntax, &0.into(), &tx);
        let baseline_reads = git_processes_started() - before;
        assert!(rx.try_recv().is_ok());
        request.inspect_workspaces = false;
        let before = git_processes_started();
        let fast = run_refresh_job(request, &syntax, &0.into(), &tx);
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
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn workspaces_only_refresh_publishes_rows_without_reading_the_active_repository() {
        let root = committed_repo("workspace-scope");
        let mut context = context_for(&root);
        context.active_repository = Some(root.join("missing"));
        let mut request = request_for(context, KnownFingerprint::default());
        request.scope = RefreshScope::Workspaces;
        let (tx, rx) = mpsc::channel();
        let syntax = SyntaxHighlighter::new();
        let result = run_refresh_job(request, &syntax, &0.into(), &tx);
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
    ) -> ChangedRefresh {
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

    fn row<'a>(
        inspection: &'a RepositoryInspection,
        path: &Path,
        kind: &RepositoryRowKind,
    ) -> &'a RepositoryRow {
        let mut matches = inspection
            .1
            .iter()
            .filter(|row| row.path == path && &row.kind == kind);
        let row = matches.next().expect("repository row is present");
        assert!(matches.next().is_none(), "repository row is unique");
        row
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
        let previous_of = |inspection: &RepositoryInspection| {
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
        let current_kind = RepositoryRowKind::Current;
        let project_kind = RepositoryRowKind::Project { root: root.clone() };
        let linked_kind = RepositoryRowKind::Worktree {
            project_root: root.clone(),
        };
        assert_eq!(first.1.len(), 3);
        assert_eq!(row(&first, &root, &project_kind).worktree_count, 1);
        assert_eq!(
            row(&first, &root, &current_kind).fingerprint,
            row(&first, &root, &project_kind).fingerprint
        );
        assert!(row(&first, &linked, &linked_kind).fingerprint.is_some());

        let previous = previous_of(&first);
        let before = git_processes_started();
        let second = inspect_repository_rows_cancellable(&context, &previous, &|| false).unwrap();
        assert_eq!(
            git_processes_started() - before,
            3,
            "one status read per worktree plus one worktree list per Project"
        );
        assert_eq!(second.0, first.0);
        for original in &first.1 {
            assert_eq!(row(&second, &original.path, &original.kind), original);
        }

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
        for kind in [&current_kind, &project_kind] {
            assert_eq!(row(&third, &root, kind), row(&first, &root, kind));
        }
        assert_ne!(
            row(&third, &linked, &linked_kind).fingerprint,
            row(&first, &linked, &linked_kind).fingerprint
        );
        assert_eq!(
            row(&third, &linked, &linked_kind)
                .branch_status
                .as_ref()
                .unwrap()
                .checked_out,
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
        for kind in [&current_kind, &project_kind] {
            assert_eq!(row(&fourth, &root, kind), row(&first, &root, kind));
        }
        assert_eq!(
            row(&fourth, &linked, &linked_kind).overview,
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
        for kind in [&current_kind, &project_kind] {
            assert_eq!(row(&fifth, &root, kind), row(&first, &root, kind));
        }
        assert_eq!(
            row(&fifth, &linked, &linked_kind).fingerprint,
            row(&fourth, &linked, &linked_kind).fingerprint
        );
        assert_eq!(
            row(&fifth, &linked, &linked_kind).overview,
            Some(ChangeOverview {
                changed_paths: 1,
                additions: 2,
                deletions: 0,
            })
        );

        fs::remove_dir_all(linked).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
