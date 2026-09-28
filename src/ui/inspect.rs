use std::collections::{HashSet, VecDeque};
use std::mem;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, ScrollbarOrientation, ScrollbarState};

use crate::git::{
    ChangeSection, ChangedPath, Commit, CommitDetails, DiffTarget, ReadError, WorkingChange,
};
use crate::ui::review::ReviewSide;

use super::diff::{emphasize_diff_line, padded_diff_lines};
use super::effect::{
    CommitDetailsReadTarget, DiffOwner, DiffReadTarget, ForegroundRequest, HighlightedDiff,
    PendingCommitDetails, PendingDiff, ReadGeneration, RequestId,
};
use super::files::{status_label, status_style};
use super::lanes::ForegroundKind;
use super::review::SelectionSurface;
use super::shell::{ActiveTab, PaneFocus};
use super::syntax::DiffDocument;
use super::widgets::{self, Axis, ScrollbarDrag, area_hovered, counted_title, truncate_to_width};
use super::{App, ScrollbarOwner, theme};

const PREVIEW_TITLE_RESERVE: u16 = 6;
const DETAILS_CACHE_CAP: usize = 64;
const PREVIEW_CACHE_CAP: usize = 8;

#[derive(Debug, Default)]
pub(super) struct DetailsCache {
    entries: VecDeque<(String, Vec<ChangedPath>)>,
}

impl DetailsCache {
    pub(super) fn remember(&mut self, details: &CommitDetails) {
        self.entries.retain(|(sha, _)| *sha != details.commit.sha);
        self.entries
            .push_back((details.commit.sha.clone(), details.changes.clone()));
        while self.entries.len() > DETAILS_CACHE_CAP {
            self.entries.pop_front();
        }
    }

    pub(super) fn details_for(&self, commit: &Commit) -> Option<CommitDetails> {
        self.entries
            .iter()
            .find(|(sha, _)| *sha == commit.sha)
            .map(|(_, changes)| CommitDetails {
                commit: commit.clone(),
                changes: changes.clone(),
            })
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug)]
struct PreviewEntry {
    target: DiffReadTarget,
    language: String,
    document: DiffDocument,
}

#[derive(Debug, Default)]
pub(super) struct PreviewCache {
    entries: VecDeque<PreviewEntry>,
}

impl PreviewCache {
    fn take(&mut self, target: &DiffReadTarget) -> Option<PreviewEntry> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.target == *target)?;
        self.entries.remove(index)
    }

    fn store(&mut self, entry: PreviewEntry) {
        self.entries
            .retain(|candidate| candidate.target != entry.target);
        self.entries.push_back(entry);
        while self.entries.len() > PREVIEW_CACHE_CAP {
            self.entries.pop_front();
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Default)]
struct DetailLines {
    details: Option<CommitDetails>,
    uncommitted: bool,
    lines: Vec<Line<'static>>,
    change_rows: Vec<usize>,
}

pub(super) struct InspectState {
    pub(super) details: Option<CommitDetails>,
    pub(super) commit_detail_area: Rect,
    pub(super) commit_detail_scroll: u16,
    pub(super) commit_detail_follow_selection: bool,
    pub(super) commit_details_error: Option<String>,
    pub(super) commit_change_selected: usize,
    pub(super) commit_change_row_areas: Vec<(usize, Rect)>,
    pub(super) commit_preview_path: Option<String>,
    pub(super) commit_preview_loaded_path: Option<String>,
    pub(super) commit_preview_document: DiffDocument,
    pub(super) commit_preview_language: String,
    pub(super) commit_preview_error: Option<String>,
    pub(super) commit_preview_scroll: u16,
    pub(super) commit_preview_horizontal_scroll: u16,
    pub(super) commit_preview_area: Rect,
    pub(super) commit_preview_before_area: Rect,
    pub(super) commit_preview_after_area: Rect,
    pub(super) commit_preview_close_area: Rect,
    pub(super) pending_commit_details: Option<PendingCommitDetails>,
    pub(super) commit_preview_loaded_target: Option<DiffReadTarget>,
    pub(super) details_cache: DetailsCache,
    pub(super) preview_cache: PreviewCache,
    detail_lines: DetailLines,
}

impl Default for InspectState {
    fn default() -> Self {
        Self {
            details: None,
            commit_detail_area: Rect::default(),
            commit_detail_scroll: 0,
            commit_detail_follow_selection: true,
            commit_details_error: None,
            commit_change_selected: 0,
            commit_change_row_areas: Vec::new(),
            commit_preview_path: None,
            commit_preview_loaded_path: None,
            commit_preview_document: DiffDocument::default(),
            commit_preview_language: String::new(),
            commit_preview_error: None,
            commit_preview_scroll: 0,
            commit_preview_horizontal_scroll: 0,
            commit_preview_area: Rect::default(),
            commit_preview_before_area: Rect::default(),
            commit_preview_after_area: Rect::default(),
            commit_preview_close_area: Rect::default(),
            pending_commit_details: None,
            commit_preview_loaded_target: None,
            details_cache: DetailsCache::default(),
            preview_cache: PreviewCache::default(),
            detail_lines: DetailLines::default(),
        }
    }
}

impl InspectState {
    pub(super) fn clear_areas(&mut self) {
        self.commit_detail_area = Rect::default();
        self.commit_change_row_areas.clear();
        self.clear_preview_areas();
    }

    pub(super) fn clear_caches(&mut self) {
        self.details_cache.clear();
        self.preview_cache.clear();
    }

    fn clear_preview_areas(&mut self) {
        self.commit_preview_area = Rect::default();
        self.commit_preview_before_area = Rect::default();
        self.commit_preview_after_area = Rect::default();
        self.commit_preview_close_area = Rect::default();
    }
}

impl App {
    #[cfg(test)]
    pub(super) fn load_details(&mut self) {
        self.shell.error = None;
        let Some(repository) = self.repository.as_ref() else {
            self.inspect.details = None;
            return;
        };
        self.inspect.details =
            self.graph
                .selected_commit()
                .and_then(|commit| match repository.details(commit) {
                    Ok(details) => Some(details),
                    Err(error) => {
                        self.shell.error = Some(error);
                        None
                    }
                });
        if let Some(details) = self.inspect.details.as_ref() {
            self.inspect.details_cache.remember(details);
        }
        self.inspect.commit_change_selected = self.inspect.commit_change_selected.min(
            self.inspect
                .details
                .as_ref()
                .map_or(0, |details| details.changes.len().saturating_sub(1)),
        );
    }

    pub(super) fn current_commit_details_target(&self) -> Option<CommitDetailsReadTarget> {
        let repository = self.repository.as_ref()?;
        let commit = self.graph.selected_commit()?.clone();
        Some(CommitDetailsReadTarget {
            path: repository.root().to_owned(),
            commit,
        })
    }

    fn commit_details_target_is_current(&self, target: &CommitDetailsReadTarget) -> bool {
        self.shell.active_tab == ActiveTab::History
            && self
                .repository
                .as_ref()
                .is_some_and(|repository| repository.root() == target.path)
            && self
                .graph
                .selected_commit()
                .is_some_and(|commit| commit.sha == target.commit.sha)
    }

    pub(super) fn selected_commit_details(&self) -> Option<&CommitDetails> {
        let selected_sha = self.graph.selected_commit()?.sha.as_str();
        self.inspect
            .details
            .as_ref()
            .filter(|details| details.commit.sha == selected_sha)
    }

