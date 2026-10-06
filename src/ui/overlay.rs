use std::mem;
use std::path::PathBuf;

use crossterm::event::Event;
use ratatui::Frame;

use crate::git::GitOperation;

use super::App;
use super::commands::{
    ActionDialog, CommandId, CommandPalette, CommitDialog, NamePrompt, OperationResultView,
    ResetFlow, TargetPicker,
};
pub(super) use super::diff::LineJump;
pub(super) use super::files::FilesSearch;
pub(super) use super::graph::{ContextMenu, CopyShaDialog};
pub(super) use super::line_history::LineHistoryDialog;
pub(super) use super::review::CopySelectionDialog;
use super::widgets::ConfirmButtons;
pub(super) use super::workspaces::WorkspacePicker;

#[derive(Debug)]
pub(super) enum Overlay {
    None,
    Update {
        tag: String,
        buttons: ConfirmButtons,
    },
    Comparison(super::comparison::ComparisonDialog),
    Rebase(super::commands::RebaseEditor),
    Workspace(WorkspacePicker),
    Commit(CommitDialog),
    Reset(ResetFlow),
    Target(TargetPicker),
    LineJump(LineJump),
    FilesSearch(FilesSearch),
    Action(ActionDialog),
    CopySha(CopyShaDialog),
    CopySelection(CopySelectionDialog),
    LineHistory(LineHistoryDialog),
    Name(NamePrompt),
    AddRemote(super::commands::AddRemoteDialog),
    Remotes(super::commands::RemoteList),
    RemoveProject {
        root: PathBuf,
        buttons: ConfirmButtons,
    },
    Confirm {
        command: CommandId,
        operation: GitOperation,
        buttons: ConfirmButtons,
        push_controls: super::commands::PushControls,
    },
    ContextMenu(ContextMenu),
    Commands(CommandPalette),
    GraphFilter,
    FileFilter,
    Result(OperationResultView, Option<Box<Overlay>>),
}

impl Overlay {
    pub(super) fn is_transient(&self, intent: super::effect::RefreshIntent) -> bool {
        match self {
            Self::None => false,
            Self::Result(_, _) => intent == super::effect::RefreshIntent::Polling,
            _ => true,
        }
    }

    pub(super) fn floats(&self) -> bool {
        !matches!(self, Self::None | Self::GraphFilter | Self::FileFilter)
    }

    pub(super) fn animating(&self) -> bool {
        match self {
            Self::None
            | Self::Update { .. }
            | Self::Workspace(_)
            | Self::Confirm { .. }
            | Self::RemoveProject { .. }
            | Self::Remotes(_)
            | Self::ContextMenu(_)
            | Self::Action(_)
            | Self::CopySha(_)
            | Self::CopySelection(_) => false,
            Self::Comparison(_)
            | Self::Rebase(_)
            | Self::Target(_)
            | Self::Commit(_)
            | Self::Reset(_)
            | Self::LineJump(_)
            | Self::FilesSearch(_)
            | Self::Name(_)
            | Self::AddRemote(_)
            | Self::Commands(_)
            | Self::GraphFilter
            | Self::FileFilter
            | Self::LineHistory(_)
            | Self::Result(_, _) => true,
        }
    }

