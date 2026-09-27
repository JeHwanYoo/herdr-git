use super::refresh::{highlight_diff, refresh_changes};
use super::*;

pub(in crate::ui) fn start_foreground_workers(
    cancellations: ReadCancellations,
    syntax: SharedSyntax,
) -> Result<ForegroundWorkers, String> {
    let (result_tx, result_rx) = mpsc::channel::<ForegroundResult>();
    let read_tx = start_foreground_lane(
        "herdr-git-read",
        cancellations.clone(),
        Arc::clone(&syntax),
        result_tx.clone(),
    )?;
    let mutation_tx =
        start_foreground_lane("herdr-git-mutation", cancellations, syntax, result_tx)?;
    Ok(ForegroundWorkers {
        read_tx,
        mutation_tx,
        result_rx,
    })
}

fn start_foreground_lane(
    name: &str,
    cancellations: ReadCancellations,
    syntax: SharedSyntax,
    result_tx: Sender<ForegroundResult>,
) -> Result<Sender<ForegroundRequest>, String> {
    let (request_tx, request_rx) = mpsc::channel::<ForegroundRequest>();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let result = run_foreground_job(request, &syntax, &cancellations);
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("Could not start Git worker: {error}"))?;
    Ok(request_tx)
}

pub(in crate::ui) fn run_foreground_job(
    request: ForegroundRequest,
    syntax: &SharedSyntax,
    cancellations: &ReadCancellations,
) -> ForegroundResult {
    match request {
        ForegroundRequest::RebasePlan {
            id,
            path,
            operation,
        } => ForegroundResult::RebasePlan {
            id,
            result: Repository::at_root(path.clone()).rebase_plan(&operation),
            path,
        },
        ForegroundRequest::RebaseRun { id, path, task } => ForegroundResult::RebaseRun {
            id,
            result: Repository::at_root(path.clone()).run_rebase(&task),
            path,
        },
        ForegroundRequest::Operation {
            id,
            path,
            command,
            operation,
        } => {
            let result =
                Repository::discover(&path).and_then(|repository| repository.execute(&operation));
            ForegroundResult::Operation {
                id,
                path,
                command,
                operation,
                result,
            }
        }
        ForegroundRequest::ComparisonTargets { id, path } => ForegroundResult::ComparisonTargets {
            id,
            result: Repository::at_root(path.clone()).branch_targets(),
            path,
        },
        ForegroundRequest::BranchTargets { id, path, command } => ForegroundResult::BranchTargets {
            command,
            id,
            result: Repository::at_root(path.clone()).branch_targets(),
            path,
        },
        ForegroundRequest::ResetContext { id, path } => ForegroundResult::ResetContext {
            id,
            path: path.clone(),
            result: Repository::discover(&path).and_then(|repository| repository.reset_context()),
        },
        ForegroundRequest::LoadAmendMessage { id, path } => ForegroundResult::AmendMessageLoaded {
            id,
            path: path.clone(),
            result: Repository::discover(&path)
                .and_then(|repository| repository.head_commit_message()),
        },
        ForegroundRequest::ListAgentPanes { id, path } => ForegroundResult::AgentPanesListed {
            id,
            path,
            result: list_agents(),
        },
        ForegroundRequest::SendAgentRequest {
            id,
            path,
            agent,
            prompt,
            amend,
        } => {
            let result = send_agent_request(&agent.pane_id, &path, &prompt, amend);
            ForegroundResult::AgentRequestSent {
                id,
                path,
                agent,
                result,
            }
        }
        ForegroundRequest::ValidateName {
            id,
            path,
            command,
            name,
        } => {
            let result = Repository::discover(&path).and_then(|repository| match command {
                CommandId::CreateBranch | CommandId::CreateBranchAtHead => {
                    repository.validate_branch_name(&name)
                }
                CommandId::CreateTag => repository.validate_tag_name(&name),
                _ => Err("This operation does not accept a name.".to_owned()),
            });
            ForegroundResult::ValidateName {
                id,
                path,
                command,
                name,
                result,
            }
        }
        ForegroundRequest::Staging {
            id,
            path,
            operation,
        } => {
            let result =
                Repository::discover(&path).and_then(|repository| repository.execute(&operation));
            ForegroundResult::Staging {
                id,
                path,
                operation,
                result,
            }
        }
        ForegroundRequest::Diff {
            id,
            generation,
            owner,
            path,
            file,
            target,
            fold_toggles,
        } => {
            let cancelled = || cancellations.diff.load(Ordering::Acquire) != generation.get();
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.diff),
                generation.get(),
                || {
                    let repository = Repository::discover(&path)?;
                    let diff_text = match repository.diff_with_context(
                        &target,
                        Some(&file),
                        Some(1_000_000),
                    )? {
                        diff if diff.is_empty() => "No diff for this file and target.".to_owned(),
                        diff => diff,
                    };
                    if cancelled() {
                        return Err(GIT_READ_CANCELLED.to_owned());
                    }
                    let highlighted = highlight_diff(
                        syntax.get_or_init(SyntaxHighlighter::new),
                        &diff_text,
                        Some(&file),
                        &fold_toggles,
                        &cancelled,
                    )?;
                    Ok((diff_text, highlighted))
                },
            );
            ForegroundResult::Diff {
                id,
                generation,
                owner,
                path,
                file,
                target,
                fold_toggles,
                result,
            }
        }
        ForegroundRequest::Refold {
            id,
            generation,
            target,
            text,
        } => {
            let cancelled = || cancellations.diff.load(Ordering::Acquire) != generation.get();
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.diff),
                generation.get(),
                || {
                    let highlighted = highlight_diff(
                        syntax.get_or_init(SyntaxHighlighter::new),
                        &text,
                        Some(&target.file),
                        &target.fold_toggles,
                        &cancelled,
                    )?;
                    Ok((text, highlighted))
                },
            );
            ForegroundResult::Diff {
                id,
                generation,
                owner: target.owner,
                path: target.path,
                file: target.file,
                target: target.target,
                fold_toggles: target.fold_toggles,
                result,
            }
        }
        ForegroundRequest::CommitDetails {
            id,
            generation,
            target,
        } => {
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.details),
                generation.get(),
                || {
                    Repository::discover(&target.path)
                        .and_then(|repository| repository.details(&target.commit))
                },
            );
            ForegroundResult::CommitDetails {
                id,
                generation,
                target,
                result: Box::new(result),
            }
        }
        ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } => {
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.blame),
                generation.get(),
                || {
                    Repository::discover(&path)
                        .and_then(|repository| repository.blame(&file, line, revision.as_deref()))
                },
            );
            ForegroundResult::Blame {
                id,
                generation,
                path,
                file,
                line,
                revision,
                result,
            }
        }
        ForegroundRequest::SelectionBlame {
            id,
            generation,
            target,
        } => {
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.selection_blame),
                generation.get(),
                || {
                    let (Some(&start), Some(&end)) = (target.lines.first(), target.lines.last())
                    else {
                        return Ok(Vec::new());
                    };
                    Repository::discover(&target.path).and_then(|repository| {
                        repository.blame_range(&target.file, start, end, target.revision.as_deref())
                    })
                },
            )
            .map(|entries| {
                entries
                    .into_iter()
                    .filter(|entry| target.lines.contains(&entry.line))
                    .collect()
            });
            ForegroundResult::SelectionBlame {
                id,
                generation,
                target,
                result,
            }
        }
        ForegroundRequest::LineHistory {
            id,
            generation,
            target,
        } => {
            let cancelled =
                || cancellations.line_history.load(Ordering::Acquire) != generation.get();
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.line_history),
                generation.get(),
                || {
                    let repository = Repository::discover(&target.path)?;
                    repository
                        .line_history(&target.file, &target.ranges, target.revision.as_deref())?
                        .into_iter()
                        .map(|commit| {
                            let hunks = commit.patch.split_once("\n@@").map_or_else(
                                || commit.patch.clone(),
                                |(_, rest)| format!("@@{rest}"),
                            );
                            let highlighted = highlight_diff(
                                syntax.get_or_init(SyntaxHighlighter::new),
                                &hunks,
                                Some(&target.file),
                                &HashSet::new(),
                                &cancelled,
                            )?;
                            Ok(LineHistoryEntry {
                                commit,
                                document: highlighted.split,
                            })
                        })
                        .collect()
                },
            );
            ForegroundResult::LineHistory {
                id,
                generation,
                result,
            }
        }
        ForegroundRequest::AddProject { id, mut registry } => {
            let result = match pick_project_directory() {
                Ok(Some(path)) => match registry.add(&path) {
                    Ok(true) => Ok((registry, ProjectMutationOutcome::Added)),
                    Ok(false) => Ok((registry, ProjectMutationOutcome::AlreadyRegistered)),
                    Err(error) => Err(error),
                },
                Ok(None) => Ok((registry, ProjectMutationOutcome::Cancelled)),
                Err(error) => Err(error),
            };
            ForegroundResult::ProjectMutation { id, result }
        }
        ForegroundRequest::RemoveProject {
            id,
            mut registry,
            root,
        } => {
            let result = match registry.remove(&root) {
                Ok(true) => Ok((registry, ProjectMutationOutcome::Removed)),
                Ok(false) => Ok((registry, ProjectMutationOutcome::NotRegistered)),
                Err(error) => Err(error),
            };
            ForegroundResult::ProjectMutation { id, result }
        }
        ForegroundRequest::Switch {
            id,
            generation,
            path,
        } => {
            let cancelled = || cancellations.switch.load(Ordering::Acquire) != generation.get();
            let result = with_read_cancellation_result(
                Arc::clone(&cancellations.switch),
                generation.get(),
                || {
                    load_repository_snapshot(
                        &path,
                        syntax.get_or_init(SyntaxHighlighter::new),
                        &cancelled,
                    )
                    .map(Box::new)
                },
            );
            ForegroundResult::Switch {
                id,
                generation,
                path,
                result,
            }
        }
        ForegroundRequest::Fetch { id, roots } => {
            fetch_remote_refs(&roots);
            ForegroundResult::Fetch { id }
        }
    }
}

