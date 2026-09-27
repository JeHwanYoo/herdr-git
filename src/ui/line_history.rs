use std::time::{Instant, SystemTime};

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::git::ReadError;

use super::diff::padded_diff_lines;
use super::effect::{ForegroundRequest, LineHistoryEntry, ReadGeneration, RequestId};
use super::overlay::Overlay;
use super::shell::ActiveTab;
use super::widgets::{self, area_hovered, truncate_to_width};
use super::{App, theme};

#[derive(Debug)]
pub(super) struct LineHistoryDialog {
    title: String,
    read: Option<(RequestId, ReadGeneration)>,
    started: Instant,
    entries: Vec<LineHistoryEntry>,
    error: Option<String>,
    selected: usize,
    list_scroll: usize,
    list_height: usize,
    diff_scroll: usize,
    row_areas: Vec<(usize, Rect)>,
    diff_area: Rect,
    close_area: Rect,
}

impl LineHistoryDialog {
    fn select(&mut self, index: usize) {
        let index = index.min(self.entries.len().saturating_sub(1));
        if index != self.selected {
            self.selected = index;
            self.diff_scroll = 0;
        }
        if index < self.list_scroll {
            self.list_scroll = index;
        } else if index >= self.list_scroll + self.list_height.max(1) {
            self.list_scroll = index + 1 - self.list_height.max(1);
        }
    }

    fn scroll_diff(&mut self, delta: isize) {
        let len = self
            .entries
            .get(self.selected)
            .map_or(0, |entry| entry.document.len());
        self.diff_scroll = self
            .diff_scroll
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
    }
}

impl App {
    pub(super) fn open_line_history(&mut self) -> bool {
        let Some((title, target)) = self.line_history_target() else {
            self.show_action_error("Select committed lines to see their history.");
            return false;
        };
        self.foreground.reads.line_history.advance();
        let id = self.foreground.next_id;
        let generation = self.foreground.reads.line_history.generation;
        let read = match self.foreground.request(ForegroundRequest::LineHistory {
            id,
            generation,
            target,
        }) {
            Ok(id) => Some((id, generation)),
            Err(error) => {
                self.show_action_error(&format!("Git worker stopped: {error}"));
                return false;
            }
        };
        self.overlay = Overlay::LineHistory(LineHistoryDialog {
            title,
            read,
            started: Instant::now(),
            entries: Vec::new(),
            error: None,
            selected: 0,
            list_scroll: 0,
            list_height: 1,
            diff_scroll: 0,
            row_areas: Vec::new(),
            diff_area: Rect::default(),
            close_area: Rect::default(),
        });
        true
    }

    pub(super) fn apply_line_history(
        &mut self,
        id: RequestId,
        generation: ReadGeneration,
        result: Result<Vec<LineHistoryEntry>, ReadError>,
    ) {
        let current = self.foreground.reads.line_history.is_current(generation);
        let Overlay::LineHistory(dialog) = &mut self.overlay else {
            return;
        };
        if !current || dialog.read != Some((id, generation)) {
            return;
        }
        dialog.read = None;
        match result {
            Ok(entries) => dialog.entries = entries,
            Err(ReadError::Cancelled) => {}
            Err(ReadError::Diagnostic(error)) => dialog.error = Some(error),
        }
    }

