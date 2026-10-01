use std::time::{Instant, SystemTime};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    List, ListItem, ListState, Paragraph, ScrollbarOrientation, ScrollbarState,
};

use crate::git::{ChangeSection, Commit, CommitRef, CommitRefKind, DiffTarget, WorkingChange};

use super::commands::{ActionCell, Availability, CommandContext, CommandId, draw_action_bar};
use super::diff::changes_pane_widths;
use super::effect::{RefreshIntent, RefreshScope};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::shell::{ActiveTab, PaneFocus};
use super::widgets::{
    self, Axis, ConfirmButton, ConfirmButtons, ListCursor, PickerOutcome, PickerState,
    ScrollbarDrag, TextEdit, TextField, anchored, centered, chord, left_click, picker_edit,
    text_display_width, truncate_to_width, update_picker,
};
use super::{App, ScrollbarOwner, theme};
use layout::{GraphLayout, GraphViewport, LayoutNode, ROW_HEIGHT};

pub(super) use curves::CurveLayer;

mod curves;
mod glyphs;
mod layout;

pub(super) const COMMIT_COMMANDS: [CommandId; 7] = [
    CommandId::CreateBranch,
    CommandId::CreateTag,
    CommandId::RebaseHere,
    CommandId::InteractiveRebase,
    CommandId::CheckoutCommit,
    CommandId::CherryPick,
    CommandId::Revert,
];

pub(super) const GRAPH_ACTIONS: [GraphAction; 6] = [
    GraphAction::Checkout,
    GraphAction::Rebase,
    GraphAction::CherryPick,
    GraphAction::Revert,
    GraphAction::Reset,
    GraphAction::CopySha,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GraphAction {
    Checkout,
    Rebase,
    CherryPick,
    Revert,
    Reset,
    CopySha,
}

impl GraphAction {
    fn label(self) -> &'static str {
        match self {
            Self::Checkout => "Checkout",
            Self::Rebase => "Rebase",
            Self::CherryPick => "Cherry-pick",
            Self::Revert => "Revert",
            Self::Reset => "Reset",
            Self::CopySha => "Copy SHA",
        }
    }

    pub(super) fn shortcut(self) -> char {
        match self {
            Self::Checkout => 'x',
            Self::Rebase => 'r',
            Self::CherryPick => 'y',
            Self::Revert => 'v',
            Self::Reset => 't',
            Self::CopySha => 'h',
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Checkout => "",
            Self::Rebase => "",
            Self::CherryPick => "⎇",
            Self::Revert => "↩",
            Self::Reset => "",
            Self::CopySha => "",
        }
    }

    fn command(self) -> Option<CommandId> {
        match self {
            Self::Checkout => Some(CommandId::CheckoutCommit),
            Self::Rebase => Some(CommandId::RebaseHere),
            Self::CherryPick => Some(CommandId::CherryPick),
            Self::Revert => Some(CommandId::Revert),
            Self::Reset => Some(CommandId::ResetCurrentBranch),
            Self::CopySha => None,
        }
    }

    pub(super) fn owns_command(self, command: CommandId) -> bool {
        match self {
            Self::Rebase => matches!(
                command,
                CommandId::RebaseHere | CommandId::InteractiveRebase
            ),
            _ => self.command() == Some(command),
        }
    }
}

#[derive(Debug)]
pub(super) struct CopyShaDialog {
    pub(super) sha: String,
    pub(super) subject: String,
    pub(super) buttons: ConfirmButtons,
}

impl CopyShaDialog {
    fn new(commit: &Commit) -> Self {
        Self {
            sha: commit.sha.clone(),
            subject: commit.subject.clone(),
            buttons: ConfirmButtons::default(),
        }
    }
}

const CONTEXT_MENU_HEIGHT: u16 = 13;
const REVEAL_LIMIT: usize = 10_000;

pub(super) struct GraphState {
    pub(super) commits: Vec<Commit>,
    pub(super) uncommitted: Option<Commit>,
    pub(super) history_loaded: bool,
    pub(super) history_has_more: bool,
    layout: GraphLayout,
    curve_viewport: Option<GraphViewport>,
    pub(super) load_more_area: Rect,
    pub(super) visible: Vec<usize>,
    pub(super) selected: usize,
    pub(super) history_scroll: usize,
    pub(super) history_content_area: Rect,
    pub(super) history_scrollbar_area: Option<Rect>,
    pub(super) query: TextField,
    pub(super) list_area: Rect,
    pub(super) filter_area: Rect,
    pub(super) action_areas: Vec<(GraphAction, Rect)>,
    pub(super) reveal: Option<String>,
    pub(super) anchor: Option<String>,
    pub(super) stashed: Option<Box<GraphState>>,
    pub(super) back_area: Rect,
    pub(super) graph_scroll: usize,
    pub(super) graph_visible_width: usize,
    graph_width: usize,
}

impl GraphState {
    pub(super) fn new(commits: Vec<Commit>, history_loaded: bool) -> Self {
        Self {
            visible: (0..commits.len()).collect(),
            layout: GraphLayout::default(),
            curve_viewport: None,
            history_has_more: false,
            load_more_area: Rect::default(),
            commits,
            uncommitted: None,
            history_loaded,
            selected: 0,
            history_scroll: 0,
            history_content_area: Rect::default(),
            history_scrollbar_area: None,
            query: TextField::new(),
            list_area: Rect::default(),
            filter_area: Rect::default(),
            action_areas: Vec::new(),
            reveal: None,
            anchor: None,
            stashed: None,
            back_area: Rect::default(),
            graph_scroll: 0,
            graph_visible_width: 0,
            graph_width: 0,
        }
    }

    pub(super) fn clear(&mut self) {
        self.commits.clear();
        self.uncommitted = None;
        self.history_loaded = false;
        self.history_has_more = false;
        self.layout = GraphLayout {
            generation: self.layout.generation + 1,
            ..GraphLayout::default()
        };
        self.curve_viewport = None;
        self.load_more_area = Rect::default();
        self.visible.clear();
        self.selected = 0;
        self.history_scroll = 0;
        self.history_content_area = Rect::default();
        self.history_scrollbar_area = None;
        self.reveal = None;
        self.anchor = None;
        self.stashed = None;
        self.back_area = Rect::default();
        self.graph_scroll = 0;
        self.query.text.clear();
    }

    pub(super) fn rebuild_layout(&mut self) {
        let filtered = !self.query.text.is_empty();
        let uncommitted = self
            .uncommitted
            .iter()
            .filter(|_| self.showing_uncommitted());
        let nodes = uncommitted
            .chain(self.visible.iter().map(|&index| &self.commits[index]))
            .map(|commit| LayoutNode {
                sha: &commit.sha,
                parents: if filtered { &[] } else { &commit.parents },
            });
        let layout = GraphLayout::build(nodes, self.layout.generation + 1);
        if layout.rows != self.layout.rows {
            self.layout = layout;
        }
    }

    pub(super) fn page_rows(&self) -> usize {
        let height = usize::from(self.history_content_area.height);
        (height / ROW_HEIGHT).max(usize::from(height > 0))
    }

    fn row_at(&self, (column, row): (u16, u16)) -> Option<usize> {
        let area = self.history_content_area;
        if !area.contains((column, row).into()) {
            return None;
        }
        let index = usize::from(row - area.y) / ROW_HEIGHT + self.history_scroll;
        (index < self.display_len()).then_some(index)
    }

    pub(super) fn showing_uncommitted(&self) -> bool {
        self.uncommitted
            .as_ref()
            .is_some_and(|commit| uncommitted_matches_query(&self.query.text, commit))
    }

    pub(super) fn display_offset(&self) -> usize {
        usize::from(self.showing_uncommitted())
    }

    pub(super) fn display_len(&self) -> usize {
        self.visible.len() + self.display_offset()
    }

    pub(super) fn uncommitted_selected(&self) -> bool {
        self.showing_uncommitted() && self.selected == 0
    }

    pub(super) fn row_commit(&self, row: usize) -> Option<&Commit> {
        let offset = self.display_offset();
        if offset == 1 && row == 0 {
            return self.uncommitted.as_ref();
        }
        self.visible
            .get(row.saturating_sub(offset))
            .and_then(|index| self.commits.get(*index))
    }

    pub(super) fn selected_commit(&self) -> Option<&Commit> {
        self.row_commit(self.selected)
            .filter(|commit| !commit.is_uncommitted())
    }

    fn page(&self) -> isize {
        isize::try_from(self.page_rows())
            .unwrap_or(isize::MAX)
            .max(1)
    }
}

#[derive(Debug, Default)]
pub(super) struct ContextMenu {
    pub(super) cursor: ListCursor,
    pub(super) anchor: Option<(u16, u16)>,
    pub(super) area: Rect,
    pub(super) list_area: Rect,
    pub(super) buttons: ConfirmButtons,
}

impl App {
    pub(super) fn apply_filter(&mut self) {
        let previous_commit = self.current_commit_details_target();
        let on_uncommitted = self.graph.uncommitted_selected();
        let selected_sha = self
            .graph
            .selected_commit()
            .map(|commit| commit.sha.clone());
        self.graph.visible = filtered_commit_indices(&self.graph.commits, &self.graph.query.text);
        self.graph.rebuild_layout();
        if on_uncommitted && self.graph.showing_uncommitted() {
            self.graph.selected = 0;
        } else if let Some(sha) = selected_sha {
            self.graph.selected = self.graph.display_offset()
                + self
                    .graph
                    .visible
                    .iter()
                    .position(|&index| self.graph.commits[index].sha == sha)
                    .unwrap_or(0);
            self.clamp_history_selection();
        } else {
            self.select_first_commit_row();
        }
        self.ensure_history_selection_visible();
        let next_commit = self.current_commit_details_target();
        self.refresh_command_selection_context();
        if previous_commit != next_commit {
            self.reset_commit_inspection();
            self.request_commit_details();
        }
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.graph.display_len() == 0 {
            return;
        }
        let last = self.graph.display_len() - 1;
        self.select(self.graph.selected.saturating_add_signed(delta).min(last));
        if delta > 0 {
            self.request_more_history(false);
        }
    }

    pub(super) fn max_history_scroll(&self) -> usize {
        let viewport = self.graph.page_rows();
        if viewport == 0 {
            0
        } else {
            self.graph.display_len().saturating_sub(viewport)
        }
    }

