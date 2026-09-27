use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, ScrollbarOrientation, ScrollbarState};
use unicode_width::UnicodeWidthChar;

use crate::git::{BlameInfo, DiffTarget, ReadError};
use crate::ui::review::{CodeSelection, ReviewSide};

use super::effect::{
    BlameRead, BlameTarget, DiffOwner, DiffReadTarget, ForegroundRequest, HighlightedDiff,
    PendingDiff, ReadGeneration, RequestId,
};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::review::SelectionSurface;
use super::shell::{ActiveTab, PaneFocus};
use super::syntax::{DiffDocument, FoldKey, FoldKind};
use super::widgets::{
    self, Axis, ConfirmButton, ConfirmButtons, ScrollbarDrag, TextEdit, TextField, area_hovered,
    chord, left_click,
};
use super::{App, ScrollbarOwner, theme};

const CHANGE_SPLIT_SCALE: u16 = 1_000;
const DEFAULT_CHANGE_SPLITS: [u16; 2] = [200, 600];
const MIN_FILES_WIDTH: u16 = 20;
const MIN_DIFF_WIDTH: u16 = 24;
const LINE_JUMP_HEIGHT: u16 = 9;
const NOT_A_REPOSITORY: &str = "Not a Git repository";
pub(super) const HOVER_BLAME_DWELL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HoverBlame {
    pub(super) row: usize,
    pub(super) since: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChangesDivider {
    FilesBefore,
    BeforeAfter,
}

pub(super) struct DiffState {
    pub(super) diff_target: DiffTarget,
    pub(super) diff_text: String,
    pub(super) diff_scroll: u16,
    pub(super) diff_horizontal_scroll: u16,
    pub(super) diff_area: Rect,
    pub(super) before_diff_area: Rect,
    pub(super) after_diff_area: Rect,
    pub(super) focused_diff_row: usize,
    pub(super) focus_after_changes_refresh: bool,
    pub(super) diff_document: DiffDocument,
    pub(super) highlight_key: Option<u64>,
    pub(super) fold_toggles: HashSet<FoldKey>,
    pub(super) pending_fold_focus: Option<FoldKey>,
    pub(super) pending_line_jump: Option<(ReviewSide, usize)>,
    pub(super) diff_side: ReviewSide,
    pub(super) diff_language: String,
    pub(super) pending_diff: Option<PendingDiff>,
    pub(super) blame: Option<BlameInfo>,
    pub(super) blame_error: Option<String>,
    pub(super) pending_blame: Option<BlameTarget>,
    pub(super) blame_read: Option<BlameRead>,
    pub(super) hover_blame: Option<HoverBlame>,
    pub(super) changes_splits: [u16; 2],
    pub(super) changes_body_area: Rect,
    pub(super) files_before_divider_area: Rect,
    pub(super) before_after_divider_area: Rect,
    pub(super) resize_drag: Option<ChangesDivider>,
}

impl DiffState {
    pub(super) fn is_resizing(&self) -> bool {
        self.resize_drag.is_some()
    }

    pub(super) fn blame_in_progress(&self) -> bool {
        self.pending_blame.is_some() || self.blame_read.is_some()
    }

    pub(super) fn replace_diff(&mut self, target: DiffTarget, text: String) {
        self.diff_target = target;
        self.diff_text = text;
        self.diff_scroll = 0;
        self.diff_horizontal_scroll = 0;
        self.focused_diff_row = 0;
        self.blame = None;
        self.blame_error = None;
        self.fold_toggles.clear();
    }
}

impl Default for DiffState {
    fn default() -> Self {
        Self {
            diff_target: DiffTarget::WorkingTreeAgainstIndex,
            diff_text: String::new(),
            diff_scroll: 0,
            diff_horizontal_scroll: 0,
            diff_area: Rect::default(),
            before_diff_area: Rect::default(),
            after_diff_area: Rect::default(),
            focused_diff_row: 0,
            focus_after_changes_refresh: false,
            highlight_key: None,
            diff_document: DiffDocument::default(),
            fold_toggles: HashSet::new(),
            pending_fold_focus: None,
            pending_line_jump: None,
            diff_side: ReviewSide::After,
            diff_language: String::new(),
            pending_diff: None,
            blame: None,
            blame_error: None,
            pending_blame: None,
            blame_read: None,
            hover_blame: None,
            changes_splits: DEFAULT_CHANGE_SPLITS,
            changes_body_area: Rect::default(),
            files_before_divider_area: Rect::default(),
            before_after_divider_area: Rect::default(),
            resize_drag: None,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct LineJump {
    pub(super) input: TextField,
    pub(super) buttons: ConfirmButtons,
}

impl App {
    pub(super) fn line_number_for_row(&self, side: ReviewSide, row: usize) -> Option<usize> {
        match side {
            ReviewSide::Before => self.diff.diff_document.before_line_number(row),
            ReviewSide::After => self.diff.diff_document.after_line_number(row),
        }
    }

    fn open_line_jump(&mut self) {
        self.overlay = Overlay::LineJump(LineJump::default());
    }

    fn submit_line_jump(&mut self) {
        let Overlay::LineJump(jump) = &mut self.overlay else {
            return;
        };
        let Ok(line) = jump.input.text.parse::<usize>() else {
            jump.input.set_error("Enter a positive line number.");
            return;
        };
        if line == 0 {
            jump.input.set_error("Line numbers start at 1.");
            return;
        }
        match self.jump_to_line(line) {
            Ok(()) => self.overlay = Overlay::None,
            Err(error) => {
                if let Overlay::LineJump(jump) = &mut self.overlay {
                    jump.input.set_error(error);
                }
            }
        }
    }

    fn jump_to_line(&mut self, line: usize) -> Result<(), String> {
        let folded_line = (self.row_for_line(self.diff.diff_side, line).is_none()).then(|| {
            self.diff
                .diff_document
                .folds()
                .map(|(_, key)| key)
                .find(|key| fold_contains_line(*key, self.diff.diff_side, line))
        });
        if let Some(key) = folded_line.flatten() {
            match key.kind {
                FoldKind::Context => {
                    self.diff.fold_toggles.insert(key);
                }
                FoldKind::Change => {
                    self.diff.fold_toggles.remove(&key);
                }
            }
            self.diff.focus_after_changes_refresh = false;
            self.diff.pending_line_jump = Some((self.diff.diff_side, line));
            self.request_refold();
            return Ok(());
        }
        let Some(row) = self.row_for_line(self.diff.diff_side, line) else {
            return Err(format!(
                "Line {line} is unavailable on {}.",
                self.diff.diff_side.label()
            ));
        };
        self.diff.focused_diff_row = row;
        self.scroll_diff_focus_into_view();
        self.load_blame_for_row(row);
        Ok(())
    }

    pub(super) fn row_for_line(&self, side: ReviewSide, line: usize) -> Option<usize> {
        match side {
            ReviewSide::Before => self.diff.diff_document.before_row_for_line(line),
            ReviewSide::After => self.diff.diff_document.after_row_for_line(line),
        }
    }

    fn focus_first_after_change(&mut self) {
        self.focus = PaneFocus::Diff;
        self.diff.diff_side = ReviewSide::After;
        self.diff.focused_diff_row = first_changed_after_row(&self.diff.diff_document);
        self.diff.diff_scroll = 0;
        self.diff.diff_horizontal_scroll = 0;
        self.clear_selection();
        self.load_blame_for_row(self.diff.focused_diff_row);
    }

    pub(super) fn toggle_fold(&mut self, row: usize) -> bool {
        let Some(key) = self.diff.diff_document.fold_key(row) else {
            return false;
        };
        self.finish_visual_selection();
        if !self.diff.fold_toggles.remove(&key) {
            self.diff.fold_toggles.insert(key);
        }
        self.diff.pending_fold_focus = Some(key);
        self.request_refold();
        true
    }

    fn request_refold(&mut self) {
        let Some(target) = self.current_diff_read_target() else {
            return;
        };
        self.foreground.reads.diff.advance();
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.diff.generation;
        let request = ForegroundRequest::Refold {
            id,
            generation,
            target: target.clone(),
            text: self.diff.diff_text.clone(),
        };
        match self.foreground.request(request) {
            Ok(_) => {
                self.diff.pending_diff = Some(PendingDiff {
                    id,
                    generation,
                    target,
                    started: Instant::now(),
                });
            }
            Err(error) => {
                self.diff.pending_diff = None;
                self.shell.error = Some(format!("Diff worker stopped: {error}"));
            }
        }
    }

    fn current_diff_read_target(&self) -> Option<DiffReadTarget> {
        let repository = self.repository.as_ref()?;
        let change = self.files.changes.get(self.files.change_selected)?;
        Some(DiffReadTarget {
            owner: DiffOwner::Changes,
            path: repository.root().to_owned(),
            file: change.path.clone(),
            target: self.diff.diff_target.clone(),
            fold_toggles: self.diff.fold_toggles.clone(),
        })
    }

    fn diff_target_is_current(&self, target: &DiffReadTarget) -> bool {
        match target.owner {
            DiffOwner::Changes => self.current_diff_read_target().as_ref() == Some(target),
            DiffOwner::CommitPreview => {
                self.shell.active_tab == ActiveTab::History
                    && self.current_commit_preview_target().as_ref() == Some(target)
            }
        }
    }

    pub(super) fn request_file_diff(&mut self) {
        if matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        ) {
            return;
        }
        let Some(target) = self.current_diff_read_target() else {
            return;
        };
        self.foreground.reads.diff.advance();
        self.foreground.reads.blame.advance();
        self.diff.pending_blame = None;
        self.diff.hover_blame = None;
        self.diff.blame = None;
        self.diff.blame_error = None;
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.diff.generation;
        let request = ForegroundRequest::Diff {
            id,
            generation,
            owner: target.owner,
            path: target.path.clone(),
            file: target.file.clone(),
            target: target.target.clone(),
            fold_toggles: target.fold_toggles.clone(),
        };
        match self.foreground.request(request) {
            Ok(_) => {
                self.diff.pending_diff = Some(PendingDiff {
                    id,
                    generation,
                    target,
                    started: Instant::now(),
                });
            }
            Err(error) => {
                self.diff.pending_diff = None;
                self.shell.error = Some(format!("Diff worker stopped: {error}"));
            }
        }
    }

    pub(super) fn show_non_repository_diff(&mut self) {
        let target = self.diff.diff_target.clone();
        self.diff.replace_diff(target, NOT_A_REPOSITORY.to_owned());
        self.diff.diff_language = "Plain text".to_owned();
        self.diff.diff_document = DiffDocument::plain(NOT_A_REPOSITORY);
        self.diff.highlight_key = None;
    }

    pub(super) fn apply_highlighted_diff(&mut self, highlighted: HighlightedDiff) {
        self.diff.diff_language = highlighted.language;
        self.diff.diff_document = highlighted.split;
        self.diff.highlight_key = Some(highlighted.key);
    }

    fn move_diff_focus(&mut self, delta: isize) {
        self.focus_diff_row(self.diff.focused_diff_row.saturating_add_signed(delta));
    }

    fn focus_diff_row(&mut self, row: usize) {
        let last = self.diff.diff_document.len().saturating_sub(1);
        self.diff.focused_diff_row = row.min(last);
        self.scroll_diff_focus_into_view();
    }

    fn max_diff_scroll(&self) -> usize {
        let viewport = self.diff_source_area(self.diff.diff_area).height as usize;
        self.diff.diff_document.len().saturating_sub(viewport)
    }

    fn scroll_diff(&mut self, delta: isize) {
        let next = (self.diff.diff_scroll as usize)
            .saturating_add_signed(delta)
            .min(self.max_diff_scroll());
        self.diff.diff_scroll = next.min(u16::MAX as usize) as u16;
    }

    fn diff_page(&self) -> isize {
        isize::try_from(self.diff_source_area(self.diff.diff_area).height.max(1))
            .unwrap_or(isize::MAX)
    }

    fn max_diff_horizontal_scroll(&self) -> usize {
        let before_viewport = self.diff_source_area(self.diff.before_diff_area).width as usize;
        let after_viewport = self.diff_source_area(self.diff.after_diff_area).width as usize;
        let before = self
            .diff
            .diff_document
            .before_max_width()
            .saturating_sub(before_viewport);
        let after = self
            .diff
            .diff_document
            .after_max_width()
            .saturating_sub(after_viewport);
        before.max(after)
    }

    fn diff_scrollbar_visibility(&self) -> (bool, bool) {
        let before_inner = self
            .diff
            .before_diff_area
            .inner(ratatui::layout::Margin::new(1, 1));
        let after_inner = self
            .diff
            .after_diff_area
            .inner(ratatui::layout::Margin::new(1, 1));
        let before_width = self.diff.diff_document.before_max_width();
        let after_width = self.diff.diff_document.after_max_width();
        let mut vertical = self.diff.diff_document.len() > before_inner.height as usize
            || self.diff.diff_document.len() > after_inner.height as usize;
        let mut horizontal =
            before_width > before_inner.width as usize || after_width > after_inner.width as usize;
        for _ in 0..2 {
            horizontal = before_width
                > before_inner.width.saturating_sub(u16::from(vertical)) as usize
                || after_width > after_inner.width.saturating_sub(u16::from(vertical)) as usize;
            vertical = self.diff.diff_document.len()
                > before_inner.height.saturating_sub(u16::from(horizontal)) as usize
                || self.diff.diff_document.len()
                    > after_inner.height.saturating_sub(u16::from(horizontal)) as usize;
        }
        (vertical, horizontal)
    }

    fn diff_source_area(&self, panel: Rect) -> Rect {
        let (vertical, horizontal) = self.diff_scrollbar_visibility();
        let inner = panel.inner(ratatui::layout::Margin::new(1, 1));
        Rect::new(
            inner.x,
            inner.y,
            inner.width.saturating_sub(u16::from(vertical)),
            inner.height.saturating_sub(u16::from(horizontal)),
        )
    }

    fn diff_vertical_scrollbar_area(&self, panel: Rect) -> Option<Rect> {
        let (vertical, horizontal) = self.diff_scrollbar_visibility();
        if !vertical {
            return None;
        }
        let inner = panel.inner(ratatui::layout::Margin::new(1, 1));
        Some(Rect::new(
            inner.right().saturating_sub(1),
            inner.y,
            1,
            inner.height.saturating_sub(u16::from(horizontal)),
        ))
    }

    fn diff_horizontal_scrollbar_area(&self, panel: Rect) -> Option<Rect> {
        let (vertical, horizontal) = self.diff_scrollbar_visibility();
        if !horizontal {
            return None;
        }
        let inner = panel.inner(ratatui::layout::Margin::new(1, 1));
        Some(Rect::new(
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width.saturating_sub(u16::from(vertical)),
            1,
        ))
    }

    fn scroll_diff_horizontal(&mut self, delta: isize) {
        let next = (self.diff.diff_horizontal_scroll as usize)
            .saturating_add_signed(delta)
            .min(self.max_diff_horizontal_scroll());
        self.diff.diff_horizontal_scroll = next.min(u16::MAX as usize) as u16;
    }

    fn diff_horizontal_scrollbar_at(&self, column: u16, row: u16) -> Option<Rect> {
        if self.max_diff_horizontal_scroll() == 0 {
            return None;
        }
        let position = (column, row).into();
        [self.diff.before_diff_area, self.diff.after_diff_area]
            .into_iter()
            .filter_map(|area| self.diff_horizontal_scrollbar_area(area))
            .find(|area| area.contains(position))
    }

    fn set_diff_horizontal_scroll_from_pointer(&mut self, area: Rect, column: u16) {
        let max_scroll = self.max_diff_horizontal_scroll();
        let track_max = usize::from(area.width.saturating_sub(1));
        if max_scroll == 0 || track_max == 0 {
            self.diff.diff_horizontal_scroll = 0;
            return;
        }
        let pointer = usize::from(column.saturating_sub(area.x)).min(track_max);
        let position = pointer
            .saturating_mul(max_scroll)
            .saturating_add(track_max / 2)
            / track_max;
        self.diff.diff_horizontal_scroll = position.min(u16::MAX as usize) as u16;
    }

    fn diff_vertical_scrollbar_at(&self, column: u16, row: u16) -> Option<Rect> {
        if self.max_diff_scroll() == 0 {
            return None;
        }
        let position = (column, row).into();
        [self.diff.before_diff_area, self.diff.after_diff_area]
            .into_iter()
            .filter_map(|area| self.diff_vertical_scrollbar_area(area))
            .find(|area| area.contains(position))
    }

    fn set_diff_scroll_from_pointer(&mut self, area: Rect, row: u16) {
        let max_scroll = self.max_diff_scroll();
        let track_max = usize::from(area.height.saturating_sub(1));
        if max_scroll == 0 || track_max == 0 {
            self.diff.diff_scroll = 0;
            return;
        }
        let pointer = usize::from(row.saturating_sub(area.y)).min(track_max);
        let position = pointer
            .saturating_mul(max_scroll)
            .saturating_add(track_max / 2)
            / track_max;
        self.diff.diff_scroll = position.min(u16::MAX as usize) as u16;
    }

    fn scroll_diff_focus_into_view(&mut self) {
        let viewport = self.diff.diff_area.height.saturating_sub(2) as usize;
        if self.diff.focused_diff_row < self.diff.diff_scroll as usize {
            self.diff.diff_scroll = self.diff.focused_diff_row as u16;
        } else if self.diff.focused_diff_row >= self.diff.diff_scroll as usize + viewport {
            self.diff.diff_scroll =
                self.diff
                    .focused_diff_row
                    .saturating_sub(viewport.saturating_sub(1)) as u16;
        }
    }

    fn blame_target_for_row(&self, row: usize) -> Option<BlameTarget> {
        let line = self.diff.diff_document.after_line_number(row)?;
        let file = self
            .files
            .changes
            .get(self.files.change_selected)
            .map(|change| change.path.clone())?;
        let revision = blame_revision(&self.diff.diff_target);
        let path = self.repository.as_ref()?.root().to_owned();
        Some(BlameTarget {
            path,
            file,
            line,
            revision,
        })
    }

    pub(super) fn load_blame_for_row(&mut self, row: usize) {
        let Some(target) = self.blame_target_for_row(row) else {
            self.diff.pending_blame = None;
            self.diff.blame = None;
            self.diff.blame_error = Some("select a context or added line".to_owned());
            return;
        };
        if self
            .diff
            .blame
            .as_ref()
            .is_some_and(|blame| blame.line == target.line)
        {
            return;
        }
        if self.diff.blame_read.as_ref().is_some_and(|read| {
            self.foreground.reads.blame.is_current(read.generation) && read.target == target
        }) {
            self.diff.pending_blame = None;
            return;
        }
        if matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        ) {
            self.diff.pending_blame = None;
            return;
        }
        self.diff.pending_blame = Some(target);
        self.foreground.reads.blame.advance();
        self.start_pending_blame();
    }

    pub(super) fn request_dwelt_blame(&mut self) -> bool {
        let Some(candidate) = self.diff.hover_blame else {
            return false;
        };
        if candidate.since.elapsed() < HOVER_BLAME_DWELL {
            return false;
        }
        self.diff.hover_blame = None;
        if self.focus != PaneFocus::Diff || self.diff.focused_diff_row != candidate.row {
            return false;
        }
        self.load_blame_for_row(candidate.row);
        true
    }

    pub(super) fn start_pending_blame(&mut self) {
        let switching = matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        );
        if self.diff.blame_read.is_some() || switching || self.workspaces.pending_switch.is_some() {
            return;
        }
        let Some(target) = self.diff.pending_blame.take() else {
            return;
        };
        if self
            .blame_target_for_row(self.diff.focused_diff_row)
            .as_ref()
            != Some(&target)
        {
            return;
        }
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.blame.generation;
        let request = ForegroundRequest::Blame {
            id,
            generation,
            path: target.path.clone(),
            file: target.file.clone(),
            line: target.line,
            revision: target.revision.clone(),
        };
        self.diff.blame = None;
        match self.foreground.request(request) {
            Ok(_) => {
                self.diff.blame_error = None;
                self.diff.blame_read = Some(BlameRead {
                    id,
                    generation,
                    target,
                    started: Instant::now(),
                });
            }
            Err(error) => {
                self.diff.blame_error = Some(format!("Git worker stopped: {error}"));
            }
        }
    }

    pub(super) fn diff_contains(&self, column: u16, row: u16) -> bool {
        let position = (column, row).into();
        self.diff.before_diff_area.contains(position)
            || self.diff.after_diff_area.contains(position)
    }

    fn changes_divider_at(&self, column: u16, row: u16) -> Option<ChangesDivider> {
        let position = (column, row).into();
        if self.diff.files_before_divider_area.contains(position) {
            Some(ChangesDivider::FilesBefore)
        } else if self.diff.before_after_divider_area.contains(position) {
            Some(ChangesDivider::BeforeAfter)
        } else {
            None
        }
    }

    fn resize_changes_divider(&mut self, column: u16) {
        let Some(divider) = self.diff.resize_drag else {
            return;
        };
        let total = self.diff.changes_body_area.width;
        if total < MIN_FILES_WIDTH + MIN_DIFF_WIDTH * 2 {
            return;
        }
        let widths = changes_pane_widths(total, self.diff.changes_splits);
        let first = widths[0];
        let second = first.saturating_add(widths[1]);
        let pointer = column
            .saturating_sub(self.diff.changes_body_area.x)
            .min(total);
        let target = match divider {
            ChangesDivider::FilesBefore => {
                pointer.clamp(MIN_FILES_WIDTH, second.saturating_sub(MIN_DIFF_WIDTH))
            }
            ChangesDivider::BeforeAfter => pointer.clamp(
                first.saturating_add(MIN_DIFF_WIDTH),
                total.saturating_sub(MIN_DIFF_WIDTH),
            ),
        };
        let split = ((u32::from(target) * u32::from(CHANGE_SPLIT_SCALE)) / u32::from(total)) as u16;
        match divider {
            ChangesDivider::FilesBefore => self.diff.changes_splits[0] = split,
            ChangesDivider::BeforeAfter => self.diff.changes_splits[1] = split,
        }
    }

    fn diff_position(&self, column: u16, row: u16) -> Option<(ReviewSide, usize)> {
        let position = (column, row).into();
        let (side, panel) = if self.diff.before_diff_area.contains(position) {
            (ReviewSide::Before, self.diff.before_diff_area)
        } else if self.diff.after_diff_area.contains(position) {
            (ReviewSide::After, self.diff.after_diff_area)
        } else {
            return None;
        };
        let area = self.diff_source_area(panel);
        if !area.contains(position) {
            return None;
        }
        let source_row = row.saturating_sub(area.y) as usize + self.diff.diff_scroll as usize;
        (source_row < self.diff.diff_document.len()).then_some((side, source_row))
    }

    pub(super) fn take_pending_diff(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        target: &DiffReadTarget,
    ) -> bool {
        let owns_result = self.diff.pending_diff.as_ref().is_some_and(|pending| {
            pending.id == id && pending.generation == generation && pending.target == *target
        });
        if !owns_result {
            return false;
        }
        self.diff.pending_diff = None;
        self.foreground.reads.diff.is_current(generation) && self.diff_target_is_current(target)
    }

    pub(super) fn apply_diff_read(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        target: DiffReadTarget,
        result: Result<(String, HighlightedDiff), ReadError>,
    ) {
        if !self.take_pending_diff(id, generation, &target) {
            return;
        }
        match target.owner {
            DiffOwner::Changes => self.apply_diff(result),
            DiffOwner::CommitPreview => self.apply_commit_preview(target, result),
        }
    }

    pub(super) fn apply_diff(&mut self, result: Result<(String, HighlightedDiff), ReadError>) {
        match result {
            Ok((diff_text, highlighted)) => {
                self.shell.error = None;
                self.diff.diff_text = diff_text;
                self.apply_highlighted_diff(highlighted);
                if self.diff.pending_fold_focus.is_some() || self.diff.pending_line_jump.is_some() {
                    self.settle_diff_after_refresh();
                    return;
                }
                self.diff.diff_scroll = 0;
                self.diff.focused_diff_row = 0;
                if std::mem::take(&mut self.diff.focus_after_changes_refresh) {
                    self.focus_first_after_change();
                } else if let Some((side, line)) = self.diff.pending_line_jump.take() {
                    self.diff.diff_side = side;
                    if let Err(error) = self.jump_to_line(line) {
                        self.show_action_error(&error);
                    }
                }
            }
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => {
                self.clear_pending_diff_focus();
                self.shell.error = Some(error);
            }
        }
    }

    pub(super) fn apply_blame(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        target: BlameTarget,
        result: Result<BlameInfo, ReadError>,
    ) {
        let claimed = self.diff.blame_read.as_ref().is_some_and(|read| {
            read.id == id && read.generation == generation && read.target == target
        });
        if claimed {
            self.diff.blame_read = None;
        }
        if !claimed
            || !self.foreground.reads.blame.is_current(generation)
            || self
                .blame_target_for_row(self.diff.focused_diff_row)
                .as_ref()
                != Some(&target)
        {
            return;
        }
        match result {
            Ok(blame) if blame.line == target.line => {
                self.diff.blame = Some(blame);
                self.diff.blame_error = None;
            }
            Ok(_) => {
                self.diff.blame = None;
                self.diff.blame_error = Some("Blame result did not match the line".to_owned());
            }
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => {
                self.diff.blame = None;
                self.diff.blame_error = Some(error);
            }
        }
    }

    pub(super) fn settle_diff_after_refresh(&mut self) {
        self.diff.diff_scroll = self
            .diff
            .diff_scroll
            .min(self.max_diff_scroll().min(u16::MAX as usize) as u16);
        self.diff.focused_diff_row = self
            .diff
            .focused_diff_row
            .min(self.diff.diff_document.len().saturating_sub(1));
        self.diff.blame = None;
        self.diff.blame_error = None;
        if std::mem::take(&mut self.diff.focus_after_changes_refresh) {
            self.diff.pending_fold_focus = None;
            self.diff.pending_line_jump = None;
            self.focus_first_after_change();
        } else if let Some((side, line)) = self.diff.pending_line_jump.take() {
            self.diff.diff_side = side;
            if let Some(row) = self.row_for_line(side, line) {
                self.diff.focused_diff_row = row;
                self.scroll_diff_focus_into_view();
                self.load_blame_for_row(row);
            } else {
                self.show_action_error(&format!("Line {line} is unavailable on {}.", side.label()));
            }
        } else if let Some(key) = self.diff.pending_fold_focus.take() {
            self.diff.focused_diff_row = self
                .diff
                .diff_document
                .folds()
                .find_map(|(row, candidate)| (candidate == key).then_some(row))
                .unwrap_or(self.diff.focused_diff_row);
        }
    }

    pub(super) fn clear_pending_diff_focus(&mut self) {
        self.diff.focus_after_changes_refresh = false;
        self.diff.pending_fold_focus = None;
        self.diff.pending_line_jump = None;
    }

    pub(super) fn handle_diff_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        let focused = self.focus == PaneFocus::Diff;
        match key.code {
            KeyCode::Char('b') => {
                self.diff.hover_blame = None;
                self.load_blame_for_row(self.diff.focused_diff_row);
            }
            KeyCode::PageDown => self.scroll_diff(self.diff_page()),
            KeyCode::PageUp => self.scroll_diff(-self.diff_page()),
            _ if !focused => return Ok(false),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_fold(self.diff.focused_diff_row);
            }
            KeyCode::Char('v') if !self.review.changes.keyboard_selecting => {
                self.begin_visual_selection(self.diff.diff_side, self.diff.focused_diff_row, true);
            }
            KeyCode::Char('g') if !self.review.changes.keyboard_selecting => self.open_line_jump(),
            KeyCode::Down | KeyCode::Char('j') => self.move_diff_focus(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_diff_focus(-1),
            KeyCode::Home => self.focus_diff_row(0),
            KeyCode::End => self.focus_diff_row(usize::MAX),
            KeyCode::Right => self.scroll_diff_horizontal(1),
            KeyCode::Left => self.scroll_diff_horizontal(-1),
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn handle_diff_mouse(&mut self, mouse: MouseEvent) -> Result<bool, String> {
        let (column, row) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(drag) = self.diff_scrollbar_at(column, row) {
                    self.scrollbar_drag = Some((ScrollbarOwner::Diff, drag));
                    self.drag_diff_scrollbar(drag, column, row);
                } else if let Some(divider) = self.changes_divider_at(column, row) {
                    self.diff.resize_drag = Some(divider);
                } else if self.diff_contains(column, row) {
                    self.click_diff(column, row);
                } else {
                    return Ok(false);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((ScrollbarOwner::Diff, drag)) = self.scrollbar_drag {
                    self.drag_diff_scrollbar(drag, column, row);
                } else if self.diff.resize_drag.is_some() {
                    self.resize_changes_divider(column);
                } else if self.review.changes.dragging_selection && self.diff_contains(column, row)
                {
                    if let Some((side, row)) = self.diff_position(column, row)
                        && side == self.diff.diff_side
                    {
                        self.diff.focused_diff_row = row;
                    }
                } else {
                    return Ok(false);
                }
            }
            MouseEventKind::ScrollDown if self.diff_contains(column, row) => self.scroll_diff(1),
            MouseEventKind::ScrollUp if self.diff_contains(column, row) => self.scroll_diff(-1),
            MouseEventKind::ScrollRight if self.diff_contains(column, row) => {
                self.scroll_diff_horizontal(1)
            }
            MouseEventKind::ScrollLeft if self.diff_contains(column, row) => {
                self.scroll_diff_horizontal(-1)
            }
            MouseEventKind::Moved if self.diff_contains(column, row) => {
                if let Some((side, row)) = self.diff_position(column, row) {
                    self.focus = PaneFocus::Diff;
                    self.diff.diff_side = side;
                    self.diff.focused_diff_row = row;
                    self.diff.hover_blame = Some(HoverBlame {
                        row,
                        since: Instant::now(),
                    });
                } else {
                    self.diff.hover_blame = None;
                    self.unfocus_diff();
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn click_diff(&mut self, column: u16, row: u16) {
        let Some((side, row)) = self.diff_position(column, row) else {
            self.unfocus_diff();
            return;
        };
        self.focus = PaneFocus::Diff;
        self.diff.diff_side = side;
        self.diff.focused_diff_row = row;
        self.diff.hover_blame = None;
        if self.toggle_fold(row) {
            return;
        }
        if self.review.changes.keyboard_selecting {
            self.finish_visual_selection();
            return;
        }
        self.begin_visual_selection(side, row, false);
        self.load_blame_for_row(row);
    }

    fn diff_scrollbar_at(&self, column: u16, row: u16) -> Option<ScrollbarDrag> {
        self.diff_horizontal_scrollbar_at(column, row)
            .map(|area| ScrollbarDrag {
                area,
                axis: Axis::Horizontal,
            })
            .or_else(|| {
                self.diff_vertical_scrollbar_at(column, row)
                    .map(|area| ScrollbarDrag {
                        area,
                        axis: Axis::Vertical,
                    })
            })
    }

    pub(super) fn drag_diff_scrollbar(&mut self, drag: ScrollbarDrag, column: u16, row: u16) {
        match drag.axis {
            Axis::Horizontal => self.set_diff_horizontal_scroll_from_pointer(drag.area, column),
            Axis::Vertical => self.set_diff_scroll_from_pointer(drag.area, row),
        }
    }

    pub(super) fn handle_line_jump(&mut self, input: &Event) {
        let Overlay::LineJump(jump) = &mut self.overlay else {
            return;
        };
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => self.submit_line_jump(),
                KeyCode::Esc => self.overlay = Overlay::None,
                KeyCode::Backspace => {
                    jump.input.edit(TextEdit::Backspace);
                }
                KeyCode::Char(character) if character.is_ascii_digit() && !chord(key.modifiers) => {
                    jump.input.edit(TextEdit::Insert(character));
                }
                _ => {}
            },
            Event::Paste(text) if text.chars().all(|character| character.is_ascii_digit()) => {
                jump.input.edit(TextEdit::Paste(text.clone()));
            }
            _ => match left_click(input).and_then(|pointer| jump.buttons.hit(pointer)) {
                Some(ConfirmButton::Primary) => self.submit_line_jump(),
                Some(ConfirmButton::Secondary) => self.overlay = Overlay::None,
                None => {}
            },
        }
    }

    pub(super) fn draw_diff(&mut self, frame: &mut Frame<'_>, before: Rect, after: Rect) {
        self.diff.before_diff_area = before;
        self.diff.after_diff_area = after;
        self.diff.diff_area = after;
        let before_source_area = self.diff_source_area(before);
        let after_source_area = self.diff_source_area(after);
        let effective_selection = self.effective_selection_on(SelectionSurface::Changes);
        let before_lines = self.panel_viewport_lines(
            ReviewSide::Before,
            before_source_area,
            effective_selection.as_ref(),
        );
        let after_lines = self.panel_viewport_lines(
            ReviewSide::After,
            after_source_area,
            effective_selection.as_ref(),
        );
        let selected_side = effective_selection.as_ref().map(|selection| selection.side);
        let selection_title = self.selection_title(SelectionSurface::Changes);
        let before_title = match &selection_title {
            Some(title) if selected_side == Some(ReviewSide::Before) => title.clone(),
            _ => Line::raw(format!("Before · {}", self.diff.diff_language)),
        };
        let after_title = match &selection_title {
            Some(title) if selected_side == Some(ReviewSide::After) => title.clone(),
            _ => Line::raw("After"),
        };
        let focused = self.focus == PaneFocus::Diff;
        let before_block = widgets::pane_block(
            before_title,
            focused && self.diff.diff_side == ReviewSide::Before,
        );
        let mut after_block = widgets::pane_block(
            after_title,
            focused && self.diff.diff_side == ReviewSide::After,
        );
        if let Some(blame) = self.blame_line() {
            after_block = after_block.title_bottom(blame);
        }
        frame.render_widget(before_block, self.diff.before_diff_area);
        frame.render_widget(after_block, self.diff.after_diff_area);
        frame.render_widget(
            Paragraph::new(before_lines).scroll((0, self.diff.diff_horizontal_scroll)),
            before_source_area,
        );
        frame.render_widget(
            Paragraph::new(after_lines).scroll((0, self.diff.diff_horizontal_scroll)),
            after_source_area,
        );
        self.draw_diff_horizontal_scrollbars(frame);
        self.draw_diff_vertical_scrollbars(frame);
        self.draw_selection_decorations(
            frame,
            SelectionSurface::Changes,
            [before_source_area, after_source_area],
            self.diff.diff_scroll as usize,
        );
    }

    pub(super) fn panel_viewport_lines(
        &self,
        side: ReviewSide,
        area: Rect,
        selection: Option<&CodeSelection>,
    ) -> Vec<Line<'static>> {
        let width = area.width as usize;
        let height = area.height as usize;
        let horizontal = self.diff.diff_horizontal_scroll as usize;
        let first = self.diff.diff_scroll as usize;
        let document = &self.diff.diff_document;
        let focused = self.focus == PaneFocus::Diff;
        let selection = selection.filter(|selection| selection.side == side);
        let mut lines = Vec::with_capacity(height);
        let mut row = first;
        while lines.len() < height {
            let source = match side {
                ReviewSide::Before => document.before_line(row),
                ReviewSide::After => document.after_line(row),
            };
            let Some(source) = source else {
                break;
            };
            let code_row = self.line_number_for_row(side, row).is_some();
            let mut line = pad_diff_line(source.clone(), width, horizontal);
            if focused && row == self.diff.focused_diff_row {
                emphasize_diff_line(&mut line, code_row, theme::focus_row(), width);
            }
            if selection.is_some_and(|selection| selection.contains(row)) {
                emphasize_diff_line(&mut line, code_row, theme::selection_row(), width);
            }
            lines.push(line);
            row += 1;
        }
        lines.truncate(height);
        lines
    }

    fn blame_line(&self) -> Option<Line<'static>> {
        if self.focus != PaneFocus::Diff {
            return None;
        }
        if self.diff.blame_in_progress() {
            let elapsed = self
                .diff
                .blame_read
                .as_ref()
                .map(|read| read.started.elapsed())
                .unwrap_or_default();
            return Some(Line::from(vec![
                Span::raw(" "),
                theme::spinner_span(elapsed),
                Span::styled(" Blame ", theme::hint()),
            ]));
        }
        if let Some(blame) = self.diff.blame.as_ref() {
            return Some(Line::from(Span::styled(
                format!(" Blame: {} ", blame_text(blame)),
                theme::hint(),
            )));
        }
        self.diff.blame_error.as_ref().map(|error| {
            Line::from(Span::styled(
                format!(" Blame unavailable · {error} "),
                theme::hint(),
            ))
        })
    }

    pub(super) fn draw_diff_horizontal_scrollbars(&self, frame: &mut Frame<'_>) {
        let max_scroll = self.max_diff_horizontal_scroll();
        if max_scroll == 0 {
            return;
        }
        let scrollbar = widgets::scrollbar(ScrollbarOrientation::HorizontalBottom);
        for area in [self.diff.before_diff_area, self.diff.after_diff_area] {
            let Some(track_area) = self.diff_horizontal_scrollbar_area(area) else {
                continue;
            };
            if track_area.is_empty() {
                continue;
            }
            let viewport_width = self.diff_source_area(area).width as usize;
            let mut state = ScrollbarState::new(max_scroll.saturating_add(1))
                .position(self.diff.diff_horizontal_scroll as usize)
                .viewport_content_length(viewport_width);
            frame.render_stateful_widget(scrollbar.clone(), track_area, &mut state);
        }
    }

    pub(super) fn draw_diff_vertical_scrollbars(&self, frame: &mut Frame<'_>) {
        let max_scroll = self.max_diff_scroll();
        if max_scroll == 0 {
            return;
        }
        let scrollbar = widgets::scrollbar(ScrollbarOrientation::VerticalRight);
        for area in [self.diff.before_diff_area, self.diff.after_diff_area] {
            let Some(track_area) = self.diff_vertical_scrollbar_area(area) else {
                continue;
            };
            if track_area.is_empty() {
                continue;
            }
            let viewport_height = self.diff_source_area(area).height as usize;
            let mut state = ScrollbarState::new(max_scroll.saturating_add(1))
                .position(self.diff.diff_scroll as usize)
                .viewport_content_length(viewport_height);
            frame.render_stateful_widget(scrollbar.clone(), track_area, &mut state);
        }
    }

    pub(super) fn draw_changes_dividers(&self, frame: &mut Frame<'_>) {
        for (divider, area) in [
            (
                ChangesDivider::FilesBefore,
                self.diff.files_before_divider_area,
            ),
            (
                ChangesDivider::BeforeAfter,
                self.diff.before_after_divider_area,
            ),
        ] {
            if self.diff.resize_drag != Some(divider)
                && !area_hovered(self.shell.mouse_position, area)
            {
                continue;
            }
            let line_area = Rect::new(area.x.saturating_add(1), area.y, 1, area.height);
            let lines = (0..line_area.height)
                .map(|_| Line::raw("│"))
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines).style(theme::accent_bold().bg(theme::SURFACE_HOVER)),
                line_area,
            );
        }
    }

    pub(super) fn draw_line_jump(&self, frame: &mut Frame<'_>, jump: &mut LineJump) {
        let inner = widgets::dialog_frame(
            frame,
            &format!("Go to {} line", self.diff.diff_side.label()),
            theme::DIALOG_NARROW,
            LINE_JUMP_HEIGHT,
        );
        let regions = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(inner);
        let input = Line::from(vec![
            Span::raw(jump.input.text.clone()),
            theme::cursor_span(jump.input.cursor_started.elapsed()),
        ]);
        frame.render_widget(
            Paragraph::new(input).block(Block::default().borders(Borders::ALL).title("Line")),
            regions[0],
        );
        let status = match jump.input.error.as_deref() {
            Some(error) => Line::styled(format!("Error: {error}"), theme::error_text()),
            None => Line::styled("Enter a positive line number", theme::hint()),
        };
        frame.render_widget(Paragraph::new(status), regions[1]);
        jump.buttons = widgets::dialog_footer(
            frame,
            regions[2],
            ["[Enter] Go", "[Esc] Cancel"],
            None,
            self.shell.mouse_position,
        );
    }
}

