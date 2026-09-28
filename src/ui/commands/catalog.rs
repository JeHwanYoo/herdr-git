use crate::git::{GitCommandFacts, GitOperation, ResetMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum CommandId {
    Refresh,
    Update,
    SwitchRepository,
    AddProject,
    RemoveProject,
    ResetCurrentBranch,
    DiscardTrackedChanges,
    Fetch,
    Pull,
    Commit,
    Push,
    AddRemote,
    ShowRemotes,
    RemoveRemote,
    Stash,
    StashChanges,
    StashPop,
    CreateBranchAtHead,
    CreateBranch,
    CreateTag,
    CheckoutCommit,
    RebaseHere,
    InteractiveRebase,
    CherryPick,
    Revert,
    CopySha,
}

pub(in crate::ui) const COMMANDS: [CommandId; 19] = [
    CommandId::Refresh,
    CommandId::SwitchRepository,
    CommandId::AddRemote,
    CommandId::ShowRemotes,
    CommandId::Fetch,
    CommandId::Pull,
    CommandId::Commit,
    CommandId::Push,
    CommandId::CreateBranchAtHead,
    CommandId::CheckoutCommit,
    CommandId::RebaseHere,
    CommandId::InteractiveRebase,
    CommandId::CherryPick,
    CommandId::Revert,
    CommandId::ResetCurrentBranch,
    CommandId::DiscardTrackedChanges,
    CommandId::CopySha,
    CommandId::Stash,
    CommandId::Update,
];

pub(in crate::ui) const QUICK_ACTIONS: [CommandId; 6] = [
    CommandId::Fetch,
    CommandId::Pull,
    CommandId::Commit,
    CommandId::Push,
    CommandId::CreateBranchAtHead,
    CommandId::StashChanges,
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::ui) struct CommandContext {
    pub(in crate::ui) has_repository: bool,
    pub(in crate::ui) can_remove_project: bool,
    pub(in crate::ui) has_remote: bool,
    pub(in crate::ui) remotes: Vec<crate::git::Remote>,
    pub(in crate::ui) current_branch: Option<String>,
    pub(in crate::ui) upstream: Option<String>,
    pub(in crate::ui) has_changes: bool,
    pub(in crate::ui) has_staged_changes: bool,
    pub(in crate::ui) has_stash: bool,
    pub(in crate::ui) head_commit: Option<String>,
    pub(in crate::ui) selected_commit: Option<String>,
}

impl CommandContext {
    pub(in crate::ui) fn from_git_facts(
        facts: GitCommandFacts,
        selected_commit: Option<String>,
    ) -> Self {
        Self {
            has_repository: true,
            can_remove_project: false,
            has_remote: facts.has_remote,
            remotes: facts.remotes,
            current_branch: facts.current_branch,
            upstream: facts.upstream,
            has_changes: facts.has_changes,
            has_staged_changes: facts.has_staged_changes,
            has_stash: facts.has_stash,
            head_commit: facts.head_commit,
            selected_commit,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) struct Availability {
    pub(in crate::ui) enabled: bool,
    pub(in crate::ui) reason: Option<&'static str>,
}

impl ResetMode {
    pub(in crate::ui) fn label(self) -> &'static str {
        match self {
            Self::Soft => "Soft",
            Self::Mixed => "Mixed",
            Self::Hard => "Hard",
        }
    }

    pub(in crate::ui) fn summary(self) -> &'static str {
        match self {
            Self::Soft => "Keep index and working tree",
            Self::Mixed => "Reset index; keep working tree",
            Self::Hard => "Reset index and tracked working tree",
        }
    }

    pub(in crate::ui) fn effect(self) -> &'static str {
        match self {
            Self::Soft => {
                "The current branch moves to the target. The index and working tree are preserved."
            }
            Self::Mixed => {
                "The current branch and index move to the target. The working tree is preserved."
            }
            Self::Hard => {
                "The current branch, index, and tracked working tree move to the target. Tracked changes will be discarded. Obstructing untracked paths may be removed."
            }
        }
    }
}