    pub(super) fn ensure_history_selection_visible(&mut self) {
        let viewport = self.graph.page_rows();
        if viewport == 0 || self.graph.display_len() == 0 {
            self.graph.history_scroll = 0;
            return;
        }
        if self.graph.selected < self.graph.history_scroll {
            self.graph.history_scroll = self.graph.selected;
        } else if self.graph.selected >= self.graph.history_scroll.saturating_add(viewport) {
            self.graph.history_scroll = self
                .graph
                .selected
                .saturating_add(1)
                .saturating_sub(viewport);
        }
        self.graph.history_scroll = self.graph.history_scroll.min(self.max_history_scroll());
    }

    fn history_scrollbar_at(&self, column: u16, row: u16) -> Option<Rect> {
        self.graph
            .history_scrollbar_area
            .filter(|area| area.contains((column, row).into()))
    }

    fn history_scroll_region_contains(&self, column: u16, row: u16) -> bool {
        let position = (column, row).into();
        self.graph.history_content_area.contains(position)
            || self
                .graph
                .history_scrollbar_area
                .is_some_and(|area| area.contains(position))
    }

    fn set_history_selection_from_pointer(&mut self, area: Rect, row: u16) {
        let last = self.graph.display_len().saturating_sub(1);
        let track_max = usize::from(area.height.saturating_sub(1));
        let viewport = self.graph.page_rows();
        if self.graph.display_len() == 0 || track_max == 0 || viewport == 0 {
            return;
        }
        let pointer = usize::from(row.saturating_sub(area.y)).min(track_max);
        let scroll = pointer
            .saturating_mul(self.max_history_scroll())
            .saturating_add(track_max / 2)
            / track_max;
        self.graph.history_scroll = scroll;
        let viewport_end = scroll.saturating_add(viewport.saturating_sub(1)).min(last);
        let selected = if pointer == 0 {
            0
        } else if pointer == track_max {
            last
        } else {
            self.graph.selected.clamp(scroll, viewport_end)
        };
        self.select(selected);
        self.request_more_history(false);
    }

    pub(super) fn reveal_pending_commit(&mut self) -> bool {
        let Some(sha) = self.graph.reveal.clone() else {
            return false;
        };
        if self.shell.active_tab != ActiveTab::History {
            self.graph.reveal = None;
            return false;
        }
        if let Some(row) = (0..self.graph.display_len()).find(|&row| {
            self.graph
                .row_commit(row)
                .is_some_and(|commit| commit.sha == sha)
        }) {
            self.graph.reveal = None;
            self.focus = PaneFocus::Commits;
            self.select(row);
            return true;
        }
        if self.history.pending.is_some() || !self.graph.history_loaded {
            return false;
        }
        let short = &sha[..sha.len().min(7)];
        if self.graph.commits.iter().any(|commit| commit.sha == sha) {
            self.graph.reveal = None;
            self.show_action_error(&format!("Clear the Graph filter to see {short}."));
            return true;
        }
        if self.graph.history_has_more && self.graph.commits.len() < REVEAL_LIMIT {
            self.request_more_history(true);
            return true;
        }
        if self.graph.history_has_more && self.graph.anchor.is_none() {
            self.open_graph_anchor(sha);
            return true;
        }
        self.graph.reveal = None;
        self.show_action_error(&format!(
            "{short} is not within the newest {} commits in Graph.",
            self.graph.commits.len()
        ));
        true
    }

    pub(super) fn open_graph_anchor(&mut self, sha: String) {
        let mut saved = std::mem::replace(&mut self.graph, GraphState::new(Vec::new(), false));
        let stashed = match saved.anchor.take() {
            Some(_) => saved.stashed.take(),
            None => Some(Box::new(saved)),
        };
        self.graph.stashed = stashed;
        self.graph.anchor = Some(sha.clone());
        self.graph.reveal = Some(sha);
        self.focus = PaneFocus::Commits;
        self.history.clear();
        self.reset_commit_inspection();
        self.request_history();
    }

    pub(super) fn leave_graph_anchor(&mut self) -> bool {
        let Some(saved) = self.graph.stashed.take() else {
            return false;
        };
        self.graph = *saved;
        self.focus = PaneFocus::Commits;
        self.history.clear();
        self.reset_commit_inspection();
        self.refresh_command_selection_context();
        self.request_commit_details();
        self.request_history_for(RefreshIntent::Polling);
        true
    }

    fn scroll_graph_horizontal(&mut self, delta: isize) {
        let max = self
            .graph
            .graph_width
            .saturating_sub(self.graph.graph_visible_width);
        self.graph.graph_scroll = self
            .graph
            .graph_scroll
            .saturating_add_signed(delta)
            .min(max);
    }

    pub(super) fn select(&mut self, index: usize) {
        self.graph.reveal = None;
        if self.graph.display_len() == 0 {
            return;
        }
        let selected = index.min(self.graph.display_len() - 1);
        if selected == self.graph.selected {
            self.ensure_history_selection_visible();
            return;
        }
        self.graph.selected = selected;
        self.ensure_history_selection_visible();
        self.reset_commit_inspection();
        self.refresh_command_selection_context();
        self.request_commit_details();
    }

    fn worktree_is_dirty(&self) -> bool {
        self.ops.command_context.has_changes
            || self
                .files
                .changes
                .iter()
                .any(|change| change.section != ChangeSection::Commit)
    }

    fn uncommitted_parents(&self) -> Vec<String> {
        self.graph
            .commits
            .iter()
            .find(|commit| {
                commit
                    .refs
                    .iter()
                    .any(|reference| reference.kind == CommitRefKind::Head)
            })
            .or_else(|| self.graph.commits.first())
            .map(|commit| vec![commit.sha.clone()])
            .unwrap_or_default()
    }

    pub(super) fn clamp_history_selection(&mut self) {
        let len = self.graph.display_len();
        self.graph.selected = if len == 0 {
            0
        } else {
            self.graph.selected.min(len - 1)
        };
    }

    pub(super) fn select_first_commit_row(&mut self) {
        self.graph.selected = self.graph.display_offset();
        self.clamp_history_selection();
    }

    pub(super) fn select_visible_commit_row(&mut self, visible_index: usize) {
        self.graph.selected = self.graph.display_offset() + visible_index;
        self.clamp_history_selection();
    }

    fn should_show_uncommitted_row(&self) -> bool {
        self.graph.anchor.is_none()
            && self.graph.history_loaded
            && (self.graph.commits.is_empty() || self.worktree_is_dirty())
    }

    pub(super) fn refresh_uncommitted_row(&mut self) {
        if self.should_show_uncommitted_row() {
            self.graph.uncommitted = Some(Commit::uncommitted(self.uncommitted_parents()));
        } else {
            self.graph.uncommitted = None;
        }
        self.graph.rebuild_layout();
        self.clamp_history_selection();
    }

    pub(super) fn sync_uncommitted_row(&mut self) {
        let on_uncommitted = self.graph.uncommitted_selected();
        let commit_pos = (!on_uncommitted).then(|| {
            self.graph
                .selected
                .saturating_sub(self.graph.display_offset())
        });
        self.refresh_uncommitted_row();
        if on_uncommitted && self.graph.showing_uncommitted() {
            self.graph.selected = 0;
        } else if let Some(pos) = commit_pos {
            self.select_visible_commit_row(pos);
        } else {
            self.select_first_commit_row();
        }
    }

    fn open_commit_diff(&mut self) {
        let Some(details) = self.selected_commit_details().cloned() else {
            return;
        };
        self.reset_commit_inspection();
        self.comparison.mode = None;
        self.diff.diff_target = DiffTarget::CommitAgainstParent {
            commit: details.commit.sha.clone(),
            parent: details.commit.parents.first().cloned(),
        };
        self.files.changes = details
            .changes
            .into_iter()
            .map(|change| WorkingChange {
                section: ChangeSection::Commit,
                status: change.status,
                path: change.path,
            })
            .collect();
        self.files.change_selected = 0;
        self.diff.fold_toggles.clear();
        self.clear_selection();
        self.files.collapsed.clear();
        self.rebuild_tree();
        self.enter_tab(ActiveTab::Changes);
        self.request_background_refresh("Loading commit diff", RefreshScope::Changes);
    }

    fn open_graph_filter(&mut self) {
        self.graph.query.cursor_started = Instant::now();
        self.overlay = Overlay::GraphFilter;
    }

    pub(super) fn open_context_menu(&mut self, anchor: Option<(u16, u16)>) {
        self.refresh_command_selection_context();
        self.overlay = Overlay::ContextMenu(ContextMenu {
            anchor,
            ..ContextMenu::default()
        });
    }

    pub(super) fn run_graph_action(&mut self, action: GraphAction) {
        self.refresh_command_selection_context();
        let availability = graph_action_availability(action, &self.ops.command_context);
        if !availability.enabled {
            self.show_result(super::commands::OperationResultView::message(
                action.command(),
                false,
                availability.reason.unwrap_or("operation is unavailable"),
            ));
            return;
        }
        match action {
            GraphAction::Checkout => self.dispatch_command(CommandId::CheckoutCommit),
            GraphAction::Rebase => self.dispatch_command(CommandId::RebaseHere),
            GraphAction::CherryPick => self.dispatch_command(CommandId::CherryPick),
            GraphAction::Revert => self.dispatch_command(CommandId::Revert),
            GraphAction::Reset => {
                let Some(commit) = self.graph.selected_commit().cloned() else {
                    return;
                };
                self.open_reset_flow_to_commit(commit);
            }
            GraphAction::CopySha => {
                let Some(commit) = self.graph.selected_commit() else {
                    return;
                };
                self.overlay = Overlay::CopySha(CopyShaDialog::new(commit));
            }
        }
    }