pub(super) fn changes_pane_widths(total: u16, splits: [u16; 2]) -> [u16; 3] {
    if total < MIN_FILES_WIDTH + MIN_DIFF_WIDTH * 2 {
        let files = total / 5;
        let before = total.saturating_mul(2) / 5;
        return [files, before, total.saturating_sub(files + before)];
    }
    let scaled =
        |split: u16| ((u32::from(total) * u32::from(split)) / u32::from(CHANGE_SPLIT_SCALE)) as u16;
    let first = scaled(splits[0]).clamp(MIN_FILES_WIDTH, total.saturating_sub(MIN_DIFF_WIDTH * 2));
    let second = scaled(splits[1]).clamp(
        first.saturating_add(MIN_DIFF_WIDTH),
        total.saturating_sub(MIN_DIFF_WIDTH),
    );
    [first, second - first, total - second]
}

pub(super) fn divider_hit_area(boundary: u16, body: Rect) -> Rect {
    Rect::new(boundary.saturating_sub(1), body.y, 3, body.height)
}

fn first_changed_after_row(document: &DiffDocument) -> usize {
    (0..document.len())
        .find(|&row| {
            document
                .after_source(row)
                .is_some_and(|after| document.before_source(row) != Some(after))
        })
        .or_else(|| (0..document.len()).find(|&row| document.after_source(row).is_some()))
        .unwrap_or_default()
}

