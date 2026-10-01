use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::git::{ChangeSection, ReadError};

use super::diff::{emphasize_diff_line, horizontal_line_slice, padded_diff_lines};
use super::effect::{FilePreview, ForegroundRequest, ReadGeneration, RepositoryFiles, RequestId};
use super::files::{status_label, status_style};
use super::overlay::Overlay;
use super::path_tree::{PathRow, PathRowKind};
use super::review::{ReviewSide, SelectionSurface};
use super::shell::{ActiveTab, PaneFocus};
use super::syntax::DiffDocument;
use super::widgets::{
    self, ListCursor, TextEdit, TextField, chord, counted_title, scrolled_content_row_at,
    viewport_offset,
};
use super::{App, theme};
use search::{MatchType, SearchResult, SearchService};

mod search;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);

const PREVIEW_WHEEL_LINES: usize = 3;
const PREVIEW_HORIZONTAL_STEP: usize = 4;

#[derive(Default)]
pub(super) struct ExplorerState {
    root: Option<PathBuf>,
    files: Option<RepositoryFiles>,
    expanded: HashSet<String>,
    rows: Vec<PathRow>,
    selected: usize,
    scroll: usize,
    list_read: Option<ListRead>,
    preview_read: Option<PreviewRead>,
    preview_file: Option<String>,
    preview: FilePreview,
    preview_scroll: usize,
    preview_horizontal_scroll: usize,
    error: Option<String>,
    filter: ExplorerFilter,
    filter_area: Rect,
    search: Option<SearchService>,
    reveal_match: bool,
    list_horizontal_scroll: usize,
    tree_pane: Rect,
    tree_area: Rect,
    preview_area: Rect,
}

#[derive(Default)]
struct ExplorerFilter {
    query: TextField,
    cursor: ListCursor,
    results: Vec<SearchResult>,
    generation: Option<u64>,
    due: Option<Instant>,
    searching: bool,
    error: Option<String>,
}

impl ExplorerFilter {
    fn applied(&self) -> bool {
        !self.query.text.trim().is_empty()
    }

    fn selected(&self) -> Option<&SearchResult> {
        self.results.get(self.cursor.selected)
    }

    fn move_by(&mut self, delta: isize) {
        self.cursor.move_by(delta, self.results.len());
    }
}

struct ListRead {
    id: RequestId,
    generation: ReadGeneration,
    root: PathBuf,
}

struct PreviewRead {
    id: RequestId,
    generation: ReadGeneration,
    root: PathBuf,
    file: String,
}

impl ExplorerState {
    fn selected_row(&self) -> Option<&PathRow> {
        self.rows.get(self.selected)
    }

    fn file_path(&self, index: usize) -> Option<&str> {
        self.files.as_ref()?.paths.get(index).map(String::as_str)
    }

    fn selected_file(&self) -> Option<&str> {
        match self.selected_row()?.kind {
            PathRowKind::File { index } => self.file_path(index),
            PathRowKind::Directory { .. } => None,
        }
    }

    fn row_identity(&self, row: &PathRow) -> Option<String> {
        match &row.kind {
            PathRowKind::Directory { key, .. } => Some(key.clone()),
            PathRowKind::File { index } => self.file_path(*index).map(str::to_owned),
        }
    }

    fn rebuild_rows(&mut self) {
        let selected = self.selected_row().and_then(|row| self.row_identity(row));
        let mut rows = Vec::new();
        if let Some(files) = &self.files {
            files
                .tree
                .emit(0, "", &|key| self.expanded.contains(key), &mut rows);
        }
        self.rows = rows;
        let position = selected.and_then(|identity| {
            self.rows
                .iter()
                .position(|row| self.row_identity(row).as_deref() == Some(identity.as_str()))
        });
        self.selected = position.unwrap_or(self.selected.min(self.rows.len().saturating_sub(1)));
    }

    pub(super) fn preview_document(&self) -> &DiffDocument {
        &self.preview.document
    }

    fn active_file(&self) -> Option<&str> {
        if self.filter.applied() {
            self.filter.selected().map(|result| result.path.as_str())
        } else {
            self.selected_file()
        }
    }

    fn active_match(&self) -> Option<&SearchResult> {
        self.filter
            .applied()
            .then(|| self.filter.selected())
            .flatten()
            .filter(|result| result.match_type == MatchType::Content)
    }

    pub(super) fn preview_path(&self) -> Option<&str> {
        self.preview_file
            .as_deref()
            .filter(|file| self.active_file() == Some(*file))
    }

    pub(super) fn search_pending(&self) -> bool {
        self.filter.due.is_some() || self.filter.searching
    }

    fn reset(&mut self, root: Option<PathBuf>) {
        *self = Self {
            root,
            ..Self::default()
        };
    }
}

impl App {
    fn repository_root(&self) -> Option<PathBuf> {
        self.repository
            .as_ref()
            .map(|repository| repository.root().to_owned())
    }

    pub(super) fn request_repository_files(&mut self) {
        let root = self.repository_root();
        if self.explorer.root != root {
            self.explorer.reset(root.clone());
            self.clear_file_selection();
        }
        let Some(root) = root else {
            return;
        };
        self.foreground.reads.repository_files.advance();
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.repository_files.generation;
        let request = ForegroundRequest::RepositoryFiles {
            id,
            generation,
            root: root.clone(),
        };
        match self.foreground.request(request) {
            Ok(_) => {
                self.explorer.list_read = Some(ListRead {
                    id,
                    generation,
                    root,
                });
            }
            Err(error) => self.explorer.error = Some(format!("Git worker stopped: {error}")),
        }
    }

    pub(super) fn apply_repository_files(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        root: PathBuf,
        result: Result<RepositoryFiles, ReadError>,
    ) {
        let claimed = self.explorer.list_read.as_ref().is_some_and(|read| {
            read.id == id && read.generation == generation && read.root == root
        });
        if claimed {
            self.explorer.list_read = None;
        }
        if !claimed
            || !self
                .foreground
                .reads
                .repository_files
                .is_current(generation)
            || self.explorer.root.as_deref() != Some(root.as_path())
        {
            return;
        }
        match result {
            Ok(files) => {
                self.explorer.files = Some(files);
                self.explorer.error = None;
                self.explorer.rebuild_rows();
                self.reload_selected_preview();
            }
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => self.explorer.error = Some(error),
        }
    }