    pub(super) fn handle_graph_actions_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        let Some(action) = self.graph.action_areas.iter().find_map(|(action, area)| {
            area.contains((mouse.column, mouse.row).into())
                .then_some(*action)
        }) else {
            return false;
        };
        self.run_graph_action(action);
        true
    }

    pub(super) fn handle_copy_sha_dialog(&mut self, input: &Event) {
        let Overlay::CopySha(dialog) = &self.overlay else {
            return;
        };
        let buttons = dialog.buttons;
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => self.confirm_copy_sha(),
                KeyCode::Esc => self.overlay = Overlay::None,
                _ => {}
            },
            _ => match left_click(input).and_then(|pointer| buttons.hit(pointer)) {
                Some(ConfirmButton::Primary) => self.confirm_copy_sha(),
                Some(ConfirmButton::Secondary) => self.overlay = Overlay::None,
                None => {}
            },
        }
    }

    fn confirm_copy_sha(&mut self) {
        let Overlay::CopySha(dialog) = &self.overlay else {
            return;
        };
        self.pending_clipboard = Some(super::PendingClipboard::Sha(dialog.sha.clone()));
        self.overlay = Overlay::None;
    }

    pub(super) fn finish_copy_sha(&mut self, sha: &str, result: Result<(), String>) {
        let (success, message) = match result {
            Ok(()) => (
                true,
                format!("{} copied to the clipboard.", short_commit(sha)),
            ),
            Err(error) => (false, format!("Could not copy commit SHA: {error}")),
        };
        self.show_result(super::commands::OperationResultView::named_message(
            "Copy SHA", success, &message,
        ));
    }

    fn run_context_command(&mut self) {
        let Overlay::ContextMenu(menu) = &self.overlay else {
            return;
        };
        let command = COMMIT_COMMANDS[menu.cursor.selected.min(COMMIT_COMMANDS.len() - 1)];
        self.overlay = Overlay::None;
        self.dispatch_command(command);
    }

    pub(super) fn handle_graph_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('/') {
            self.open_graph_filter();
            return true;
        }
        if self.focus != PaneFocus::Commits {
            return false;
        }
        match key.code {
            KeyCode::Esc if self.graph.anchor.is_some() => {
                self.leave_graph_anchor();
            }
            KeyCode::Char('h') => self.scroll_graph_horizontal(-2),
            KeyCode::Char('l') => self.scroll_graph_horizontal(2),
            KeyCode::Enter => self.open_commit_diff(),
            KeyCode::Right => self.focus_commit_changes(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => {
                self.move_selection(self.graph.page());
                self.request_more_history(
                    !self.graph.query.text.is_empty() || self.graph.visible.is_empty(),
                );
            }
            KeyCode::PageUp => self.move_selection(-self.graph.page()),
            KeyCode::Home => self.select(0),
            KeyCode::End => {
                self.select(self.graph.display_len().saturating_sub(1));
                self.request_more_history(false);
            }
            _ => return false,
        }
        true
    }

    pub(super) fn handle_graph_mouse(&mut self, mouse: MouseEvent) -> bool {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self
                .graph
                .load_more_area
                .contains((mouse.column, mouse.row).into())
        {
            self.request_more_history(true);
            return true;
        }

        let (column, row) = (mouse.column, mouse.row);
        let pointer = (column, row).into();
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.graph.back_area.contains(pointer) {
                    self.leave_graph_anchor();
                } else if let Some(area) = self.history_scrollbar_at(column, row) {
                    self.focus = PaneFocus::Commits;
                    let drag = ScrollbarDrag {
                        area,
                        axis: Axis::Vertical,
                    };
                    self.scrollbar_drag = Some((ScrollbarOwner::History, drag));
                    self.set_history_selection_from_pointer(area, row);
                } else if self.graph.filter_area.contains(pointer) {
                    self.open_graph_filter();
                } else if self.graph.list_area.contains(pointer) {
                    self.select_commit_row_at((column, row));
                } else {
                    return false;
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.graph.list_area.contains(pointer) => {
                if self.select_commit_row_at((column, row)) {
                    self.open_context_menu(Some((column, row)));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((ScrollbarOwner::History, drag)) = self.scrollbar_drag else {
                    return false;
                };
                self.set_history_selection_from_pointer(drag.area, row);
            }
            MouseEventKind::ScrollRight if self.graph.list_area.contains(pointer) => {
                self.scroll_graph_horizontal(2)
            }
            MouseEventKind::ScrollLeft if self.graph.list_area.contains(pointer) => {
                self.scroll_graph_horizontal(-2)
            }
            MouseEventKind::ScrollDown
                if mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && self.graph.list_area.contains(pointer) =>
            {
                self.scroll_graph_horizontal(2)
            }
            MouseEventKind::ScrollUp
                if mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && self.graph.list_area.contains(pointer) =>
            {
                self.scroll_graph_horizontal(-2)
            }
            MouseEventKind::ScrollDown if self.history_scroll_region_contains(column, row) => {
                self.focus = PaneFocus::Commits;
                self.move_selection(1);
            }
            MouseEventKind::ScrollUp if self.history_scroll_region_contains(column, row) => {
                self.focus = PaneFocus::Commits;
                self.move_selection(-1);
            }
            _ => return false,
        }
        true
    }

    fn select_commit_row_at(&mut self, pointer: (u16, u16)) -> bool {
        let Some(row) = self.graph.row_at(pointer) else {
            return false;
        };
        self.focus = PaneFocus::Commits;
        self.select(row);
        true
    }

    pub(super) fn handle_context_menu(&mut self, input: &Event) {
        let Overlay::ContextMenu(menu) = &mut self.overlay else {
            return;
        };
        if let Some(point) = left_click(input) {
            match menu.buttons.hit(point) {
                Some(ConfirmButton::Primary) => {
                    self.run_context_command();
                    return;
                }
                Some(ConfirmButton::Secondary) => {
                    self.overlay = Overlay::None;
                    return;
                }
                None => {}
            }
        }
        if let Event::Mouse(mouse) = input
            && matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left | MouseButton::Right)
            )
            && !menu.area.contains((mouse.column, mouse.row).into())
        {
            self.overlay = Overlay::None;
            return;
        }
        let Some(edit) = picker_edit(input, menu.list_area, menu.cursor.scroll, false) else {
            return;
        };
        let page = usize::from(menu.list_area.height);
        match update_picker(
            PickerState {
                query: None,
                cursor: &mut menu.cursor,
                len: COMMIT_COMMANDS.len(),
                page,
            },
            edit,
        ) {
            PickerOutcome::Activate => self.run_context_command(),
            PickerOutcome::Cancel => self.overlay = Overlay::None,
            PickerOutcome::Moved | PickerOutcome::Filtered | PickerOutcome::Unchanged => {}
        }
    }

    pub(super) fn handle_graph_filter(&mut self, input: &Event) -> bool {
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match key.code {
                    KeyCode::Esc => {
                        self.graph.query.text.clear();
                        self.apply_filter();
                        self.overlay = Overlay::None;
                    }
                    KeyCode::Enter => self.overlay = Overlay::None,
                    KeyCode::PageDown => self.request_more_history(true),
                    KeyCode::Backspace => {
                        self.graph.query.edit(TextEdit::Backspace);
                        self.apply_filter();
                    }
                    KeyCode::Char(character) if !chord(key.modifiers) => {
                        self.graph.query.edit(TextEdit::Insert(character));
                        self.apply_filter();
                    }
                    _ => {}
                }
                true
            }
            Event::Key(_) => true,
            Event::Paste(text) => {
                self.graph.query.edit(TextEdit::Paste(text.clone()));
                self.apply_filter();
                true
            }
            _ => false,
        }
    }

    pub(super) fn draw_graph_filter(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.graph.filter_area = area;
        widgets::draw_filter_bar(
            frame,
            area,
            &self.graph.query,
            matches!(self.overlay, Overlay::GraphFilter),
        );
    }

    pub(super) fn draw_graph_actions(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let actions = GRAPH_ACTIONS.map(|action| {
            let availability = graph_action_availability(action, &self.ops.command_context);
            let running = self.foreground.action.as_ref().and_then(|foreground| {
                let command = foreground.kind.command()?;
                let progress = match &foreground.kind {
                    ForegroundKind::BranchTargets { .. } => "Loading branches",
                    ForegroundKind::ResetContext { .. } => "Loading Reset targets",
                    _ => command.progress_label(),
                };
                action
                    .owns_command(command)
                    .then(|| (foreground.started.elapsed(), progress))
            });
            ActionCell {
                label: action.label(),
                shortcut: action.shortcut(),
                icon: action.icon(),
                enabled: availability.enabled,
                selected: false,
                running,
            }
        });
        let areas = draw_action_bar(
            frame,
            area,
            &actions,
            self.shell.shortcut_hints,
            self.shell.mouse_position,
        );
        self.graph.action_areas = GRAPH_ACTIONS.into_iter().zip(areas).collect();
    }

    pub(super) fn draw_copy_sha_dialog(&self, frame: &mut Frame<'_>, dialog: &mut CopyShaDialog) {
        let inner = widgets::dialog_frame(frame, "Copy commit SHA", theme::DIALOG_MEDIUM, 10);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw("Copy this commit SHA?"),
                Line::raw(""),
                Line::styled(dialog.sha.clone(), theme::accent_bold()),
                Line::raw(dialog.subject.clone()),
            ])
            .wrap(ratatui::widgets::Wrap { trim: true }),
            regions[0],
        );
        frame.render_widget(
            widgets::footer_hint(&format!("{} selected", short_commit(&dialog.sha))),
            regions[1],
        );
        dialog.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Enter] Copy", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }

    pub(super) fn draw_history(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(area);
        let list = body[0];
        let lower = body[1];
        self.graph.list_area = list;
        let title = if let Some(request) = &self.history.pending {
            let label = if !request.replace {
                "Loading more commits"
            } else if self.graph.history_loaded {
                "Refreshing commits"
            } else {
                "Loading Graph"
            };
            Line::from(vec![
                theme::spinner_span(self.history.started.elapsed()),
                Span::raw(format!(" {label}")),
            ])
        } else if let Some(anchor) = &self.graph.anchor {
            Line::from(vec![
                Span::raw(format!("Commits from {} · ", short_commit(anchor))),
                Span::styled("[Esc] Back to HEAD", theme::accent_bold()),
            ])
        } else {
            let mut title = "Commits".to_owned();
            if !self.graph.query.text.is_empty() {
                title.push_str(&format!(" · {} matches", self.graph.display_len()));
            }
            if let Some(elapsed) = self.history.last_load_duration {
                title.push_str(&format!(" · Loaded in {:.2} s", elapsed.as_secs_f64()));
            }
            Line::from(title)
        };
        let footer = if self.history.error.is_some() {
            "Refresh to retry"
        } else if self.graph.history_has_more {
            "[PgDn] Load more"
        } else {
            ""
        };
        let block = widgets::pane_block(title, self.focus == PaneFocus::Commits)
            .title_bottom(Line::styled(footer, theme::hint()));
        self.graph.load_more_area = if self.graph.history_has_more && self.history.error.is_none() {
            Rect::new(
                list.x.saturating_add(1),
                list.bottom().saturating_sub(1),
                (footer.len() as u16).min(list.width.saturating_sub(2)),
                1,
            )
        } else {
            Rect::default()
        };
        self.graph.back_area = match (&self.graph.anchor, self.history.pending.is_none()) {
            (Some(anchor), true) => {
                let prefix = format!("Commits from {} · ", short_commit(anchor)).len() as u16;
                Rect::new(list.x + 1 + prefix, list.y, 18, 1).intersection(list)
            }
            _ => Rect::default(),
        };
        let inner = block.inner(list);
        frame.render_widget(block, list);
        let overflow =
            inner.width > 1 && self.graph.display_len() * ROW_HEIGHT > usize::from(inner.height);
        self.graph.history_content_area = Rect::new(
            inner.x,
            inner.y,
            inner.width.saturating_sub(u16::from(overflow)),
            inner.height,
        );
        self.graph.history_scrollbar_area = overflow.then_some(Rect::new(
            inner.right().saturating_sub(1),
            inner.y,
            1,
            inner.height,
        ));
        self.ensure_history_selection_visible();
        self.draw_commit_rows(frame);
        if let Some(scrollbar_area) = self.graph.history_scrollbar_area {
            let mut scrollbar_state =
                ScrollbarState::new(self.max_history_scroll().saturating_add(1))
                    .position(self.graph.history_scroll)
                    .viewport_content_length(self.graph.page_rows());
            frame.render_stateful_widget(
                widgets::scrollbar(ScrollbarOrientation::VerticalRight),
                scrollbar_area,
                &mut scrollbar_state,
            );
        }
        if lower.width >= 90 {
            let widths = changes_pane_widths(lower.width, self.diff.changes_splits);
            if self.inspect.commit_preview_path.is_some() {
                let panes = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(widths[0]),
                        Constraint::Length(widths[1]),
                        Constraint::Length(widths[2]),
                    ])
                    .split(lower);
                self.draw_workspaces(frame, panes[0]);
                self.draw_commit_details(frame, panes[1]);
                self.draw_commit_preview(frame, panes[2]);
            } else {
                let panes = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(widths[0]),
                        Constraint::Length(widths[1].saturating_add(widths[2])),
                    ])
                    .split(lower);
                self.draw_workspaces(frame, panes[0]);
                self.draw_commit_details(frame, panes[1]);
            }
        } else if self.inspect.commit_preview_path.is_some() {
            self.draw_commit_preview(frame, lower);
        } else {
            self.draw_commit_details(frame, lower);
        }
    }

    fn draw_commit_rows(&mut self, frame: &mut Frame<'_>) {
        let area = self.graph.history_content_area;
        let hovered = self
            .shell
            .mouse_position
            .and_then(|pointer| self.graph.row_at(pointer));
        let row_width = usize::from(area.width);
        let first = self.graph.history_scroll;
        let rows = usize::from(area.height)
            .div_ceil(ROW_HEIGHT)
            .min(self.graph.display_len().saturating_sub(first));
        let graph_width = self.graph.layout.width(first, rows);
        self.graph.graph_width = graph_width;
        self.graph.graph_visible_width = graph_view_width(row_width).min(graph_width);
        self.graph.graph_scroll = self
            .graph
            .graph_scroll
            .min(graph_width.saturating_sub(self.graph.graph_visible_width));
        let now = SystemTime::now();
        for offset in 0..rows {
            let row = first + offset;
            let commit = self.graph.row_commit(row).expect("displayed graph row");
            let selected = row == self.graph.selected;
            let style = if selected {
                theme::selection_row()
            } else {
                theme::hover(Style::default(), hovered == Some(row))
            };
            let top = area.y + (offset * ROW_HEIGHT) as u16;
            let lines = commit_rows(commit, row_width, self.graph.graph_visible_width, now);
            let styles = [style, style.remove_modifier(Modifier::BOLD)];
            for (line_offset, (line, style)) in lines.into_iter().zip(styles).enumerate() {
                let y = top + line_offset as u16;
                if y < area.bottom() {
                    frame.render_widget(
                        Paragraph::new(line).style(style),
                        Rect::new(area.x, y, area.width, 1),
                    );
                }
            }
        }
        let viewport = GraphViewport {
            selected: Some(self.graph.selected),
            hovered,
            ..GraphViewport::new(
                Rect::new(
                    area.x,
                    area.y,
                    self.graph.graph_visible_width as u16,
                    area.height,
                ),
                first,
                self.graph.graph_scroll,
                rows,
            )
        };
        self.graph.curve_viewport = None;
        if self.curves.available() && self.overlay.leaves_graph_visible() {
            self.graph.curve_viewport = Some(viewport);
        } else {
            glyphs::paint(frame.buffer_mut(), &self.graph.layout, &viewport);
        }
        if self.graph.graph_scroll > 0 {
            mark_clipped_graph(frame, area.x, area.y, "‹");
        }
        if graph_width > self.graph.graph_scroll + self.graph.graph_visible_width {
            let right = area.x + self.graph.graph_visible_width.saturating_sub(1) as u16;
            mark_clipped_graph(frame, right, area.y, "›");
        }
    }

    pub(super) fn present_graph_curves(&mut self) -> bool {
        let viewport = self.graph.curve_viewport.take();
        self.curves.present(&self.graph.layout, viewport)
    }

    pub(super) fn draw_context_menu(&self, frame: &mut Frame<'_>, menu: &mut ContextMenu) {
        let area = menu.anchor.map_or_else(
            || centered(theme::DIALOG_MEDIUM, CONTEXT_MENU_HEIGHT, frame.area()),
            |anchor| {
                anchored(
                    anchor,
                    theme::DIALOG_MEDIUM,
                    CONTEXT_MENU_HEIGHT,
                    frame.area(),
                )
            },
        );
        menu.area = area;
        let inner = widgets::dialog_frame_at(frame, "Commit actions", area);
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        menu.list_area = regions[0];
        let hovered = self.shell.mouse_position.and_then(|pointer| {
            menu.cursor
                .row_at(regions[0], pointer, COMMIT_COMMANDS.len(), 0)
        });
        let items = COMMIT_COMMANDS.iter().enumerate().map(|(index, command)| {
            let availability = command.availability(&self.ops.command_context);
            let suffix = availability
                .reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            ListItem::new(format!("{}{}", command.title(), suffix)).style(theme::hover(
                if availability.enabled {
                    Style::default()
                } else {
                    theme::disabled()
                },
                hovered == Some(index),
            ))
        });
        let mut state = ListState::default().with_selected(Some(menu.cursor.selected));
        *state.offset_mut() = menu.cursor.scroll;
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(theme::LIST_MARKER)
                .highlight_style(theme::focus_row()),
            regions[0],
            &mut state,
        );
        menu.cursor.scroll = state.offset();
        let target = self.graph.selected_commit().map_or_else(
            || "No commit selected".to_owned(),
            |commit| format!("{} {}", short_commit(&commit.sha), commit.subject),
        );
        frame.render_widget(
            widgets::footer_hint(&truncate_to_width(&target, regions[1].width as usize)),
            regions[1],
        );
        menu.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Enter] Run", "[Esc] Close"],
            None,
            self.shell.mouse_position,
        );
    }
}