impl GitOperation {
    pub(in crate::ui) fn command(&self) -> Option<CommandId> {
        match self {
            Self::Fetch => Some(CommandId::Fetch),
            Self::PullFastForward => Some(CommandId::Pull),
            Self::Commit { .. } => Some(CommandId::Commit),
            Self::Push { .. } => Some(CommandId::Push),
            Self::AddRemote { .. } => Some(CommandId::AddRemote),
            Self::RemoveRemote(_) => Some(CommandId::RemoveRemote),
            Self::StashChanges => Some(CommandId::StashChanges),
            Self::StashPop => Some(CommandId::StashPop),
            Self::CreateBranch { .. } => Some(CommandId::CreateBranch),
            Self::CreateTag { .. } => Some(CommandId::CreateTag),
            Self::CheckoutCommit(_) | Self::CheckoutBranch(_) => Some(CommandId::CheckoutCommit),
            Self::RebaseHere(_) => Some(CommandId::RebaseHere),
            Self::InteractiveRebase(_) | Self::InteractiveRebaseOnto(_) => {
                Some(CommandId::InteractiveRebase)
            }
            Self::CherryPick(_) => Some(CommandId::CherryPick),
            Self::Revert(_) => Some(CommandId::Revert),
            Self::DiscardTrackedChanges { .. } | Self::DiscardUnstagedPaths(_) => {
                Some(CommandId::DiscardTrackedChanges)
            }
            Self::Reset { .. } => Some(CommandId::ResetCurrentBranch),
            Self::StageAll
            | Self::StagePath(_)
            | Self::StagePaths(_)
            | Self::UnstageAll
            | Self::UnstagePath(_)
            | Self::UnstagePaths(_) => None,
        }
    }
}