fn fetch_remote_refs(roots: &[PathBuf]) {
    let mut fetched = HashSet::new();
    for path in roots {
        let Ok(repository) = Repository::discover(path) else {
            continue;
        };
        let root = repository.root().to_owned();
        if fetched.insert(root) {
            let _ = repository.fetch_remote_refs();
        }
    }
}

pub(in crate::ui) fn load_repository_snapshot(
    path: &Path,
    syntax: &SyntaxHighlighter,
    cancelled: &dyn Fn() -> bool,
) -> Result<RepositorySnapshot, String> {
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let repository = Repository::discover(path)?;
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let root = repository.root().to_owned();
    let local_identity = repository.local_identity();
    let github_origin = repository.origin_is_github();
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let fingerprint = repository.refresh_fingerprint().ok();
    let context = RefreshContext {
        generation: ReadGeneration::ZERO,
        invoking_path: root.clone(),
        project_roots: Vec::new(),
        expanded_projects: Vec::new(),
        active_repository: Some(root.clone()),
        history_loaded: false,
        wants_history: false,
        query: String::new(),
        selected_commit_sha: None,
        selected_change: None,
        comparison: Some(crate::git::ChangesComparison::Last),
        diff_target: DiffTarget::WorkingTreeAgainstIndex,
        fold_toggles: HashSet::new(),
    };
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let changes = match refresh_changes(&repository, &context, &[], None, syntax, cancelled) {
        Ok(changes) => changes,
        Err(error) if error.starts_with("Branch start unavailable") => ChangesRefresh {
            changes: Vec::new(),
            selected: 0,
            target: DiffTarget::WorkingTreeAgainstIndex,
            commit_titles: Vec::new(),
            summaries: Vec::new(),
            highlighted: Some(highlight_diff(
                syntax,
                &error,
                None,
                &HashSet::new(),
                cancelled,
            )?),
            diff_text: error,
        },
        Err(error) => return Err(error),
    };
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    let command_context = CommandContext::from_git_facts(repository.command_facts(), None);
    if cancelled() {
        return Err(GIT_READ_CANCELLED.to_owned());
    }
    Ok(RepositorySnapshot {
        repository,
        root,
        local_identity,
        github_origin,
        fingerprint,
        changes,
        command_context,
    })
}