fn graph_action_availability(action: GraphAction, context: &CommandContext) -> Availability {
    let disabled = |reason| Availability {
        enabled: false,
        reason: Some(reason),
    };
    match action {
        GraphAction::CopySha if context.selected_commit.is_none() => disabled("no commit selected"),
        GraphAction::CopySha => Availability {
            enabled: true,
            reason: None,
        },
        GraphAction::Reset if context.selected_commit.is_none() => disabled("no commit selected"),
        GraphAction::Reset => CommandId::ResetCurrentBranch.availability(context),
        _ => action
            .command()
            .expect("mutating Graph action has a command")
            .availability(context),
    }
}

pub(super) fn short_commit(value: &str) -> String {
    value.chars().take(8).collect()
}

fn uncommitted_matches_query(query: &str, commit: &Commit) -> bool {
    let query = query.to_lowercase();
    query.is_empty() || commit.subject.to_lowercase().contains(&query)
}

const TIME_MIN_WIDTH: usize = 56;
const TIME_WIDTH: usize = 14;
const MIN_GRAPH_WIDTH: usize = 3;
const MIN_SUBJECT_WIDTH: usize = 12;

fn graph_view_width(width: usize) -> usize {
    (width * 2 / 5)
        .max(MIN_GRAPH_WIDTH)
        .min(width.saturating_sub(1))
}

fn mark_clipped_graph(frame: &mut Frame<'_>, x: u16, y: u16, symbol: &str) {
    if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(theme::hint());
    }
}

fn commit_rows(
    commit: &Commit,
    width: usize,
    graph_width: usize,
    now: SystemTime,
) -> [Line<'static>; 2] {
    if width == 0 {
        return [Line::default(), Line::default()];
    }
    let gutter = (graph_width + 1).min(width);
    let text_width = width - gutter;
    let time = (!commit.is_uncommitted() && width >= TIME_MIN_WIDTH)
        .then(|| truncate_to_width(&commit.author_relative(now), TIME_WIDTH))
        .filter(|time| !time.is_empty());
    let time_width = time.as_ref().map_or(0, |time| text_display_width(time) + 2);

    let mut title = vec![Span::raw(" ".repeat(gutter))];
    let subject_width = text_width.saturating_sub(time_width);
    if commit.is_uncommitted() {
        title.push(Span::styled(
            truncate_to_width("Uncommitted changes", subject_width),
            theme::hint().add_modifier(Modifier::BOLD),
        ));
    } else {
        let badges = commit_ref_badges(
            &commit.refs,
            subject_width
                .saturating_sub(MIN_SUBJECT_WIDTH)
                .min(subject_width / 2),
        );
        let badge_width = Line::from(badges.clone()).width();
        title.extend(badges);
        title.extend(commit_subject_spans(
            &commit.subject,
            subject_width.saturating_sub(badge_width),
        ));
    }
    if let Some(time) = time {
        pad_spans_to_width(&mut title, (width + 1).saturating_sub(time_width));
        title.push(Span::styled(time, theme::hint()));
    }
    pad_spans_to_width(&mut title, width);

    let mut detail = vec![Span::raw(" ".repeat(gutter))];
    if commit.is_uncommitted() {
        detail.push(Span::styled(
            truncate_to_width("Working tree", text_width),
            theme::disabled(),
        ));
    } else {
        let sha = truncate_to_width(&short_commit(&commit.sha), text_width);
        let remaining = text_width.saturating_sub(text_display_width(&sha) + 1);
        detail.push(Span::styled(sha, theme::accent()));
        detail.push(Span::raw(" "));
        detail.push(Span::styled(
            truncate_to_width(&commit.author_name, remaining),
            theme::secondary(),
        ));
    }
    pad_spans_to_width(&mut detail, width);
    [Line::from(title), Line::from(detail)]
}