    pub(super) fn request_commit_details(&mut self) {
        if matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        ) || self.workspaces.pending_switch.is_some()
        {
            return;
        }
        let Some(target) = self.current_commit_details_target() else {
            if self.inspect.pending_commit_details.is_some() {
                self.foreground.reads.details.advance();
            }
            self.inspect.pending_commit_details = None;
            self.inspect.details = None;
            self.inspect.commit_details_error = None;
            return;
        };
        if let Some(details) = self.inspect.details_cache.details_for(&target.commit) {
            self.foreground.reads.details.advance();
            self.inspect.pending_commit_details = None;
            self.show_commit_details(details);
            return;
        }
        self.foreground.reads.details.advance();
        self.inspect.commit_details_error = None;
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.details.generation;
        let request = ForegroundRequest::CommitDetails {
            id,
            generation,
            target: target.clone(),
        };
        match self.foreground.request(request) {
            Ok(_) => {
                self.inspect.pending_commit_details = Some(PendingCommitDetails {
                    id,
                    generation,
                    target,
                    started: Instant::now(),
                });
            }
            Err(error) => {
                self.inspect.pending_commit_details = None;
                self.inspect.commit_details_error =
                    Some(format!("Commit details worker stopped: {error}"));
            }
        }
    }

    pub(super) fn current_commit_preview_target(&self) -> Option<DiffReadTarget> {
        let repository = self.repository.as_ref()?;
        let file = self.inspect.commit_preview_path.clone()?;
        let target = if self.graph.uncommitted_selected() {
            let change = self
                .working_graph_change(self.inspect.commit_change_selected)
                .filter(|change| change.path == file)
                .or_else(|| {
                    self.files
                        .changes
                        .iter()
                        .filter(|change| change.section != ChangeSection::Commit)
                        .find(|change| change.path == file)
                })?;
            change.section.diff_target()?
        } else {
            let details = self.selected_commit_details()?;
            DiffTarget::CommitAgainstParent {
                commit: details.commit.sha.clone(),
                parent: details.commit.parents.first().cloned(),
            }
        };
        Some(DiffReadTarget {
            owner: DiffOwner::CommitPreview,
            path: repository.root().to_owned(),
            file,
            target,
            fold_toggles: HashSet::new(),
        })
    }

    fn working_graph_change(&self, index: usize) -> Option<&WorkingChange> {
        self.files
            .changes
            .iter()
            .filter(|change| change.section != ChangeSection::Commit)
            .nth(index)
    }

    fn graph_change_count(&self) -> Option<usize> {
        if self.graph.uncommitted_selected() {
            Some(
                self.files
                    .changes
                    .iter()
                    .filter(|change| change.section != ChangeSection::Commit)
                    .count(),
            )
        } else {
            self.selected_commit_details()
                .map(|details| details.changes.len())
        }
    }

    fn commit_change_path(&self, index: usize) -> Option<String> {
        if self.inspect.pending_commit_details.is_some() {
            return None;
        }
        if self.graph.uncommitted_selected() {
            let change = self.working_graph_change(index)?;
            change.section.diff_target()?;
            return Some(change.path.clone());
        }
        self.selected_commit_details()
            .and_then(|details| details.changes.get(index))
            .map(|change| change.path.clone())
    }

    fn open_commit_preview(&mut self, index: usize, path: String) {
        self.inspect.commit_change_selected = index;
        self.inspect.commit_detail_follow_selection = true;
        self.focus = PaneFocus::Details;
        self.inspect.commit_preview_path = Some(path);
        self.inspect.commit_preview_error = None;
        self.request_commit_preview();
    }

    fn toggle_commit_preview(&mut self, index: usize) {
        let Some(path) = self.commit_change_path(index) else {
            return;
        };
        if self.inspect.commit_preview_path.as_deref() == Some(path.as_str()) {
            self.inspect.commit_change_selected = index;
            self.inspect.commit_detail_follow_selection = true;
            self.focus = PaneFocus::Details;
            self.close_commit_preview();
            return;
        }
        self.open_commit_preview(index, path);
    }

    fn activate_commit_change(&mut self, index: usize) {
        let Some(path) = self.commit_change_path(index) else {
            return;
        };
        if self.inspect.commit_preview_path.as_deref() == Some(path.as_str()) {
            self.focus = PaneFocus::Preview;
            return;
        }
        self.open_commit_preview(index, path);
    }

    fn request_commit_preview(&mut self) {
        if matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        ) {
            return;
        }
        let Some(target) = self.current_commit_preview_target() else {
            return;
        };
        if let Some(entry) = self.inspect.preview_cache.take(&target) {
            self.foreground.reads.diff.advance();
            self.diff.pending_diff = None;
            self.show_commit_preview(entry);
            return;
        }
        self.foreground.reads.diff.advance();
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
                self.inspect.commit_preview_error =
                    Some(format!("Preview worker stopped: {error}"));
            }
        }
    }

    pub(super) fn apply_commit_preview(
        &mut self,
        target: DiffReadTarget,
        result: Result<(String, HighlightedDiff), ReadError>,
    ) {
        match result {
            Ok((_, highlighted)) => self.show_commit_preview(PreviewEntry {
                target,
                language: highlighted.language,
                document: highlighted.split,
            }),
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => self.inspect.commit_preview_error = Some(error),
        }
    }

    fn show_commit_preview(&mut self, entry: PreviewEntry) {
        self.stash_commit_preview();
        self.clear_preview_selection();
        self.inspect.commit_preview_loaded_path = Some(entry.target.file.clone());
        self.inspect.commit_preview_loaded_target = Some(entry.target);
        self.inspect.commit_preview_document = entry.document;
        self.inspect.commit_preview_language = entry.language;
        self.inspect.commit_preview_error = None;
        self.inspect.commit_preview_scroll = 0;
        self.inspect.commit_preview_horizontal_scroll = 0;
    }

    fn stash_commit_preview(&mut self) {
        let Some(target) = self.inspect.commit_preview_loaded_target.take() else {
            return;
        };
        self.inspect.preview_cache.store(PreviewEntry {
            target,
            language: mem::take(&mut self.inspect.commit_preview_language),
            document: mem::take(&mut self.inspect.commit_preview_document),
        });
    }

    pub(super) fn close_commit_preview(&mut self) {
        if self.pending_commit_preview().is_some() {
            self.foreground.reads.diff.advance();
            self.diff.pending_diff = None;
        }
        self.stash_commit_preview();
        self.clear_preview_selection();
        self.inspect.commit_preview_path = None;
        self.inspect.commit_preview_loaded_path = None;
        self.inspect.commit_preview_document = DiffDocument::default();
        self.inspect.commit_preview_language.clear();
        self.inspect.commit_preview_error = None;
        self.inspect.commit_preview_scroll = 0;
        self.inspect.commit_preview_horizontal_scroll = 0;
        self.end_scrollbar_drag(ScrollbarOwner::CommitPreview);
        self.inspect.clear_preview_areas();
        if self.focus == PaneFocus::Preview {
            self.focus = PaneFocus::Details;
        }
    }

    pub(super) fn reset_commit_inspection(&mut self) {
        if self.inspect.pending_commit_details.is_some() {
            self.foreground.reads.details.advance();
            self.inspect.pending_commit_details = None;
        }
        self.close_commit_preview();
        if matches!(self.focus, PaneFocus::Details | PaneFocus::Preview) {
            self.focus = PaneFocus::Commits;
        }
        self.inspect.commit_detail_scroll = 0;
        self.inspect.commit_detail_follow_selection = true;
        self.inspect.commit_details_error = None;
        self.inspect.commit_change_selected = 0;
        self.inspect.commit_change_row_areas.clear();
    }

    pub(super) fn move_commit_change_selection(&mut self, delta: isize) {
        self.select_commit_change(
            self.inspect
                .commit_change_selected
                .saturating_add_signed(delta),
        );
    }

    fn select_commit_change(&mut self, index: usize) {
        if self.inspect.pending_commit_details.is_some() {
            return;
        }
        let Some(change_count) = self.graph_change_count().filter(|count| *count > 0) else {
            return;
        };
        self.inspect.commit_change_selected = index.min(change_count - 1);
        self.inspect.commit_detail_follow_selection = true;
    }

    pub(super) fn focus_commit_changes(&mut self) {
        if self.inspect.pending_commit_details.is_some() {
            return;
        }
        let Some(change_count) = self.graph_change_count() else {
            return;
        };
        if change_count == 0 {
            return;
        }
        self.focus = PaneFocus::Details;
        self.inspect.commit_change_selected =
            self.inspect.commit_change_selected.min(change_count - 1);
        self.inspect.commit_detail_follow_selection = true;
    }

    fn commit_preview_scrollbar_visibility(&self) -> (bool, bool) {
        let before_area = self
            .inspect
            .commit_preview_before_area
            .inner(Margin::new(1, 1));
        let after_area = self
            .inspect
            .commit_preview_after_area
            .inner(Margin::new(1, 1));
        let panels = [
            (
                before_area,
                self.inspect.commit_preview_document.before_max_width(),
            ),
            (
                after_area,
                self.inspect.commit_preview_document.after_max_width(),
            ),
        ];
        if panels.iter().all(|(area, _)| area.is_empty()) {
            return (false, false);
        }
        let content_height = self.inspect.commit_preview_document.len();
        let can_reserve_vertical = panels
            .iter()
            .filter(|(area, _)| !area.is_empty())
            .all(|(area, _)| area.width > 1);
        let can_reserve_horizontal = panels
            .iter()
            .filter(|(area, _)| !area.is_empty())
            .all(|(area, _)| area.height > 1);
        let mut reserve_vertical = false;
        let mut reserve_horizontal = false;
        loop {
            let next_vertical = reserve_vertical
                || (can_reserve_vertical
                    && panels.iter().any(|(area, _)| {
                        !area.is_empty()
                            && content_height
                                > area.height.saturating_sub(u16::from(reserve_horizontal)) as usize
                    }));
            let next_horizontal = reserve_horizontal
                || (can_reserve_horizontal
                    && panels.iter().any(|(area, content_width)| {
                        !area.is_empty()
                            && *content_width
                                > area.width.saturating_sub(u16::from(reserve_vertical)) as usize
                    }));
            if (next_vertical, next_horizontal) == (reserve_vertical, reserve_horizontal) {
                return (reserve_vertical, reserve_horizontal);
            }
            reserve_vertical = next_vertical;
            reserve_horizontal = next_horizontal;
        }
    }

    pub(super) fn commit_preview_source_area(&self, panel: Rect) -> Rect {
        let inner = panel.inner(Margin::new(1, 1));
        let (reserve_vertical, reserve_horizontal) = self.commit_preview_scrollbar_visibility();
        Rect::new(
            inner.x,
            inner.y,
            inner
                .width
                .saturating_sub(if reserve_vertical { 1 } else { 0 }),
            inner
                .height
                .saturating_sub(if reserve_horizontal { 1 } else { 0 }),
        )
    }

    fn commit_preview_horizontal_scrollbar_area(&self, panel: Rect) -> Option<Rect> {
        let (_, horizontal) = self.commit_preview_scrollbar_visibility();
        if !horizontal || panel.is_empty() {
            return None;
        }
        let inner = panel.inner(Margin::new(1, 1));
        let source = self.commit_preview_source_area(panel);
        (inner.height > 1 && !source.is_empty()).then_some(Rect::new(
            inner.x,
            inner.bottom().saturating_sub(1),
            source.width,
            1,
        ))
    }

    fn commit_preview_vertical_scrollbar_area(&self, panel: Rect) -> Option<Rect> {
        let (vertical, _) = self.commit_preview_scrollbar_visibility();
        if !vertical || panel.is_empty() {
            return None;
        }
        let inner = panel.inner(Margin::new(1, 1));
        let source = self.commit_preview_source_area(panel);
        (inner.width > 1 && !source.is_empty()).then_some(Rect::new(
            inner.right().saturating_sub(1),
            inner.y,
            1,
            source.height,
        ))
    }

    fn commit_preview_horizontal_scrollbar_at(&self, column: u16, row: u16) -> Option<Rect> {
        if self.max_commit_preview_horizontal_scroll() == 0 {
            return None;
        }
        let position = (column, row).into();
        [
            self.inspect.commit_preview_before_area,
            self.inspect.commit_preview_after_area,
        ]
        .into_iter()
        .filter_map(|area| self.commit_preview_horizontal_scrollbar_area(area))
        .find(|area| area.contains(position))
    }

    fn commit_preview_vertical_scrollbar_at(&self, column: u16, row: u16) -> Option<Rect> {
        if self.max_commit_preview_scroll() == 0 {
            return None;
        }
        let position = (column, row).into();
        [
            self.inspect.commit_preview_before_area,
            self.inspect.commit_preview_after_area,
        ]
        .into_iter()
        .filter_map(|area| self.commit_preview_vertical_scrollbar_area(area))
        .find(|area| area.contains(position))
    }

    fn set_commit_preview_horizontal_scroll_from_pointer(&mut self, area: Rect, column: u16) {
        let max_scroll = self.max_commit_preview_horizontal_scroll();
        let track_max = usize::from(area.width.saturating_sub(1));
        if max_scroll == 0 || track_max == 0 {
            self.inspect.commit_preview_horizontal_scroll = 0;
            return;
        }
        let pointer = usize::from(column.saturating_sub(area.x)).min(track_max);
        let position = pointer
            .saturating_mul(max_scroll)
            .saturating_add(track_max / 2)
            / track_max;
        self.inspect.commit_preview_horizontal_scroll = position.min(u16::MAX as usize) as u16;
    }

    fn set_commit_preview_vertical_scroll_from_pointer(&mut self, area: Rect, row: u16) {
        let max_scroll = self.max_commit_preview_scroll();
        let track_max = usize::from(area.height.saturating_sub(1));
        if max_scroll == 0 || track_max == 0 {
            self.inspect.commit_preview_scroll = 0;
            return;
        }
        let pointer = usize::from(row.saturating_sub(area.y)).min(track_max);
        let position = pointer
            .saturating_mul(max_scroll)
            .saturating_add(track_max / 2)
            / track_max;
        self.inspect.commit_preview_scroll = position.min(u16::MAX as usize) as u16;
    }

    fn commit_preview_viewport(&self) -> usize {
        [
            self.inspect.commit_preview_before_area,
            self.inspect.commit_preview_after_area,
        ]
        .into_iter()
        .filter(|area| !area.is_empty())
        .map(|area| self.commit_preview_source_area(area).height as usize)
        .min()
        .unwrap_or_default()
    }

    fn max_commit_preview_scroll(&self) -> usize {
        self.inspect
            .commit_preview_document
            .len()
            .saturating_sub(self.commit_preview_viewport())
    }

    fn commit_preview_page(&self) -> isize {
        isize::try_from(self.commit_preview_viewport().max(1)).unwrap_or(isize::MAX)
    }

    fn commit_changes_page(&self) -> isize {
        isize::try_from(
            self.inspect
                .commit_detail_area
                .height
                .saturating_sub(2)
                .max(1),
        )
        .unwrap_or(isize::MAX)
    }

    fn max_commit_preview_horizontal_scroll(&self) -> usize {
        let before_viewport = self
            .commit_preview_source_area(self.inspect.commit_preview_before_area)
            .width as usize;
        let after_viewport = self
            .commit_preview_source_area(self.inspect.commit_preview_after_area)
            .width as usize;
        let before = self
            .inspect
            .commit_preview_document
            .before_max_width()
            .saturating_sub(before_viewport);
        let after = self
            .inspect
            .commit_preview_document
            .after_max_width()
            .saturating_sub(after_viewport);
        before.max(after)
    }

    pub(super) fn scroll_commit_preview_vertical(&mut self, delta: isize) {
        self.inspect.commit_preview_scroll = (self.inspect.commit_preview_scroll as usize)
            .saturating_add_signed(delta)
            .min(self.max_commit_preview_scroll())
            .min(u16::MAX as usize) as u16;
    }

    fn scroll_commit_preview_horizontal(&mut self, delta: isize) {
        self.inspect.commit_preview_horizontal_scroll =
            (self.inspect.commit_preview_horizontal_scroll as usize)
                .saturating_add_signed(delta)
                .min(self.max_commit_preview_horizontal_scroll())
                .min(u16::MAX as usize) as u16;
    }

    pub(super) fn apply_commit_details(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        target: CommitDetailsReadTarget,
        result: Result<CommitDetails, ReadError>,
    ) {
        let owns_result = self
            .inspect
            .pending_commit_details
            .as_ref()
            .is_some_and(|pending| {
                pending.id == id && pending.generation == generation && pending.target == target
            });
        if !owns_result {
            return;
        }
        self.inspect.pending_commit_details = None;
        if !self.foreground.reads.details.is_current(generation)
            || !self.commit_details_target_is_current(&target)
        {
            return;
        }
        match result {
            Ok(details) => self.show_commit_details(details),
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => {
                self.inspect.commit_details_error = Some(error);
            }
        }
    }

    fn show_commit_details(&mut self, details: CommitDetails) {
        self.inspect.details_cache.remember(&details);
        self.inspect.details = Some(details);
        self.inspect.commit_details_error = None;
        self.inspect.commit_detail_scroll = 0;
        self.inspect.commit_detail_follow_selection = true;
        self.inspect.commit_change_selected = 0;
    }

    pub(super) fn pending_commit_preview(&self) -> Option<&PendingDiff> {
        self.diff
            .pending_diff
            .as_ref()
            .filter(|pending| pending.target.owner == DiffOwner::CommitPreview)
    }

    pub(super) fn handle_inspect_key(&mut self, key: KeyEvent) -> bool {
        match self.focus {
            PaneFocus::Details => self.handle_details_key(key.code),
            PaneFocus::Preview => self.handle_preview_key(key.code),
            _ => false,
        }
    }

    fn handle_details_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Enter => self.activate_commit_change(self.inspect.commit_change_selected),
            KeyCode::Char(' ') => self.toggle_commit_preview(self.inspect.commit_change_selected),
            KeyCode::Right if self.inspect.commit_preview_path.is_some() => {
                self.focus = PaneFocus::Preview
            }
            KeyCode::Left => self.focus = PaneFocus::Commits,
            KeyCode::Down | KeyCode::Char('j') => self.move_commit_change_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_commit_change_selection(-1),
            KeyCode::PageDown => self.move_commit_change_selection(self.commit_changes_page()),
            KeyCode::PageUp => self.move_commit_change_selection(-self.commit_changes_page()),
            KeyCode::Home => self.select_commit_change(0),
            KeyCode::End => self.select_commit_change(usize::MAX),
            _ => return false,
        }
        true
    }

    fn handle_preview_key(&mut self, code: KeyCode) -> bool {
        let selecting = self.review.preview.keyboard_selecting;
        match code {
            KeyCode::Char('v') if !selecting => self.begin_preview_visual_selection(),
            KeyCode::Down | KeyCode::Char('j') if selecting => self.move_preview_cursor(1),
            KeyCode::Up | KeyCode::Char('k') if selecting => self.move_preview_cursor(-1),
            KeyCode::Right => self.scroll_commit_preview_horizontal(1),
            KeyCode::Left if self.inspect.commit_preview_horizontal_scroll == 0 => {
                self.focus = PaneFocus::Details
            }
            KeyCode::Left => self.scroll_commit_preview_horizontal(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_commit_preview_vertical(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_commit_preview_vertical(-1),
            KeyCode::PageDown => self.scroll_commit_preview_vertical(self.commit_preview_page()),
            KeyCode::PageUp => self.scroll_commit_preview_vertical(-self.commit_preview_page()),
            KeyCode::Home => self.scroll_commit_preview_vertical(isize::MIN),
            KeyCode::End => self.scroll_commit_preview_vertical(isize::MAX),
            _ => return false,
        }
        true
    }

    pub(super) fn handle_inspect_mouse(&mut self, mouse: MouseEvent) -> bool {
        let (column, row) = (mouse.column, mouse.row);
        let pointer = (column, row).into();
        let in_preview = self.inspect.commit_preview_area.contains(pointer);
        let in_details = self.inspect.commit_detail_area.contains(pointer);
        let horizontal = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(drag) = self.commit_preview_scrollbar_at(column, row) {
                    self.focus = PaneFocus::Preview;
                    self.scrollbar_drag = Some((ScrollbarOwner::CommitPreview, drag));
                    self.drag_commit_preview_scrollbar(drag, column, row);
                } else if self.inspect.commit_preview_close_area.contains(pointer) {
                    self.close_commit_preview();
                } else if let Some(index) = self
                    .inspect
                    .commit_change_row_areas
                    .iter()
                    .find_map(|(index, area)| area.contains(pointer).then_some(*index))
                {
                    self.toggle_commit_preview(index);
                } else if in_preview {
                    self.focus = PaneFocus::Preview;
                    self.click_commit_preview(column, row);
                } else {
                    return false;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.review.preview.dragging_selection => {
                if let Some((side, row)) = self.commit_preview_position(column, row)
                    && side == self.review.preview_cursor.0
                {
                    self.review.preview_cursor.1 = row;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((ScrollbarOwner::CommitPreview, drag)) = self.scrollbar_drag else {
                    return false;
                };
                self.drag_commit_preview_scrollbar(drag, column, row);
            }
            MouseEventKind::ScrollDown if in_preview && horizontal => {
                self.scroll_commit_preview_horizontal(1)
            }
            MouseEventKind::ScrollUp if in_preview && horizontal => {
                self.scroll_commit_preview_horizontal(-1)
            }
            MouseEventKind::ScrollDown if in_preview => self.scroll_commit_preview_vertical(1),
            MouseEventKind::ScrollUp if in_preview => self.scroll_commit_preview_vertical(-1),
            MouseEventKind::ScrollRight if in_preview => self.scroll_commit_preview_horizontal(1),
            MouseEventKind::ScrollLeft if in_preview => self.scroll_commit_preview_horizontal(-1),
            MouseEventKind::ScrollDown if in_details => self.scroll_commit_details(1),
            MouseEventKind::ScrollUp if in_details => self.scroll_commit_details(-1),
            _ => return false,
        }
        true
    }

    fn commit_preview_position(&self, column: u16, row: u16) -> Option<(ReviewSide, usize)> {
        [
            (ReviewSide::Before, self.inspect.commit_preview_before_area),
            (ReviewSide::After, self.inspect.commit_preview_after_area),
        ]
        .into_iter()
        .find_map(|(side, panel)| {
            let source = self.commit_preview_source_area(panel);
            source.contains((column, row).into()).then(|| {
                (
                    side,
                    self.inspect.commit_preview_scroll as usize + usize::from(row - source.y),
                )
            })
        })
        .filter(|(_, row)| *row < self.inspect.commit_preview_document.len())
    }

    fn click_commit_preview(&mut self, column: u16, row: u16) {
        if self.review.preview.keyboard_selecting {
            self.finish_visual_selection();
            return;
        }
        if let Some((side, row)) = self.commit_preview_position(column, row)
            && self
                .surface_source(SelectionSurface::Preview, side, row)
                .is_some()
        {
            self.begin_visual_selection(side, row, false);
        }
    }

    fn begin_preview_visual_selection(&mut self) {
        let scroll = self.inspect.commit_preview_scroll as usize;
        let rows = scroll..self.inspect.commit_preview_document.len();
        let start = [ReviewSide::After, ReviewSide::Before]
            .into_iter()
            .find_map(|side| {
                rows.clone()
                    .find(|&row| {
                        self.surface_source(SelectionSurface::Preview, side, row)
                            .is_some()
                    })
                    .map(|row| (side, row))
            });
        match start {
            Some((side, row)) => self.begin_visual_selection(side, row, true),
            None => self.show_action_error("The preview has no source line to select."),
        }
    }

    fn move_preview_cursor(&mut self, delta: isize) {
        let last = self.inspect.commit_preview_document.len().saturating_sub(1);
        let row = self
            .review
            .preview_cursor
            .1
            .saturating_add_signed(delta)
            .min(last);
        self.review.preview_cursor.1 = row;
        let viewport = self.commit_preview_viewport().max(1);
        let scroll = self.inspect.commit_preview_scroll as usize;
        if row < scroll {
            self.inspect.commit_preview_scroll = row.min(u16::MAX as usize) as u16;
        } else if row >= scroll + viewport {
            self.inspect.commit_preview_scroll = (row + 1 - viewport).min(u16::MAX as usize) as u16;
        }
    }

    fn scroll_commit_details(&mut self, delta: isize) {
        self.inspect.commit_detail_follow_selection = false;
        self.inspect.commit_detail_scroll = (self.inspect.commit_detail_scroll as usize)
            .saturating_add_signed(delta)
            .min(u16::MAX as usize) as u16;
    }

    fn commit_preview_scrollbar_at(&self, column: u16, row: u16) -> Option<ScrollbarDrag> {
        self.commit_preview_horizontal_scrollbar_at(column, row)
            .map(|area| ScrollbarDrag {
                area,
                axis: Axis::Horizontal,
            })
            .or_else(|| {
                self.commit_preview_vertical_scrollbar_at(column, row)
                    .map(|area| ScrollbarDrag {
                        area,
                        axis: Axis::Vertical,
                    })
            })
    }

    pub(super) fn drag_commit_preview_scrollbar(
        &mut self,
        drag: ScrollbarDrag,
        column: u16,
        row: u16,
    ) {
        match drag.axis {
            Axis::Horizontal => {
                self.set_commit_preview_horizontal_scroll_from_pointer(drag.area, column)
            }
            Axis::Vertical => self.set_commit_preview_vertical_scroll_from_pointer(drag.area, row),
        }
    }

    pub(super) fn draw_commit_details(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inspect.commit_detail_area = area;
        let title = if let Some(pending) = self.inspect.pending_commit_details.as_ref() {
            Line::from(vec![
                theme::spinner_span(pending.started.elapsed()),
                Span::raw(" Commit details"),
            ])
        } else if self.graph.uncommitted_selected() {
            Line::raw("Uncommitted")
        } else if let Some(error) = self.inspect.commit_details_error.as_deref() {
            let error = error.lines().next().unwrap_or("unknown error").trim();
            Line::styled(
                format!("Commit details failed · {error} · move selection to retry"),
                theme::error_title(),
            )
        } else {
            Line::raw("Commit details")
        };
        let block = widgets::pane_block(title, self.focus == PaneFocus::Details);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        self.refresh_detail_lines();
        let error_line = self
            .shell
            .error
            .as_ref()
            .map(|error| Line::styled(format!("Error: {error}"), theme::error_text()));
        let (lines, change_rows): (&[Line<'static>], &[usize]) = match error_line.as_ref() {
            Some(line) => (std::slice::from_ref(line), &[]),
            None => (
                &self.inspect.detail_lines.lines,
                &self.inspect.detail_lines.change_rows,
            ),
        };
        let max_scroll = lines.len().saturating_sub(inner.height as usize);
        self.inspect.commit_detail_scroll = (self.inspect.commit_detail_scroll as usize)
            .min(max_scroll)
            .min(u16::MAX as usize) as u16;
        if self.focus == PaneFocus::Details && self.inspect.commit_detail_follow_selection {
            if let Some(row) = change_rows
                .get(self.inspect.commit_change_selected)
                .copied()
            {
                let top = self.inspect.commit_detail_scroll as usize;
                let bottom = top.saturating_add(inner.height as usize);
                if row < top {
                    self.inspect.commit_detail_scroll = row.min(u16::MAX as usize) as u16;
                } else if row >= bottom {
                    self.inspect.commit_detail_scroll =
                        row.saturating_add(1)
                            .saturating_sub(inner.height as usize)
                            .min(u16::MAX as usize) as u16;
                }
            }
            self.inspect.commit_detail_follow_selection = false;
        }
        let viewport_start = (self.inspect.commit_detail_scroll as usize).min(lines.len());
        let viewport_end = viewport_start
            .saturating_add(inner.height as usize)
            .min(lines.len());
        frame.render_widget(
            Paragraph::new(lines[viewport_start..viewport_end].to_vec()),
            inner,
        );
        self.inspect.commit_change_row_areas = change_rows
            .iter()
            .enumerate()
            .filter_map(|(change_index, row)| {
                (*row >= viewport_start && *row < viewport_end).then_some((
                    change_index,
                    Rect::new(
                        inner.x,
                        inner
                            .y
                            .saturating_add(row.saturating_sub(viewport_start) as u16),
                        inner.width,
                        1,
                    ),
                ))
            })
            .collect();
        let hovered = self.shell.mouse_position.and_then(|position| {
            self.inspect
                .commit_change_row_areas
                .iter()
                .find_map(|(index, row)| row.contains(position.into()).then_some(*index))
        });
        for (change_index, row_area) in &self.inspect.commit_change_row_areas {
            let Some(line_index) = change_rows.get(*change_index).copied() else {
                continue;
            };
            let Some(line) = lines.get(line_index).cloned() else {
                continue;
            };
            let selected = self.focus == PaneFocus::Details
                && self.inspect.commit_change_selected == *change_index;
            let style = if selected {
                theme::focus_row()
            } else {
                theme::hover(Style::default(), hovered == Some(*change_index))
            };
            frame.render_widget(Paragraph::new(line).style(style), *row_area);
        }
    }

    pub(super) fn draw_commit_preview(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inspect.commit_preview_area = area;
        let shown_path = self
            .inspect
            .commit_preview_loaded_path
            .as_deref()
            .or(self.inspect.commit_preview_path.as_deref())
            .unwrap_or("File");
        let language = (!self.inspect.commit_preview_language.is_empty())
            .then_some(self.inspect.commit_preview_language.as_str());
        let title_width = usize::from(area.width.saturating_sub(PREVIEW_TITLE_RESERVE));
        let title = if let Some(error) = self.inspect.commit_preview_error.as_deref() {
            let error = error.lines().next().unwrap_or("unknown error").trim();
            Line::styled(
                truncate_to_width(
                    &format!("Preview failed · {error} · close and reopen to retry"),
                    title_width,
                ),
                theme::error_title(),
            )
        } else {
            let label = language.map_or_else(
                || format!("Preview · {shown_path}"),
                |language| format!("Preview · {shown_path} · {language}"),
            );
            match self.pending_commit_preview() {
                Some(pending) => Line::from(vec![
                    theme::spinner_span(pending.started.elapsed()),
                    Span::raw(format!(
                        " {}",
                        truncate_to_width(&label, title_width.saturating_sub(2))
                    )),
                ]),
                None => Line::raw(truncate_to_width(&label, title_width)),
            }
        };
        let block = widgets::pane_block(title, self.focus == PaneFocus::Preview);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if area.width >= 5 {
            self.inspect.commit_preview_close_area =
                Rect::new(area.right().saturating_sub(4), area.y, 3, 1);
            frame.render_widget(
                Paragraph::new("[×]").style(theme::hover(
                    theme::hint(),
                    area_hovered(
                        self.shell.mouse_position,
                        self.inspect.commit_preview_close_area,
                    ),
                )),
                self.inspect.commit_preview_close_area,
            );
        }
        if inner.is_empty() {
            return;
        }
        let panels = Layout::default()
            .direction(if inner.width >= 72 || inner.height < 8 {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        self.inspect.commit_preview_before_area = panels[0];
        self.inspect.commit_preview_after_area = panels[1];
        let selection = self.effective_selection_on(SelectionSurface::Preview);
        let selected_side = selection.as_ref().map(|selection| selection.side);
        let selection_title = self.selection_title(SelectionSurface::Preview);
        let panel_title = |side: ReviewSide| match &selection_title {
            Some(title) if selected_side == Some(side) => title.clone(),
            _ => Line::raw(side.label()),
        };
        let before_block = Block::default()
            .borders(Borders::ALL)
            .title(panel_title(ReviewSide::Before));
        let after_block = Block::default()
            .borders(Borders::ALL)
            .title(panel_title(ReviewSide::After));
        frame.render_widget(before_block, self.inspect.commit_preview_before_area);
        frame.render_widget(after_block, self.inspect.commit_preview_after_area);
        self.inspect.commit_preview_scroll = (self.inspect.commit_preview_scroll as usize)
            .min(self.max_commit_preview_scroll())
            .min(u16::MAX as usize) as u16;
        self.inspect.commit_preview_horizontal_scroll =
            (self.inspect.commit_preview_horizontal_scroll as usize)
                .min(self.max_commit_preview_horizontal_scroll())
                .min(u16::MAX as usize) as u16;
        let before_source =
            self.commit_preview_source_area(self.inspect.commit_preview_before_area);
        let after_source = self.commit_preview_source_area(self.inspect.commit_preview_after_area);
        let scroll = self.inspect.commit_preview_scroll as usize;
        let horizontal = self.inspect.commit_preview_horizontal_scroll;
        let document = &self.inspect.commit_preview_document;
        let mut before_lines = padded_diff_lines(
            (scroll..)
                .map_while(|row| document.before_line(row))
                .take(before_source.height as usize),
            before_source.width as usize,
            horizontal as usize,
        );
        let mut after_lines = padded_diff_lines(
            (scroll..)
                .map_while(|row| document.after_line(row))
                .take(after_source.height as usize),
            after_source.width as usize,
            horizontal as usize,
        );
        if let Some(selection) = &selection {
            let (lines, width) = match selection.side {
                ReviewSide::Before => (&mut before_lines, before_source.width),
                ReviewSide::After => (&mut after_lines, after_source.width),
            };
            for (offset, line) in lines.iter_mut().enumerate() {
                let row = scroll + offset;
                if selection.contains(row) {
                    let code_row = self
                        .surface_source(SelectionSurface::Preview, selection.side, row)
                        .is_some();
                    emphasize_diff_line(line, code_row, theme::selection_row(), width as usize);
                }
            }
        }
        frame.render_widget(
            Paragraph::new(before_lines).scroll((0, horizontal)),
            before_source,
        );
        frame.render_widget(
            Paragraph::new(after_lines).scroll((0, horizontal)),
            after_source,
        );
        self.draw_commit_preview_scrollbars(frame);
        self.draw_selection_decorations(
            frame,
            SelectionSurface::Preview,
            [before_source, after_source],
            scroll,
        );
    }

    fn draw_commit_preview_scrollbars(&self, frame: &mut Frame<'_>) {
        let vertical_max = self.max_commit_preview_scroll();
        let horizontal_max = self.max_commit_preview_horizontal_scroll();
        let (show_vertical, show_horizontal) = self.commit_preview_scrollbar_visibility();
        for panel in [
            self.inspect.commit_preview_before_area,
            self.inspect.commit_preview_after_area,
        ] {
            if panel.is_empty() {
                continue;
            }
            let source = self.commit_preview_source_area(panel);
            if show_horizontal
                && horizontal_max > 0
                && let Some(track) = self.commit_preview_horizontal_scrollbar_area(panel)
            {
                let mut state = ScrollbarState::new(horizontal_max.saturating_add(1))
                    .position(self.inspect.commit_preview_horizontal_scroll as usize)
                    .viewport_content_length(source.width as usize);
                frame.render_stateful_widget(
                    widgets::scrollbar(ScrollbarOrientation::HorizontalBottom),
                    track,
                    &mut state,
                );
            }
            if show_vertical
                && vertical_max > 0
                && let Some(track) = self.commit_preview_vertical_scrollbar_area(panel)
            {
                let mut state = ScrollbarState::new(vertical_max.saturating_add(1))
                    .position(self.inspect.commit_preview_scroll as usize)
                    .viewport_content_length(source.height as usize);
                frame.render_stateful_widget(
                    widgets::scrollbar(ScrollbarOrientation::VerticalRight),
                    track,
                    &mut state,
                );
            }
        }
    }

    fn refresh_detail_lines(&mut self) {
        let uncommitted = self.graph.uncommitted_selected();
        let cache = &mut self.inspect.detail_lines;
        if uncommitted {
            let (lines, change_rows) = DetailLines::build_working(&self.files.changes);
            cache.details = None;
            cache.uncommitted = true;
            cache.lines = lines;
            cache.change_rows = change_rows;
            return;
        }
        if !cache.uncommitted && !cache.lines.is_empty() && cache.details == self.inspect.details {
            return;
        }
        let (lines, change_rows) = DetailLines::build(self.inspect.details.as_ref());
        cache.details = self.inspect.details.clone();
        cache.uncommitted = false;
        cache.lines = lines;
        cache.change_rows = change_rows;
    }
}

impl DetailLines {
    fn build(details: Option<&CommitDetails>) -> (Vec<Line<'static>>, Vec<usize>) {
        let Some(details) = details else {
            return (vec![Line::raw("No commits")], Vec::new());
        };
        let commit = &details.commit;
        let field = |label: &'static str, value: String| {
            Line::from(vec![
                Span::styled(format!("{label}: "), theme::hint()),
                Span::raw(value),
            ])
        };
        let mut lines = vec![
            Line::styled(
                commit.subject.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            field(
                "Author",
                format!("{} <{}>", commit.author_name, commit.author_email),
            ),
            field("Date", commit.author_time.clone()),
            field("SHA", commit.sha.clone()),
            field(
                "Parents",
                if commit.parents.is_empty() {
                    "(root)".to_owned()
                } else {
                    commit.parents.join(" ")
                },
            ),
            field(
                "Refs",
                if commit.refs.is_empty() {
                    "—".to_owned()
                } else {
                    commit
                        .refs
                        .iter()
                        .map(|reference| reference.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            ),
            Line::raw(""),
        ];
        lines.extend(commit.body.lines().map(|line| Line::raw(line.to_owned())));
        if !commit.body.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            counted_title("Changes", details.changes.len()),
            theme::section_header(),
        ));
        let mut change_rows = Vec::with_capacity(details.changes.len());
        if details.changes.is_empty() {
            lines.push(Line::raw("(none)"));
        } else {
            for change in &details.changes {
                change_rows.push(lines.len());
                lines.push(Line::from(vec![
                    Span::styled(
                        status_label(&change.status).to_owned(),
                        status_style(&change.status),
                    ),
                    Span::styled("  ", theme::hint()),
                    Span::raw(change.path.clone()),
                ]));
            }
        }
        (lines, change_rows)
    }

    fn build_working(changes: &[WorkingChange]) -> (Vec<Line<'static>>, Vec<usize>) {
        let working: Vec<_> = changes
            .iter()
            .filter(|change| change.section != ChangeSection::Commit)
            .collect();
        let mut lines = vec![
            Line::styled(
                "Uncommitted".to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled(
                counted_title("Changes", working.len()),
                theme::section_header(),
            ),
        ];
        let mut change_rows = Vec::with_capacity(working.len());
        if working.is_empty() {
            lines.push(Line::raw("(none)"));
        } else {
            for change in working {
                change_rows.push(lines.len());
                lines.push(Line::from(vec![
                    Span::styled(
                        status_label(&change.status).to_owned(),
                        status_style(&change.status),
                    ),
                    Span::styled("  ", theme::hint()),
                    Span::raw(change.path.clone()),
                ]));
            }
        }
        (lines, change_rows)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Line;

    use crate::git::{
        ChangedPath, Commit, CommitDetails, CommitRef, CommitRefKind, DiffTarget, Repository,
    };
    use crate::ui::effect::{
        CommitDetailsReadTarget, DiffOwner, ForegroundRequest, ForegroundResult, HighlightedDiff,
    };
    use crate::ui::shell::{ActiveTab, PaneFocus};
    use crate::ui::test_support::{
        git, intercept_foreground, offline_app, press, render, row_text, temp_repo,
        test_diff_document,
    };
    use crate::ui::{App, theme};

    use super::DetailsCache;

    #[test]
    fn graph_row_navigation_enqueues_only_the_latest_commit_details_read() {
        let root = temp_repo("details-async");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "latest\n").unwrap();
        git(&root, &["commit", "-am", "Latest"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let previous_details = app.inspect.details.as_ref().unwrap().commit.sha.clone();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        let (foreground_rx, _foreground_result_tx) = intercept_foreground(&mut app);

        app.move_selection(1);

        assert!(refresh_rx.try_recv().is_err());
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.sha,
            previous_details
        );
        assert!(matches!(
            foreground_rx.try_recv().expect("commit details request"),
            ForegroundRequest::CommitDetails {
                target: CommitDetailsReadTarget { path, commit },
                ..
            } if path == fs::canonicalize(&root).unwrap() && commit.subject == "Base"
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rapid_graph_navigation_applies_only_the_latest_commit_details_result() {
        let root = temp_repo("details-latest");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "middle\n").unwrap();
        git(&root, &["commit", "-am", "Middle"]);
        fs::write(root.join("tracked.txt"), "latest\n").unwrap();
        git(&root, &["commit", "-am", "Latest"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let initial_details_sha = app.inspect.details.as_ref().unwrap().commit.sha.clone();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        let (foreground_rx, foreground_result_tx) = intercept_foreground(&mut app);

        app.move_selection(1);
        let ForegroundRequest::CommitDetails {
            id: stale_id,
            generation: stale_generation,
            target: stale_target,
        } = foreground_rx.recv().unwrap()
        else {
            panic!("expected first commit-details request");
        };
        app.move_selection(1);
        let ForegroundRequest::CommitDetails {
            id: latest_id,
            generation: latest_generation,
            target: latest_target,
        } = foreground_rx.recv().unwrap()
        else {
            panic!("expected latest commit-details request");
        };

        assert!(refresh_rx.try_recv().is_err());
        assert_eq!(
            app.ops.command_context.selected_commit.as_deref(),
            Some(latest_target.commit.sha.as_str())
        );
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.sha,
            initial_details_sha
        );

        let repository = Repository::discover(&root).unwrap();
        foreground_result_tx
            .send(ForegroundResult::CommitDetails {
                id: stale_id,
                generation: stale_generation,
                result: Box::new(Ok(repository.details(&stale_target.commit).unwrap())),
                target: stale_target,
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.sha,
            initial_details_sha
        );
        assert_eq!(
            app.inspect
                .pending_commit_details
                .as_ref()
                .map(|pending| pending.id),
            Some(latest_id)
        );

        foreground_result_tx
            .send(ForegroundResult::CommitDetails {
                id: latest_id,
                generation: latest_generation,
                result: Box::new(Ok(repository.details(&latest_target.commit).unwrap())),
                target: latest_target.clone(),
            })
            .unwrap();
        app.receive_foreground_results();
        assert!(app.inspect.pending_commit_details.is_none());
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.sha,
            latest_target.commit.sha
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_changed_path_click_toggles_an_owned_async_preview() {
        let root = temp_repo("commit-preview");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Add tracked file"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        let original_changes = app.files.changes.clone();
        let original_diff_text = app.diff.diff_text.clone();
        render(&mut app, 160, 48);
        let (_, area) = app.inspect.commit_change_row_areas[0];

        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();

        assert_eq!(app.shell.active_tab, ActiveTab::History);
        assert_eq!(app.files.changes, original_changes);
        assert_eq!(
            app.inspect.commit_preview_path.as_deref(),
            Some("tracked.txt")
        );
        assert_eq!(app.focus, PaneFocus::Details);
        let request = request_rx.try_recv().expect("commit preview request");
        let ForegroundRequest::Diff {
            id,
            generation,
            owner,
            path,
            file,
            target,
            fold_toggles,
        } = request
        else {
            panic!("expected commit preview diff request");
        };
        assert_eq!(owner, DiffOwner::CommitPreview);
        assert_eq!(file, "tracked.txt");
        assert!(matches!(
            target,
            DiffTarget::CommitAgainstParent { parent: None, .. }
        ));
        let buffer = render(&mut app, 160, 48);
        let preview = app.inspect.commit_preview_area;
        let spinner = &buffer[(preview.x + 1, preview.y)];
        assert!(theme::SPINNER_FRAMES.contains(&spinner.symbol()));
        assert_eq!(spinner.fg, theme::ACCENT);
        assert_eq!(buffer[(preview.x, preview.y)].fg, Color::Reset);
        let close = app.inspect.commit_preview_close_area;
        assert_eq!(row_text(&buffer, close, close.y), "[×]");
        assert_eq!(buffer[(close.x, close.y)].fg, theme::HINT);
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
                    "preview patch".to_owned(),
                    HighlightedDiff {
                        language: "Text".to_owned(),
                        split: test_diff_document(
                            vec![Line::raw("before")],
                            vec![Line::raw("after")],
                        ),
                        key: 0,
                    },
                )),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(
            app.inspect.commit_preview_loaded_path.as_deref(),
            Some("tracked.txt")
        );
        assert_eq!(
            app.inspect
                .commit_preview_document
                .after_line(0)
                .unwrap()
                .to_string(),
            "after"
        );
        assert_eq!(app.files.changes, original_changes);
        assert_eq!(app.diff.diff_text, original_diff_text);

        render(&mut app, 160, 48);
        let (_, area) = app.inspect.commit_change_row_areas[0];
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        assert!(app.inspect.commit_preview_path.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uncommitted_graph_changed_path_click_opens_working_tree_preview() {
        let root = temp_repo("uncommitted-preview");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Add tracked file"]);
        fs::write(root.join("tracked.txt"), "dirty\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.select(0);
        assert!(app.graph.uncommitted_selected());
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        render(&mut app, 160, 48);
        let (_, area) = app.inspect.commit_change_row_areas[0];

        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();

        assert_eq!(
            app.inspect.commit_preview_path.as_deref(),
            Some("tracked.txt")
        );
        let request = request_rx.try_recv().expect("uncommitted preview request");
        let ForegroundRequest::Diff {
            owner,
            file,
            target,
            ..
        } = request
        else {
            panic!("expected commit preview diff request");
        };
        assert_eq!(owner, DiffOwner::CommitPreview);
        assert_eq!(file, "tracked.txt");
        assert_eq!(target, DiffTarget::WorkingTreeAgainstIndex);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn commit_preview_scrolls_both_axes_and_clamps() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.inspect.commit_preview_path = Some("src/long.rs".to_owned());
        let long = "x".repeat(300);
        app.inspect.commit_preview_document = test_diff_document(
            (0..100).map(|_| Line::raw(long.clone())).collect(),
            (0..100).map(|_| Line::raw(long.clone())).collect(),
        );
        render(&mut app, 160, 32);

        app.scroll_commit_preview_vertical(10_000);
        app.scroll_commit_preview_horizontal(10_000);
        assert_eq!(
            app.inspect.commit_preview_scroll as usize,
            app.max_commit_preview_scroll()
        );
        assert_eq!(
            app.inspect.commit_preview_horizontal_scroll as usize,
            app.max_commit_preview_horizontal_scroll()
        );
        let source = app.commit_preview_source_area(app.inspect.commit_preview_after_area);
        assert!(source.right() < app.inspect.commit_preview_after_area.right());
        assert!(source.bottom() < app.inspect.commit_preview_after_area.bottom());
        let buffer = render(&mut app, 160, 32);
        let track_column = source.right();
        let thumb = (source.y..source.bottom())
            .find(|row| buffer[(track_column, *row)].symbol() == "█")
            .expect("vertical thumb");
        assert_eq!(buffer[(track_column, thumb)].fg, theme::ACCENT);
        assert!(
            !buffer[(track_column, thumb)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let track_row = source.bottom();
        let horizontal_thumb = (source.x..source.right())
            .find(|column| buffer[(*column, track_row)].symbol() == "▄")
            .expect("horizontal thumb");
        assert_eq!(buffer[(horizontal_thumb, track_row)].fg, theme::ACCENT);
        let horizontal_track = (source.x..source.right())
            .find(|column| buffer[(*column, track_row)].symbol() == "─")
            .expect("horizontal track");
        assert_eq!(buffer[(horizontal_track, track_row)].fg, theme::MUTED);

        app.scroll_commit_preview_vertical(-10_000);
        app.scroll_commit_preview_horizontal(-10_000);
        assert_eq!(app.inspect.commit_preview_scroll, 0);
        assert_eq!(app.inspect.commit_preview_horizontal_scroll, 0);

        app.focus = PaneFocus::Preview;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.inspect.commit_preview_horizontal_scroll, 1);
        app.handle(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.inspect.commit_preview_horizontal_scroll, 0);
        assert_eq!(app.focus, PaneFocus::Preview);
        app.handle(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.focus, PaneFocus::Details);
    }

    #[test]
    fn commit_preview_horizontal_wheel_track_and_drag_share_one_bounded_offset() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.inspect.commit_preview_path = Some("src/long.rs".to_owned());
        app.inspect.commit_preview_document = test_diff_document(
            (0..100).map(|_| Line::raw("b".repeat(300))).collect(),
            (0..100)
                .map(|_| Line::raw("abcdefghijklmnopqrstuvwxyz".repeat(12)))
                .collect(),
        );
        let buffer = render(&mut app, 240, 32);
        let source = app.commit_preview_source_area(app.inspect.commit_preview_after_area);
        assert_eq!(buffer[(source.x, source.y)].symbol(), "a");
        let pointer = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        app.handle(pointer(MouseEventKind::ScrollRight, source.x, source.y))
            .unwrap();
        assert_eq!(app.inspect.commit_preview_horizontal_scroll, 1);
        let buffer = render(&mut app, 240, 32);
        assert_eq!(buffer[(source.x, source.y)].symbol(), "b");
        app.inspect.commit_preview_horizontal_scroll = 0;

        let track_row = source.bottom();
        app.handle(pointer(
            MouseEventKind::Down(MouseButton::Left),
            source.x.saturating_add(source.width / 2),
            track_row,
        ))
        .unwrap();
        assert!(app.inspect.commit_preview_horizontal_scroll > 0);
        app.handle(pointer(
            MouseEventKind::Drag(MouseButton::Left),
            u16::MAX,
            track_row,
        ))
        .unwrap();
        assert_eq!(
            app.inspect.commit_preview_horizontal_scroll as usize,
            app.max_commit_preview_horizontal_scroll()
        );
        app.handle(pointer(
            MouseEventKind::Up(MouseButton::Left),
            u16::MAX,
            track_row,
        ))
        .unwrap();

        app.handle(pointer(
            MouseEventKind::Down(MouseButton::Left),
            source.right(),
            source.y.saturating_add(source.height / 2),
        ))
        .unwrap();
        assert!(app.inspect.commit_preview_scroll > 0);
        app.handle(pointer(
            MouseEventKind::Drag(MouseButton::Left),
            source.right(),
            u16::MAX,
        ))
        .unwrap();
        assert_eq!(
            app.inspect.commit_preview_scroll as usize,
            app.max_commit_preview_scroll()
        );
        app.handle(pointer(
            MouseEventKind::Up(MouseButton::Left),
            source.right(),
            u16::MAX,
        ))
        .unwrap();
        assert!(app.scrollbar_drag.is_none());
    }

    #[test]
    fn compact_commit_preview_keeps_its_only_code_row_instead_of_a_scrollbar() {
        let mut app = offline_app();
        app.inspect.commit_preview_before_area = Rect::new(0, 0, 20, 3);
        app.inspect.commit_preview_after_area = Rect::new(20, 0, 20, 3);
        app.inspect.commit_preview_document = test_diff_document(
            vec![Line::raw("a line much wider than its compact panel")],
            vec![Line::raw("another line wider than its compact panel")],
        );

        let (_, horizontal) = app.commit_preview_scrollbar_visibility();
        let source = app.commit_preview_source_area(app.inspect.commit_preview_after_area);

        assert!(!horizontal);
        assert_eq!(source.height, 1);
    }

    #[test]
    fn asymmetric_commit_preview_only_reserves_a_horizontal_gutter_for_real_overflow() {
        let mut app = offline_app();
        app.inspect.commit_preview_before_area = Rect::new(0, 0, 11, 5);
        app.inspect.commit_preview_after_area = Rect::new(11, 0, 10, 5);
        app.inspect.commit_preview_document =
            test_diff_document(vec![Line::raw("123456789")], vec![Line::raw("12345678")]);

        let (_, horizontal) = app.commit_preview_scrollbar_visibility();
        let before_source = app.commit_preview_source_area(app.inspect.commit_preview_before_area);
        let after_source = app.commit_preview_source_area(app.inspect.commit_preview_after_area);

        assert!(!horizontal);
        assert_eq!(before_source.height, 3);
        assert_eq!(after_source.height, 3);
        assert_eq!(app.max_commit_preview_horizontal_scroll(), 0);
    }

    #[test]
    fn manual_commit_detail_scroll_does_not_snap_back_to_the_selected_path() {
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
                subject: "feat: inspect details".to_owned(),
                body: String::new(),
            },
            changes: (0..30)
                .map(|index| ChangedPath {
                    status: "M".to_owned(),
                    path: format!("src/file-{index:02}.rs"),
                })
                .collect(),
        });
        app.focus = PaneFocus::Details;
        app.inspect.commit_change_selected = 29;
        render(&mut app, 120, 32);
        let followed_scroll = app.inspect.commit_detail_scroll;
        assert!(followed_scroll > 0);

        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: app.inspect.commit_detail_area.x + 1,
            row: app.inspect.commit_detail_area.y + 1,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();
        let manual_scroll = app.inspect.commit_detail_scroll;
        assert!(manual_scroll < followed_scroll);
        render(&mut app, 120, 32);

        assert_eq!(app.inspect.commit_detail_scroll, manual_scroll);
    }

    #[test]
    fn commit_details_use_a_file_icon_instead_of_a_view_icon() {
        let mut app = offline_app();
        app.inspect.details = Some(CommitDetails {
            commit: Commit {
                sha: "abc12345".to_owned(),
                parents: Vec::new(),
                author_name: "Ada".to_owned(),
                author_email: "ada@example.com".to_owned(),
                author_time: "2026-09-02T15:28:00+09:00".to_owned(),
                refs: Vec::new(),
                subject: "feat: inspect details".to_owned(),
                body: String::new(),
            },
            changes: vec![ChangedPath {
                status: "M".to_owned(),
                path: "src/main.rs".to_owned(),
            }],
        });

        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let buffer = &render(&mut app, 120, 60);
        let rendered = (0..60)
            .flat_map(|row| (0..120).map(move |column| buffer[(column, row)].symbol().to_owned()))
            .collect::<String>();

        assert!(rendered.contains(''));
        assert!(!rendered.contains(''));
    }

    #[test]
    fn failed_commit_preview_keeps_the_last_complete_document_visible() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.inspect.commit_preview_path = Some("src/new.rs".to_owned());
        app.inspect.commit_preview_loaded_path = Some("src/previous.rs".to_owned());
        app.inspect.commit_preview_document = test_diff_document(
            vec![Line::raw("last complete before")],
            vec![Line::raw("last complete after")],
        );
        app.inspect.commit_preview_error = Some("Git read failed".to_owned());
        let buffer = &render(&mut app, 120, 32);
        let rendered = (0..32)
            .flat_map(|row| (0..120).map(move |column| buffer[(column, row)].symbol().to_owned()))
            .collect::<String>();

        assert!(rendered.contains("Preview failed"));
        assert!(rendered.contains("last complete after"));
    }

    #[test]
    fn short_graph_preview_keeps_at_least_one_visible_code_row() {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app.inspect.commit_preview_path = Some("src/short.rs".to_owned());
        app.inspect.commit_preview_document = test_diff_document(
            vec![Line::raw("visible-before")],
            vec![Line::raw("visible-after")],
        );
        let buffer = &render(&mut app, 60, 24);
        let source = app.commit_preview_source_area(app.inspect.commit_preview_after_area);
        let rendered = (0..24)
            .flat_map(|row| (0..60).map(move |column| buffer[(column, row)].symbol().to_owned()))
            .collect::<String>();

        assert!(source.height > 0);
        assert!(rendered.contains("visible-after"));
    }

    fn sample_details(paths: &[&str]) -> CommitDetails {
        CommitDetails {
            commit: Commit {
                sha: "abc12345".to_owned(),
                parents: Vec::new(),
                author_name: "Ada".to_owned(),
                author_email: "ada@example.com".to_owned(),
                author_time: "2026-09-02T15:28:00+09:00".to_owned(),
                refs: Vec::new(),
                subject: "feat: inspect details".to_owned(),
                body: String::new(),
            },
            changes: paths
                .iter()
                .map(|path| ChangedPath {
                    status: "M".to_owned(),
                    path: (*path).to_owned(),
                })
                .collect(),
        }
    }

    fn change_row(app: &App, index: usize) -> Rect {
        app.inspect
            .commit_change_row_areas
            .iter()
            .find_map(|(change, row)| (*change == index).then_some(*row))
            .unwrap()
    }

    fn offline_graph() -> App {
        let mut app = offline_app();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        app
    }

    #[test]
    fn commit_details_pane_marks_focus_with_the_border_and_the_change_with_focus_row() {
        let mut app = offline_graph();
        app.inspect.details = Some(sample_details(&["src/a.rs", "src/b.rs"]));
        app.focus = PaneFocus::Details;
        app.inspect.commit_change_selected = 1;
        let buffer = render(&mut app, 120, 48);
        let area = app.inspect.commit_detail_area;

        assert_eq!(buffer[(area.x, area.y)].fg, theme::ACCENT);
        let title = row_text(&buffer, area, area.y);
        assert!(title.contains("Commit details"));
        let title_column = (area.x..area.right())
            .find(|column| buffer[(*column, area.y)].symbol() == "C")
            .unwrap();
        assert_eq!(buffer[(title_column, area.y)].fg, Color::Reset);
        let selected = change_row(&app, 1);
        assert_eq!(buffer[(selected.x, selected.y)].bg, theme::SURFACE_FOCUS);
        assert!(
            buffer[(selected.x, selected.y)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(buffer[(selected.x, selected.y)].symbol(), "M");
        assert_eq!(buffer[(selected.x, selected.y)].fg, theme::WARNING);
        let other = change_row(&app, 0);
        assert_eq!(buffer[(other.x, other.y)].bg, Color::Reset);
        assert_eq!(buffer[(other.x, other.y)].fg, theme::WARNING);
        let header_y = other.y - 1;
        assert!(row_text(&buffer, area, header_y).contains("Changes · 2"));
        assert_eq!(buffer[(area.x + 1, header_y)].fg, theme::ACCENT);
        assert!(
            buffer[(area.x + 1, header_y)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let author_y = area.y + 3;
        assert!(row_text(&buffer, area, author_y).contains("Author: Ada"));
        assert_eq!(buffer[(area.x + 1, author_y)].fg, theme::HINT);

        app.focus = PaneFocus::Commits;
        let buffer = render(&mut app, 120, 48);
        assert_eq!(buffer[(area.x, area.y)].fg, Color::Reset);
        let other = change_row(&app, 0);
        assert_eq!(buffer[(other.x, other.y)].bg, Color::Reset);
    }

    #[test]
    fn failed_details_and_preview_titles_use_the_error_title_style() {
        let mut app = offline_graph();
        app.inspect.commit_details_error = Some("boom\nsecond line".to_owned());
        app.inspect.commit_preview_path = Some("src/x.rs".to_owned());
        app.inspect.commit_preview_error = Some("Git read failed".to_owned());
        let buffer = render(&mut app, 160, 40);

        let details = app.inspect.commit_detail_area;
        let title = row_text(&buffer, details, details.y);
        assert!(title.contains("Commit details failed · boom · move selection to retry"));
        assert_eq!(buffer[(details.x + 1, details.y)].fg, theme::ERROR);
        assert!(
            buffer[(details.x + 1, details.y)]
                .modifier
                .contains(Modifier::BOLD)
        );
        let preview = app.inspect.commit_preview_area;
        let title = row_text(&buffer, preview, preview.y);
        assert!(title.contains("Preview failed · Git read failed"));
        assert_eq!(buffer[(preview.x + 1, preview.y)].fg, theme::ERROR);
        assert!(
            buffer[(preview.x + 1, preview.y)]
                .modifier
                .contains(Modifier::BOLD)
        );

        app.focus = PaneFocus::Preview;
        let buffer = render(&mut app, 160, 40);
        assert_eq!(buffer[(preview.x, preview.y)].fg, theme::ACCENT);
        assert_eq!(buffer[(details.x, details.y)].fg, Color::Reset);
    }

    #[test]
    fn details_and_preview_keys_follow_the_list_grammar() {
        let root = temp_repo("inspect-keys");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        for index in 0..30 {
            fs::write(root.join(format!("file-{index:02}.txt")), "content\n").unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Thirty files"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        render(&mut app, 160, 40);
        assert_eq!(app.selected_commit_details().unwrap().changes.len(), 30);

        press(&mut app, KeyCode::Right);
        assert_eq!(app.focus, PaneFocus::Details);
        let page = usize::from(app.inspect.commit_detail_area.height - 2);
        assert!(page > 1 && page < 29, "details rows: {page}");
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.inspect.commit_change_selected, page);
        press(&mut app, KeyCode::End);
        assert_eq!(app.inspect.commit_change_selected, 29);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.inspect.commit_change_selected, 29 - page);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.inspect.commit_change_selected, 0);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.inspect.commit_change_selected, 2);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.inspect.commit_change_selected, 0);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.focus, PaneFocus::Details);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.focus, PaneFocus::Commits);

        app.inspect.commit_preview_path = Some("file-00.txt".to_owned());
        app.inspect.commit_preview_document = test_diff_document(
            (0..100)
                .map(|index| Line::raw(format!("before {index}")))
                .collect(),
            (0..100)
                .map(|index| Line::raw(format!("after {index}")))
                .collect(),
        );
        render(&mut app, 160, 40);
        app.focus = PaneFocus::Details;
        press(&mut app, KeyCode::Right);
        assert_eq!(app.focus, PaneFocus::Preview);
        let page = app.commit_preview_viewport();
        assert!(page > 1 && page < 50, "preview rows: {page}");
        press(&mut app, KeyCode::End);
        assert_eq!(
            app.inspect.commit_preview_scroll as usize,
            app.max_commit_preview_scroll()
        );
        press(&mut app, KeyCode::Home);
        assert_eq!(app.inspect.commit_preview_scroll, 0);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.inspect.commit_preview_scroll as usize, page);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.inspect.commit_preview_scroll as usize, page - 1);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.inspect.commit_preview_scroll, 0);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, PaneFocus::Details);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, PaneFocus::Commits);
        assert_eq!(app.graph.selected, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn details_enter_opens_then_focuses_the_preview_and_space_toggles_it() {
        let root = temp_repo("inspect-enter-space");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("a.txt"), "a\n").unwrap();
        fs::write(root.join("b.txt"), "b\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Two files"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Details;
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        render(&mut app, 160, 40);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.inspect.commit_preview_path.as_deref(), Some("a.txt"));
        assert_eq!(app.focus, PaneFocus::Details);
        assert!(matches!(
            request_rx.try_recv().expect("preview read"),
            ForegroundRequest::Diff {
                owner: DiffOwner::CommitPreview,
                ..
            }
        ));

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.inspect.commit_preview_path.as_deref(), Some("a.txt"));
        assert_eq!(app.focus, PaneFocus::Preview);
        assert!(
            request_rx.try_recv().is_err(),
            "Enter on an open preview only focuses it"
        );

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, PaneFocus::Details);
        press(&mut app, KeyCode::Char(' '));
        assert!(app.inspect.commit_preview_path.is_none());
        assert_eq!(app.focus, PaneFocus::Details);

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.inspect.commit_preview_path.as_deref(), Some("b.txt"));
        assert_eq!(app.focus, PaneFocus::Details);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.inspect.commit_preview_path.as_deref(), Some("a.txt"));
        assert_eq!(app.focus, PaneFocus::Details);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_title_is_truncated_to_leave_room_for_the_close_action() {
        let mut app = offline_graph();
        let path = "src/a/very/long/directory/chain/that/keeps/going/past/the/pane/width/file.rs";
        app.inspect.commit_preview_path = Some(path.to_owned());
        app.inspect.commit_preview_loaded_path = Some(path.to_owned());
        app.inspect.commit_preview_language = "Rust".to_owned();
        let buffer = render(&mut app, 120, 32);
        let area = app.inspect.commit_preview_area;
        let close = app.inspect.commit_preview_close_area;
        assert_eq!(close.y, area.y);
        assert_eq!(close.right(), area.right() - 1);
        assert_eq!(row_text(&buffer, close, close.y), "[×]");
        let title = row_text(&buffer, area, area.y);
        assert!(title.starts_with("┌Preview · src/a/very"));
        assert!(title.contains('…'));
        assert!(!title.contains("file.rs"));
        let gap = &buffer[(close.x - 1, close.y)];
        assert_eq!(gap.symbol(), "─");

        app.inspect.commit_preview_error = Some("x".repeat(200));
        let buffer = render(&mut app, 120, 32);
        let title = row_text(&buffer, area, area.y);
        assert!(title.contains("Preview failed · xxx"));
        assert!(title.contains('…'));
        assert_eq!(row_text(&buffer, close, close.y), "[×]");
        assert_eq!(buffer[(close.x - 1, close.y)].symbol(), "─");
    }

    #[test]
    fn a_file_diff_request_leaves_the_pending_commit_details_to_apply() {
        let root = temp_repo("details-survive-diff");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "latest\n").unwrap();
        git(&root, &["commit", "-am", "Latest"]);
        fs::write(root.join("tracked.txt"), "edited\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let (request_rx, result_tx) = intercept_foreground(&mut app);

        app.move_selection(1);
        let ForegroundRequest::CommitDetails {
            id,
            generation,
            target,
        } = request_rx.try_recv().expect("commit details request")
        else {
            panic!("unexpected request")
        };
        assert_eq!(target.commit.subject, "Base");
        app.request_file_diff();
        assert!(matches!(
            request_rx.try_recv(),
            Ok(ForegroundRequest::Diff {
                owner: DiffOwner::Changes,
                ..
            })
        ));
        assert!(app.inspect.pending_commit_details.is_some());

        let repository = Repository::discover(&root).unwrap();
        result_tx
            .send(ForegroundResult::CommitDetails {
                id,
                generation,
                result: Box::new(Ok(repository.details(&target.commit).unwrap())),
                target: target.clone(),
            })
            .unwrap();
        app.receive_foreground_results();

        assert!(app.inspect.pending_commit_details.is_none());
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.sha,
            target.commit.sha
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revisiting_a_commit_serves_its_details_from_the_cache_without_a_read() {
        let root = temp_repo("details-cache");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "latest\n").unwrap();
        git(&root, &["commit", "-am", "Latest"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Commits;
        let (request_rx, result_tx) = intercept_foreground(&mut app);

        app.move_selection(1);
        let ForegroundRequest::CommitDetails {
            id,
            generation,
            target,
        } = request_rx.try_recv().expect("first read of Base")
        else {
            panic!("unexpected request")
        };
        assert_eq!(target.commit.subject, "Base");
        let repository = Repository::discover(&root).unwrap();
        result_tx
            .send(ForegroundResult::CommitDetails {
                id,
                generation,
                result: Box::new(Ok(repository.details(&target.commit).unwrap())),
                target: target.clone(),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.sha,
            target.commit.sha
        );

        app.move_selection(-1);
        assert!(
            request_rx.try_recv().is_err(),
            "the details loaded at startup are cached"
        );
        assert!(app.inspect.pending_commit_details.is_none());
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.subject,
            "Latest"
        );

        app.move_selection(1);
        assert!(
            request_rx.try_recv().is_err(),
            "a revisited commit needs no read"
        );
        assert!(app.inspect.pending_commit_details.is_none());
        let details = app.inspect.details.as_ref().unwrap();
        assert_eq!(details.commit.sha, target.commit.sha);
        assert_eq!(details.changes.len(), 1);
        assert_eq!(app.inspect.details_cache.len(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn details_cache_keeps_sixty_four_commits_and_reports_the_current_refs() {
        let commit = |index: usize| Commit {
            sha: format!("{index:040x}"),
            parents: Vec::new(),
            author_name: "Ada".to_owned(),
            author_email: "ada@example.com".to_owned(),
            author_time: "2026-09-02T15:28:00+09:00".to_owned(),
            refs: Vec::new(),
            subject: format!("commit {index}"),
            body: String::new(),
        };
        let mut cache = DetailsCache::default();
        for index in 0..65 {
            cache.remember(&CommitDetails {
                commit: commit(index),
                changes: vec![ChangedPath {
                    status: "M".to_owned(),
                    path: format!("file-{index}.rs"),
                }],
            });
        }

        assert_eq!(cache.len(), 64);
        assert!(cache.details_for(&commit(0)).is_none());
        let mut tagged = commit(64);
        tagged.refs.push(CommitRef {
            name: "v1".to_owned(),
            kind: CommitRefKind::Tag,
        });
        let details = cache.details_for(&tagged).unwrap();
        assert_eq!(details.commit.refs.len(), 1);
        assert_eq!(details.changes[0].path, "file-64.rs");
        cache.remember(&CommitDetails {
            commit: commit(64),
            changes: Vec::new(),
        });
        assert_eq!(cache.len(), 64, "a remembered sha replaces its entry");
        assert!(cache.details_for(&commit(64)).unwrap().changes.is_empty());
    }

    #[test]
    fn reopening_a_commit_preview_reuses_its_document_without_a_read() {
        let root = temp_repo("preview-cache");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Add tracked file"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.shell.active_tab = ActiveTab::History;
        app.focus = PaneFocus::Details;
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        render(&mut app, 160, 48);

        press(&mut app, KeyCode::Char(' '));
        let ForegroundRequest::Diff {
            id,
            generation,
            owner,
            path,
            file,
            target,
            fold_toggles,
        } = request_rx.try_recv().expect("first preview read")
        else {
            panic!("unexpected request")
        };
        assert_eq!(owner, DiffOwner::CommitPreview);
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
                    "preview patch".to_owned(),
                    HighlightedDiff {
                        language: "Text".to_owned(),
                        split: test_diff_document(
                            vec![Line::raw("before")],
                            vec![Line::raw("after")],
                        ),
                        key: 0,
                    },
                )),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(
            app.inspect.commit_preview_loaded_path.as_deref(),
            Some("tracked.txt")
        );
        assert!(app.inspect.commit_preview_loaded_target.is_some());

        press(&mut app, KeyCode::Char(' '));
        assert!(app.inspect.commit_preview_path.is_none());
        assert!(app.inspect.commit_preview_loaded_target.is_none());
        assert_eq!(app.inspect.preview_cache.len(), 1);

        press(&mut app, KeyCode::Char(' '));
        assert_eq!(
            app.inspect.commit_preview_path.as_deref(),
            Some("tracked.txt")
        );
        assert!(
            request_rx.try_recv().is_err(),
            "a cached preview needs no read"
        );
        assert!(app.diff.pending_diff.is_none());
        assert_eq!(
            app.inspect.commit_preview_loaded_path.as_deref(),
            Some("tracked.txt")
        );
        assert_eq!(
            app.inspect
                .commit_preview_document
                .after_line(0)
                .unwrap()
                .to_string(),
            "after"
        );
        assert_eq!(app.inspect.commit_preview_language, "Text");
        assert_eq!(
            app.inspect.preview_cache.len(),
            0,
            "the shown document lives in the preview, not the cache"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