    pub(super) fn handle_line_history(&mut self, input: &Event) {
        let Overlay::LineHistory(dialog) = &mut self.overlay else {
            return;
        };
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => self.close_line_history(),
                KeyCode::Down | KeyCode::Char('j') => dialog.select(dialog.selected + 1),
                KeyCode::Up | KeyCode::Char('k') => {
                    dialog.select(dialog.selected.saturating_sub(1))
                }
                KeyCode::Home => dialog.select(0),
                KeyCode::End => dialog.select(usize::MAX),
                KeyCode::PageDown => dialog.scroll_diff(dialog.diff_area.height as isize),
                KeyCode::PageUp => dialog.scroll_diff(-(dialog.diff_area.height as isize)),
                KeyCode::Enter => self.open_line_history_commit(),
                _ => {}
            },
            Event::Mouse(mouse) => {
                let pointer = (mouse.column, mouse.row).into();
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if dialog.close_area.contains(pointer) =>
                    {
                        self.close_line_history()
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(index) = dialog
                            .row_areas
                            .iter()
                            .find_map(|(index, area)| area.contains(pointer).then_some(*index))
                        {
                            if index == dialog.selected {
                                self.open_line_history_commit();
                            } else {
                                dialog.select(index);
                            }
                        }
                    }
                    MouseEventKind::ScrollDown if dialog.diff_area.contains(pointer) => {
                        dialog.scroll_diff(1)
                    }
                    MouseEventKind::ScrollUp if dialog.diff_area.contains(pointer) => {
                        dialog.scroll_diff(-1)
                    }
                    MouseEventKind::ScrollDown => dialog.select(dialog.selected + 1),
                    MouseEventKind::ScrollUp => dialog.select(dialog.selected.saturating_sub(1)),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn close_line_history(&mut self) {
        self.foreground.reads.line_history.advance();
        self.overlay = Overlay::None;
    }

    fn open_line_history_commit(&mut self) {
        let Overlay::LineHistory(dialog) = &self.overlay else {
            return;
        };
        let Some(sha) = dialog
            .entries
            .get(dialog.selected)
            .map(|entry| entry.commit.sha.clone())
        else {
            return;
        };
        self.close_line_history();
        self.clear_selection();
        self.set_tab(ActiveTab::History);
        self.graph.reveal = Some(sha);
        self.reveal_pending_commit();
    }

    pub(super) fn draw_line_history(&self, frame: &mut Frame<'_>, dialog: &mut LineHistoryDialog) {
        let screen = frame.area();
        let area = Rect::new(
            screen.x + screen.width / 20,
            screen.y + 1,
            screen.width - screen.width / 10,
            screen.height.saturating_sub(2),
        );
        let title = format!("Line History · {}", dialog.title);
        let inner = widgets::dialog_frame_at(
            frame,
            &truncate_to_width(&title, area.width.saturating_sub(8) as usize),
            area,
        );
        dialog.close_area = Rect::new(area.right().saturating_sub(4), area.y, 3, 1);
        frame.render_widget(
            Paragraph::new("[×]").style(theme::hover(
                theme::hint(),
                area_hovered(self.shell.mouse_position, dialog.close_area),
            )),
            dialog.close_area,
        );
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(50),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(inner.inner(Margin::new(1, 0)));
        dialog.list_height = rows[0].height as usize;
        dialog.select(dialog.selected);
        frame.render_widget(
            widgets::footer_hint("j/k commit · PgUp/PgDn scroll · Enter open in Graph · Esc close"),
            rows[2],
        );
        dialog.row_areas.clear();
        dialog.diff_area = Rect::default();
        if let Some((_, _)) = dialog.read {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    theme::spinner_span(dialog.started.elapsed()),
                    Span::raw(" Reading line history"),
                ])),
                rows[0],
            );
            return;
        }
        if let Some(error) = &dialog.error {
            frame.render_widget(
                Paragraph::new(error.lines().next().unwrap_or_default().to_owned())
                    .style(theme::error_text()),
                rows[0],
            );
            return;
        }
        if dialog.entries.is_empty() {
            frame.render_widget(
                Paragraph::new("No commits touched these lines.").style(theme::hint()),
                rows[0],
            );
            return;
        }
        let now = SystemTime::now();
        let width = rows[0].width as usize;
        for (offset, (index, entry)) in dialog
            .entries
            .iter()
            .enumerate()
            .skip(dialog.list_scroll)
            .take(dialog.list_height)
            .enumerate()
        {
            let row = Rect::new(rows[0].x, rows[0].y + offset as u16, rows[0].width, 1);
            dialog.row_areas.push((index, row));
            let commit = &entry.commit;
            let text = format!(
                "{} {}  {:<18}  {:<14}  {}",
                if index == dialog.selected { "▶" } else { " " },
                &commit.sha[..commit.sha.len().min(7)],
                truncate_to_width(&commit.author, 18),
                commit.author_relative(now),
                commit.summary
            );
            let style = if index == dialog.selected {
                theme::selection_row()
            } else {
                theme::hint()
            };
            frame.render_widget(
                Paragraph::new(truncate_to_width(&text, width)).style(style),
                row,
            );
        }
        let Some(entry) = dialog.entries.get(dialog.selected) else {
            return;
        };
        dialog.diff_area = rows[1];
        let panels = Layout::default()
            .direction(if rows[1].width >= 72 {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        let short = |sha: &str| sha[..sha.len().min(7)].to_owned();
        let previous = match dialog.entries.get(dialog.selected + 1) {
            Some(previous) => short(&previous.commit.sha),
            None if entry.commit.patch.contains("\n--- /dev/null") => "none".to_owned(),
            None => format!("{}^", short(&entry.commit.sha)),
        };
        let titles = [
            format!("Before · {previous}"),
            format!("After · {}", short(&entry.commit.sha)),
        ];
        let document = &entry.document;
        for (panel, title, before) in [
            (panels[0], &titles[0], true),
            (panels[1], &titles[1], false),
        ] {
            let block = Block::default().borders(Borders::ALL).title(title.as_str());
            let source = block.inner(panel);
            frame.render_widget(block, panel);
            let lines = padded_diff_lines(
                (dialog.diff_scroll..)
                    .map_while(|row| {
                        if before {
                            document.before_line(row)
                        } else {
                            document.after_line(row)
                        }
                    })
                    .take(source.height as usize),
                source.width as usize,
                0,
            );
            frame.render_widget(Paragraph::new(lines), source);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::git::LineHistoryCommit;
    use crate::ui::shell::PaneFocus;
    use crate::ui::test_support::{
        buffer_text, committed_change, intercept_foreground, press, render, test_diff_document,
    };

    use super::*;

    fn entry(sha: &str, summary: &str, after: &str) -> LineHistoryEntry {
        LineHistoryEntry {
            commit: LineHistoryCommit {
                sha: sha.repeat(40 / sha.len()),
                author: "Ada".to_owned(),
                author_time: 0,
                summary: summary.to_owned(),
                patch: String::new(),
            },
            document: test_diff_document(vec![Line::raw("old")], vec![Line::raw(after.to_owned())]),
        }
    }

    #[test]
    fn line_history_lists_commits_and_shows_the_selected_diff() {
        let (root, mut app) = committed_change("line-history");
        let (request_rx, _result_tx) = intercept_foreground(&mut app);
        app.focus = PaneFocus::Diff;
        app.diff.diff_target = crate::git::DiffTarget::CommitAgainstParent {
            commit: "HEAD".to_owned(),
            parent: None,
        };
        let row = app.diff.diff_document.after_row_for_line(1).unwrap();
        app.diff.diff_side = crate::ui::review::ReviewSide::After;
        app.diff.focused_diff_row = row;
        press(&mut app, KeyCode::Char('v'));
        assert!(app.open_line_history());
        let Ok(ForegroundRequest::LineHistory {
            id,
            generation,
            target,
        }) = request_rx.try_recv()
        else {
            panic!("line history request");
        };
        assert_eq!(target.ranges, [(1, 1)]);
        assert_eq!(target.revision.as_deref(), Some("HEAD"));
        assert!(buffer_text(&render(&mut app, 120, 40)).contains("Reading line history"));

        app.apply_line_history(
            id,
            generation,
            Ok(vec![
                entry("a", "Newest change", "newest after"),
                entry("b", "Older change", "older after"),
            ]),
        );
        let text = buffer_text(&render(&mut app, 120, 40));
        assert!(text.contains("aaaaaaa"));
        assert!(text.contains("Older change"));
        assert!(text.contains("newest after"));
        assert!(text.contains("Before · bbbbbbb"));
        assert!(text.contains("After · aaaaaaa"));
        press(&mut app, KeyCode::Char('j'));
        assert!(buffer_text(&render(&mut app, 120, 40)).contains("older after"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn line_history_reads_real_commits_through_the_worker() {
        let (root, mut app) = committed_change("line-history-worker");
        app.focus = PaneFocus::Diff;
        app.diff.diff_target = crate::git::DiffTarget::CommitAgainstParent {
            commit: "HEAD".to_owned(),
            parent: None,
        };
        app.diff.diff_side = crate::ui::review::ReviewSide::After;
        app.diff.focused_diff_row = app.diff.diff_document.after_row_for_line(1).unwrap();
        press(&mut app, KeyCode::Char('v'));
        assert!(app.open_line_history());
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while matches!(&app.overlay, Overlay::LineHistory(dialog) if dialog.read.is_some())
            && Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
            app.receive_foreground_results();
        }
        let Overlay::LineHistory(dialog) = &app.overlay else {
            panic!("line history closed");
        };
        assert_eq!(dialog.error, None);
        assert_eq!(dialog.entries.len(), 1);
        assert_eq!(dialog.entries[0].commit.summary, "Base");
        let text = buffer_text(&render(&mut app, 120, 40));
        assert!(text.contains("Before · none"));
        assert!(text.contains("one"));
        assert!(!text.contains("diff --git"));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn dialog_with(entries: Vec<LineHistoryEntry>) -> LineHistoryDialog {
        LineHistoryDialog {
            title: "tracked.txt:1".to_owned(),
            read: None,
            started: Instant::now(),
            entries,
            error: None,
            selected: 0,
            list_scroll: 0,
            list_height: 1,
            diff_scroll: 0,
            row_areas: Vec::new(),
            diff_area: Rect::default(),
            close_area: Rect::default(),
        }
    }

    fn settle_reveal(app: &mut App) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.graph.reveal.is_some() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            app.receive_history();
            app.receive_foreground_results();
            app.reveal_pending_commit();
        }
        assert!(app.graph.reveal.is_none(), "reveal timed out");
    }

    #[test]
    fn enter_selects_the_commit_in_graph_once_its_page_loads() {
        let (root, mut app) = committed_change("line-history-open");
        let head = String::from_utf8(
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
        let mut found = entry("c", "Base", "after");
        found.commit.sha.clone_from(&head);
        app.overlay = Overlay::LineHistory(dialog_with(vec![found]));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.shell.active_tab, ActiveTab::History);
        settle_reveal(&mut app);
        assert_eq!(app.focus, PaneFocus::Commits);
        assert_eq!(
            app.graph
                .selected_commit()
                .map(|commit| commit.sha.as_str()),
            Some(head.as_str())
        );
        assert!(app.pending_clipboard.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_pages_graph_history_until_an_old_commit_appears() {
        let (root, mut app) = committed_change("line-history-paging");
        let git_output = |args: &[&str]| {
            String::from_utf8(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_owned()
        };
        let oldest = git_output(&["rev-parse", "HEAD"]);
        for index in 0..120 {
            git_output(&["commit", "--allow-empty", "-qm", &format!("Empty {index}")]);
        }
        app.request_history();
        crate::ui::test_support::wait_for_history(&mut app);
        let mut found = entry("e", "Base", "after");
        found.commit.sha.clone_from(&oldest);
        app.overlay = Overlay::LineHistory(dialog_with(vec![found]));
        press(&mut app, KeyCode::Enter);
        settle_reveal(&mut app);
        assert!(app.graph.commits.len() > 100);
        assert_eq!(
            app.graph
                .selected_commit()
                .map(|commit| commit.sha.as_str()),
            Some(oldest.as_str())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_reports_a_commit_graph_cannot_reach_without_copying() {
        let (root, mut app) = committed_change("line-history-missing");
        app.overlay = Overlay::LineHistory(dialog_with(vec![entry("d", "Gone", "after")]));
        press(&mut app, KeyCode::Enter);
        settle_reveal(&mut app);
        assert!(app.pending_clipboard.is_none());
        assert!(
            app.overlay
                .result()
                .is_some_and(|result| format!("{result:?}").contains("not within the newest"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