    fn reload_selected_preview(&mut self) {
        match self.explorer.selected_file().map(str::to_owned) {
            Some(file) => self.request_file_preview(file),
            None => self.clear_file_preview(),
        }
    }

    fn clear_file_preview(&mut self) {
        self.foreground.reads.file_preview.advance();
        self.explorer.preview_read = None;
        self.explorer.preview_file = None;
        self.explorer.preview = FilePreview::default();
        self.clear_file_selection();
    }

    fn working_status(&self, path: &str) -> Option<&str> {
        self.files
            .changes
            .iter()
            .find(|change| change.section != ChangeSection::Commit && change.path == path)
            .map(|change| change.status.as_str())
    }

    fn request_file_preview(&mut self, file: String) {
        let Some(root) = self.explorer.root.clone() else {
            return;
        };
        if self.explorer.preview_file.as_ref() != Some(&file) {
            self.explorer.preview_file = None;
            self.explorer.preview = FilePreview::default();
            self.explorer.preview_scroll = 0;
            self.explorer.preview_horizontal_scroll = 0;
            self.clear_file_selection();
        }
        self.foreground.reads.file_preview.advance();
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.file_preview.generation;
        let new_file = self
            .working_status(&file)
            .is_some_and(|status| status.starts_with(['?', 'A']));
        let request = ForegroundRequest::FilePreview {
            id,
            generation,
            root: root.clone(),
            file: file.clone(),
            new_file,
        };
        match self.foreground.request(request) {
            Ok(_) => {
                self.explorer.preview_read = Some(PreviewRead {
                    id,
                    generation,
                    root,
                    file,
                });
            }
            Err(error) => self.explorer.error = Some(format!("Git worker stopped: {error}")),
        }
    }

    pub(super) fn apply_file_preview(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        root: PathBuf,
        file: String,
        result: Result<FilePreview, ReadError>,
    ) {
        let claimed = self.explorer.preview_read.as_ref().is_some_and(|read| {
            read.id == id && read.generation == generation && read.root == root && read.file == file
        });
        if claimed {
            self.explorer.preview_read = None;
        }
        if !claimed
            || !self.foreground.reads.file_preview.is_current(generation)
            || self.explorer.root.as_deref() != Some(root.as_path())
            || self.explorer.active_file() != Some(file.as_str())
        {
            return;
        }
        let preview = match result {
            Ok(preview) => preview,
            Err(ReadError::Cancelled) => return,
            Err(ReadError::Diagnostic(error)) => FilePreview::notice(error),
        };
        self.explorer.preview_file = Some(file);
        self.explorer.preview = preview;
    }

    fn select_explorer_row(&mut self, index: usize) {
        if self.explorer.rows.is_empty() {
            return;
        }
        self.explorer.selected = index.min(self.explorer.rows.len() - 1);
        if let Some(file) = self.explorer.selected_file().map(str::to_owned) {
            self.show_file_preview(file);
        }
    }

    fn show_file_preview(&mut self, file: String) {
        let shown = self.explorer.preview_file.as_ref() == Some(&file);
        let loading = self
            .explorer
            .preview_read
            .as_ref()
            .is_some_and(|read| read.file == file);
        if !shown && !loading {
            self.request_file_preview(file);
        }
    }

    pub(super) fn open_file_filter(&mut self) {
        if self.repository.is_none() {
            return;
        }
        self.explorer.filter.query.cursor_started = Instant::now();
        self.overlay = Overlay::FileFilter;
    }

    fn clear_file_filter(&mut self) {
        if let Some(search) = &self.explorer.search {
            search.cancel();
        }
        self.explorer.filter = ExplorerFilter::default();
        self.explorer.reveal_match = false;
        self.focus = PaneFocus::Explorer;
        if let Some(file) = self.explorer.selected_file().map(str::to_owned) {
            self.show_file_preview(file);
        }
    }

    fn edit_file_filter(&mut self, edit: TextEdit) {
        let filter = &mut self.explorer.filter;
        if !filter.query.edit(edit) {
            return;
        }
        if filter.applied() {
            filter.cursor = ListCursor::default();
            filter.due = Some(Instant::now() + SEARCH_DEBOUNCE);
            filter.searching = true;
        } else {
            let query = filter.query.clone();
            self.clear_file_filter();
            self.explorer.filter.query = query;
        }
    }

    fn move_filter_selection(&mut self, delta: isize) {
        self.explorer.filter.move_by(delta);
        self.show_selected_result();
    }