fn commit_ref_badges(references: &[CommitRef], width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut remaining = width;
    for reference in references {
        let separator = usize::from(!spans.is_empty());
        if remaining <= separator + 4 {
            break;
        }
        let (icon, style) = match reference.kind {
            CommitRefKind::Head => ("●", Style::default().bg(theme::BADGE_HEAD_BG)),
            CommitRefKind::LocalBranch => (
                theme::BRANCH_GLYPH,
                Style::default().bg(theme::BADGE_LOCAL_BG),
            ),
            CommitRefKind::RemoteBranch => (
                theme::REMOTE_GLYPH,
                Style::default().bg(theme::BADGE_REMOTE_BG),
            ),
            CommitRefKind::RemoteHead => (
                theme::REMOTE_GLYPH,
                Style::default().bg(theme::BADGE_REMOTE_HEAD_BG),
            ),
            CommitRefKind::Tag => (theme::TAG_GLYPH, Style::default().bg(theme::BADGE_TAG_BG)),
            CommitRefKind::Other => ("◆", Style::default().bg(theme::BADGE_OTHER_BG)),
        };
        let available = remaining.saturating_sub(separator);
        let fixed_width = text_display_width(icon) + 3;
        if available <= fixed_width {
            break;
        }
        let name = truncate_to_width(&reference.name, available.saturating_sub(fixed_width));
        let badge = format!(" {icon} {name} ");
        let badge_width = text_display_width(&badge);
        if badge_width > available {
            break;
        }
        if separator > 0 {
            spans.push(Span::raw(" "));
            remaining = remaining.saturating_sub(1);
        }
        spans.push(Span::styled(badge, style));
        remaining = remaining.saturating_sub(badge_width);
    }
    if !spans.is_empty() && remaining > 0 {
        spans.push(Span::raw(" "));
    }
    spans
}

fn commit_subject_spans(subject: &str, width: usize) -> Vec<Span<'static>> {
    commit_subject_prose_spans(&truncate_to_width(subject, width))
}

fn commit_subject_prose_spans(prose: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut search = 0;
    while let Some(offset) = prose[search..].find('#') {
        let hash = search + offset;
        let mut end = hash + 1;
        while prose.as_bytes().get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == hash + 1 {
            search = hash + 1;
            continue;
        }
        if cursor < hash {
            spans.push(Span::raw(prose[cursor..hash].to_owned()));
        }
        spans.push(Span::styled(
            prose[hash..end].to_owned(),
            Style::default()
                .fg(theme::REFERENCE)
                .add_modifier(Modifier::BOLD),
        ));
        cursor = end;
        search = end;
    }
    if cursor < prose.len() {
        spans.push(Span::raw(prose[cursor..].to_owned()));
    }
    spans
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize) {
    let current = Line::from(spans.clone()).width();
    push_padding(spans, width.saturating_sub(current));
}

fn push_padding(spans: &mut Vec<Span<'static>>, width: usize) {
    if width > 0 {
        spans.push(Span::raw(" ".repeat(width)));
    }
}