impl CommandId {
    pub(in crate::ui) fn title(self) -> &'static str {
        match self {
            Self::Refresh => "Refresh",
            Self::Update => "Update Herdr Git…",
            Self::SwitchRepository => "Switch workspace",
            Self::AddProject => "Add Project…",
            Self::RemoveProject => "Remove Project",
            Self::ResetCurrentBranch => "Reset…",
            Self::DiscardTrackedChanges => "Discard all tracked changes…",
            Self::Fetch => "Fetch",
            Self::Pull => "Pull",
            Self::Commit => "Commit…",
            Self::Push => "Push",
            Self::AddRemote => "Add Remotes…",
            Self::ShowRemotes => "Show Remotes…",
            Self::RemoveRemote => "Remove Remote",
            Self::Stash => "Stash…",
            Self::StashChanges => "Stash changes",
            Self::StashPop => "Pop stash",
            Self::CreateBranchAtHead => "Branch…",
            Self::CreateBranch => "Branch at commit…",
            Self::CreateTag => "Tag…",
            Self::CheckoutCommit => "Checkout…",
            Self::RebaseHere => "Rebase…",
            Self::InteractiveRebase => "Interactive rebase…",
            Self::CherryPick => "Cherry-pick…",
            Self::Revert => "Revert…",
            Self::CopySha => "Copy SHA…",
        }
    }

    pub(in crate::ui) fn availability(self, context: &CommandContext) -> Availability {
        let disabled = |reason| Availability {
            enabled: false,
            reason: Some(reason),
        };
        match self {
            Self::Refresh | Self::Update | Self::SwitchRepository | Self::AddProject => {
                Availability {
                    enabled: true,
                    reason: None,
                }
            }
            Self::RemoveProject if !context.can_remove_project => {
                disabled("no registered Project selected")
            }
            Self::RemoveProject => Availability {
                enabled: true,
                reason: None,
            },
            _ if !context.has_repository => disabled("not a Git repository"),
            Self::DiscardTrackedChanges if context.head_commit.is_none() => {
                disabled("no HEAD commit to restore")
            }
            Self::DiscardTrackedChanges if !context.has_changes => {
                disabled("working tree is clean")
            }
            Self::DiscardTrackedChanges => Availability {
                enabled: true,
                reason: None,
            },
            Self::ResetCurrentBranch if context.current_branch.is_none() => {
                disabled("checkout a branch before resetting")
            }
            Self::ResetCurrentBranch if context.head_commit.is_none() => {
                disabled("no commits to reset")
            }
            Self::CreateBranchAtHead if context.head_commit.is_none() => {
                disabled("no commits available")
            }
            Self::ResetCurrentBranch | Self::CreateBranchAtHead => Availability {
                enabled: true,
                reason: None,
            },
            Self::AddRemote | Self::ShowRemotes | Self::RemoveRemote => Availability {
                enabled: true,
                reason: None,
            },
            Self::Fetch if !context.has_remote => disabled("no remotes configured"),
            Self::Fetch => Availability {
                enabled: true,
                reason: None,
            },
            Self::Pull if context.current_branch.is_none() => disabled("detached HEAD"),
            Self::Pull if context.upstream.is_none() => disabled("no upstream configured"),
            Self::Pull => Availability {
                enabled: true,
                reason: None,
            },
            Self::Commit => Availability {
                enabled: true,
                reason: None,
            },
            Self::Push if context.current_branch.is_none() => disabled("detached HEAD"),
            Self::Push if !context.has_remote => disabled("no remotes configured"),
            Self::Push => Availability {
                enabled: true,
                reason: None,
            },
            Self::InteractiveRebase | Self::Stash => Availability {
                enabled: true,
                reason: None,
            },
            Self::StashChanges if !context.has_changes => disabled("working tree is clean"),
            Self::StashChanges => Availability {
                enabled: true,
                reason: None,
            },
            Self::StashPop if !context.has_stash => disabled("stash is empty"),
            Self::StashPop => Availability {
                enabled: true,
                reason: None,
            },
            _ if context.selected_commit.is_none() => disabled("no commit selected"),
            Self::CheckoutCommit | Self::RebaseHere | Self::CherryPick | Self::Revert
                if context.has_changes =>
            {
                disabled("working tree has changes; stash or commit first")
            }
            _ => Availability {
                enabled: true,
                reason: None,
            },
        }
    }

    pub(in crate::ui) fn result_name(self) -> &'static str {
        match self {
            Self::Update => "Update Herdr Git",
            Self::AddRemote => "Add Remote",
            Self::ShowRemotes => "Remotes",
            Self::RemoveRemote => "Remove Remote",
            Self::Fetch => "Fetch",
            Self::Push => "Push",
            Self::Stash | Self::StashChanges => "Stash",
            Self::StashPop => "Stash pop",
            Self::CreateBranch | Self::CreateBranchAtHead => "Branch",
            Self::CreateTag => "Tag",
            Self::CheckoutCommit => "Checkout",
            Self::RebaseHere => "Rebase",
            Self::CherryPick => "Cherry-pick",
            Self::Revert => "Revert",
            Self::CopySha => "Copy SHA",
            Self::ResetCurrentBranch => "Reset",
            Self::DiscardTrackedChanges => "Discard changes",
            Self::Pull => "Pull",
            Self::Commit => "Commit",
            Self::InteractiveRebase => "Interactive rebase",
            Self::Refresh | Self::SwitchRepository | Self::AddProject | Self::RemoveProject => {
                "Git operation"
            }
        }
    }

    pub(in crate::ui) fn progress_label(self) -> &'static str {
        match self {
            Self::Fetch => "Fetching",
            Self::Pull => "Pulling",
            Self::Commit => "Committing",
            Self::Push => "Pushing",
            Self::StashChanges => "Stashing",
            Self::CreateBranchAtHead => "Creating",
            Self::ResetCurrentBranch => "Resetting",
            _ => "Running",
        }
    }

    pub(in crate::ui) fn quick_action_label(self) -> &'static str {
        match self {
            Self::Fetch => "Fetch",
            Self::Pull => "Pull",
            Self::Commit => "Commit",
            Self::Push => "Push",
            Self::CreateBranchAtHead => "Branch",
            Self::Stash | Self::StashChanges => "Stash",
            _ => unreachable!("command is not a quick action"),
        }
    }

    pub(in crate::ui) fn quick_action_shortcut(self) -> char {
        match self {
            Self::Fetch => 'f',
            Self::Pull => 'l',
            Self::Commit => 'c',
            Self::Push => 'u',
            Self::CreateBranchAtHead => 'b',
            Self::StashChanges => 's',
            _ => unreachable!("command is not a quick action"),
        }
    }

    pub(in crate::ui) fn requires_confirmation(self) -> bool {
        !matches!(
            self,
            Self::Refresh
                | Self::SwitchRepository
                | Self::AddProject
                | Self::RemoveProject
                | Self::Fetch
                | Self::Commit
                | Self::Stash
        )
    }

    pub(in crate::ui) fn matches(self, query: &str) -> bool {
        self.title().to_lowercase().contains(&query.to_lowercase())
    }

    pub(in crate::ui) fn operation(
        self,
        context: &CommandContext,
    ) -> Result<GitOperation, &'static str> {
        let commit = || context.selected_commit.clone().ok_or("no commit selected");
        match self {
            Self::Update => Err("plugin update confirmation is required"),
            Self::DiscardTrackedChanges => Ok(GitOperation::DiscardTrackedChanges {
                head: context
                    .head_commit
                    .clone()
                    .ok_or("no HEAD commit to restore")?,
            }),
            Self::Fetch => Ok(GitOperation::Fetch),
            Self::Pull => Ok(GitOperation::PullFastForward),
            Self::Commit => Err("commit message input is required"),
            Self::Push => Ok(GitOperation::Push {
                force: false,
                remote: context
                    .remotes
                    .first()
                    .ok_or("no remotes configured")?
                    .name
                    .clone(),
                branch: context.current_branch.clone().ok_or("detached HEAD")?,
            }),
            Self::AddRemote | Self::ShowRemotes | Self::RemoveRemote => {
                Err("remote input is required")
            }
            Self::Stash => Err("stash action input is required"),
            Self::StashChanges => Ok(GitOperation::StashChanges),
            Self::StashPop => Ok(GitOperation::StashPop),
            Self::CheckoutCommit => commit().map(GitOperation::CheckoutCommit),
            Self::RebaseHere => commit().map(GitOperation::RebaseHere),
            Self::InteractiveRebase => commit().map(GitOperation::InteractiveRebase),
            Self::CherryPick => commit().map(GitOperation::CherryPick),
            Self::Revert => commit().map(GitOperation::Revert),
            Self::CreateBranch | Self::CreateBranchAtHead | Self::CreateTag => {
                Err("name input is required")
            }
            Self::CopySha => Err("copy SHA dialog is required"),
            Self::ResetCurrentBranch => Err("reset target and mode input is required"),
            Self::Refresh | Self::SwitchRepository | Self::AddProject | Self::RemoveProject => {
                Err("operation runs without confirmation")
            }
        }
    }

    pub(in crate::ui) fn named_operation(
        self,
        context: &CommandContext,
        name: String,
    ) -> Result<GitOperation, &'static str> {
        match self {
            Self::CreateBranch => context
                .selected_commit
                .clone()
                .ok_or("no commit selected")
                .map(|commit| GitOperation::CreateBranch { name, commit }),
            Self::CreateBranchAtHead => context
                .head_commit
                .clone()
                .ok_or("no commits available")
                .map(|commit| GitOperation::CreateBranch { name, commit }),
            Self::CreateTag => context
                .selected_commit
                .clone()
                .ok_or("no commit selected")
                .map(|commit| GitOperation::CreateTag { name, commit }),
            _ => Err("operation does not accept a name"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, CommandContext, CommandId, GitOperation, QUICK_ACTIONS, ResetMode};

    #[test]
    fn commands_palette_contains_header_actions_and_core_commands() {
        let titles = COMMANDS.map(CommandId::title).to_vec();
        assert_eq!(
            titles,
            [
                "Refresh",
                "Switch workspace",
                "Add Remotes…",
                "Show Remotes…",
                "Fetch",
                "Pull",
                "Commit…",
                "Push",
                "Branch…",
                "Checkout…",
                "Rebase…",
                "Interactive rebase…",
                "Cherry-pick…",
                "Revert…",
                "Reset…",
                "Discard all tracked changes…",
                "Copy SHA…",
                "Stash…",
                "Update Herdr Git…"
            ]
        );
    }

    #[test]
    fn labels_and_operation_mapping_live_with_the_command_ids() {
        assert_eq!(CommandId::StashPop.result_name(), "Stash pop");
        assert_eq!(CommandId::Refresh.result_name(), "Git operation");
        assert_eq!(CommandId::Fetch.progress_label(), "Fetching");
        assert_eq!(CommandId::Revert.progress_label(), "Running");
        for command in QUICK_ACTIONS {
            assert!(!command.quick_action_label().is_empty());
        }
        assert_eq!(CommandId::Push.quick_action_shortcut(), 'u');
        assert_eq!(
            GitOperation::PullFastForward.command(),
            Some(CommandId::Pull)
        );
        assert_eq!(GitOperation::StageAll.command(), None);
        assert_eq!(
            ResetMode::Hard.summary(),
            "Reset index and tracked working tree"
        );
    }

    #[test]
    fn disables_commands_with_a_visible_reason() {
        let context = CommandContext::default();
        assert_eq!(
            CommandId::Fetch.availability(&context).reason,
            Some("not a Git repository")
        );
        let context = CommandContext {
            has_repository: true,
            ..CommandContext::default()
        };
        assert_eq!(
            CommandId::Fetch.availability(&context).reason,
            Some("no remotes configured")
        );
        assert_eq!(
            CommandId::Pull.availability(&context).reason,
            Some("detached HEAD")
        );
        assert_eq!(
            CommandId::CherryPick.availability(&context).reason,
            Some("no commit selected")
        );
    }

    #[test]
    fn only_read_refresh_operations_skip_confirmation() {
        assert!(!CommandId::Refresh.requires_confirmation());
        assert!(!CommandId::SwitchRepository.requires_confirmation());
        assert!(!CommandId::AddProject.requires_confirmation());
        assert!(!CommandId::RemoveProject.requires_confirmation());
        assert!(!CommandId::Fetch.requires_confirmation());
        assert!(CommandId::Pull.requires_confirmation());
        assert!(CommandId::RebaseHere.requires_confirmation());
    }

    #[test]
    fn builds_closed_argv_for_selected_commit() {
        let context = CommandContext {
            has_repository: true,
            selected_commit: Some("abc123".to_owned()),
            ..CommandContext::default()
        };
        assert_eq!(
            CommandId::CherryPick.operation(&context).unwrap(),
            GitOperation::CherryPick("abc123".to_owned())
        );
        assert_eq!(
            CommandId::Revert.operation(&context).unwrap().args(),
            ["revert", "--no-edit", "abc123"]
        );
        assert_eq!(
            CommandId::InteractiveRebase
                .operation(&context)
                .unwrap()
                .args(),
            ["rebase", "-i", "abc123^"]
        );
    }

    #[test]
    fn builds_closed_fetch_operation_and_preview() {
        let operation = CommandId::Fetch
            .operation(&CommandContext::default())
            .unwrap();
        assert_eq!(operation, GitOperation::Fetch);
        assert_eq!(operation.args(), ["fetch", "--all", "--prune"]);
        assert_eq!(operation.preview(), "git fetch --all --prune");
    }

    #[test]
    fn push_carries_force_with_lease_only_when_the_toggle_is_on() {
        let context = CommandContext {
            has_repository: true,
            has_remote: true,
            current_branch: Some("main".to_owned()),
            ..CommandContext::default()
        };
        let context = CommandContext {
            remotes: vec![crate::git::Remote {
                name: "origin".into(),
                url: "repo".into(),
                push_url: "repo".into(),
            }],
            ..context
        };
        let operation = CommandId::Push.operation(&context).unwrap();
        assert_eq!(
            operation,
            GitOperation::Push {
                force: false,
                remote: "origin".into(),
                branch: "main".into()
            }
        );
        assert_eq!(
            operation.preview(),
            "git push --set-upstream -- origin HEAD:refs/heads/main"
        );
        assert_eq!(
            GitOperation::Push {
                force: true,
                remote: "origin".into(),
                branch: "main".into()
            }
            .preview(),
            "git push --force-with-lease --set-upstream -- origin HEAD:refs/heads/main"
        );
        assert_eq!(
            GitOperation::Push {
                force: true,
                remote: "origin".into(),
                branch: "main".into()
            }
            .command(),
            Some(CommandId::Push)
        );
    }

    #[test]
    fn builds_closed_argv_for_named_refs() {
        let context = CommandContext {
            has_repository: true,
            selected_commit: Some("abc123".to_owned()),
            ..CommandContext::default()
        };
        assert_eq!(
            CommandId::CreateBranch
                .named_operation(&context, "feature/topic".to_owned())
                .unwrap()
                .args(),
            ["branch", "--", "feature/topic", "abc123"]
        );
        assert_eq!(
            CommandId::CreateTag
                .named_operation(&context, "v0.1.0".to_owned())
                .unwrap()
                .args(),
            ["tag", "--", "v0.1.0", "abc123"]
        );
    }

    #[test]
    fn builds_closed_argv_for_staging_operations() {
        assert_eq!(GitOperation::StageAll.args(), ["add", "--all"]);
        assert_eq!(
            GitOperation::StagePath("odd $(name).txt".to_owned()).args(),
            ["add", "--all", "--", "odd $(name).txt"]
        );
        assert_eq!(
            GitOperation::UnstageAll.args(),
            ["restore", "--staged", "--", "."]
        );
        assert_eq!(
            GitOperation::UnstagePath("odd $(name).txt".to_owned()).args(),
            ["restore", "--staged", "--", "odd $(name).txt"]
        );
        assert_eq!(
            GitOperation::StagePaths(vec!["a file.txt".to_owned(), "b.txt".to_owned()]).args(),
            ["add", "--all", "--", "a file.txt", "b.txt"]
        );
        assert_eq!(
            GitOperation::UnstagePaths(vec!["a file.txt".to_owned(), "b.txt".to_owned()]).args(),
            ["restore", "--staged", "--", "a file.txt", "b.txt"]
        );
    }

    #[test]
    fn quick_actions_keep_the_global_order_and_head_branch_target() {
        assert_eq!(
            QUICK_ACTIONS,
            [
                CommandId::Fetch,
                CommandId::Pull,
                CommandId::Commit,
                CommandId::Push,
                CommandId::CreateBranchAtHead,
                CommandId::StashChanges,
            ]
        );
        assert!(!COMMANDS.contains(&CommandId::StashChanges));
        assert!(!COMMANDS.contains(&CommandId::StashPop));
        assert!(COMMANDS.contains(&CommandId::CheckoutCommit));
        assert!(QUICK_ACTIONS.contains(&CommandId::StashChanges));
        assert!(!QUICK_ACTIONS.contains(&CommandId::ResetCurrentBranch));
        assert!(!QUICK_ACTIONS.contains(&CommandId::CheckoutCommit));
        let context = CommandContext {
            has_repository: true,
            current_branch: Some("main".to_owned()),
            head_commit: Some("abc123".to_owned()),
            selected_commit: Some("def456".to_owned()),
            ..CommandContext::default()
        };
        assert_eq!(
            CommandId::CreateBranchAtHead
                .named_operation(&context, "feature/toolbar".to_owned())
                .unwrap(),
            GitOperation::CreateBranch {
                name: "feature/toolbar".to_owned(),
                commit: "abc123".to_owned(),
            }
        );
    }

    #[test]
    fn commit_and_amend_keep_message_as_one_argument() {
        assert_eq!(
            GitOperation::Commit {
                message: "subject\n\nbody".to_owned(),
                amend: false,
            }
            .args(),
            ["commit", "-m", "subject\n\nbody"]
        );
        assert_eq!(
            GitOperation::Commit {
                message: "replacement".to_owned(),
                amend: true,
            }
            .args(),
            ["commit", "--amend", "-m", "replacement"]
        );
    }

    #[test]
    fn reset_requires_a_checked_out_branch_and_commit() {
        let repository = CommandContext {
            has_repository: true,
            ..CommandContext::default()
        };
        assert_eq!(
            CommandId::ResetCurrentBranch
                .availability(&repository)
                .reason,
            Some("checkout a branch before resetting")
        );
        let unborn = CommandContext {
            current_branch: Some("main".to_owned()),
            ..repository
        };
        assert_eq!(
            CommandId::ResetCurrentBranch.availability(&unborn).reason,
            Some("no commits to reset")
        );
        let ready = CommandContext {
            head_commit: Some("abc123".to_owned()),
            ..unborn
        };
        assert!(CommandId::ResetCurrentBranch.availability(&ready).enabled);
        assert!(CommandId::ResetCurrentBranch.requires_confirmation());
    }

    #[test]
    fn reset_modes_build_closed_arguments_and_describe_their_effects() {
        let operation = |mode| GitOperation::Reset {
            mode,
            target: "def456".to_owned(),
            target_name: "origin/main".to_owned(),
            expected_branch: "main".to_owned(),
            expected_head: "abc123".to_owned(),
        };
        assert_eq!(
            operation(ResetMode::Soft).args(),
            ["reset", "--soft", "def456"]
        );
        assert_eq!(
            operation(ResetMode::Mixed).args(),
            ["reset", "--mixed", "def456"]
        );
        assert_eq!(
            operation(ResetMode::Hard).args(),
            ["reset", "--hard", "def456"]
        );
        assert!(
            ResetMode::Soft
                .effect()
                .contains("index and working tree are preserved")
        );
        assert!(
            ResetMode::Mixed
                .effect()
                .contains("working tree is preserved")
        );
        assert!(ResetMode::Hard.effect().contains("Tracked changes"));
    }
}