    pub(super) fn handle_file_filter(&mut self, input: &Event) -> bool {
        let page = isize::try_from(self.explorer.tree_area.height.max(1)).unwrap_or(isize::MAX);
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match key.code {
                    KeyCode::Esc => {
                        self.overlay = Overlay::None;
                        self.clear_file_filter();
                    }
                    KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.focus = PaneFocus::Explorer;
                    }
                    KeyCode::Down => self.move_filter_selection(1),
                    KeyCode::Up => self.move_filter_selection(-1),
                    KeyCode::PageDown => self.move_filter_selection(page),
                    KeyCode::PageUp => self.move_filter_selection(-page),
                    KeyCode::Backspace => self.edit_file_filter(TextEdit::Backspace),
                    KeyCode::Char(character) if !chord(key.modifiers) => {
                        self.edit_file_filter(TextEdit::Insert(character));
                    }
                    _ => {}
                }
                true
            }
            Event::Key(_) => true,
            Event::Paste(text) => {
                self.edit_file_filter(TextEdit::Paste(text.clone()));
                true
            }
            _ => false,
        }
    }

    fn show_selected_result(&mut self) {
        let Some(result) = self.explorer.filter.selected().cloned() else {
            return;
        };
        self.explorer.reveal_match = result.line.is_some();
        self.show_file_preview(result.path);
    }

    pub(super) fn tick_explorer_search(&mut self) -> bool {
        let mut changed = self.start_due_search();
        let mut show = false;
        while let Some(update) = self
            .explorer
            .search
            .as_ref()
            .and_then(SearchService::try_recv)
        {
            let filter = &mut self.explorer.filter;
            if filter.generation != Some(update.generation)
                || self.explorer.root.as_ref() != Some(&update.root)
            {
                continue;
            }
            let selected = filter.selected().cloned();
            filter.results = update.results;
            filter.error = update.error;
            filter.searching = !update.done;
            filter.cursor.selected = selected
                .and_then(|selected| filter.results.iter().position(|result| *result == selected))
                .unwrap_or(
                    filter
                        .cursor
                        .selected
                        .min(filter.results.len().saturating_sub(1)),
                );
            changed = true;
            show = true;
        }
        if show {
            self.show_selected_result();
        }
        changed
    }

    fn start_due_search(&mut self) -> bool {
        let filter = &mut self.explorer.filter;
        if filter.due.is_none_or(|due| Instant::now() < due) {
            return false;
        }
        filter.due = None;
        let query = filter.query.text.clone();
        let Some(root) = self
            .explorer
            .root
            .clone()
            .filter(|_| !query.trim().is_empty())
        else {
            if let Some(search) = &self.explorer.search {
                search.cancel();
            }
            *filter = ExplorerFilter {
                query: filter.query.clone(),
                ..ExplorerFilter::default()
            };
            return true;
        };
        if self.explorer.search.is_none() {
            match SearchService::start() {
                Ok(search) => self.explorer.search = Some(search),
                Err(error) => {
                    filter.error = Some(error);
                    filter.searching = false;
                    return true;
                }
            }
        }
        let search = self
            .explorer
            .search
            .as_ref()
            .expect("search service started");
        match search.search(root, query) {
            Ok(generation) => filter.generation = Some(generation),
            Err(error) => {
                filter.error = Some(error);
                filter.searching = false;
            }
        }
        true
    }

    fn move_explorer_selection(&mut self, delta: isize) {
        self.select_explorer_row(self.explorer.selected.saturating_add_signed(delta));
    }

    fn set_explorer_directory(&mut self, key: &str, expanded: bool) {
        if expanded {
            self.explorer.expanded.insert(key.to_owned());
        } else {
            self.explorer.expanded.remove(key);
        }
        self.explorer.rebuild_rows();
    }

    fn expand_or_enter(&mut self) {
        match self.explorer.selected_row().map(|row| row.kind.clone()) {
            Some(PathRowKind::Directory {
                key,
                expanded: false,
            }) => self.set_explorer_directory(&key, true),
            Some(PathRowKind::Directory { expanded: true, .. }) => {
                self.move_explorer_selection(1);
            }
            Some(PathRowKind::File { .. }) => self.focus = PaneFocus::FilePreview,
            None => {}
        }
    }

    fn collapse_or_leave(&mut self) {
        let Some(row) = self.explorer.selected_row().cloned() else {
            return;
        };
        if let PathRowKind::Directory {
            key,
            expanded: true,
        } = &row.kind
        {
            self.set_explorer_directory(key, false);
            return;
        }
        if let Some(parent) = row.depth.checked_sub(1).and_then(|depth| {
            self.explorer.rows[..self.explorer.selected]
                .iter()
                .rposition(|candidate| candidate.depth == depth)
        }) {
            self.select_explorer_row(parent);
        }
    }

    fn activate_explorer_row(&mut self) {
        match self.explorer.selected_row().map(|row| row.kind.clone()) {
            Some(PathRowKind::Directory { key, expanded }) => {
                self.set_explorer_directory(&key, !expanded);
            }
            Some(PathRowKind::File { .. }) => self.focus = PaneFocus::FilePreview,
            None => {}
        }
    }

    pub(super) fn explorer_wheel_moves_selection(&self, mouse: &MouseEvent) -> bool {
        self.shell.active_tab == ActiveTab::Files
            && matches!(
                mouse.kind,
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
            )
            && !mouse.modifiers.contains(KeyModifiers::SHIFT)
            && self
                .explorer
                .tree_area
                .contains((mouse.column, mouse.row).into())
    }

    fn explorer_list_width(&self) -> usize {
        if self.explorer.filter.applied() {
            self.explorer
                .filter
                .results
                .iter()
                .map(|result| search_result_line(result).width())
                .max()
                .unwrap_or_default()
        } else {
            self.explorer
                .rows
                .iter()
                .map(|row| row.depth * 2 + 2 + Line::raw(row.label.as_str()).width())
                .max()
                .unwrap_or_default()
        }
    }

    fn max_explorer_horizontal_scroll(&self) -> usize {
        let viewport = usize::from(self.explorer.tree_area.width.saturating_sub(2));
        self.explorer_list_width().saturating_sub(viewport)
    }

    fn scroll_explorer_horizontal(&mut self, delta: isize) {
        self.explorer.list_horizontal_scroll = self
            .explorer
            .list_horizontal_scroll
            .saturating_add_signed(delta)
            .min(self.max_explorer_horizontal_scroll());
    }

    fn explorer_page(&self) -> isize {
        isize::try_from(self.explorer.tree_area.height.max(1)).unwrap_or(isize::MAX)
    }

    fn preview_page(&self) -> usize {
        usize::from(self.explorer.preview_area.height.max(1))
    }

    fn scroll_preview(&mut self, delta: isize) {
        self.explorer.preview_scroll = self.explorer.preview_scroll.saturating_add_signed(delta);
    }

    fn scroll_preview_horizontal(&mut self, delta: isize) {
        self.explorer.preview_horizontal_scroll = self
            .explorer
            .preview_horizontal_scroll
            .saturating_add_signed(delta);
    }

    fn file_preview_row_at(&self, pointer: (u16, u16)) -> Option<usize> {
        scrolled_content_row_at(
            Some(pointer),
            self.explorer.preview_area,
            self.explorer.preview.document.len(),
            self.explorer.preview_scroll,
        )
    }

    fn click_file_preview(&mut self, pointer: (u16, u16)) {
        if self.review.file.keyboard_selecting {
            self.finish_visual_selection();
            return;
        }
        if let Some(row) = self.file_preview_row_at(pointer)
            && self
                .surface_source(SelectionSurface::File, ReviewSide::After, row)
                .is_some()
        {
            self.begin_visual_selection(ReviewSide::After, row, false);
        }
    }

    fn begin_file_visual_selection(&mut self) {
        let start =
            (self.explorer.preview_scroll..self.explorer.preview.document.len()).find(|&row| {
                self.surface_source(SelectionSurface::File, ReviewSide::After, row)
                    .is_some()
            });
        match start {
            Some(row) => self.begin_visual_selection(ReviewSide::After, row, true),
            None => self.show_action_error("The preview has no source line to select."),
        }
    }

    fn move_file_cursor(&mut self, delta: isize) {
        let last = self.explorer.preview.document.len().saturating_sub(1);
        let row = self
            .review
            .file_cursor
            .saturating_add_signed(delta)
            .min(last);
        self.review.file_cursor = row;
        let height = self.preview_page();
        if row < self.explorer.preview_scroll {
            self.explorer.preview_scroll = row;
        } else if row >= self.explorer.preview_scroll + height {
            self.explorer.preview_scroll = row + 1 - height;
        }
    }

    pub(super) fn handle_explorer_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('/')
            && matches!(self.focus, PaneFocus::Explorer | PaneFocus::FilePreview)
        {
            self.open_file_filter();
            return true;
        }
        match self.focus {
            PaneFocus::Explorer if self.explorer.filter.applied() => {
                let page = self.explorer_page();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => self.move_filter_selection(1),
                    KeyCode::Up | KeyCode::Char('k') => self.move_filter_selection(-1),
                    KeyCode::PageDown => self.move_filter_selection(page),
                    KeyCode::PageUp => self.move_filter_selection(-page),
                    KeyCode::Home => self.move_filter_selection(isize::MIN),
                    KeyCode::End => self.move_filter_selection(isize::MAX),
                    KeyCode::Enter if self.explorer.active_file().is_some() => {
                        self.focus = PaneFocus::FilePreview;
                    }
                    KeyCode::Esc => self.clear_file_filter(),
                    _ => return false,
                }
            }
            PaneFocus::Explorer => match key.code {
                KeyCode::Down | KeyCode::Char('j') => self.move_explorer_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_explorer_selection(-1),
                KeyCode::PageDown => self.move_explorer_selection(self.explorer_page()),
                KeyCode::PageUp => self.move_explorer_selection(-self.explorer_page()),
                KeyCode::Home => self.select_explorer_row(0),
                KeyCode::End => self.select_explorer_row(usize::MAX),
                KeyCode::Right => self.expand_or_enter(),
                KeyCode::Left => self.collapse_or_leave(),
                KeyCode::Enter => self.activate_explorer_row(),
                KeyCode::Char(' ') => {
                    if let Some(PathRowKind::Directory { key, expanded }) =
                        self.explorer.selected_row().map(|row| row.kind.clone())
                    {
                        self.set_explorer_directory(&key, !expanded);
                    }
                }
                _ => return false,
            },
            PaneFocus::FilePreview => {
                let page = isize::try_from(self.preview_page()).unwrap_or(isize::MAX);
                let step = PREVIEW_HORIZONTAL_STEP as isize;
                let selecting = self.review.file.keyboard_selecting;
                match key.code {
                    KeyCode::Char('v') if !selecting => self.begin_file_visual_selection(),
                    KeyCode::Down | KeyCode::Char('j') if selecting => self.move_file_cursor(1),
                    KeyCode::Up | KeyCode::Char('k') if selecting => self.move_file_cursor(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.scroll_preview(1),
                    KeyCode::Up | KeyCode::Char('k') => self.scroll_preview(-1),
                    KeyCode::PageDown => self.scroll_preview(page),
                    KeyCode::PageUp => self.scroll_preview(-page),
                    KeyCode::Home => self.explorer.preview_scroll = 0,
                    KeyCode::End => self.explorer.preview_scroll = usize::MAX,
                    KeyCode::Char('h')
                        if self
                            .effective_selection_on(SelectionSurface::File)
                            .is_some() =>
                    {
                        return false;
                    }
                    KeyCode::Right | KeyCode::Char('l') => self.scroll_preview_horizontal(step),
                    KeyCode::Left | KeyCode::Char('h') => self.scroll_preview_horizontal(-step),
                    _ => return false,
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn handle_explorer_mouse(&mut self, mouse: MouseEvent) -> bool {
        let pointer = (mouse.column, mouse.row);
        let wheel = PREVIEW_WHEEL_LINES as isize;
        let step = PREVIEW_HORIZONTAL_STEP as isize;
        if self.explorer.filter_area.contains(pointer.into())
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.open_file_filter();
            return true;
        }
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        if self.explorer.tree_area.contains(pointer.into()) {
            match mouse.kind {
                MouseEventKind::ScrollRight => {
                    self.scroll_explorer_horizontal(1);
                    return true;
                }
                MouseEventKind::ScrollLeft => {
                    self.scroll_explorer_horizontal(-1);
                    return true;
                }
                MouseEventKind::ScrollDown if shift => {
                    self.scroll_explorer_horizontal(1);
                    return true;
                }
                MouseEventKind::ScrollUp if shift => {
                    self.scroll_explorer_horizontal(-1);
                    return true;
                }
                _ => {}
            }
        }
        if self.explorer.filter.applied() && self.explorer.tree_area.contains(pointer.into()) {
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.focus = PaneFocus::Explorer;
                    self.move_filter_selection(1);
                }
                MouseEventKind::ScrollUp => {
                    self.focus = PaneFocus::Explorer;
                    self.move_filter_selection(-1);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.focus = PaneFocus::Explorer;
                    let rows = result_rows(&self.explorer.filter.results);
                    if let Some(ResultRow::Result(index)) = scrolled_content_row_at(
                        Some(pointer),
                        self.explorer.tree_area,
                        rows.len(),
                        self.explorer.filter.cursor.scroll,
                    )
                    .and_then(|row| rows.get(row))
                    {
                        self.explorer.filter.cursor.selected = *index;
                        self.show_selected_result();
                    }
                }
                _ => return false,
            }
            return true;
        }
        if self.explorer.tree_area.contains(pointer.into()) {
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.focus = PaneFocus::Explorer;
                    self.move_explorer_selection(1);
                }
                MouseEventKind::ScrollUp => {
                    self.focus = PaneFocus::Explorer;
                    self.move_explorer_selection(-1);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.focus = PaneFocus::Explorer;
                    if let Some(index) = scrolled_content_row_at(
                        Some(pointer),
                        self.explorer.tree_area,
                        self.explorer.rows.len(),
                        self.explorer.scroll,
                    ) {
                        self.select_explorer_row(index);
                        if let Some(PathRowKind::Directory { key, expanded }) =
                            self.explorer.selected_row().map(|row| row.kind.clone())
                        {
                            self.set_explorer_directory(&key, !expanded);
                        }
                    }
                }
                _ => return false,
            }
            return true;
        }
        if self.explorer.preview_area.contains(pointer.into()) {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.scroll_preview(wheel),
                MouseEventKind::ScrollUp => self.scroll_preview(-wheel),
                MouseEventKind::ScrollRight => self.scroll_preview_horizontal(step),
                MouseEventKind::ScrollLeft => self.scroll_preview_horizontal(-step),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.focus = PaneFocus::FilePreview;
                    self.click_file_preview(pointer);
                }
                MouseEventKind::Drag(MouseButton::Left) if self.review.file.dragging_selection => {
                    if let Some(row) = self.file_preview_row_at(pointer) {
                        self.review.file_cursor = row;
                    }
                }
                _ => return false,
            }
            return true;
        }
        false
    }

    pub(super) fn draw_explorer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let wide = area.width >= 90;
        let panes = Layout::default()
            .direction(if wide {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints(if wide {
                [Constraint::Percentage(35), Constraint::Percentage(65)]
            } else {
                [Constraint::Percentage(50), Constraint::Percentage(50)]
            })
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(panes[0]);
        if self.explorer.filter.applied() {
            self.draw_filter_results(frame, left[0]);
        } else {
            self.draw_explorer_tree(frame, left[0]);
        }
        self.draw_workspaces(frame, left[1]);
        self.draw_file_preview(frame, panes[1]);
    }

    pub(super) fn draw_file_filter(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.explorer.filter_area = area;
        widgets::draw_filter_bar(
            frame,
            area,
            &self.explorer.filter.query,
            matches!(self.overlay, Overlay::FileFilter),
        );
    }

    fn draw_filter_results(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.explorer.tree_pane = area;
        let focused = self.focus == PaneFocus::Explorer;
        let list = Block::default().borders(Borders::ALL).inner(area);
        self.explorer.tree_area = list;
        self.explorer.list_horizontal_scroll = self
            .explorer
            .list_horizontal_scroll
            .min(self.max_explorer_horizontal_scroll());
        let horizontal = self.explorer.list_horizontal_scroll;
        let viewport = usize::from(list.width.saturating_sub(2));
        let filter = &mut self.explorer.filter;
        let mut title = counted_title("Matches", filter.results.len());
        if filter.searching {
            title.push_str(" · Searching…");
        }
        frame.render_widget(widgets::pane_block(title, focused), area);
        let message = if let Some(error) = &filter.error {
            Some((error.as_str(), theme::error_text()))
        } else if filter.results.is_empty() && filter.searching {
            Some(("Searching…", theme::hint()))
        } else if filter.results.is_empty() {
            Some(("No matches", theme::hint()))
        } else {
            None
        };
        if let Some((text, style)) = message {
            frame.render_widget(
                Paragraph::new(text.to_owned())
                    .style(style)
                    .wrap(Wrap { trim: false }),
                list,
            );
            return;
        }
        let height = usize::from(list.height);
        let rows = result_rows(&filter.results);
        let selected_row = rows
            .iter()
            .position(|row| *row == ResultRow::Result(filter.cursor.selected))
            .unwrap_or_default();
        let mut offset = viewport_offset(filter.cursor.scroll, selected_row, rows.len(), height);
        if offset == selected_row
            && let Some(ResultRow::Group(..)) = selected_row.checked_sub(1).map(|row| rows[row])
        {
            offset -= 1;
        }
        filter.cursor.scroll = offset;
        let hovered = scrolled_content_row_at(self.shell.mouse_position, list, rows.len(), offset)
            .filter(|row| matches!(rows[*row], ResultRow::Result(_)));
        let items = rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(index, row)| {
                let line = match *row {
                    ResultRow::Group(label, count) => {
                        Line::styled(counted_title(label, count), theme::section_header())
                    }
                    ResultRow::Result(result) => search_result_line(&filter.results[result]),
                };
                ListItem::new(horizontal_line_slice(line, horizontal, viewport))
                    .style(theme::hover(Style::default(), hovered == Some(index)))
            });
        let selected = selected_row.checked_sub(offset).filter(|row| *row < height);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(theme::focus_row())
                .highlight_symbol(theme::LIST_MARKER),
            list,
            &mut state,
        );
    }

    fn draw_explorer_tree(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.explorer.tree_pane = area;
        let inner = Block::default().borders(Borders::ALL).inner(area);
        self.explorer.tree_area = inner;
        self.explorer.list_horizontal_scroll = self
            .explorer
            .list_horizontal_scroll
            .min(self.max_explorer_horizontal_scroll());
        let horizontal = self.explorer.list_horizontal_scroll;
        let viewport = usize::from(inner.width.saturating_sub(2));
        let count = self
            .explorer
            .files
            .as_ref()
            .map_or(0, |files| files.paths.len());
        frame.render_widget(
            widgets::pane_block(
                counted_title("Files", count),
                self.focus == PaneFocus::Explorer,
            ),
            area,
        );
        let message = if self.repository.is_none() {
            Some(("Not a Git repository", theme::warning_text()))
        } else if let Some(error) = &self.explorer.error {
            Some((error.as_str(), theme::error_text()))
        } else if self.explorer.files.is_none() {
            Some(("Loading files…", theme::hint()))
        } else if self.explorer.rows.is_empty() {
            Some(("No files", theme::hint()))
        } else {
            None
        };
        if let Some((text, style)) = message {
            frame.render_widget(
                Paragraph::new(text.to_owned())
                    .style(style)
                    .wrap(Wrap { trim: false }),
                inner,
            );
            return;
        }
        let height = usize::from(inner.height);
        self.explorer.scroll = viewport_offset(
            self.explorer.scroll,
            self.explorer.selected,
            self.explorer.rows.len(),
            height,
        );
        let offset = self.explorer.scroll;
        let hovered = scrolled_content_row_at(
            self.shell.mouse_position,
            inner,
            self.explorer.rows.len(),
            offset,
        );
        let mut statuses = HashMap::new();
        for change in &self.files.changes {
            if change.section == ChangeSection::Commit {
                continue;
            }
            let status = change.status.as_str();
            statuses.entry(change.path.as_str()).or_insert(status);
            for (end, _) in change.path.match_indices('/') {
                statuses
                    .entry(&change.path[..end])
                    .and_modify(|existing: &mut &str| {
                        if status_label(existing) != status_label(status) {
                            *existing = "M";
                        }
                    })
                    .or_insert(status);
            }
        }
        let explorer = &self.explorer;
        let items = explorer
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(index, row)| {
                let path = match &row.kind {
                    PathRowKind::Directory { key, .. } => Some(key.as_str()),
                    PathRowKind::File { index } => explorer.file_path(*index),
                };
                let status = path.and_then(|path| statuses.get(path)).copied();
                ListItem::new(horizontal_line_slice(
                    explorer_line(row, status),
                    horizontal,
                    viewport,
                ))
                .style(theme::hover(Style::default(), hovered == Some(index)))
            });
        let selected = self
            .explorer
            .selected
            .checked_sub(offset)
            .filter(|row| *row < height);
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(theme::focus_row())
                .highlight_symbol(theme::LIST_MARKER),
            inner,
            &mut state,
        );
    }

    fn draw_file_preview(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        self.explorer.preview_area = inner;
        let selected = self.explorer.active_file().map(str::to_owned);
        let title = self
            .selection_title(SelectionSurface::File)
            .unwrap_or_else(|| Line::raw(selected.clone().unwrap_or_else(|| "Preview".to_owned())));
        frame.render_widget(
            widgets::pane_block(title, self.focus == PaneFocus::FilePreview),
            area,
        );
        if self.explorer.preview_path().is_none() {
            let text = if selected.is_some() {
                "Loading…"
            } else {
                "Select a file to preview it"
            };
            frame.render_widget(Paragraph::new(text).style(theme::hint()), inner);
            return;
        }
        if let Some(notice) = &self.explorer.preview.notice {
            frame.render_widget(
                Paragraph::new(notice.clone())
                    .style(theme::hint())
                    .wrap(Wrap { trim: false }),
                inner,
            );
            return;
        }
        let height = usize::from(inner.height);
        let width = usize::from(inner.width);
        let target = self.explorer.active_match().and_then(|result| {
            let line = usize::try_from(result.line?).ok()?;
            let row = self.explorer.preview.document.after_row_for_line(line)?;
            let source = self.explorer.preview.document.after_source(row)?;
            let gutter = format!("{line:>4} ").len();
            let columns = display_columns(source, result.match_range.as_ref()?);
            Some((row, gutter + columns.start..gutter + columns.end))
        });
        if self.explorer.reveal_match
            && let Some((row, columns)) = &target
        {
            self.explorer.reveal_match = false;
            self.explorer.preview_scroll = row.saturating_sub(height / 3);
            self.explorer.preview_horizontal_scroll = if columns.end > width {
                columns.start.saturating_sub(width / 4)
            } else {
                0
            };
        }
        let document = &self.explorer.preview.document;
        self.explorer.preview_scroll = self
            .explorer
            .preview_scroll
            .min(document.len().saturating_sub(height));
        self.explorer.preview_horizontal_scroll = self
            .explorer
            .preview_horizontal_scroll
            .min(document.after_max_width().saturating_sub(width));
        let scroll = self.explorer.preview_scroll;
        let horizontal = self.explorer.preview_horizontal_scroll;
        let document = &self.explorer.preview.document;
        let mut lines = padded_diff_lines(
            (scroll..)
                .map_while(|row| document.after_line(row))
                .take(height),
            width,
            horizontal,
        );
        if let Some(selection) = self.effective_selection_on(SelectionSurface::File) {
            for (offset, line) in lines.iter_mut().enumerate() {
                if selection.contains(scroll + offset) {
                    emphasize_diff_line(line, true, theme::selection_row(), width);
                }
            }
        }
        if let Some((row, columns)) = target
            && let Some(line) = row
                .checked_sub(scroll)
                .and_then(|offset| lines.get_mut(offset))
        {
            style_columns(line, columns, theme::search_match());
        }
        frame.render_widget(
            Paragraph::new(lines).scroll((0, u16::try_from(horizontal).unwrap_or(u16::MAX))),
            inner,
        );
        self.draw_selection_decorations(
            frame,
            SelectionSurface::File,
            [Rect::default(), inner],
            scroll,
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultRow {
    Group(&'static str, usize),
    Result(usize),
}

fn result_rows(results: &[SearchResult]) -> Vec<ResultRow> {
    let lines = results
        .iter()
        .position(|result| result.match_type == MatchType::Content)
        .unwrap_or(results.len());
    let mut rows = Vec::with_capacity(results.len() + 2);
    for (label, range) in [("Files", 0..lines), ("Lines", lines..results.len())] {
        if !range.is_empty() {
            rows.push(ResultRow::Group(label, range.len()));
            rows.extend(range.map(ResultRow::Result));
        }
    }
    rows
}

fn search_result_line(result: &SearchResult) -> Line<'static> {
    let mut spans = vec![Span::raw(format!("  {}", result.path))];
    if let Some(line) = result.line {
        spans.push(Span::styled(format!(":{line}"), theme::hint()));
    }
    Line::from(spans)
}

fn display_columns(source: &str, range: &Range<usize>) -> Range<usize> {
    let mut column = 0;
    let mut start = None;
    let mut end = None;
    for (index, character) in source.char_indices() {
        if index == range.start {
            start = Some(column);
        }
        if index == range.end {
            end = Some(column);
        }
        column += if character == '\t' {
            8 - column % 8
        } else {
            character.width().unwrap_or_default()
        };
    }
    let start = start.unwrap_or(column);
    start..end.unwrap_or(column).max(start)
}

fn style_columns(line: &mut Line<'static>, columns: Range<usize>, style: Style) {
    let mut column = 0;
    let mut spans = Vec::new();
    for span in line.spans.drain(..) {
        let mut text = String::new();
        let mut inside = None;
        for character in span.content.chars() {
            let hit = columns.contains(&column);
            if inside.is_some_and(|previous| previous != hit) {
                let piece_style = if hit {
                    span.style
                } else {
                    span.style.patch(style)
                };
                spans.push(Span::styled(std::mem::take(&mut text), piece_style));
            }
            inside = Some(hit);
            text.push(character);
            column += character.width().unwrap_or_default();
        }
        if !text.is_empty() {
            let piece_style = if inside == Some(true) {
                span.style.patch(style)
            } else {
                span.style
            };
            spans.push(Span::styled(text, piece_style));
        }
    }
    line.spans = spans;
}

fn explorer_line(row: &PathRow, status: Option<&str>) -> Line<'static> {
    let indent = "  ".repeat(row.depth);
    let name_style = status.map_or_else(Style::default, |status| {
        Style::default().fg(status_style(status).fg.unwrap_or_default())
    });
    let marker = match (&row.kind, status) {
        (PathRowKind::Directory { expanded, .. }, _) => Span::styled(
            format!("{indent}{} ", if *expanded { "▾" } else { "▸" }),
            theme::hint(),
        ),
        (PathRowKind::File { .. }, Some(status)) => Span::styled(
            format!("{indent}{} ", status_label(status)),
            status_style(status),
        ),
        (PathRowKind::File { .. }, None) => Span::raw(format!("{indent}  ")),
    };
    Line::from(vec![marker, Span::styled(row.label.clone(), name_style)])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::super::effect::{ForegroundRequest, RepositoryFiles};
    use super::super::overlay::Overlay;
    use super::super::path_tree::PathTree;
    use super::super::review::SelectionSurface;
    use super::super::shell::{ActiveTab, PaneFocus};
    use super::super::test_support::{
        committed_change, find_text, find_text_in_row, intercept_foreground, press, render,
    };
    use super::super::{App, PendingClipboard, theme};

    fn wait_for_explorer(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while (app.explorer.list_read.is_some() || app.explorer.preview_read.is_some())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
            app.receive_foreground_results();
        }
        assert!(app.explorer.list_read.is_none(), "file list timed out");
        assert!(app.explorer.preview_read.is_none(), "preview timed out");
    }

    fn labels(app: &App) -> Vec<&str> {
        app.explorer
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .collect()
    }

    #[test]
    fn alt_three_opens_files_and_previews_the_selected_file() {
        let (root, mut app) = committed_change("explorer-preview");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn answer() -> u8 {\n    42\n}\n",
        )
        .unwrap();
        fs::write(root.join("data.bin"), b"\0\x01binary").unwrap();
        let comparison = app.comparison.mode.clone();

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('3'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert_eq!(app.shell.active_tab, ActiveTab::Files);
        assert_eq!(app.focus, PaneFocus::Explorer);
        assert_eq!(app.comparison.mode, comparison);
        wait_for_explorer(&mut app);
        assert_eq!(labels(&app), ["src", "data.bin", "tracked.txt"]);
        assert!(find_text(&render(&mut app, 100, 30), "▸ src").is_some());

        press(&mut app, KeyCode::Right);
        assert_eq!(labels(&app), ["src", "lib.rs", "data.bin", "tracked.txt"]);
        press(&mut app, KeyCode::Down);
        wait_for_explorer(&mut app);
        let buffer = render(&mut app, 100, 30);
        assert!(find_text(&buffer, "src/lib.rs").is_some());
        assert!(find_text(&buffer, "   1 pub fn answer() -> u8 {").is_some());
        assert!(find_text(&buffer, "   2     42").is_some());

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.focus, PaneFocus::FilePreview);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, PaneFocus::Explorer);

        press(&mut app, KeyCode::Left);
        assert_eq!(app.explorer.selected, 0);
        press(&mut app, KeyCode::Left);
        assert_eq!(labels(&app), ["src", "data.bin", "tracked.txt"]);

        press(&mut app, KeyCode::Down);
        wait_for_explorer(&mut app);
        assert!(find_text(&render(&mut app, 100, 30), "Binary file").is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_marks_working_tree_changes_and_selected_lines_reach_copy_and_history() {
        let (root, mut app) = committed_change("explorer-copy");
        app.set_tab(ActiveTab::Files);
        wait_for_explorer(&mut app);
        let buffer = render(&mut app, 100, 30);
        assert!(find_text(&buffer, "M tracked.txt").is_some());
        press(&mut app, KeyCode::Down);
        wait_for_explorer(&mut app);
        assert_eq!(app.explorer.preview_path(), Some("tracked.txt"));
        let buffer = render(&mut app, 100, 30);
        let (x, y) = find_text(&buffer, "     two").expect("removed line");
        assert_eq!(buffer[(x + 5, y)].bg, theme::DIFF_REMOVED_BG);
        let (x, y) = find_text(&buffer, "   2 two changed").expect("added line");
        assert_eq!(buffer[(x + 5, y)].bg, theme::DIFF_ADDED_BG);
        assert!(find_text(&buffer, "   3 three").is_some());

        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        let buffer = render(&mut app, 100, 30);
        assert!(find_text(&buffer, "tracked.txt:1–2").is_some());
        assert!(find_text(&buffer, "y Yank · h History · Esc Cancel").is_some());
        let (_, target) = app.line_history_target().expect("history target");
        assert_eq!(target.ranges, [(1, 1)]);
        assert_eq!(target.revision, None);
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.overlay, Overlay::CopySelection(_)));
        press(&mut app, KeyCode::Enter);
        let Some(PendingClipboard::Selection(text)) = &app.pending_clipboard else {
            panic!("selection was not copied");
        };
        assert_eq!(text, "File: tracked.txt\nLines: 1–2\n\none\ntwo changed");

        press(&mut app, KeyCode::Char('h'));
        assert!(matches!(app.overlay, Overlay::LineHistory(_)));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        press(&mut app, KeyCode::Esc);
        assert!(app.effective_selection_on(SelectionSurface::File).is_none());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, PaneFocus::Explorer);
        fs::remove_dir_all(root).unwrap();
    }

    fn wait_for_search(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.explorer.search_pending() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.tick_explorer_search();
            app.receive_foreground_results();
        }
        assert!(!app.explorer.search_pending(), "search timed out");
        wait_for_explorer(app);
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            press(app, KeyCode::Char(character));
        }
    }

    #[test]
    fn filter_finds_names_and_lines_and_reveals_the_match_in_the_preview() {
        let (root, mut app) = committed_change("explorer-filter");
        fs::create_dir_all(root.join("src")).unwrap();
        let mut source = (1..=40)
            .map(|line| format!("// line {line}"))
            .collect::<Vec<_>>();
        source[29] = "    let answer = compute();".to_owned();
        fs::write(root.join("src/lib.rs"), source.join("\n")).unwrap();
        fs::write(root.join("answer.md"), "notes\n").unwrap();
        app.set_tab(ActiveTab::Files);
        wait_for_explorer(&mut app);
        let tree = labels(&app)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "answer");
        wait_for_search(&mut app);
        let buffer = render(&mut app, 120, 40);
        assert!(find_text(&buffer, "/ answer").is_some());
        assert!(find_text(&buffer, "Matches · 2").is_some());
        let (files_x, files_y) = find_text(&buffer, "Files · 1").expect("file group");
        assert_eq!(
            find_text_in_row(&buffer, files_y + 1, "answer.md"),
            Some(files_x + 2)
        );
        let (lines_x, lines_y) = find_text(&buffer, "Lines · 1").expect("line group");
        assert_eq!(lines_x, files_x);
        assert_eq!(
            find_text_in_row(&buffer, lines_y + 1, "src/lib.rs:30"),
            Some(lines_x + 2)
        );
        assert!(matches!(app.overlay, Overlay::FileFilter));
        let names = app
            .explorer
            .filter
            .results
            .iter()
            .map(|result| match result.line {
                Some(line) => format!("{}:{line}", result.path),
                None => result.path.clone(),
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["answer.md", "src/lib.rs:30"]);
        assert_eq!(app.explorer.preview_path(), Some("answer.md"));

        press(&mut app, KeyCode::Down);
        wait_for_explorer(&mut app);
        let buffer = render(&mut app, 120, 40);
        assert!(find_text(&buffer, "src/lib.rs:30").is_some());
        let (x, y) = find_text(&buffer, "  30     let answer = compute();").unwrap();
        assert!(app.explorer.preview_scroll > 0);
        assert_eq!(buffer[(x + 13, y)].bg, theme::WARNING);
        assert_ne!(buffer[(x + 12, y)].bg, theme::WARNING);
        assert_ne!(buffer[(x + 19, y)].bg, theme::WARNING);

        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.explorer.filter.applied());
        press(&mut app, KeyCode::Char('k'));
        wait_for_explorer(&mut app);
        assert_eq!(app.explorer.preview_path(), Some("answer.md"));

        press(&mut app, KeyCode::Esc);
        assert!(!app.explorer.filter.applied());
        assert_eq!(labels(&app), tree);
        assert!(find_text(&render(&mut app, 120, 40), "/ answer").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tree_wheel_moves_one_row_per_notch_and_scrolls_long_names_sideways() {
        use crossterm::event::{MouseEvent, MouseEventKind};

        let (root, mut app) = committed_change("explorer-wheel");
        let long_name = format!("{}.txt", "long-name-".repeat(8));
        fs::write(root.join(&long_name), "long\n").unwrap();
        app.set_tab(ActiveTab::Files);
        wait_for_explorer(&mut app);
        render(&mut app, 80, 30);
        let tree = app.explorer.tree_area;
        let wheel = |kind, modifiers| MouseEvent {
            kind,
            column: tree.x + 1,
            row: tree.y,
            modifiers,
        };
        let outside = MouseEvent {
            column: app.explorer.preview_area.x + 1,
            row: app.explorer.preview_area.y,
            ..wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE)
        };
        let down = wheel(MouseEventKind::ScrollDown, KeyModifiers::NONE);
        assert_eq!(app.wheel_burst_limit(&Event::Mouse(down)), 1);
        assert_eq!(app.wheel_burst_limit(&Event::Mouse(outside)), 4);

        app.focus = PaneFocus::FilePreview;
        app.handle(Event::Mouse(down)).unwrap();
        assert_eq!(app.explorer.selected, 1);
        assert_eq!(app.focus, PaneFocus::Explorer);

        for _ in 0..200 {
            app.handle(Event::Mouse(wheel(
                MouseEventKind::ScrollDown,
                KeyModifiers::SHIFT,
            )))
            .unwrap();
        }
        assert_eq!(app.explorer.selected, 1);
        let scrolled = app.explorer.list_horizontal_scroll;
        assert!(scrolled > 0);
        let buffer = render(&mut app, 80, 30);
        assert!(find_text(&buffer, "long-name-long-name-.txt").is_some());
        app.handle(Event::Mouse(wheel(
            MouseEventKind::ScrollLeft,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.explorer.list_horizontal_scroll, scrolled - 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn superseded_file_lists_are_ignored() {
        let (root, mut app) = committed_change("explorer-stale");
        let (requests, _) = intercept_foreground(&mut app);
        app.set_tab(ActiveTab::Files);
        let stale = requests.try_recv().unwrap();
        app.request_repository_files();
        let current = requests.try_recv().unwrap();
        let files = |paths: &[&str]| RepositoryFiles {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            tree: PathTree::from_paths(paths.iter().copied().enumerate()),
        };
        for (request, paths) in [(stale, ["old.txt"]), (current, ["new.txt"])] {
            let ForegroundRequest::RepositoryFiles {
                id,
                generation,
                root,
            } = request
            else {
                panic!("expected a file list request");
            };
            app.apply_repository_files(id, generation, root, Ok(files(&paths)));
        }
        assert_eq!(labels(&app), ["new.txt"]);
        fs::remove_dir_all(root).unwrap();
    }
}