fn fold_contains_line(key: FoldKey, side: ReviewSide, line: usize) -> bool {
    let (start, end) = match side {
        ReviewSide::Before => (key.old_start, key.old_end),
        ReviewSide::After => (key.new_start, key.new_end),
    };
    (start..end).contains(&line)
}

pub(super) fn horizontal_line_slice(
    line: Line<'static>,
    horizontal_offset: usize,
    viewport_width: usize,
) -> Line<'static> {
    let mut skipped: usize = 0;
    let mut rendered: usize = 0;
    let mut output = Vec::new();
    for span in line.spans {
        let mut text = String::new();
        for character in span.content.chars() {
            let width = character.width().unwrap_or_default();
            if skipped < horizontal_offset {
                skipped = skipped.saturating_add(width);
                continue;
            }
            if rendered.saturating_add(width) > viewport_width {
                break;
            }
            text.push(character);
            rendered = rendered.saturating_add(width);
        }
        if !text.is_empty() {
            output.push(Span::styled(text, span.style));
        }
        if rendered >= viewport_width {
            break;
        }
    }
    Line::from(output).style(line.style)
}

pub(super) fn pad_diff_line(
    mut line: Line<'static>,
    viewport_width: usize,
    horizontal_offset: usize,
) -> Line<'static> {
    if line.style.bg.is_some() {
        let width = viewport_width.saturating_add(horizontal_offset);
        let padding = width.saturating_sub(line.width());
        if padding > 0 {
            line.spans
                .push(Span::styled(" ".repeat(padding), line.style));
        }
    }
    line
}

