use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::git::GitOperation;
use crate::ui::effect::RequestId;
use crate::ui::lanes::{ForegroundAction, ForegroundKind};

use super::TreeSelectionKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) enum StagingOwner {
    Directory(String),
    File(TreeSelectionKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StagingKey {
    pub(super) id: RequestId,
    pub(super) repository: PathBuf,
    pub(super) operation: GitOperation,
}

#[derive(Debug)]
pub(super) enum FilesAction {
    Requested {
        operation: GitOperation,
        owner: Option<StagingOwner>,
        now: Instant,
    },
    Dispatched {
        key: StagingKey,
    },
    DispatchFailed {
        key: StagingKey,
        error: String,
    },
    Finished {
        key: StagingKey,
        result: Result<String, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StagingEffect {
    InvalidateRefresh,
    Run(StagingKey),
    ShowError(String),
    RefreshChanges { completion: String, success: bool },
}

pub(super) struct StagingState<'a> {
    pub(super) repository: Option<&'a Path>,
    pub(super) foreground_action: &'a mut Option<ForegroundAction>,
    pub(super) next_id: &'a mut RequestId,
}

pub(super) fn update_staging(state: StagingState<'_>, action: FilesAction) -> Vec<StagingEffect> {
    match action {
        FilesAction::Requested {
            operation,
            owner,
            now,
        } => {
            if state.foreground_action.is_some() {
                return vec![StagingEffect::ShowError(
                    "Wait for the current Git operation.".to_owned(),
                )];
            }
            let Some(repository) = state.repository else {
                return vec![StagingEffect::ShowError("Not a Git repository".to_owned())];
            };
            let key = StagingKey {
                id: *state.next_id,
                repository: repository.to_owned(),
                operation,
            };
            *state.foreground_action = Some(ForegroundAction {
                id: key.id,
                kind: ForegroundKind::Staging {
                    path: key.repository.clone(),
                    operation: key.operation.clone(),
                    owner,
                },
                started: now,
            });
            vec![StagingEffect::InvalidateRefresh, StagingEffect::Run(key)]
        }
        FilesAction::Dispatched { key } => {
            if owns_staging(state.foreground_action, &key) && *state.next_id == key.id {
                state.next_id.advance();
            }
            Vec::new()
        }
        FilesAction::DispatchFailed { key, error } => {
            if owns_staging(state.foreground_action, &key) {
                *state.foreground_action = None;
                return vec![StagingEffect::ShowError(format!(
                    "Git worker stopped: {error}"
                ))];
            }
            Vec::new()
        }
        FilesAction::Finished { key, result } => {
            if !owns_staging(state.foreground_action, &key) {
                return Vec::new();
            }
            *state.foreground_action = None;
            let (completion, success) = match result {
                Ok(_) => (staging_success_message(&key.operation), true),
                Err(error) => (
                    format!("{} failed: {error}", key.operation.preview()),
                    false,
                ),
            };
            vec![StagingEffect::RefreshChanges {
                completion,
                success,
            }]
        }
    }
}

fn owns_staging(foreground_action: &Option<ForegroundAction>, key: &StagingKey) -> bool {
    foreground_action.as_ref().is_some_and(|action| {
        action.id == key.id
            && matches!(
                &action.kind,
                ForegroundKind::Staging {
                    path,
                    operation,
                    ..
                } if path == &key.repository && operation == &key.operation
            )
    })
}

fn staging_success_message(operation: &GitOperation) -> String {
    match operation {
        GitOperation::StageAll => "Staged all changes".to_owned(),
        GitOperation::UnstageAll => "Unstaged all changes".to_owned(),
        GitOperation::StagePath(path) => format!("Staged {path}"),
        GitOperation::UnstagePath(path) => format!("Unstaged {path}"),
        GitOperation::StagePaths(paths) => format!("Staged {} selected files", paths.len()),
        GitOperation::UnstagePaths(paths) => format!("Unstaged {} selected files", paths.len()),
        _ => "Git index updated".to_owned(),
    }
}

pub(in crate::ui) fn staging_progress_label(operation: &GitOperation) -> &'static str {
    match operation {
        GitOperation::StageAll | GitOperation::StagePath(_) | GitOperation::StagePaths(_) => {
            "Staging"
        }
        GitOperation::UnstageAll | GitOperation::UnstagePath(_) | GitOperation::UnstagePaths(_) => {
            "Unstaging"
        }
        _ => "Updating index",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_lifecycle_emits_closed_effects_and_rejects_stale_completion() {
        let root = Path::new("/work/repository");
        let mut foreground_action = None;
        let mut next_id = RequestId::FIRST;
        let now = Instant::now();
        let effects = update_staging(
            StagingState {
                repository: Some(root),
                foreground_action: &mut foreground_action,
                next_id: &mut next_id,
            },
            FilesAction::Requested {
                operation: GitOperation::StageAll,
                owner: None,
                now,
            },
        );
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0], StagingEffect::InvalidateRefresh);
        let StagingEffect::Run(key) = effects[1].clone() else {
            panic!("staging request must emit one worker effect");
        };
        assert_eq!(key.id, RequestId::FIRST);
        assert_eq!(key.repository, root);
        assert_eq!(key.operation, GitOperation::StageAll);
        assert_eq!(foreground_action.as_ref().unwrap().started, now);
        for operation in [
            GitOperation::UnstageAll,
            GitOperation::UnstagePath("a".to_owned()),
            GitOperation::StagePaths(vec!["b".to_owned()]),
            GitOperation::Fetch,
        ] {
            assert!(!super::staging_progress_label(&operation).ends_with('…'));
        }

        let stale = StagingKey {
            id: RequestId::new(key.id.get().wrapping_add(1)),
            repository: key.repository.clone(),
            operation: key.operation.clone(),
        };
        assert!(
            update_staging(
                StagingState {
                    repository: Some(root),
                    foreground_action: &mut foreground_action,
                    next_id: &mut next_id,
                },
                FilesAction::Finished {
                    key: stale,
                    result: Ok(String::new()),
                },
            )
            .is_empty()
        );
        assert!(foreground_action.is_some());

        assert!(
            update_staging(
                StagingState {
                    repository: Some(root),
                    foreground_action: &mut foreground_action,
                    next_id: &mut next_id,
                },
                FilesAction::Dispatched { key: key.clone() },
            )
            .is_empty()
        );
        assert_eq!(next_id.get(), 2);
        let completion = update_staging(
            StagingState {
                repository: Some(root),
                foreground_action: &mut foreground_action,
                next_id: &mut next_id,
            },
            FilesAction::Finished {
                key,
                result: Ok(String::new()),
            },
        );
        assert_eq!(
            completion,
            vec![StagingEffect::RefreshChanges {
                completion: "Staged all changes".to_owned(),
                success: true,
            }]
        );
        assert!(foreground_action.is_none());
    }

    #[test]
    fn staging_request_without_a_repository_is_a_pure_rejection() {
        let mut foreground_action: Option<ForegroundAction> = None;
        let mut next_id = RequestId::FIRST;

        let effects = update_staging(
            StagingState {
                repository: None,
                foreground_action: &mut foreground_action,
                next_id: &mut next_id,
            },
            FilesAction::Requested {
                operation: GitOperation::StageAll,
                owner: None,
                now: Instant::now(),
            },
        );

        assert_eq!(
            effects,
            vec![StagingEffect::ShowError("Not a Git repository".to_owned())]
        );
        assert!(foreground_action.is_none());
    }
}