pub(super) fn filtered_commit_indices(commits: &[Commit], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    commits
        .iter()
        .enumerate()
        .filter_map(|(index, commit)| {
            let matches = query.is_empty()
                || commit.sha.to_lowercase().starts_with(&query)
                || commit.author_name.to_lowercase().contains(&query)
                || commit.author_email.to_lowercase().contains(&query)
                || commit.subject.to_lowercase().contains(&query)
                || commit.body.to_lowercase().contains(&query)
                || commit
                    .refs
                    .iter()
                    .any(|reference| reference.name.to_lowercase().contains(&query));
            matches.then_some(index)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use crate::git::{
        ChangedPath, Commit, CommitDetails, CommitRef, CommitRefKind, GitOperation, Repository,
    };
    use crate::ui::commands::{CommandId, ResetStep};
    use crate::ui::effect::RefreshProgress;
    use crate::ui::overlay::Overlay;
    use crate::ui::shell::{ActiveTab, PaneFocus};
    use crate::ui::test_support::{
        buffer_text, click, committed_change, find_text_in_row, git, offline_app, press, render,
        row_text, temp_repo, wait_for_refresh,
    };
    use crate::ui::widgets::text_display_width;
    use crate::ui::{App, theme};

    use super::{
        GRAPH_ACTIONS, GraphAction, GraphState, ROW_HEIGHT, commit_ref_badges, commit_rows,
        commit_subject_spans,
    };

    fn numbered_commit(index: usize) -> Commit {
        Commit {
            sha: format!("{index:08x}"),
            parents: Vec::new(),
            author_name: "Ada".to_owned(),
            author_email: "ada@example.com".to_owned(),
            author_time: "2026-09-02T15:28:00+09:00".to_owned(),
            refs: Vec::new(),
            subject: format!("commit-{index:02}"),
            body: String::new(),
        }
    }

    #[test]
    fn all_header_actions_are_searchable_in_commands() {
        let labels = super::GRAPH_ACTIONS.map(|action| action.label());
        for label in labels
            .into_iter()
            .chain(crate::ui::commands::QUICK_ACTIONS.map(|command| command.quick_action_label()))
        {
            assert!(
                crate::ui::commands::COMMANDS
                    .iter()
                    .any(|command| command.matches(label)),
                "missing header action: {label}"
            );
        }
    }

    #[test]
    fn loaded_history_title_ignores_repository_refresh_and_shows_duration() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.graph.history_loaded = true;
        app.graph.history_has_more = true;
        app.history.last_load_duration = Some(Duration::from_millis(420));
        app.refresh.in_flight = true;
        app.refresh.progress = Some(RefreshProgress {
            started: Instant::now(),
            running: "Loading Graph".into(),
        });
        let buffer = render(&mut app, 100, 32);
        let title = row_text(&buffer, app.graph.list_area, app.graph.list_area.y);
        assert!(title.contains("Commits · Loaded in 0.42 s"));
        assert!(!title.contains("Loading Graph"));
        assert!(!title.contains("matches"));
        app.graph.query.text = "missing".into();
        let buffer = render(&mut app, 100, 32);
        let title = row_text(&buffer, app.graph.list_area, app.graph.list_area.y);
        assert!(title.contains("0 matches · Loaded in 0.42 s"));
    }

    #[test]
    fn clear_drops_the_history_and_the_filter_query() {
        let mut graph = GraphState::new(vec![numbered_commit(1), numbered_commit(2)], true);
        graph.selected = 1;
        graph.query.text = "fix".to_owned();

        graph.clear();

        assert!(graph.commits.is_empty());
        assert!(graph.visible.is_empty());
        assert!(!graph.history_loaded);
        assert_eq!(graph.selected, 0);
        assert_eq!(graph.query.text, "");
    }

    #[test]
    fn wide_graphs_are_clipped_so_the_subject_stays_visible_and_scroll_sideways() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.graph.history_loaded = true;
        app.graph.commits = (0..40)
            .map(|index| Commit {
                parents: vec![format!("pending-{index}")],
                subject: format!("keep subject {index}"),
                ..numbered_commit(index)
            })
            .collect();
        app.graph.visible = (0..40).collect();
        app.graph.rebuild_layout();
        let text = |app: &mut App| {
            let buffer = render(app, 120, 30);
            let area = app.graph.history_content_area;
            row_text(&buffer, area, area.y)
        };
        let start = text(&mut app);
        assert!(start.contains("keep subject 0"), "{start}");
        assert!(!start.contains('›') && !start.contains('‹'), "{start}");
        press(&mut app, KeyCode::End);
        let end = text(&mut app);
        assert!(end.contains('›') && !end.contains('‹'), "{end}");
        assert!(end.contains("keep subject"), "{end}");
        for _ in 0..60 {
            press(&mut app, KeyCode::Char('l'));
        }
        let scrolled = text(&mut app);
        assert!(
            scrolled.contains('‹') && !scrolled.contains('›'),
            "{scrolled}"
        );
        assert!(scrolled.contains("keep subject"), "{scrolled}");
        let buffer = render(&mut app, 120, 30);
        let area = app.graph.history_content_area;
        let last = app.graph.selected - app.graph.history_scroll;
        let node_row = row_text(&buffer, area, area.y + (last * ROW_HEIGHT) as u16);
        assert!(node_row.contains('●'), "{node_row}");
    }

    #[test]
    fn anchored_graph_starts_at_the_commit_and_returns_to_head_intact() {
        let (root, mut app) = committed_change("graph-anchor");
        let base = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        git(&root, &["commit", "--allow-empty", "-qm", "Newer"]);
        app.set_tab(ActiveTab::History);
        app.request_history();
        super::super::test_support::wait_for_history(&mut app);
        let head_commits = app.graph.commits.len();
        assert!(head_commits >= 2);

        app.open_graph_anchor(base.clone());
        super::super::test_support::wait_for_history(&mut app);
        app.reveal_pending_commit();
        assert_eq!(app.graph.anchor.as_deref(), Some(base.as_str()));
        assert_eq!(app.graph.commits.len(), 1);
        assert_eq!(
            app.graph
                .selected_commit()
                .map(|commit| commit.sha.as_str()),
            Some(base.as_str())
        );
        let text = buffer_text(&render(&mut app, 160, 40));
        assert!(text.contains("[Esc] Back to HEAD"), "{text}");

        press(&mut app, KeyCode::Esc);
        assert!(app.graph.anchor.is_none());
        assert_eq!(app.graph.commits.len(), head_commits);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_rows_put_the_subject_first_and_metadata_below_then_adapt() {
        let commit = Commit {
            sha: "f889396123456789".to_owned(),
            parents: vec!["parent".to_owned()],
            author_name: "Donghyeok Byun".to_owned(),
            author_email: "donghyeok@example.com".to_owned(),
            author_time: "2026-09-02T15:28:00+09:00".to_owned(),
            refs: vec![
                CommitRef {
                    name: "origin/staging".to_owned(),
                    kind: CommitRefKind::RemoteBranch,
                },
                CommitRef {
                    name: "feature/customer".to_owned(),
                    kind: CommitRefKind::LocalBranch,
                },
                CommitRef {
                    name: "v0.1.0".to_owned(),
                    kind: CommitRefKind::Tag,
                },
            ],
            subject: "feat(customer): add email lookup API".to_owned(),
            body: String::new(),
        };
        let text = |line: &ratatui::text::Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        let now = UNIX_EPOCH + Duration::from_secs(1_788_330_480 + 25 * 60);
        let [title, detail] = commit_rows(&commit, 120, 3, now);
        assert_eq!((title.width(), detail.width()), (120, 120));
        let title_text = text(&title);
        assert!(title_text.ends_with("25 minutes ago "), "{title_text}");
        assert_eq!(text(&detail).trim_end(), "    f8893961 Donghyeok Byun");
        let remote_badge = format!("{} origin/staging", theme::REMOTE_GLYPH);
        let branch_badge = format!("{} feature/customer", theme::BRANCH_GLYPH);
        let tag_badge = format!("{} v0.1.0", theme::TAG_GLYPH);
        let mut previous = 0;
        for token in [
            remote_badge.as_str(),
            branch_badge.as_str(),
            tag_badge.as_str(),
            "feat(customer): add email lookup API",
        ] {
            let position = title_text[previous..]
                .find(token)
                .map(|offset| previous + offset)
                .unwrap_or_else(|| panic!("missing title token: {token}"));
            previous = position + token.len();
        }
        assert!(title.spans.iter().any(|span| {
            span.content.contains("origin/staging")
                && span.style.fg.is_none()
                && span.style.bg.is_some()
        }));

        let second = Commit {
            author_time: "2026-09-02T15:51:30+09:00".to_owned(),
            subject: "fix: compact response".to_owned(),
            ..commit.clone()
        };
        let second_title = text(&commit_rows(&second, 120, 3, now)[0]);
        assert_eq!(
            text_display_width(&title_text[..title_text.find("25 minutes").unwrap()]),
            text_display_width(&second_title[..second_title.find("2 minutes").unwrap()]) - 1
        );

        let [title, detail] = commit_rows(&commit, 40, 3, now);
        assert_eq!((title.width(), detail.width()), (40, 40));
        assert!(text(&title).contains("feat(customer)"));
        assert!(!text(&title).contains("minutes ago"));
        assert!(text(&detail).contains("f8893961"));

        let korean = Commit {
            author_name: "김선우".to_owned(),
            refs: Vec::new(),
            subject: "feat(catalog): 채널 variant 생성·조회·식별자 반영 API 추가".to_owned(),
            ..commit.clone()
        };
        let [title, detail] = commit_rows(&korean, 88, 5, now);
        assert_eq!((title.width(), detail.width()), (88, 88));
        assert!(text(&title).contains("feat(catalog)"));
        assert!(text(&title).contains("minutes ago"));
        assert!(text(&detail).contains("f8893961 김선우"));
    }

    #[test]
    fn graph_summary_accents_only_numeric_reference_and_sha() {
        let subject = "feat(core): English 한국어 API 추가 (#4053)";
        let subject_spans = commit_subject_spans(subject, 80);
        let reference = subject_spans
            .iter()
            .find(|span| span.content == "#4053")
            .expect("numeric reference");
        assert_eq!(reference.style.fg, Some(theme::REFERENCE));
        assert!(reference.style.add_modifier.contains(Modifier::BOLD));
        for prose in subject_spans.iter().filter(|span| span.content != "#4053") {
            assert_eq!(prose.style.fg, None);
            assert!(!prose.style.add_modifier.contains(Modifier::BOLD));
        }
        assert!(
            commit_subject_spans("fix: keep tokens", 80)
                .iter()
                .all(|span| span.style.fg.is_none())
        );

        let commit = Commit {
            sha: "f889396123456789".to_owned(),
            parents: Vec::new(),
            author_name: "Jun Lee".to_owned(),
            author_email: "jun@example.com".to_owned(),
            author_time: "2026-09-02T15:28:00+09:00".to_owned(),
            refs: Vec::new(),
            subject: subject.to_owned(),
            body: String::new(),
        };
        let [title, detail] = commit_rows(
            &commit,
            120,
            3,
            UNIX_EPOCH + Duration::from_secs(1_788_330_480),
        );
        let author = detail
            .spans
            .iter()
            .find(|span| span.content == "Jun Lee")
            .expect("author");
        assert_eq!(author.style.fg, Some(theme::SECONDARY));
        assert!(!author.style.add_modifier.contains(Modifier::BOLD));
        let sha = detail
            .spans
            .iter()
            .find(|span| span.content == "f8893961")
            .expect("short SHA");
        assert_eq!(sha.style.fg, Some(theme::ACCENT));
        assert!(!sha.style.add_modifier.contains(Modifier::BOLD));
        let relative = title
            .spans
            .iter()
            .find(|span| span.content == "0 seconds ago")
            .expect("relative time");
        assert_eq!(relative.style.fg, Some(theme::HINT));
        assert!(!relative.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn graph_ref_roles_use_distinct_neutral_badge_surfaces() {
        let refs = [
            CommitRef {
                name: "HEAD".to_owned(),
                kind: CommitRefKind::Head,
            },
            CommitRef {
                name: "main".to_owned(),
                kind: CommitRefKind::LocalBranch,
            },
            CommitRef {
                name: "origin/main".to_owned(),
                kind: CommitRefKind::RemoteBranch,
            },
            CommitRef {
                name: "origin/HEAD".to_owned(),
                kind: CommitRefKind::RemoteHead,
            },
        ];
        let badges = commit_ref_badges(&refs, 100);
        let badge_spans = badges
            .iter()
            .filter(|span| {
                refs.iter()
                    .any(|reference| span.content.contains(&reference.name))
            })
            .collect::<Vec<_>>();

        assert_eq!(badge_spans.len(), refs.len());
        assert!(badge_spans[1].content.contains(theme::BRANCH_GLYPH));
        assert!(badge_spans[2].content.contains(theme::REMOTE_GLYPH));
        assert!(badge_spans[3].content.contains(theme::REMOTE_GLYPH));
        let tag = commit_ref_badges(
            &[CommitRef {
                name: "v1.0.0".to_owned(),
                kind: CommitRefKind::Tag,
            }],
            40,
        );
        assert!(tag[0].content.contains(theme::TAG_GLYPH));
        assert!(badge_spans.iter().all(|span| span.style.fg.is_none()));
        assert!(
            badge_spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD))
        );
        let backgrounds = badge_spans
            .iter()
            .map(|span| span.style.bg.expect("badge background"))
            .collect::<HashSet<_>>();
        assert_eq!(backgrounds.len(), refs.len());
    }

    #[test]
    fn selected_graph_row_fills_the_surface_without_hiding_badge_color() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.graph.history_loaded = true;
        app.graph.commits = vec![Commit {
            sha: "f889396123456789".to_owned(),
            parents: Vec::new(),
            author_name: "Ada".to_owned(),
            author_email: "ada@example.com".to_owned(),
            author_time: "2026-09-02T15:28:00+09:00".to_owned(),
            refs: vec![CommitRef {
                name: "origin/main".to_owned(),
                kind: CommitRefKind::RemoteBranch,
            }],
            subject: "feat: color graph rows".to_owned(),
            body: String::new(),
        }];
        app.graph.visible = vec![0];
        app.graph.selected = 0;
        app.graph.rebuild_layout();
        let buffer = render(&mut app, 200, 20);
        let row = app.graph.list_area.y + 1;
        let left = app.graph.list_area.x + 1;
        let right = app.graph.list_area.right() - 2;
        for line in [row, row + 1] {
            assert_eq!(buffer[(left, line)].bg, theme::SURFACE_SELECTION);
            assert_eq!(buffer[(right, line)].bg, theme::SURFACE_SELECTION);
        }
        assert!(buffer[(left + 1, row)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(left + 1, row)].symbol(), "●");
        assert!(
            !buffer[(left + 3, row + 1)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let badge = (left..=right)
            .find(|column| buffer[(*column, row)].symbol() == theme::REMOTE_GLYPH)
            .expect("remote badge icon");
        assert_ne!(buffer[(badge, row)].bg, theme::SURFACE_SELECTION);
    }

    #[test]
    fn graph_commit_table_uses_the_full_width_above_details() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        render(&mut app, 160, 24);

        assert_eq!(app.graph.list_area.x, 0);
        assert_eq!(app.graph.list_area.width, 160);
    }

    #[test]
    fn graph_commit_list_scrolls_with_a_viewport_relative_pointer_and_scrollbar() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.graph.history_loaded = true;
        let template = Commit {
            sha: "00000000".to_owned(),
            parents: Vec::new(),
            author_name: "Ada".to_owned(),
            author_email: "ada@example.com".to_owned(),
            author_time: "2026-09-02T15:28:00+09:00".to_owned(),
            refs: Vec::new(),
            subject: String::new(),
            body: String::new(),
        };
        app.graph.commits = (0..30)
            .map(|index| Commit {
                sha: format!("{index:08x}"),
                subject: format!("commit-{index:02}"),
                ..template.clone()
            })
            .collect();
        app.graph.visible = (0..app.graph.commits.len()).collect();
        app.graph.selected = 0;
        let buffer = render(&mut app, 100, 20);
        let list = app.graph.list_area;
        let scrollbar_column = list.right().saturating_sub(2);
        let thumb_row = (list.y + 1..list.bottom().saturating_sub(1))
            .find(|row| buffer[(scrollbar_column, *row)].symbol() == "█")
            .expect("scrollbar thumb");
        assert_eq!(buffer[(scrollbar_column, thumb_row)].fg, theme::ACCENT);
        assert!(
            !buffer[(scrollbar_column, thumb_row)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let track_row = (list.y + 1..list.bottom().saturating_sub(1))
            .find(|row| buffer[(scrollbar_column, *row)].symbol() == "┃")
            .expect("scrollbar track");
        assert_eq!(buffer[(scrollbar_column, track_row)].fg, theme::MUTED);

        let wheel = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        let scrollbar = app.graph.history_scrollbar_area.expect("history scrollbar");
        let track_max = usize::from(scrollbar.height.saturating_sub(1));
        let midpoint = usize::from(scrollbar.height / 2).min(track_max);
        let expected_scroll = midpoint
            .saturating_mul(app.max_history_scroll())
            .saturating_add(track_max / 2)
            / track_max;
        app.handle(wheel(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.x,
            scrollbar.y.saturating_add(midpoint as u16),
        ))
        .unwrap();
        assert_eq!(app.graph.history_scroll, expected_scroll);
        assert!(app.graph.selected >= app.graph.history_scroll);
        assert!(
            app.graph.selected
                < app
                    .graph
                    .history_scroll
                    .saturating_add(app.graph.history_content_area.height as usize)
        );
        app.handle(wheel(
            MouseEventKind::Up(MouseButton::Left),
            scrollbar.x,
            scrollbar.y.saturating_add(midpoint as u16),
        ))
        .unwrap();
        app.graph.selected = 0;
        app.graph.history_scroll = 0;
        render(&mut app, 100, 20);

        for _ in 0..10 {
            app.handle(wheel(
                MouseEventKind::ScrollDown,
                list.x.saturating_add(1),
                list.y.saturating_add(1),
            ))
            .unwrap();
            render(&mut app, 100, 20);
        }
        assert_eq!(app.graph.selected, 10);
        let buffer = render(&mut app, 100, 20);
        let top_row = (list.x + 1..scrollbar_column)
            .map(|column| buffer[(column, list.y + 1)].symbol())
            .collect::<String>();
        let first_visible = (0..30)
            .find(|index| top_row.contains(&format!("commit-{index:02}")))
            .expect("top visible commit");
        assert!(first_visible > 0);

        app.handle(wheel(
            MouseEventKind::Down(MouseButton::Left),
            list.x.saturating_add(1),
            list.y.saturating_add(1),
        ))
        .unwrap();
        assert_eq!(app.graph.selected, first_visible);

        let selected = app.graph.selected;
        app.handle(wheel(MouseEventKind::ScrollDown, 0, 0)).unwrap();
        assert_eq!(app.graph.selected, selected);

        app.handle(wheel(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar_column,
            list.bottom().saturating_sub(2),
        ))
        .unwrap();
        assert!(app.scrollbar_drag.is_some());
        assert_eq!(app.graph.selected, 29);
        assert_eq!(app.graph.history_scroll, app.max_history_scroll());
        let buffer = render(&mut app, 100, 20);
        assert_eq!(
            buffer[(scrollbar_column, list.bottom().saturating_sub(2))].symbol(),
            "█"
        );
        app.handle(wheel(
            MouseEventKind::Drag(MouseButton::Left),
            scrollbar_column,
            list.y.saturating_add(1),
        ))
        .unwrap();
        assert_eq!(app.graph.selected, 0);
        app.handle(wheel(
            MouseEventKind::Up(MouseButton::Left),
            scrollbar_column,
            list.y.saturating_add(1),
        ))
        .unwrap();
        assert!(app.scrollbar_drag.is_none());

        app.graph.commits.truncate(1);
        app.graph.visible = vec![0];
        app.graph.selected = 0;
        render(&mut app, 100, 20);
        assert_eq!(app.graph.history_scroll, 0);
        assert!(app.graph.history_scrollbar_area.is_none());
        assert_eq!(
            app.graph.history_content_area.width,
            list.width.saturating_sub(2)
        );
    }

    #[test]
    fn graph_lower_row_reuses_changes_ratio_and_expands_preview() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.inspect.details = Some(CommitDetails {
            commit: Commit {
                sha: "abc12345".to_owned(),
                parents: Vec::new(),
                author_name: "Ada".to_owned(),
                author_email: "ada@example.com".to_owned(),
                author_time: "2026-09-02T15:28:00+09:00".to_owned(),
                refs: Vec::new(),
                subject: "feat: preview a commit file".to_owned(),
                body: String::new(),
            },
            changes: vec![ChangedPath {
                status: "M".to_owned(),
                path: "src/lib.rs".to_owned(),
            }],
        });
        render(&mut app, 200, 32);
        let widths = super::super::diff::changes_pane_widths(200, app.diff.changes_splits);

        assert_eq!(app.workspaces.repository_area.width, widths[0]);
        assert_eq!(app.inspect.commit_detail_area.width, widths[1] + widths[2]);
        assert_eq!(app.inspect.commit_preview_area, Rect::default());
        assert!(app.workspaces.repository_area.y >= app.graph.list_area.bottom());

        app.inspect.commit_preview_path = Some("src/lib.rs".to_owned());
        let buffer = &render(&mut app, 200, 32);

        assert_eq!(app.workspaces.repository_area.width, widths[0]);
        assert_eq!(app.inspect.commit_detail_area.width, widths[1]);
        assert_eq!(app.inspect.commit_preview_area.width, widths[2]);
        assert_eq!(
            app.inspect.commit_preview_area.x,
            app.inspect.commit_detail_area.right()
        );
        let rendered = (0..32)
            .flat_map(|row| (0..200).map(move |column| buffer[(column, row)].symbol().to_owned()))
            .collect::<String>();
        assert!(rendered.contains("Before"));
        assert!(rendered.contains("After"));
        assert!(!rendered.contains("Loading preview"));
    }

    #[test]
    fn context_menu_survives_pointer_motion_and_closes_on_an_outside_click() {
        let root = temp_repo("context-menu-mouse");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);
        render(&mut app, 100, 32);
        let list = app.graph.history_content_area;
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: list.x + 2,
            row: list.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        let Overlay::ContextMenu(menu) = &app.overlay else {
            panic!("right-click opens Commit actions");
        };
        assert_eq!(menu.anchor, Some((list.x + 2, list.y)));
        render(&mut app, 100, 32);
        let area = app.overlay.context_menu().unwrap().list_area;
        assert_eq!((area.x, area.y), (list.x + 3, list.y + 1));
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Up(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            app.handle(Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
            assert!(
                matches!(app.overlay, Overlay::ContextMenu(_)),
                "{kind:?} keeps the menu"
            );
        }
        app.shell.mouse_position = Some((area.x + 2, area.y + 1));
        let buffer = render(&mut app, 100, 32);
        assert_eq!(buffer[(area.x + 2, area.y + 1)].bg, theme::SURFACE_HOVER);
        let menu = app.overlay.context_menu().unwrap();
        let dialog = menu.area;
        let buttons = menu.buttons;
        assert_eq!((dialog.x, dialog.y), (list.x + 2, list.y));
        assert_eq!(dialog.width, theme::DIALOG_MEDIUM);
        assert_eq!(area.height, 7);
        let text = buffer_text(&buffer);
        assert!(text.contains("Commit actions"));
        assert!(text.contains("[Enter] Run"));
        assert!(text.contains("[Esc] Close"));
        assert!(row_text(&buffer, area, area.y).starts_with(theme::LIST_MARKER));
        assert_eq!(buffer[(area.x + 3, area.y)].bg, theme::SURFACE_FOCUS);
        assert!(
            buffer[(area.x + 3, area.y)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let status = row_text(&buffer, area, area.bottom());
        assert!(status.contains("Base"));
        assert_eq!(buffer[(area.x, area.bottom())].fg, theme::HINT);
        assert_eq!(buttons.primary.y, area.bottom() + 1);
        click(&mut app, area.x + 1, area.bottom());
        assert!(matches!(app.overlay, Overlay::ContextMenu(_)));
        click(&mut app, buttons.secondary.x + 2, buttons.secondary.y + 1);
        assert!(matches!(app.overlay, Overlay::None));
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: list.x + 2,
            row: list.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        render(&mut app, 100, 32);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.overlay.context_menu().unwrap().cursor.selected, 1);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.overlay.context_menu().unwrap().cursor.selected, 0);
        click(&mut app, 0, 0);
        assert!(matches!(app.overlay, Overlay::None));

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert_eq!(app.overlay.context_menu().unwrap().anchor, None);
        render(&mut app, 100, 32);
        let menu = app.overlay.context_menu().unwrap();
        assert_eq!(menu.area.x, (100 - theme::DIALOG_MEDIUM) / 2);
        let buttons = menu.buttons;
        click(&mut app, buttons.primary.x + 2, buttons.primary.y + 1);
        assert!(
            matches!(app.overlay, Overlay::Name(_)),
            "[Enter] Run dispatches the selected commit action"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_commit_actions_share_the_action_bar_geometry_and_keep_the_requested_order() {
        let root = temp_repo("graph-action-bar");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);

        let buffer = render(&mut app, 120, 24);
        let normal = (0..120)
            .map(|column| buffer[(column, 5)].symbol())
            .collect::<String>();
        for action in ["  Checkout", "  Rebase", "  Reset", "  Copy SHA"] {
            assert!(normal.contains(action), "missing {action:?} in {normal:?}");
        }
        let mut previous = 0;
        for action in GRAPH_ACTIONS {
            let position = normal[previous..]
                .find(action.label())
                .map(|offset| previous + offset)
                .expect("Graph action label");
            previous = position + action.label().len();
        }
        assert_eq!(app.graph.action_areas.len(), GRAPH_ACTIONS.len());
        assert!(
            app.graph
                .action_areas
                .iter()
                .all(|(_, area)| area.y == 5 && area.height == 1)
        );
        let normal_positions = GRAPH_ACTIONS.map(|action| {
            let area = app
                .graph
                .action_areas
                .iter()
                .find_map(|(owner, area)| (*owner == action).then_some(*area))
                .expect("Graph action area");
            let label_x =
                find_text_in_row(&buffer, area.y, action.label()).expect("Graph action label");
            let icon_x = (area.x..area.right())
                .find(|x| buffer[(*x, area.y)].symbol() == action.icon())
                .expect("Graph action icon");
            assert_eq!(label_x, icon_x + 3, "{action:?} icon-label gap");
            (action, icon_x, label_x, area.y)
        });
        let copy_area = app.graph.action_areas.last().unwrap().1;
        click(&mut app, copy_area.x + 1, copy_area.y);
        assert!(matches!(app.overlay, Overlay::CopySha(_)));
        press(&mut app, KeyCode::Esc);

        app.shell.shortcut_hints = true;
        let buffer = render(&mut app, 120, 24);
        let hinted = (0..120)
            .map(|column| buffer[(column, 5)].symbol())
            .collect::<String>();
        for hint in [
            "X  Checkout",
            "R  Rebase",
            "Y  Cherry-pick",
            "V  Revert",
            "T  Reset",
            "H  Copy SHA",
        ] {
            assert!(hinted.contains(hint), "missing {hint:?} in {hinted:?}");
        }
        for (action, icon_x, label_x, row) in normal_positions {
            assert_eq!(
                find_text_in_row(&buffer, row, action.label()),
                Some(label_x),
                "{action:?} label moved in shortcut mode"
            );
            assert_eq!(
                buffer[(icon_x, row)].symbol(),
                action.shortcut().to_ascii_uppercase().to_string(),
                "{action:?} shortcut moved from the icon column"
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_commit_actions_open_dialogs_for_the_selected_commit() {
        let root = temp_repo("graph-action-dialogs");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "second\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Second"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);
        let current_head = app.ops.command_context.head_commit.clone().unwrap();
        app.select(1);
        let selected = app.graph.selected_commit().unwrap().sha.clone();
        assert_ne!(current_head, selected);

        app.run_graph_action(GraphAction::Checkout);
        crate::ui::test_support::wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Target(_)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::CheckoutCommit(selected.clone()))
        );
        let screen = buffer_text(&render(&mut app, 100, 32));
        assert!(screen.contains(&format!(
            "Checkout target: {} · Base",
            super::short_commit(&selected)
        )));
        press(&mut app, KeyCode::Esc);

        app.run_graph_action(GraphAction::Rebase);
        crate::ui::test_support::wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Target(_)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::RebaseHere(selected.clone()))
        );
        let screen = buffer_text(&render(&mut app, 100, 32));
        assert!(screen.contains(&format!(
            "Rebase onto: {} · Base",
            super::short_commit(&selected)
        )));
        press(&mut app, KeyCode::Esc);

        app.run_graph_action(GraphAction::CherryPick);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::CherryPick(selected.clone()))
        );
        let screen = buffer_text(&render(&mut app, 100, 32));
        assert!(screen.contains(&format!(
            "Commit to apply: {} · Base",
            super::short_commit(&selected)
        )));
        press(&mut app, KeyCode::Esc);
        app.run_graph_action(GraphAction::Revert);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::Revert(selected.clone()))
        );
        let screen = buffer_text(&render(&mut app, 100, 32));
        assert!(screen.contains(&format!(
            "Commit to revert: {} · Base",
            super::short_commit(&selected)
        )));
        press(&mut app, KeyCode::Esc);

        app.run_graph_action(GraphAction::Reset);
        let reset = app.overlay.reset().expect("Reset flow");
        assert_eq!(reset.step, ResetStep::Mode);
        assert!(reset.fixed_target);
        assert_eq!(reset.current_head, current_head);
        assert_eq!(reset.selected_target().unwrap().commit, selected);
        let screen = buffer_text(&render(&mut app, 100, 32));
        assert!(screen.contains(&format!(
            "Target  {} · Base",
            super::short_commit(&selected)
        )));
        assert!(screen.contains(&format!("HEAD  {}", super::short_commit(&current_head))));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay.reset().unwrap().step, ResetStep::Confirm);
        let (request_rx, _result_tx) = super::super::test_support::intercept_foreground(&mut app);
        press(&mut app, KeyCode::Enter);
        let request = request_rx
            .try_recv()
            .expect("selected-commit reset request");
        assert!(matches!(
            request,
            super::super::effect::ForegroundRequest::Operation {
                command: CommandId::ResetCurrentBranch,
                operation: GitOperation::Reset {
                    mode: crate::git::ResetMode::Soft,
                    target,
                    expected_head,
                    ..
                },
                ..
            } if target == selected && expected_head == current_head
        ));
        app.foreground.action = None;

        app.run_graph_action(GraphAction::CopySha);
        let copy = app.overlay.copy_sha().expect("Copy SHA dialog");
        assert_eq!(copy.sha, selected);
        let buffer = render(&mut app, 100, 24);
        assert!(buffer_text(&buffer).contains("Copy this commit SHA?"));
        press(&mut app, KeyCode::Enter);
        assert!(
            matches!(app.pending_clipboard.as_ref(), Some(super::super::PendingClipboard::Sha(value)) if value == &selected)
        );
        assert!(matches!(app.overlay, Overlay::None));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_filter_escape_clears_the_query_and_enter_keeps_it() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        press(&mut app, KeyCode::Char('/'));
        assert!(matches!(app.overlay, Overlay::GraphFilter));
        for character in "fix".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert_eq!(app.graph.query.text, "fix");
        assert!(matches!(app.overlay, Overlay::GraphFilter));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.graph.query.text, "fix");

        press(&mut app, KeyCode::Char('/'));
        app.handle(Event::Paste("es".to_owned())).unwrap();
        assert_eq!(app.graph.query.text, "fixes");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.graph.query.text, "");
    }

    #[test]
    fn commits_pane_marks_focus_with_the_border_and_spins_while_loading() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let buffer = render(&mut app, 80, 24);
        let list = app.graph.list_area;
        assert_eq!(buffer[(list.x, list.y)].fg, theme::ACCENT);
        assert_eq!(buffer[(list.x + 1, list.y)].symbol(), "C");
        assert_eq!(buffer[(list.x + 1, list.y)].fg, Color::Reset);

        app.focus = PaneFocus::Details;
        let buffer = render(&mut app, 80, 24);
        assert_eq!(buffer[(list.x, list.y)].fg, Color::Reset);

        app.history.pending = Some(crate::ui::history::HistoryRequest {
            id: crate::ui::effect::RequestId::FIRST,
            generation: crate::ui::effect::ReadGeneration::ZERO,
            path: std::path::PathBuf::from("/repo"),
            offset: 0,
            count: 100,
            replace: true,
            intent: crate::ui::effect::RefreshIntent::Interaction,
            follow_head: false,
            start: None,
        });
        let buffer = render(&mut app, 80, 24);
        let glyph = buffer[(list.x + 1, list.y)].symbol().to_owned();
        assert!(theme::SPINNER_FRAMES.contains(&glyph.as_str()));
        assert_eq!(buffer[(list.x + 1, list.y)].fg, theme::ACCENT);
        assert!(row_text(&buffer, list, list.y).contains("Loading Graph"));
    }

    #[test]
    fn filter_box_click_opens_the_filter_and_shows_a_cursor_in_warning_style() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let buffer = render(&mut app, 80, 24);
        let filter = app.graph.filter_area;
        assert_eq!(buffer[(filter.x + 1, filter.y + 1)].symbol(), "/");
        assert_eq!(buffer[(filter.x + 1, filter.y + 1)].fg, Color::Reset);
        assert!(!row_text(&buffer, filter, filter.y + 1).contains(theme::CURSOR_GLYPH));

        click(&mut app, filter.x + 4, filter.y + 1);
        assert!(matches!(app.overlay, Overlay::GraphFilter));
        press(&mut app, KeyCode::Char('a'));
        let buffer = render(&mut app, 80, 24);
        assert!(row_text(&buffer, filter, filter.y + 1).starts_with("│/ a"));
        assert_eq!(buffer[(filter.x + 1, filter.y + 1)].fg, theme::WARNING);
        let cursor = (filter.x + 4, filter.y + 1);
        assert_eq!(buffer[cursor].symbol(), theme::CURSOR_GLYPH);
        assert_eq!(buffer[cursor].fg, theme::ACCENT);

        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        let buffer = render(&mut app, 80, 24);
        assert_eq!(buffer[(filter.x + 1, filter.y + 1)].fg, Color::Reset);
        assert!(!row_text(&buffer, filter, filter.y + 1).contains(theme::CURSOR_GLYPH));
    }

    #[test]
    fn commits_keys_follow_the_list_grammar() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.graph.history_loaded = true;
        app.graph.commits = (0..30).map(numbered_commit).collect();
        app.graph.visible = (0..30).collect();
        render(&mut app, 100, 32);
        let page = app.graph.page_rows();
        assert_eq!(page, usize::from(app.graph.history_content_area.height) / 2);
        assert!(page > 1);

        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.graph.selected, page);
        press(&mut app, KeyCode::End);
        assert_eq!(app.graph.selected, 29);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.graph.selected, 29 - page);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.graph.selected, 0);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.graph.selected, 2);
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.graph.selected, 0);

        app.focus = PaneFocus::Details;
        press(&mut app, KeyCode::Char('/'));
        assert!(matches!(app.overlay, Overlay::GraphFilter));
    }

    #[test]
    fn empty_repository_starts_the_graph_at_uncommitted() {
        let root = temp_repo("graph-uncommitted-unborn");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);

        assert!(app.graph.commits.is_empty());
        assert!(app.graph.showing_uncommitted());
        assert_eq!(app.graph.display_len(), 1);
        assert!(app.graph.selected_commit().is_none());
        let buffer = render(&mut app, 100, 32);
        let list = app.graph.history_content_area;
        assert!(row_text(&buffer, list, list.y).contains("Uncommitted"));
        assert!(buffer_text(&buffer).contains("Branch: main · Uncommitted"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_repository_with_untracked_files_shows_an_uncommitted_row() {
        let root = temp_repo("graph-uncommitted-empty");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("new.txt"), "hello\n").unwrap();
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);

        assert!(app.graph.commits.is_empty());
        assert!(app.graph.showing_uncommitted());
        assert!(app.graph.selected_commit().is_none());
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(text.contains("Uncommitted"));
        assert!(text.contains('●'));
        assert!(
            !row_text(
                &buffer,
                app.inspect.commit_detail_area,
                app.inspect.commit_detail_area.y + 1
            )
            .contains("No commits")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_history_keeps_head_selected_and_disables_actions_on_uncommitted() {
        let (root, mut app) = committed_change("graph-uncommitted-dirty");
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);

        assert_eq!(app.graph.commits[0].subject, "Base");
        assert!(app.graph.showing_uncommitted());
        assert_eq!(app.graph.selected, 1);
        assert_eq!(app.graph.selected_commit().unwrap().subject, "Base");
        let buffer = render(&mut app, 100, 32);
        let list = app.graph.history_content_area;
        assert!(row_text(&buffer, list, list.y).contains("Uncommitted"));
        assert!(row_text(&buffer, list, list.y + 1).contains("Working tree"));
        assert!(row_text(&buffer, list, list.y + 2).contains("Base"));
        assert!(row_text(&buffer, list, list.y).contains('○'));
        assert!(row_text(&buffer, list, list.y + 1).contains('│'));

        app.select(0);
        assert!(app.graph.selected_commit().is_none());
        app.refresh_command_selection_context();
        assert!(
            !super::graph_action_availability(GraphAction::CopySha, &app.ops.command_context)
                .enabled
        );
        assert!(
            !super::graph_action_availability(GraphAction::Checkout, &app.ops.command_context)
                .enabled
        );

        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Capture working tree"]);
        let (context, changes) = {
            let repository = app.repository.as_ref().unwrap();
            (
                crate::ui::commands::CommandContext::from_git_facts(
                    repository.command_facts(),
                    None,
                ),
                repository.working_changes().unwrap(),
            )
        };
        app.ops.command_context = context;
        app.files.changes = changes;
        app.sync_uncommitted_row();
        assert!(!app.graph.showing_uncommitted());
        assert!(app.ops.command_context.head_commit.is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
