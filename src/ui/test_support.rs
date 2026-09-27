use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::App;
use super::effect::{ForegroundRequest, ForegroundResult};
use crate::git::Repository;
use crate::project::ProjectRegistry;
use crate::ui::syntax::{DiffCell, DiffDocument, DiffRow};

pub(super) fn test_diff_document(
    before: Vec<Line<'static>>,
    after: Vec<Line<'static>>,
) -> DiffDocument {
    test_diff_document_with_after_metadata(before, after, Vec::new(), Vec::new())
}

pub(super) fn test_diff_document_with_after_metadata(
    before: Vec<Line<'static>>,
    after: Vec<Line<'static>>,
    after_line_numbers: Vec<Option<usize>>,
    after_sources: Vec<Option<String>>,
) -> DiffDocument {
    let len = before
        .len()
        .max(after.len())
        .max(after_line_numbers.len())
        .max(after_sources.len());
    DiffDocument::from_rows(
        (0..len)
            .map(|row| {
                DiffRow::new(
                    DiffCell::new(
                        before.get(row).cloned().unwrap_or_else(|| Line::raw("")),
                        None,
                        None,
                    ),
                    DiffCell::new(
                        after.get(row).cloned().unwrap_or_else(|| Line::raw("")),
                        after_line_numbers.get(row).copied().flatten(),
                        after_sources.get(row).cloned().flatten(),
                    ),
                    None,
                )
            })
            .collect(),
    )
}

pub(super) fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

pub(super) fn temp_repo(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("herdr-git-{name}-{unique}"));
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["config", "user.name", "Test Author"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    root
}

pub(super) fn temp_registry(name: &str) -> ProjectRegistry {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let state = std::env::temp_dir().join(format!("herdr-git-{name}-state-{unique}"));
    ProjectRegistry::load(Some(state.join("projects.json"))).unwrap()
}

pub(super) fn offline_app() -> App {
    App::load_registered(
        &PathBuf::from("/tmp/project"),
        ProjectRegistry::load(None).unwrap(),
    )
    .unwrap()
}

pub(super) fn committed_change(name: &str) -> (PathBuf, App) {
    let root = temp_repo(name);
    git(&root, &["config", "user.name", "Test Author"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    fs::write(root.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-m", "Base"]);
    fs::write(root.join("tracked.txt"), "one\ntwo changed\nthree\n").unwrap();
    let repository = Repository::discover(&root).unwrap();
    let app = App::load(repository).unwrap();
    (root, app)
}

pub(super) fn intercept_foreground(
    app: &mut App,
) -> (Receiver<ForegroundRequest>, Sender<ForegroundResult>) {
    let (request_tx, request_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    app.foreground.read_tx = request_tx.clone();
    app.foreground.mutation_tx = request_tx;
    app.foreground.result_rx = result_rx;
    (request_rx, result_tx)
}

pub(super) fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}

pub(super) fn row_text(buffer: &Buffer, area: Rect, y: u16) -> String {
    (area.x..area.right())
        .map(|column| buffer[(column, y)].symbol())
        .collect()
}

pub(super) fn find_text_in_row(buffer: &Buffer, y: u16, needle: &str) -> Option<u16> {
    let needle = needle.chars().map(|character| character.to_string());
    let needle = needle.collect::<Vec<_>>();
    let width = buffer.area.width;
    (0..width.saturating_sub(needle.len() as u16).saturating_add(1)).find(|x| {
        needle
            .iter()
            .enumerate()
            .all(|(offset, symbol)| buffer[(x + offset as u16, y)].symbol() == symbol)
    })
}

pub(super) fn find_text(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
    (0..buffer.area.height).find_map(|y| find_text_in_row(buffer, y, needle).map(|x| (x, y)))
}

pub(super) fn buffer_text(buffer: &Buffer) -> String {
    buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}

pub(super) fn press(app: &mut App, code: KeyCode) {
    app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .unwrap();
}

pub(super) fn click(app: &mut App, column: u16, row: u16) {
    app.handle(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }))
    .unwrap();
}

pub(super) fn wait_for_foreground(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.foreground.action.is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        app.receive_foreground_results();
    }
    assert!(app.foreground.action.is_none(), "foreground job timed out");
}

pub(super) fn wait_for_diff(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.diff.pending_diff.is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        app.receive_foreground_results();
    }
    assert!(app.diff.pending_diff.is_none(), "diff job timed out");
}

pub(super) fn wait_for_refresh(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while (app.refresh.pending || app.refresh.in_flight || app.refresh.deferred_result.is_some())
        && Instant::now() < deadline
    {
        if app.refresh.pending && !app.refresh.in_flight && app.refresh.deferred_result.is_none() {
            app.start_auto_refresh();
        }
        std::thread::sleep(Duration::from_millis(10));
        app.receive_refresh_result();
        app.receive_workspace_results();
        app.apply_workspace_refresh();
        app.apply_deferred_refresh();
    }
    assert!(!app.refresh.pending, "refresh remained pending");
    assert!(!app.refresh.in_flight, "refresh job timed out");
    assert!(app.refresh.deferred_result.is_none());
    wait_for_history(app);
}

pub(super) fn wait_for_history(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.history.pending.is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        app.receive_history();
        app.receive_foreground_results();
    }
    assert!(app.history.pending.is_none(), "history read timed out");
    assert!(app.history.error.is_none(), "{:?}", app.history.error);
    while app.inspect.pending_commit_details.is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        app.receive_foreground_results();
    }
}