pub(super) fn padded_diff_lines<'a>(
    lines: impl IntoIterator<Item = &'a Line<'static>>,
    viewport_width: usize,
    horizontal_offset: usize,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .cloned()
        .map(|line| pad_diff_line(line, viewport_width, horizontal_offset))
        .collect()
}

pub(super) fn emphasize_diff_line(
    line: &mut Line<'static>,
    code_row: bool,
    overlay: Style,
    width: usize,
) {
    if code_row {
        if let Some(gutter) = line.spans.first_mut() {
            gutter.style = gutter.style.patch(overlay).fg(theme::TEXT);
        }
        return;
    }
    line.style = line.style.patch(overlay);
    for span in &mut line.spans {
        span.style = span.style.patch(overlay);
    }
    let padding = width.saturating_sub(line.width());
    if padding > 0 {
        line.spans
            .push(Span::styled(" ".repeat(padding), line.style));
    }
}

pub(super) fn blame_revision(target: &DiffTarget) -> Option<String> {
    match target {
        DiffTarget::CommitAgainstParent { commit, .. } => Some(commit.clone()),
        _ => None,
    }
}

fn blame_text(blame: &BlameInfo) -> String {
    if blame.sha.chars().all(|character| character == '0') {
        return format!(
            "Line {} · Uncommitted · {} · Working tree change",
            blame.line, blame.author
        );
    }
    let short = &blame.sha[..blame.sha.len().min(8)];
    format!(
        "Line {} · {} · {} · {}",
        blame.line, short, blame.author, blame.summary
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    use crate::git::{BlameInfo, GitOperation, ReadError, Repository};
    use crate::ui::commands::CommandId;
    use crate::ui::effect::{ForegroundRequest, ForegroundResult, HighlightedDiff, RequestId};
    use crate::ui::lanes::{ForegroundAction, ForegroundKind};
    use crate::ui::overlay::Overlay;
    use crate::ui::review::{CodeSelection, ReviewSide};
    use crate::ui::shell::PaneFocus;
    use crate::ui::syntax::{DiffDocument, FoldKind};
    use crate::ui::test_support::{
        buffer_text, click, committed_change, git, intercept_foreground, offline_app, press,
        render, row_text, temp_repo, test_diff_document, test_diff_document_with_after_metadata,
        wait_for_diff,
    };
    use crate::ui::{App, theme};

    use super::{HOVER_BLAME_DWELL, blame_text, emphasize_diff_line, padded_diff_lines};

    #[test]
    fn blame_applies_only_to_the_latest_focused_line() {
        let root = temp_repo("blame-latest");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "one changed\ntwo changed\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let first_row = app.row_for_line(ReviewSide::After, 1).unwrap();
        let second_row = app.row_for_line(ReviewSide::After, 2).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);

        app.diff.focused_diff_row = first_row;
        app.load_blame_for_row(first_row);
        let first = request_rx.try_recv().expect("first Blame request");
        let ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } = first
        else {
            panic!("unexpected request")
        };
        let first_generation = generation;
        app.diff.focused_diff_row = second_row;
        app.load_blame_for_row(second_row);
        assert!(app.foreground.reads.blame.generation > first_generation);
        assert_eq!(
            app.foreground
                .reads
                .blame
                .cancellation
                .load(Ordering::Acquire),
            app.foreground.reads.blame.generation.get()
        );
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        result_tx
            .send(ForegroundResult::Blame {
                id,
                generation: first_generation,
                path,
                file,
                line,
                revision,
                result: Ok(BlameInfo {
                    sha: "1111111111111111111111111111111111111111".to_owned(),
                    author: "First".to_owned(),
                    author_email: "first@example.com".to_owned(),
                    author_time: 0,
                    summary: "First line".to_owned(),
                    line: 1,
                }),
            })
            .unwrap();
        app.receive_foreground_results();
        assert!(app.diff.blame.is_none());

        let second = request_rx.try_recv().expect("latest Blame request");
        let ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } = second
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 2);
        assert!(generation > first_generation);
        result_tx
            .send(ForegroundResult::Blame {
                id,
                generation,
                path,
                file,
                line,
                revision,
                result: Ok(BlameInfo {
                    sha: "2222222222222222222222222222222222222222".to_owned(),
                    author: "Second".to_owned(),
                    author_email: "second@example.com".to_owned(),
                    author_time: 0,
                    summary: "Second line".to_owned(),
                    line: 2,
                }),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(app.diff.blame.as_ref().map(|blame| blame.line), Some(2));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_queued_blame_starts_behind_a_mutation_but_waits_for_a_running_blame() {
        let (root, mut app) = committed_change("blame-behind-mutation");
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        let first_row = app.row_for_line(ReviewSide::After, 1).unwrap();
        let second_row = app.row_for_line(ReviewSide::After, 2).unwrap();
        let operation_in_flight = |app: &App| {
            matches!(
                app.foreground.action.as_ref().map(|action| &action.kind),
                Some(ForegroundKind::Operation { .. })
            )
        };
        app.foreground.action = Some(ForegroundAction {
            id: RequestId::new(1),
            kind: ForegroundKind::Operation {
                path: root.clone(),
                command: CommandId::Push,
                operation: GitOperation::Push {
                    force: false,
                    remote: "origin".into(),
                    branch: "main".into(),
                },
            },
            started: Instant::now(),
        });

        app.diff.focused_diff_row = first_row;
        app.load_blame_for_row(first_row);
        let ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } = request_rx
            .try_recv()
            .expect("a blame starts while a mutation runs")
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 1);
        assert!(app.diff.pending_blame.is_none());
        assert!(app.diff.blame_read.is_some());
        assert!(operation_in_flight(&app), "the mutation keeps its slot");

        app.diff.focused_diff_row = second_row;
        app.load_blame_for_row(second_row);
        assert!(
            request_rx.try_recv().is_err(),
            "a queued blame waits for the running blame"
        );
        assert!(app.diff.pending_blame.is_some());

        result_tx
            .send(ForegroundResult::Blame {
                id,
                generation,
                path,
                file,
                line,
                revision,
                result: Err(ReadError::Cancelled),
            })
            .unwrap();
        app.receive_foreground_results();
        let ForegroundRequest::Blame { line, .. } = request_rx
            .try_recv()
            .expect("the queued blame starts once the running one ends")
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 2);
        assert!(app.diff.pending_blame.is_none());
        assert!(operation_in_flight(&app));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn returning_to_a_line_whose_superseded_blame_still_runs_reads_it_again() {
        let root = temp_repo("blame-return");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "one changed\ntwo changed\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let first_row = app.row_for_line(ReviewSide::After, 1).unwrap();
        let second_row = app.row_for_line(ReviewSide::After, 2).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        let blame_for = |line: usize| BlameInfo {
            sha: "1111111111111111111111111111111111111111".to_owned(),
            author: "First".to_owned(),
            author_email: "first@example.com".to_owned(),
            author_time: 0,
            summary: "First line".to_owned(),
            line,
        };

        app.diff.focused_diff_row = first_row;
        app.load_blame_for_row(first_row);
        let ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } = request_rx.try_recv().expect("first Blame request")
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 1);

        app.diff.focused_diff_row = second_row;
        app.load_blame_for_row(second_row);
        app.diff.focused_diff_row = first_row;
        app.load_blame_for_row(first_row);
        assert!(
            request_rx.try_recv().is_err(),
            "the first read is still running"
        );

        result_tx
            .send(ForegroundResult::Blame {
                id,
                generation,
                path,
                file,
                line,
                revision,
                result: Ok(blame_for(1)),
            })
            .unwrap();
        app.receive_foreground_results();
        assert!(app.diff.blame.is_none(), "a superseded read never applies");

        let ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } = request_rx
            .try_recv()
            .expect("the focused line is read again")
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 1);
        result_tx
            .send(ForegroundResult::Blame {
                id,
                generation,
                path,
                file,
                line,
                revision,
                result: Ok(blame_for(1)),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(app.diff.blame.as_ref().map(|blame| blame.line), Some(1));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changes_does_not_render_a_persistent_blame_pane() {
        let mut app = offline_app();
        app.diff.blame = Some(BlameInfo {
            sha: "bdbe50e712345678".to_owned(),
            author: "Jun Lee".to_owned(),
            author_email: "jun@example.com".to_owned(),
            author_time: 1_768_463_440,
            line: 42,
            summary: "Keep this outside the layout".to_owned(),
        });
        let buffer = render(&mut app, 120, 30);
        let screen = buffer
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!screen.contains("Blame"));
        assert!(!screen.contains("Keep this outside the layout"));
    }

    #[test]
    fn blame_card_keeps_only_line_sha_author_and_summary() {
        let committed = BlameInfo {
            sha: "bdbe50e712345678".to_owned(),
            author: "Jun Lee".to_owned(),
            author_email: "jun@example.com".to_owned(),
            author_time: 1_768_463_440,
            summary: "Fix refund sync (#1230)".to_owned(),
            line: 482,
        };
        assert_eq!(
            blame_text(&committed),
            "Line 482 · bdbe50e7 · Jun Lee · Fix refund sync (#1230)"
        );

        let uncommitted = BlameInfo {
            sha: "0000000000000000000000000000000000000000".to_owned(),
            author: "Not Committed Yet".to_owned(),
            author_email: "not.committed.yet".to_owned(),
            author_time: 1_787_801_063,
            summary: "Version of src/lib.rs from src/lib.rs".to_owned(),
            line: 1_881,
        };
        assert_eq!(
            blame_text(&uncommitted),
            "Line 1881 · Uncommitted · Not Committed Yet · Working tree change"
        );
    }

    #[test]
    fn changes_dividers_drag_with_limits() {
        let root = temp_repo("resize-panes");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        render(&mut app, 200, 30);
        let divider_center = |area: Rect| area.x.saturating_add(1);
        let default_files = divider_center(app.diff.files_before_divider_area);
        let default_after = divider_center(app.diff.before_after_divider_area);
        assert_eq!((default_files, default_after), (40, 120));
        let pointer_row = app.diff.changes_body_area.y.saturating_add(5);
        let mouse = |kind, column| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row: pointer_row,
                modifiers: KeyModifiers::NONE,
            })
        };

        app.handle(mouse(
            MouseEventKind::Down(MouseButton::Left),
            default_files,
        ))
        .unwrap();
        assert!(app.diff.resize_drag.is_some());
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 70))
            .unwrap();
        app.handle(mouse(MouseEventKind::Up(MouseButton::Left), 70))
            .unwrap();
        assert!(app.diff.resize_drag.is_none());
        render(&mut app, 200, 30);
        assert_eq!(divider_center(app.diff.files_before_divider_area), 70);
        assert_eq!(
            divider_center(app.diff.before_after_divider_area),
            default_after
        );

        app.handle(mouse(
            MouseEventKind::Down(MouseButton::Left),
            default_after,
        ))
        .unwrap();
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 155))
            .unwrap();
        app.handle(mouse(MouseEventKind::Up(MouseButton::Left), 155))
            .unwrap();
        render(&mut app, 200, 30);
        assert_eq!(divider_center(app.diff.before_after_divider_area), 155);

        let first = divider_center(app.diff.files_before_divider_area);
        app.handle(mouse(MouseEventKind::Down(MouseButton::Left), first))
            .unwrap();
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 0))
            .unwrap();
        app.handle(mouse(MouseEventKind::Up(MouseButton::Left), 0))
            .unwrap();
        render(&mut app, 200, 30);
        assert!(divider_center(app.diff.files_before_divider_area) >= 20);

        let second = divider_center(app.diff.before_after_divider_area);
        app.handle(mouse(MouseEventKind::Down(MouseButton::Left), second))
            .unwrap();
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 199))
            .unwrap();
        app.handle(mouse(MouseEventKind::Up(MouseButton::Left), 199))
            .unwrap();
        render(&mut app, 200, 30);
        assert!(
            app.diff
                .changes_body_area
                .right()
                .saturating_sub(divider_center(app.diff.before_after_divider_area))
                >= 24
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diff_wheel_scroll_is_one_row_and_clamped_to_content() {
        let root = temp_repo("scroll-clamp");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let mut after = (1..=20)
            .map(|line| Line::raw(format!("line {line}")))
            .collect::<Vec<_>>();
        after[0] = Line::raw("x".repeat(80));
        app.diff.diff_document = test_diff_document(Vec::new(), after);
        app.diff.before_diff_area = Rect::new(40, 0, 40, 6);
        app.diff.after_diff_area = Rect::new(0, 0, 40, 6);
        app.diff.diff_area = app.diff.after_diff_area;
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE,
            })
        };

        app.handle(wheel(MouseEventKind::ScrollDown)).unwrap();
        assert_eq!(app.diff.diff_scroll, 1);
        for _ in 0..1_000 {
            app.handle(wheel(MouseEventKind::ScrollDown)).unwrap();
        }
        assert_eq!(app.diff.diff_scroll, 17);
        for _ in 0..1_000 {
            app.handle(wheel(MouseEventKind::ScrollUp)).unwrap();
        }
        assert_eq!(app.diff.diff_scroll, 0);

        app.handle(wheel(MouseEventKind::ScrollRight)).unwrap();
        assert_eq!(app.diff.diff_horizontal_scroll, 1);
        for _ in 0..1_000 {
            app.handle(wheel(MouseEventKind::ScrollRight)).unwrap();
        }
        assert_eq!(app.diff.diff_horizontal_scroll, 43);
        for _ in 0..1_000 {
            app.handle(wheel(MouseEventKind::ScrollLeft)).unwrap();
        }
        assert_eq!(app.diff.diff_horizontal_scroll, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overflowing_diff_renders_a_horizontal_scrollbar_on_each_panel() {
        let root = temp_repo("horizontal-bar");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.diff.before_diff_area = Rect::new(0, 0, 30, 8);
        app.diff.after_diff_area = Rect::new(30, 0, 30, 8);
        app.diff.diff_document = test_diff_document(
            vec![Line::raw("x".repeat(80))],
            vec![Line::raw("y".repeat(80))],
        );
        app.diff.diff_horizontal_scroll = 10;

        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| app.draw_diff_horizontal_scrollbars(frame))
            .unwrap();
        let bottom = 6;
        let buffer = terminal.backend().buffer();
        assert!((1..29).any(|column| buffer[(column, bottom)].symbol() == "─"));
        assert!((31..59).any(|column| buffer[(column, bottom)].symbol() == "─"));
        assert!((1..29).any(|column| buffer[(column, bottom)].symbol() == "▄"));
        assert!((31..59).any(|column| buffer[(column, bottom)].symbol() == "▄"));
        let thumb = (1..29)
            .find(|&column| buffer[(column, bottom)].symbol() == "▄")
            .unwrap();
        assert_eq!(buffer[(thumb, bottom)].fg, theme::ACCENT);
        assert!(!buffer[(thumb, bottom)].modifier.contains(Modifier::BOLD));
        let track = (1..29)
            .find(|&column| buffer[(column, bottom)].symbol() == "─")
            .unwrap();
        assert_eq!(buffer[(track, bottom)].fg, theme::MUTED);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diff_pointer_rejects_borders_and_empty_space_after_the_last_row() {
        let root = temp_repo("diff-hit-test");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.diff.before_diff_area = Rect::new(0, 2, 30, 10);
        app.diff.after_diff_area = Rect::new(30, 2, 30, 10);
        app.diff.diff_document = test_diff_document(
            vec![Line::raw("before 1"), Line::raw("before 2")],
            vec![Line::raw("after 1"), Line::raw("after 2")],
        );

        assert_eq!(app.diff_position(31, 3), Some((ReviewSide::After, 0)));
        assert_eq!(app.diff_position(31, 4), Some((ReviewSide::After, 1)));
        assert_eq!(app.diff_position(31, 2), None);
        assert_eq!(app.diff_position(31, 5), None);
        assert_eq!(app.diff_position(31, 11), None);

        app.focus = PaneFocus::Diff;
        app.diff.focused_diff_row = 1;
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 31,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert_eq!(app.focus, PaneFocus::Files);
        assert_eq!(app.diff.focused_diff_row, 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn horizontal_diff_scrollbar_clicks_and_drags_to_bounded_positions() {
        let root = temp_repo("scrollbar-drag");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.diff.before_diff_area = Rect::new(0, 0, 40, 8);
        app.diff.after_diff_area = Rect::new(40, 0, 40, 8);
        app.diff.diff_document = test_diff_document(
            vec![Line::raw("x".repeat(80))],
            vec![Line::raw("y".repeat(80))],
        );

        let pointer = |kind, column| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row: 6,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle(pointer(MouseEventKind::Down(MouseButton::Left), 20))
            .unwrap();
        assert!(app.scrollbar_drag.is_some());
        assert!((20..=23).contains(&app.diff.diff_horizontal_scroll));

        app.handle(pointer(MouseEventKind::Drag(MouseButton::Left), 100))
            .unwrap();
        assert_eq!(app.diff.diff_horizontal_scroll, 42);
        app.handle(pointer(MouseEventKind::Up(MouseButton::Left), 100))
            .unwrap();
        assert!(app.scrollbar_drag.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vertical_diff_scrollbar_clicks_and_drags_to_bounded_positions() {
        let root = temp_repo("vertical-drag");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.diff.before_diff_area = Rect::new(0, 0, 40, 10);
        app.diff.after_diff_area = Rect::new(40, 0, 40, 10);
        app.diff.diff_area = app.diff.after_diff_area;
        app.diff.diff_document = test_diff_document(
            (0..30)
                .map(|row| Line::raw(format!("before {row}")))
                .collect(),
            (0..30)
                .map(|row| Line::raw(format!("after {row}")))
                .collect(),
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| app.draw_diff_vertical_scrollbars(frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!((1..9).any(|row| buffer[(38, row)].symbol() == "█"));
        assert!((1..9).any(|row| buffer[(78, row)].symbol() == "█"));

        let pointer = |kind, row| {
            Event::Mouse(MouseEvent {
                kind,
                column: 78,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle(pointer(MouseEventKind::Down(MouseButton::Left), 5))
            .unwrap();
        assert!(app.scrollbar_drag.is_some());
        assert!((11..=13).contains(&app.diff.diff_scroll));

        app.handle(pointer(MouseEventKind::Drag(MouseButton::Left), 100))
            .unwrap();
        assert_eq!(app.diff.diff_scroll, 22);
        app.handle(pointer(MouseEventKind::Up(MouseButton::Left), 100))
            .unwrap();
        assert!(app.scrollbar_drag.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diff_scrollbars_reserve_gutters_outside_the_source_viewport() {
        let root = temp_repo("scrollbar-gutters");

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.diff.before_diff_area = Rect::new(0, 0, 40, 10);
        app.diff.after_diff_area = Rect::new(40, 0, 40, 10);
        app.diff.diff_area = app.diff.after_diff_area;
        app.diff.diff_document = test_diff_document(
            (0..30).map(|_| Line::raw("x".repeat(80))).collect(),
            (0..30).map(|_| Line::raw("y".repeat(80))).collect(),
        );

        assert_eq!(
            app.diff_source_area(app.diff.after_diff_area),
            Rect::new(41, 1, 37, 7)
        );
        assert_eq!(
            app.diff_vertical_scrollbar_area(app.diff.after_diff_area),
            Some(Rect::new(78, 1, 1, 7))
        );
        assert_eq!(
            app.diff_horizontal_scrollbar_area(app.diff.after_diff_area),
            Some(Rect::new(41, 8, 37, 1))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn horizontal_scroll_keeps_changed_background_across_the_viewport() {
        let viewport = 20;
        let offset = 10;
        let lines = vec![Line::styled(
            "short",
            Style::default().bg(theme::DIFF_ADDED_BG),
        )];
        let lines = padded_diff_lines(&lines, viewport, offset);
        let mut terminal = Terminal::new(TestBackend::new(viewport as u16, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(lines).scroll((0, offset as u16)),
                    frame.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!((0..viewport as u16).all(|column| buffer[(column, 0)].bg == theme::DIFF_ADDED_BG));
    }

    #[test]
    fn line_jump_expands_a_fold_and_focuses_the_exact_after_line() {
        let root = temp_repo("ui-line-jump");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        let before = (1..=100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(root.join("tracked.txt"), &before).unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(
            root.join("tracked.txt"),
            before.replace("line 80", "line 80 changed"),
        )
        .unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let file_row = app
            .files
            .tree_rows
            .iter()
            .position(|row| matches!(row.kind, crate::ui::files::TreeRowKind::File { .. }))
            .unwrap();
        app.select_tree(file_row);
        app.activate_tree_row();
        assert!(app.row_for_line(ReviewSide::After, 50).is_none());

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(!matches!(app.overlay, Overlay::LineJump(_)));

        for key in [
            KeyCode::Char('g'),
            KeyCode::Char('5'),
            KeyCode::Char('0'),
            KeyCode::Enter,
        ] {
            app.handle(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)))
                .unwrap();
        }

        assert_eq!(app.diff.pending_line_jump, Some((ReviewSide::After, 50)));
        assert!(!app.diff.focus_after_changes_refresh);
        wait_for_diff(&mut app);
        assert!(!matches!(app.overlay, Overlay::LineJump(_)));
        assert_eq!(
            app.diff
                .diff_document
                .after_line_number(app.diff.focused_diff_row),
            Some(50)
        );

        for key in [
            KeyCode::Char('g'),
            KeyCode::Char('8'),
            KeyCode::Char('0'),
            KeyCode::Char(' '),
        ] {
            app.handle(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)))
                .unwrap();
        }
        let Overlay::LineJump(jump) = &app.overlay else {
            panic!("Space is not a submit key in the line jump");
        };
        assert_eq!(jump.input.text, "80");
        press(&mut app, KeyCode::Enter);
        wait_for_diff(&mut app);
        assert!(!matches!(app.overlay, Overlay::LineJump(_)));
        assert_eq!(
            app.diff
                .diff_document
                .after_line_number(app.diff.focused_diff_row),
            Some(80)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fold_toggle_refolds_the_loaded_diff_without_a_repository_refresh() {
        let (root, mut app) = crate::ui::test_support::committed_change("refold");
        let (request_rx, _result_tx) = crate::ui::test_support::intercept_foreground(&mut app);
        app.refresh.pending = false;
        let row = app
            .diff
            .diff_document
            .folds()
            .map(|(row, _)| row)
            .next()
            .expect("fold row");
        assert!(app.toggle_fold(row));
        assert!(!app.refresh.pending);
        assert!(matches!(
            request_rx.try_recv(),
            Ok(ForegroundRequest::Refold { text, .. }) if text == app.diff.diff_text
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fold_controls_remain_available_after_each_toggle() {
        let root = temp_repo("ui-fold-test");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        let base = (1..=25)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(root.join("tracked.txt"), &base).unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(
            root.join("tracked.txt"),
            base.replace("line 13", "line 13 changed"),
        )
        .unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let initial_rows = app.diff.diff_document.len();

        let context_row = app
            .diff
            .diff_document
            .folds()
            .find_map(|(row, key)| (key.kind == FoldKind::Context).then_some(row))
            .unwrap();
        let context_key = app.diff.diff_document.fold_key(context_row).unwrap();
        app.focus = PaneFocus::Diff;
        app.diff.focused_diff_row = context_row;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_diff(&mut app);
        assert!(app.diff.diff_document.len() > initial_rows);
        let context_row = app
            .diff
            .diff_document
            .folds()
            .find_map(|(row, key)| (key == context_key).then_some(row))
            .unwrap();
        app.diff.focused_diff_row = context_row;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        wait_for_diff(&mut app);
        assert_eq!(app.diff.diff_document.len(), initial_rows);

        let change_row = app
            .diff
            .diff_document
            .folds()
            .find_map(|(row, key)| (key.kind == FoldKind::Change).then_some(row))
            .unwrap();
        let change_key = app.diff.diff_document.fold_key(change_row).unwrap();
        assert!(app.toggle_fold(change_row));
        wait_for_diff(&mut app);
        assert!(app.diff.diff_document.len() < initial_rows);
        let change_row = app
            .diff
            .diff_document
            .folds()
            .find_map(|(row, key)| (key == change_key).then_some(row))
            .unwrap();
        assert!(app.toggle_fold(change_row));
        wait_for_diff(&mut app);
        assert_eq!(app.diff.diff_document.len(), initial_rows);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn emphasize_diff_line_marks_code_gutters_and_fills_rows_without_line_numbers() {
        let tint = Style::default().bg(theme::DIFF_ADDED_BG);
        let mut lines = [
            Line::from(vec![
                Span::styled("   1 ", tint.fg(theme::MUTED)),
                Span::styled("code", tint),
            ])
            .style(tint),
            Line::styled("@@ -1 +1 @@", Style::default().fg(theme::WARNING)),
        ];
        emphasize_diff_line(&mut lines[0], true, theme::focus_row(), 20);
        emphasize_diff_line(&mut lines[1], false, theme::selection_row(), 20);

        assert_eq!(lines[0].spans[0].style.bg, Some(theme::SURFACE_FOCUS));
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::TEXT));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[1].style.bg, Some(theme::DIFF_ADDED_BG));
        assert_eq!(lines[0].style.bg, Some(theme::DIFF_ADDED_BG));
        assert_eq!(lines[1].style.bg, Some(theme::SURFACE_SELECTION));
        assert_eq!(lines[1].width(), 20);
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.style.bg == Some(theme::SURFACE_SELECTION))
        );
        assert_eq!(lines[1].style.fg, Some(theme::WARNING));
    }

    #[test]
    fn diff_panes_mark_the_focused_side_and_highlight_only_the_gutter() {
        let (root, mut app) = committed_change("diff-focus-style");
        render(&mut app, 120, 30);
        let changed = (0..app.diff.diff_document.len())
            .find(|&row| {
                app.diff.diff_document.after_line(row).unwrap().style.bg
                    == Some(theme::DIFF_ADDED_BG)
            })
            .expect("changed After row");
        let metadata = (0..app.diff.diff_document.len())
            .find(|&row| app.diff.diff_document.after_line_number(row).is_none())
            .expect("metadata row");
        app.focus = PaneFocus::Diff;
        app.diff.diff_side = ReviewSide::After;
        app.diff.focused_diff_row = changed;
        let buffer = render(&mut app, 120, 30);
        let after = app.diff.after_diff_area;
        let before = app.diff.before_diff_area;
        assert_eq!(buffer[(after.x, after.y)].fg, theme::ACCENT);
        assert_eq!(buffer[(before.x, before.y)].fg, Color::Reset);
        let source = app.diff_source_area(after);
        let y = source.y + changed as u16;
        assert_eq!(buffer[(source.x, y)].bg, theme::SURFACE_FOCUS);
        assert!(buffer[(source.x, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(source.x + 5, y)].bg, theme::DIFF_ADDED_BG);
        assert!(!buffer[(source.x + 5, y)].modifier.contains(Modifier::BOLD));

        app.diff.focused_diff_row = metadata;
        let buffer = render(&mut app, 120, 30);
        let y = source.y + metadata as u16;
        assert_eq!(buffer[(source.x, y)].bg, theme::SURFACE_FOCUS);
        assert_eq!(buffer[(source.right() - 1, y)].bg, theme::SURFACE_FOCUS);

        app.focus = PaneFocus::Files;
        app.review.changes.code_selection =
            Some(CodeSelection::from_rows(ReviewSide::After, [changed]));
        let buffer = render(&mut app, 120, 30);
        assert_eq!(buffer[(after.x, after.y)].fg, Color::Reset);
        let y = source.y + changed as u16;
        assert_eq!(buffer[(source.x, y)].bg, theme::SURFACE_SELECTION);
        assert!(buffer[(source.x, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(source.x + 5, y)].bg, theme::DIFF_ADDED_BG);
        let path = &app.files.changes[app.files.change_selected].path;
        let name = path.rsplit('/').next().unwrap();
        let line = app.diff.diff_document.after_line_number(changed).unwrap();
        assert!(row_text(&buffer, after, after.y).contains(&format!("After · {name}:{line}")));
        assert!(buffer_text(&buffer).contains("y Yank · h History · Esc Cancel"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn line_jump_uses_the_narrow_dialog_with_a_status_row_and_clickable_buttons() {
        let (root, mut app) = committed_change("line-jump-dialog");
        app.focus = PaneFocus::Diff;
        press(&mut app, KeyCode::Char('g'));
        assert!(matches!(app.overlay, Overlay::LineJump(_)));
        let buffer = render(&mut app, 100, 32);
        let text = buffer_text(&buffer);
        assert!(text.contains("Go to After line"));
        assert!(text.contains("Enter a positive line number"));
        assert!(text.contains("[Enter] Go"));
        assert!(text.contains("[Esc] Cancel"));
        let Overlay::LineJump(jump) = &app.overlay else {
            unreachable!()
        };
        let buttons = jump.buttons;
        assert_eq!(
            buttons.primary.width + buttons.secondary.width,
            theme::DIALOG_NARROW - 2
        );

        press(&mut app, KeyCode::Enter);
        let buffer = render(&mut app, 100, 32);
        let symbols = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>();
        let error = symbols
            .windows(6)
            .position(|window| window.concat() == "Error:")
            .expect("empty input shows an error in the status row");
        assert_eq!(buffer.content()[error].fg, theme::ERROR);
        assert!(buffer_text(&buffer).contains("Error: Enter a positive line number."));
        click(&mut app, buttons.secondary.x + 1, buttons.secondary.y + 1);
        assert!(matches!(app.overlay, Overlay::None));

        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Char(' '));
        assert!(matches!(app.overlay, Overlay::LineJump(_)));
        render(&mut app, 100, 32);
        let Overlay::LineJump(jump) = &app.overlay else {
            unreachable!()
        };
        let buttons = jump.buttons;
        click(&mut app, buttons.primary.x + 1, buttons.primary.y + 1);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(
            app.diff
                .diff_document
                .after_line_number(app.diff.focused_diff_row),
            Some(3)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blame_line_shows_a_spinner_while_pending_and_the_blame_text_with_diff_focus() {
        let (root, mut app) = committed_change("blame-line");
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        let row = app.row_for_line(ReviewSide::After, 2).unwrap();
        app.focus = PaneFocus::Diff;
        app.diff.focused_diff_row = row;
        app.load_blame_for_row(row);
        let ForegroundRequest::Blame {
            id,
            generation,
            path,
            file,
            line,
            revision,
        } = request_rx.try_recv().expect("Blame request")
        else {
            panic!("unexpected request")
        };

        let buffer = render(&mut app, 160, 30);
        let after = app.diff.after_diff_area;
        let bottom = row_text(&buffer, after, after.bottom() - 1);
        assert!(bottom.contains("Blame"), "{bottom}");
        assert!(
            theme::SPINNER_FRAMES
                .iter()
                .any(|frame| bottom.contains(frame)),
            "{bottom}"
        );
        assert!(!bottom.contains("Line 2"));

        result_tx
            .send(ForegroundResult::Blame {
                id,
                generation,
                path,
                file,
                line,
                revision,
                result: Ok(BlameInfo {
                    sha: "abcdef1234567890abcdef1234567890abcdef12".to_owned(),
                    author: "Jun".to_owned(),
                    author_email: "jun@example.com".to_owned(),
                    author_time: 0,
                    summary: "Fix two".to_owned(),
                    line: 2,
                }),
            })
            .unwrap();
        app.receive_foreground_results();
        let buffer = render(&mut app, 160, 30);
        let bottom = row_text(&buffer, after, after.bottom() - 1);
        assert!(
            bottom.contains("Blame: Line 2 · abcdef12 · Jun · Fix two"),
            "{bottom}"
        );

        app.focus = PaneFocus::Files;
        assert!(!buffer_text(&render(&mut app, 160, 30)).contains("Blame"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diff_home_and_end_jump_to_the_first_and_last_row() {
        let root = temp_repo("diff-home-end");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.diff.diff_document = test_diff_document(
            (0..30)
                .map(|row| Line::raw(format!("before {row}")))
                .collect(),
            (0..30)
                .map(|row| Line::raw(format!("after {row}")))
                .collect(),
        );
        app.diff.before_diff_area = Rect::new(0, 0, 40, 10);
        app.diff.after_diff_area = Rect::new(40, 0, 40, 10);
        app.diff.diff_area = app.diff.after_diff_area;
        app.focus = PaneFocus::Diff;

        press(&mut app, KeyCode::End);
        assert_eq!(app.diff.focused_diff_row, 29);
        assert!(app.diff.diff_scroll > 0);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.diff.focused_diff_row, 0);
        assert_eq!(app.diff.diff_scroll, 0);
        let page = app.diff_source_area(app.diff.diff_area).height;
        assert_eq!(page, 8, "ten pane rows minus the border pair");
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.diff.diff_scroll, page);
        assert_eq!(app.diff.focused_diff_row, 0);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.diff.diff_scroll, 2 * page);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.diff.diff_scroll as usize, app.max_diff_scroll());
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.diff.diff_scroll as usize, app.max_diff_scroll() - 8);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hover_blame_request_leaves_the_pending_file_diff_to_apply() {
        let root = temp_repo("diff-survives-blame");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("a.txt"), "before a\n").unwrap();
        fs::write(root.join("b.txt"), "before b\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("a.txt"), "after a\n").unwrap();
        fs::write(root.join("b.txt"), "after b\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        let b_row = app
            .files
            .tree_rows
            .iter()
            .position(|row| row.label == "b.txt")
            .expect("b.txt row");

        app.select_tree(b_row);
        let ForegroundRequest::Diff {
            id,
            generation,
            owner,
            path,
            file,
            target,
            fold_toggles,
        } = request_rx.try_recv().expect("file diff request")
        else {
            panic!("unexpected request")
        };
        assert_eq!(file, "b.txt");
        let row = (0..app.diff.diff_document.len())
            .find(|&row| app.diff.diff_document.after_line_number(row).is_some())
            .expect("diff line");
        app.diff.focused_diff_row = row;
        app.load_blame_for_row(row);
        assert!(matches!(
            request_rx.try_recv(),
            Ok(ForegroundRequest::Blame { .. })
        ));
        assert!(app.diff.pending_diff.is_some());

        result_tx
            .send(ForegroundResult::Diff {
                id,
                generation,
                owner,
                path,
                file,
                target,
                fold_toggles,
                result: Ok((
                    "diff for b".to_owned(),
                    HighlightedDiff {
                        language: "Plain text".to_owned(),
                        split: DiffDocument::plain("diff for b"),
                        key: 0,
                    },
                )),
            })
            .unwrap();
        app.receive_foreground_results();

        assert!(app.diff.pending_diff.is_none());
        assert_eq!(app.diff.diff_text, "diff for b");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hover_blame_waits_for_the_pointer_to_dwell_then_reads_the_last_row_once() {
        let (root, mut app) = committed_change("hover-blame-dwell");
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        let (refresh_tx, _refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        app.refresh.last_check = Instant::now();
        render(&mut app, 160, 30);
        let source = app.diff_source_area(app.diff.after_diff_area);
        let first_row = app.row_for_line(ReviewSide::After, 1).unwrap();
        let second_row = app.row_for_line(ReviewSide::After, 2).unwrap();
        let moved = |row: usize| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: source.x + 1,
                row: source.y + row as u16,
                modifiers: KeyModifiers::NONE,
            })
        };

        app.handle(moved(first_row)).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        app.handle(moved(second_row)).unwrap();
        assert_eq!(app.focus, PaneFocus::Diff);
        assert_eq!(app.diff.focused_diff_row, second_row);
        assert!(
            request_rx.try_recv().is_err(),
            "pointer motion alone reads no blame"
        );
        assert!(!app.maybe_auto_refresh());
        assert!(request_rx.try_recv().is_err(), "the dwell has not elapsed");

        app.diff.hover_blame.as_mut().unwrap().since = Instant::now() - HOVER_BLAME_DWELL;
        assert!(app.maybe_auto_refresh());
        let ForegroundRequest::Blame { line, .. } = request_rx
            .try_recv()
            .expect("one Blame read after the dwell")
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 2);
        assert!(request_rx.try_recv().is_err());
        assert!(app.diff.hover_blame.is_none());

        app.diff.blame_read = None;
        app.diff.focused_diff_row = first_row;
        press(&mut app, KeyCode::Char('b'));
        let ForegroundRequest::Blame { line, .. } =
            request_rx.try_recv().expect("b reads the blame at once")
        else {
            panic!("unexpected request")
        };
        assert_eq!(line, 1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_twenty_thousand_row_diff_draws_in_under_a_second() {
        let mut app = offline_app();
        let rows = 20_000;
        let tint = Style::default().bg(theme::DIFF_ADDED_BG);
        app.diff.diff_document = test_diff_document_with_after_metadata(
            (1..=rows)
                .map(|n| Line::raw(format!("before {n}")))
                .collect(),
            (1..=rows)
                .map(|n| Line::styled(format!("after {n}"), tint))
                .collect(),
            (1..=rows).map(Some).collect(),
            Vec::new(),
        );
        app.focus = PaneFocus::Diff;
        app.diff.focused_diff_row = 3;

        let started = Instant::now();
        let buffer = render(&mut app, 120, 30);
        assert!(started.elapsed() < Duration::from_secs(1));
        let after = app.diff.after_diff_area;
        assert!(row_text(&buffer, after, after.y + 1).contains("after 1"));
        assert!(app.max_diff_scroll() > 0);
    }
}
