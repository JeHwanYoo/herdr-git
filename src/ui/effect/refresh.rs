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

pub(super) fn inspect_repository_rows_cancellable(
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

pub(super) fn refresh_active_repository(
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
