use std::collections::HashSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::git::ReadError;

use super::diff::horizontal_line_slice;
use super::effect::{FilePreview, ForegroundRequest, ReadGeneration, RepositoryFiles, RequestId};
use super::path_tree::{PathRow, PathRowKind};
use super::shell::PaneFocus;
use super::widgets::{self, counted_title, scrolled_content_row_at, viewport_offset};
use super::{App, theme};

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
    preview: Option<(String, FilePreview)>,
    preview_scroll: usize,
    preview_horizontal_scroll: usize,
    error: Option<String>,
    tree_area: Rect,
    preview_area: Rect,
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
        self.explorer.preview = None;
    }

    fn request_file_preview(&mut self, file: String) {
        let Some(root) = self.explorer.root.clone() else {
            return;
        };
        if self
            .explorer
            .preview
            .as_ref()
            .is_some_and(|(shown, _)| *shown != file)
        {
            self.explorer.preview = None;
            self.explorer.preview_scroll = 0;
            self.explorer.preview_horizontal_scroll = 0;
        }
        self.foreground.reads.file_preview.advance();
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.file_preview.generation;
        let request = ForegroundRequest::FilePreview {
            id,
            generation,
            root: root.clone(),
            file: file.clone(),
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
            || self.explorer.selected_file() != Some(file.as_str())
        {
            return;
        }
        let preview = match result {
            Ok(preview) => preview,
            Err(ReadError::Cancelled) => return,
            Err(ReadError::Diagnostic(error)) => FilePreview::Notice(error),
        };
        self.explorer.preview = Some((file, preview));
    }

    fn select_explorer_row(&mut self, index: usize) {
        if self.explorer.rows.is_empty() {
            return;
        }
        self.explorer.selected = index.min(self.explorer.rows.len() - 1);
        if let Some(file) = self.explorer.selected_file().map(str::to_owned) {
            let shown = self
                .explorer
                .preview
                .as_ref()
                .is_some_and(|(path, _)| *path == file);
            let loading = self
                .explorer
                .preview_read
                .as_ref()
                .is_some_and(|read| read.file == file);
            if !shown && !loading {
                self.request_file_preview(file);
            }
        }
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

    pub(super) fn handle_explorer_key(&mut self, key: KeyEvent) -> bool {
        match self.focus {
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
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => self.scroll_preview(1),
                    KeyCode::Up | KeyCode::Char('k') => self.scroll_preview(-1),
                    KeyCode::PageDown => self.scroll_preview(page),
                    KeyCode::PageUp => self.scroll_preview(-page),
                    KeyCode::Home => self.explorer.preview_scroll = 0,
                    KeyCode::End => self.explorer.preview_scroll = usize::MAX,
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
        if self.explorer.tree_area.contains(pointer.into()) {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.move_explorer_selection(1),
                MouseEventKind::ScrollUp => self.move_explorer_selection(-1),
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
                MouseEventKind::Down(MouseButton::Left) => self.focus = PaneFocus::FilePreview,
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
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);
        self.draw_explorer_tree(frame, panes[0]);
        self.draw_file_preview(frame, panes[1]);
    }

    fn draw_explorer_tree(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        self.explorer.tree_area = inner;
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
        let items = self
            .explorer
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(index, row)| {
                ListItem::new(explorer_line(row))
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
        let selected = self.explorer.selected_file().map(str::to_owned);
        let title = selected.clone().unwrap_or_else(|| "Preview".to_owned());
        frame.render_widget(
            widgets::pane_block(title, self.focus == PaneFocus::FilePreview),
            area,
        );
        let preview = self
            .explorer
            .preview
            .as_ref()
            .filter(|(path, _)| Some(path) == selected.as_ref())
            .map(|(_, preview)| preview);
        let lines = match preview {
            Some(FilePreview::Text(lines)) => lines,
            Some(FilePreview::Notice(notice)) => {
                frame.render_widget(
                    Paragraph::new(notice.clone())
                        .style(theme::hint())
                        .wrap(Wrap { trim: false }),
                    inner,
                );
                return;
            }
            None => {
                let text = if selected.is_some() {
                    "Loading…"
                } else {
                    "Select a file to preview it"
                };
                frame.render_widget(Paragraph::new(text).style(theme::hint()), inner);
                return;
            }
        };
        let height = usize::from(inner.height);
        let max_scroll = lines.len().saturating_sub(height);
        self.explorer.preview_scroll = self.explorer.preview_scroll.min(max_scroll);
        let widest = lines.iter().map(Line::width).max().unwrap_or_default();
        self.explorer.preview_horizontal_scroll = self
            .explorer
            .preview_horizontal_scroll
            .min(widest.saturating_sub(usize::from(inner.width)));
        let visible = lines
            .iter()
            .skip(self.explorer.preview_scroll)
            .take(height)
            .map(|line| {
                horizontal_line_slice(
                    line.clone(),
                    self.explorer.preview_horizontal_scroll,
                    usize::from(inner.width),
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), inner);
    }
}

fn explorer_line(row: &PathRow) -> Line<'static> {
    let indent = "  ".repeat(row.depth);
    match &row.kind {
        PathRowKind::Directory { expanded, .. } => Line::from(vec![
            Span::styled(
                format!("{indent}{} ", if *expanded { "▾" } else { "▸" }),
                theme::hint(),
            ),
            Span::raw(row.label.clone()),
        ]),
        PathRowKind::File { .. } => Line::from(format!("{indent}  {}", row.label)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::super::App;
    use super::super::effect::{ForegroundRequest, RepositoryFiles};
    use super::super::path_tree::PathTree;
    use super::super::shell::{ActiveTab, PaneFocus};
    use super::super::test_support::{
        committed_change, find_text, intercept_foreground, press, render,
    };

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