    #[cfg(test)]
    pub(super) fn result(&self) -> Option<&OperationResultView> {
        match self {
            Self::Result(view, _) => Some(view),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn result_mut(&mut self) -> Option<&mut OperationResultView> {
        match self {
            Self::Result(view, _) => Some(view),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn confirm_operation(&self) -> Option<&GitOperation> {
        match self {
            Self::Confirm { operation, .. } => Some(operation),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn commit(&self) -> Option<&CommitDialog> {
        match self {
            Self::Commit(dialog) => Some(dialog),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn commit_mut(&mut self) -> Option<&mut CommitDialog> {
        match self {
            Self::Commit(dialog) => Some(dialog),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn reset(&self) -> Option<&ResetFlow> {
        match self {
            Self::Reset(flow) => Some(flow),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn copy_sha(&self) -> Option<&CopyShaDialog> {
        match self {
            Self::CopySha(dialog) => Some(dialog),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn workspace(&self) -> Option<&WorkspacePicker> {
        match self {
            Self::Workspace(picker) => Some(picker),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn files_search(&self) -> Option<&FilesSearch> {
        match self {
            Self::FilesSearch(search) => Some(search),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn context_menu(&self) -> Option<&ContextMenu> {
        match self {
            Self::ContextMenu(menu) => Some(menu),
            _ => None,
        }
    }
}

impl App {
    pub(super) fn handle_overlay(&mut self, input: &Event) -> Result<bool, String> {
        match &self.overlay {
            Overlay::None => return Ok(false),
            Overlay::GraphFilter => return Ok(self.handle_graph_filter(input)),
            Overlay::FileFilter => return Ok(self.handle_file_filter(input)),
            Overlay::Result(_, _) => self.handle_result(input),
            Overlay::Workspace(_) => self.handle_workspace_picker(input),
            Overlay::Commit(dialog) if dialog.agent_picker.is_some() => {
                self.handle_agent_picker(input)
            }
            Overlay::Commit(_) => self.handle_commit_dialog(input),
            Overlay::Comparison(_) => self.handle_comparison_dialog(input),
            Overlay::Rebase(_) => self.handle_rebase_editor(input),
            Overlay::Reset(_) => self.handle_reset_flow(input),
            Overlay::Target(_) => self.handle_target_picker(input),
            Overlay::LineJump(_) => self.handle_line_jump(input),
            Overlay::FilesSearch(_) => self.handle_files_search(input),
            Overlay::Action(_) => self.handle_action_dialog(input),
            Overlay::CopySha(_) => self.handle_copy_sha_dialog(input),
            Overlay::CopySelection(_) => self.handle_copy_selection(input),
            Overlay::LineHistory(_) => self.handle_line_history(input),
            Overlay::Name(_) => self.handle_name_prompt(input),
            Overlay::AddRemote(_) => self.handle_add_remote(input),
            Overlay::Remotes(_) => self.handle_remotes(input),
            Overlay::RemoveProject { .. } => self.handle_remove_project_confirmation(input),
            Overlay::Confirm { .. } => self.handle_confirmation(input),
            Overlay::Update { .. } => self.handle_update_confirmation(input),
            Overlay::ContextMenu(_) => self.handle_context_menu(input),
            Overlay::Commands(_) => self.handle_commands(input),
        }
        Ok(true)
    }

    pub(super) fn draw_overlay(&mut self, frame: &mut Frame<'_>) {
        let overlay = mem::replace(&mut self.overlay, Overlay::None);
        self.overlay = match overlay {
            Overlay::None => Overlay::None,
            Overlay::Update { tag, mut buttons } => {
                self.draw_update_confirmation(frame, &tag, &mut buttons);
                Overlay::Update { tag, buttons }
            }
            Overlay::GraphFilter => Overlay::GraphFilter,
            Overlay::FileFilter => Overlay::FileFilter,
            Overlay::Result(view, previous) => {
                self.draw_result(frame, &view);
                Overlay::Result(view, previous)
            }
            Overlay::Workspace(mut picker) => {
                self.draw_workspace_picker(frame, &mut picker);
                Overlay::Workspace(picker)
            }
            Overlay::Commit(mut dialog) => {
                match dialog.agent_picker.as_mut() {
                    Some(picker) => self.draw_agent_picker(frame, picker),
                    None => self.draw_commit_dialog(frame, &mut dialog),
                }
                Overlay::Commit(dialog)
            }
            Overlay::Target(mut picker) => {
                self.draw_target_picker(frame, &mut picker);
                Overlay::Target(picker)
            }
            Overlay::Rebase(mut editor) => {
                self.draw_rebase_editor(frame, &mut editor);
                Overlay::Rebase(editor)
            }
            Overlay::Reset(mut flow) => {
                self.draw_reset_flow(frame, &mut flow);
                Overlay::Reset(flow)
            }
            Overlay::LineJump(mut jump) => {
                self.draw_line_jump(frame, &mut jump);
                Overlay::LineJump(jump)
            }
            Overlay::FilesSearch(mut search) => {
                self.draw_files_search(frame, &mut search);
                Overlay::FilesSearch(search)
            }
            Overlay::Action(mut dialog) => {
                self.draw_action_dialog(frame, &mut dialog);
                Overlay::Action(dialog)
            }
            Overlay::CopySha(mut dialog) => {
                self.draw_copy_sha_dialog(frame, &mut dialog);
                Overlay::CopySha(dialog)
            }
            Overlay::CopySelection(mut dialog) => {
                self.draw_copy_selection(frame, &mut dialog);
                Overlay::CopySelection(dialog)
            }
            Overlay::LineHistory(mut dialog) => {
                self.draw_line_history(frame, &mut dialog);
                Overlay::LineHistory(dialog)
            }
            Overlay::Comparison(mut dialog) => {
                self.draw_comparison_dialog(frame, &mut dialog);
                Overlay::Comparison(dialog)
            }
            Overlay::Name(mut prompt) => {
                self.draw_name_prompt(frame, &mut prompt);
                Overlay::Name(prompt)
            }
            Overlay::AddRemote(mut dialog) => {
                self.draw_add_remote(frame, &mut dialog);
                Overlay::AddRemote(dialog)
            }
            Overlay::Remotes(mut list) => {
                self.draw_remotes(frame, &mut list);
                Overlay::Remotes(list)
            }
            Overlay::RemoveProject { root, mut buttons } => {
                self.draw_remove_project_confirmation(frame, &root, &mut buttons);
                Overlay::RemoveProject { root, buttons }
            }
            Overlay::Confirm {
                command,
                operation,
                mut buttons,
                mut push_controls,
            } => {
                self.draw_confirmation(
                    frame,
                    command,
                    &operation,
                    &mut buttons,
                    &mut push_controls,
                );
                Overlay::Confirm {
                    command,
                    operation,
                    buttons,
                    push_controls,
                }
            }
            Overlay::ContextMenu(mut menu) => {
                self.draw_context_menu(frame, &mut menu);
                Overlay::ContextMenu(menu)
            }
            Overlay::Commands(mut palette) => {
                self.draw_command_palette(frame, &mut palette);
                Overlay::Commands(palette)
            }
        };
    }
}
