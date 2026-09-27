use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::git::{
    Commit, CommitRefKind, HISTORY_PAGE_SIZE, HistoryPage, HistorySession, ReadError, Repository,
    with_read_cancellation_result,
};

use super::App;
use super::effect::{ReadGeneration, RefreshIntent, RequestId};
use super::graph::filtered_commit_indices;
use super::shell::ActiveTab;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryRequest {
    pub id: RequestId,
    pub generation: ReadGeneration,
    pub path: PathBuf,
    pub offset: usize,
    pub count: usize,
    pub replace: bool,
    pub intent: RefreshIntent,
    pub follow_head: bool,
    pub start: Option<String>,
}

pub(super) struct HistoryResult {
    pub request: HistoryRequest,
    pub page: Result<HistoryPage, ReadError>,
    pub elapsed: Duration,
}

enum Work {
    Read(HistoryRequest),
    Close,
    Stop,
}

pub(super) struct HistoryLane {
    tx: Sender<Work>,
    pub result_rx: Receiver<HistoryResult>,
    generation: ReadGeneration,
    cancellation: Arc<AtomicU64>,
    next_id: RequestId,
    pub pending: Option<HistoryRequest>,
    pub follow_head: bool,
    deferred: Option<HistoryResult>,
    pub started: Instant,
    pub last_load_duration: Option<Duration>,
    pub error: Option<String>,
    thread: Option<JoinHandle<()>>,
}

impl HistoryLane {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let cancellation = Arc::new(AtomicU64::new(0));
        let cancel = Arc::clone(&cancellation);
        let thread = thread::Builder::new()
            .name("herdr-git-history".to_owned())
            .spawn(move || {
                let mut session: Option<(PathBuf, ReadGeneration, Option<String>, HistorySession)> =
                    None;
                while let Ok(work) = rx.recv() {
                    match work {
                        Work::Stop => break,
                        Work::Close => session = None,
                        Work::Read(request) => {
                            let started = Instant::now();
                            let page = with_read_cancellation_result(
                                Arc::clone(&cancel),
                                request.generation.get(),
                                || {
                                    if request.replace
                                        || session.as_ref().is_none_or(
                                            |(path, generation, start, _)| {
                                                path != &request.path
                                                    || generation != &request.generation
                                                    || start != &request.start
                                            },
                                        )
                                    {
                                        session = None;
                                        let stream = Repository::at_root(request.path.clone())
                                            .history_session_from(request.start.as_deref())?;
                                        session = Some((
                                            request.path.clone(),
                                            request.generation,
                                            request.start.clone(),
                                            stream,
                                        ));
                                    }
                                    let stream = &mut session.as_mut().expect("history session").3;
                                    let mut commits = Vec::new();
                                    let mut has_more = true;
                                    let mut found_head = false;
                                    while has_more
                                        && (commits.len() < request.count
                                            || (request.follow_head && !found_head))
                                    {
                                        let count = if commits.len() < request.count {
                                            HISTORY_PAGE_SIZE.min(request.count - commits.len())
                                        } else {
                                            HISTORY_PAGE_SIZE
                                        };
                                        let page = stream.next_page(count)?;
                                        found_head |= page.commits.iter().any(is_head);
                                        if page.offset != request.offset + commits.len() {
                                            return Err(
                                                "History position changed; Refresh to retry"
                                                    .to_owned(),
                                            );
                                        }
                                        has_more = page.has_more;
                                        commits.extend(page.commits);
                                    }
                                    Ok(HistoryPage {
                                        commits,
                                        offset: request.offset,
                                        has_more,
                                    })
                                },
                            );
                            if page.is_err() {
                                session = None;
                            }
                            if result_tx
                                .send(HistoryResult {
                                    request,
                                    page,
                                    elapsed: started.elapsed(),
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tx,
            result_rx,
            generation: ReadGeneration::ZERO,
            cancellation,
            next_id: RequestId::FIRST,
            pending: None,
            follow_head: false,
            deferred: None,
            started: Instant::now(),
            last_load_duration: None,
            error: None,
            thread: Some(thread),
        })
    }

    pub fn clear(&mut self) {
        self.generation.advance();
        self.cancellation
            .store(self.generation.get(), Ordering::Release);
        self.pending = None;
        self.follow_head = false;
        self.deferred = None;
        self.error = None;
        self.last_load_duration = None;
        let _ = self.tx.send(Work::Close);
    }

    fn request(
        &mut self,
        path: PathBuf,
        start: Option<String>,
        offset: usize,
        count: usize,
        replace: bool,
        intent: RefreshIntent,
    ) {
        if replace {
            let last_load_duration = self.last_load_duration;
            let follow_head = self.follow_head;
            self.clear();
            self.follow_head = follow_head;
            self.last_load_duration = last_load_duration;
        }
        if self.pending.is_some() || self.error.is_some() {
            return;
        }
        let request = HistoryRequest {
            id: self.next_id.take_and_advance(),
            generation: self.generation,
            path,
            offset,
            count,
            replace,
            intent,
            follow_head: self.follow_head && start.is_none(),
            start,
        };
        self.started = Instant::now();
        match self.tx.send(Work::Read(request.clone())) {
            Ok(()) => self.pending = Some(request),
            Err(_) => self.error = Some("History worker stopped; reopen the pane".to_owned()),
        }
    }
}

impl Drop for HistoryLane {
    fn drop(&mut self) {
        self.cancellation.fetch_add(1, Ordering::AcqRel);
        let _ = self.tx.send(Work::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn is_head(commit: &Commit) -> bool {
    commit
        .refs
        .iter()
        .any(|reference| reference.kind == CommitRefKind::Head)
}

impl App {
    pub(super) fn request_history(&mut self) {
        self.request_history_for(RefreshIntent::Interaction);
    }

    pub(super) fn request_history_for(&mut self, intent: RefreshIntent) {
        if let Some(repository) = &self.repository {
            self.history.request(
                repository.root().to_owned(),
                self.graph.anchor.clone(),
                0,
                self.graph.commits.len().max(HISTORY_PAGE_SIZE),
                true,
                intent,
            );
        }
    }

    pub(super) fn request_more_history(&mut self, explicit: bool) {
        if self.shell.active_tab != ActiveTab::History
            || !self.graph.history_has_more
            || !self.graph.history_loaded
        {
            return;
        }
        if !explicit
            && (!self.graph.query.text.is_empty()
                || self
                    .graph
                    .history_scroll
                    .saturating_add(2 * self.graph.history_content_area.height as usize)
                    < self.graph.display_len())
        {
            return;
        }
        if let Some(repository) = &self.repository {
            self.history.request(
                repository.root().to_owned(),
                self.graph.anchor.clone(),
                self.graph.commits.len(),
                HISTORY_PAGE_SIZE,
                false,
                RefreshIntent::Interaction,
            );
        }
    }

    pub(super) fn receive_history(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.history.result_rx.try_recv() {
            if self.history.pending.as_ref() != Some(&result.request) {
                continue;
            }
            self.history.deferred = Some(result);
        }
        if self.history.deferred.as_ref().is_some_and(|result| {
            !result.request.replace || !self.refresh_is_transient(result.request.intent)
        }) {
            let result = self.history.deferred.take().expect("deferred history");
            changed |= self.apply_history_page(result);
        }
        changed
    }

    fn apply_history_page(&mut self, result: HistoryResult) -> bool {
        if self.history.pending.as_ref() != Some(&result.request)
            || result.request.generation != self.history.generation
            || self
                .repository
                .as_ref()
                .is_none_or(|repo| repo.root() != result.request.path)
        {
            return false;
        }
        self.history.pending = None;
        let page = match result.page {
            Ok(page) => page,
            Err(ReadError::Cancelled) => return false,
            Err(ReadError::Diagnostic(error)) => {
                self.history.error = Some(format!("{error}. Refresh to retry"));
                return true;
            }
        };
        if page.offset != result.request.offset
            || (!result.request.replace && page.offset != self.graph.commits.len())
        {
            return false;
        }
        self.history.last_load_duration = Some(result.elapsed);
        let selected = self.graph.selected_commit().map(|c| c.sha.clone());
        let on_uncommitted = self.graph.uncommitted_selected();
        let previous_visible = self.graph.visible.len();
        self.graph.history_has_more = page.has_more;
        self.graph.history_loaded = true;
        if result.request.replace {
            self.graph.commits = page.commits;
            self.graph.visible =
                filtered_commit_indices(&self.graph.commits, &self.graph.query.text);
        } else {
            let offset = self.graph.commits.len();
            self.graph.visible.extend(
                filtered_commit_indices(&page.commits, &self.graph.query.text)
                    .into_iter()
                    .map(|i| offset + i),
            );
            self.graph.commits.extend(page.commits);
        }
        if result.request.follow_head {
            if let Some(head) = self.graph.commits.iter().position(is_head)
                && !self.graph.visible.contains(&head)
            {
                self.graph.query = super::widgets::TextField::new();
                self.graph.visible = (0..self.graph.commits.len()).collect();
            }
            self.history.follow_head = false;
        }
        if result.request.replace {
            self.graph.update_graph_width();
        } else {
            let added_width = self.graph.visible[previous_visible..]
                .iter()
                .map(|&index| crate::ui::graph::prefix_width(&self.graph.commits[index].graph))
                .max()
                .unwrap_or(1);
            self.graph.graph_column_width = self.graph.graph_column_width.max(added_width);
        }
        self.refresh_uncommitted_row();
        if result.request.follow_head {
            if let Some(head) = self.graph.commits.iter().position(is_head)
                && let Some(pos) = self.graph.visible.iter().position(|&index| index == head)
            {
                self.select_visible_commit_row(pos);
            } else {
                self.select_first_commit_row();
            }
        } else if on_uncommitted && self.graph.showing_uncommitted() {
            self.graph.selected = 0;
        } else if let Some(sha) = &selected {
            if let Some(pos) = self
                .graph
                .visible
                .iter()
                .position(|&index| self.graph.commits[index].sha == *sha)
            {
                self.select_visible_commit_row(pos);
            } else {
                self.select_first_commit_row();
            }
        } else {
            self.select_first_commit_row();
        }
        self.ensure_history_selection_visible();
        self.refresh_command_selection_context();
        let next = self.graph.selected_commit().map(|c| c.sha.clone());
        if selected != next || self.inspect.details.is_none() {
            self.reset_commit_inspection();
            self.request_commit_details();
        } else if result.request.replace {
            if let Some(commit) = self.graph.selected_commit()
                && let Some(details) = self.inspect.details.as_mut()
                && details.commit.sha == commit.sha
            {
                details.commit = commit.clone();
                self.inspect.commit_change_selected = self
                    .inspect
                    .commit_change_selected
                    .min(details.changes.len().saturating_sub(1));
                self.foreground.reads.details.advance();
                self.inspect.pending_commit_details = None;
                if self
                    .inspect
                    .commit_preview_path
                    .as_ref()
                    .is_some_and(|path| !details.changes.iter().any(|change| &change.path == path))
                {
                    self.close_commit_preview();
                }
            } else {
                self.request_commit_details();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::graph::GraphState;
    use crate::ui::test_support::{committed_change, render};

    use super::*;

    #[test]
    fn page_completion_preserves_selection_and_does_not_prefetch_again() {
        let (root, mut app) = committed_change("page-append");
        app.shell.active_tab = ActiveTab::History;
        let commit = app.graph.commits[0].clone();
        app.graph = GraphState::new(vec![commit.clone()], true);
        app.graph.history_has_more = true;
        render(&mut app, 100, 30);
        let request = HistoryRequest {
            id: RequestId::FIRST,
            generation: app.history.generation,
            path: app.repository.as_ref().unwrap().root().to_owned(),
            offset: 1,
            count: 100,
            replace: false,
            intent: RefreshIntent::Interaction,
            follow_head: false,
            start: None,
        };
        app.history.pending = Some(request.clone());
        let selected_sha = app.graph.selected_commit().unwrap().sha.clone();
        assert!(app.apply_history_page(HistoryResult {
            elapsed: Duration::from_millis(420),
            request,
            page: Ok(HistoryPage {
                commits: vec![commit],
                offset: 1,
                has_more: true
            })
        }));
        assert_eq!(app.graph.commits.len(), 2);
        assert_eq!(app.graph.selected, app.graph.display_offset());
        assert_eq!(app.graph.selected_commit().unwrap().sha, selected_sha);
        assert!(app.history.pending.is_none());
        assert_eq!(
            app.history.last_load_duration,
            Some(Duration::from_millis(420))
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_filter_can_load_more_by_page_down_and_mouse_without_duplicates() {
        use super::super::{
            overlay::Overlay,
            test_support::{click, press},
        };
        use crossterm::event::KeyCode;
        let (root, mut app) = committed_change("filtered-page-input");
        app.shell.active_tab = ActiveTab::History;
        app.graph.history_has_more = true;
        app.graph.query.text = "no-such-subject".to_owned();
        app.apply_filter();
        app.overlay = Overlay::GraphFilter;
        let (tx, rx) = mpsc::channel();
        app.history.tx = tx;
        render(&mut app, 100, 30);
        assert!(app.graph.visible.is_empty());
        app.request_more_history(false);
        assert!(rx.try_recv().is_err());
        press(&mut app, KeyCode::PageDown);
        assert!(matches!(rx.try_recv(), Ok(Work::Read(_))));
        let area = app.graph.load_more_area;
        click(&mut app, area.x, area.y);
        assert!(rx.try_recv().is_err(), "one request at a time");
        app.history.pending = None;
        click(&mut app, area.x, area.y);
        assert!(matches!(rx.try_recv(), Ok(Work::Read(_))));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn polling_replacement_waits_for_overlay_and_restores_sha_with_fresh_refs() {
        use super::super::overlay::Overlay;
        use crate::git::{CommitRef, CommitRefKind};
        let (root, mut app) = committed_change("history-overlay-replacement");
        app.shell.active_tab = ActiveTab::History;
        let mut selected = app.graph.commits[0].clone();
        selected.refs.push(CommitRef {
            name: "new-tag".to_owned(),
            kind: CommitRefKind::Tag,
        });
        let mut newer = selected.clone();
        newer.sha = "0000000000000000000000000000000000000000".to_owned();
        let request = HistoryRequest {
            id: RequestId::FIRST,
            generation: app.history.generation,
            path: app.active_path.clone(),
            offset: 0,
            count: 2,
            replace: true,
            intent: RefreshIntent::Polling,
            follow_head: false,
            start: None,
        };
        app.history.pending = Some(request.clone());
        app.history.deferred = Some(HistoryResult {
            elapsed: Duration::from_millis(420),
            request,
            page: Ok(HistoryPage {
                commits: vec![newer, selected.clone()],
                offset: 0,
                has_more: true,
            }),
        });
        app.overlay = Overlay::GraphFilter;
        assert!(!app.receive_history());
        assert_eq!(app.graph.commits.len(), 1);
        app.overlay = Overlay::None;
        assert!(app.receive_history());
        assert_eq!(app.graph.selected, 1 + app.graph.display_offset());
        assert_eq!(app.graph.selected_commit().unwrap().sha, selected.sha);
        assert_eq!(
            app.inspect.details.as_ref().unwrap().commit.refs,
            selected.refs
        );
        assert!(app.history.pending.is_none());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_page_and_failed_page_leave_loaded_rows_in_place() {
        let (root, mut app) = committed_change("page-stale");
        let original = app.graph.commits.clone();
        app.history.last_load_duration = Some(Duration::from_millis(100));
        let request = HistoryRequest {
            id: RequestId::FIRST,
            generation: app.history.generation,
            path: app.active_path.clone(),
            offset: 1,
            count: 100,
            replace: false,
            intent: RefreshIntent::Interaction,
            follow_head: false,
            start: None,
        };
        assert!(!app.apply_history_page(HistoryResult {
            elapsed: Duration::from_millis(420),
            request: request.clone(),
            page: Ok(HistoryPage {
                commits: Vec::new(),
                offset: 1,
                has_more: false
            })
        }));
        app.history.pending = Some(request.clone());
        assert!(app.apply_history_page(HistoryResult {
            elapsed: Duration::from_millis(420),
            request,
            page: Err(ReadError::Diagnostic("read failed".to_owned()))
        }));
        assert_eq!(app.graph.commits, original);
        assert_eq!(
            app.history.last_load_duration,
            Some(Duration::from_millis(100))
        );
        assert!(app.history.error.as_ref().unwrap().contains("Refresh"));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}

const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
struct MaintenanceRequest {
    id: RequestId,
    path: PathBuf,
    refs: Option<u64>,
}

struct MaintenanceResult {
    request: MaintenanceRequest,
    retry: Result<bool, ReadError>,
}

pub(super) struct MaintenanceLane {
    tx: Sender<Option<MaintenanceRequest>>,
    rx: Receiver<MaintenanceResult>,
    shutdown: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    pending: Option<MaintenanceRequest>,
    last_context: Option<(PathBuf, Option<u64>)>,
    retry_at: Option<Instant>,
    next_id: RequestId,
    pub enabled: bool,
    pub started: Instant,
    pub notice: Option<(PathBuf, String)>,
}

impl MaintenanceLane {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Option<MaintenanceRequest>>();
        let (result_tx, result_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicU64::new(0));
        let cancellation = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("herdr-git-maintenance".to_owned())
            .spawn(move || {
                let mut attempted = std::collections::HashMap::<PathBuf, Instant>::new();
                while let Ok(Some(request)) = rx.recv() {
                    if cancellation.load(Ordering::Acquire) != 0 {
                        break;
                    }
                    let retry = with_read_cancellation_result(Arc::clone(&cancellation), 0, || {
                        let repository = Repository::at_root(request.path.clone());
                        let Some(target) = repository.history_maintenance_target()? else {
                            return Ok(false);
                        };
                        if attempted
                            .get(&target.common_dir)
                            .is_some_and(|last| last.elapsed() < MAINTENANCE_INTERVAL)
                        {
                            return Ok(true);
                        }
                        attempted.insert(target.common_dir.clone(), Instant::now());
                        repository.maintain_history(&target)?;
                        Ok(repository
                            .history_maintenance_target()?
                            .is_some_and(|target| !target.has_graph))
                    });
                    if result_tx
                        .send(MaintenanceResult { request, retry })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tx,
            rx: result_rx,
            shutdown,
            thread: Some(thread),
            pending: None,
            last_context: None,
            retry_at: None,
            next_id: RequestId::FIRST,
            enabled: false,
            started: Instant::now(),
            notice: None,
        })
    }

    pub fn running_for(&self, path: &std::path::Path) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|request| request.path == path)
    }

    pub fn tick(&mut self, path: Option<PathBuf>, refs: Option<u64>) -> bool {
        if !self.enabled {
            return false;
        }
        let mut changed = false;
        while let Ok(result) = self.rx.try_recv() {
            if self.pending.as_ref() != Some(&result.request) {
                continue;
            }
            self.pending = None;
            changed = true;
            self.retry_at = match result.retry {
                Ok(false) => {
                    self.notice = None;
                    None
                }
                Ok(true) => Some(Instant::now() + MAINTENANCE_INTERVAL),
                Err(ReadError::Cancelled) => None,
                Err(ReadError::Diagnostic(error)) => {
                    self.notice = Some((
                        result.request.path,
                        format!("History optimization unavailable: {error}; retrying later"),
                    ));
                    Some(Instant::now() + MAINTENANCE_INTERVAL)
                }
            };
        }
        let Some(path) = path else {
            return changed;
        };
        if self.pending.is_some() {
            return changed;
        }
        let context = (path.clone(), refs);
        let same_context = self.last_context.as_ref() == Some(&context);
        if same_context && self.retry_at.is_none_or(|at| Instant::now() < at) {
            return changed;
        }
        if self
            .last_context
            .as_ref()
            .is_some_and(|(previous, _)| previous == &path)
            && self.started.elapsed() < MAINTENANCE_INTERVAL
        {
            self.retry_at = Some(self.started + MAINTENANCE_INTERVAL);
            return changed;
        }
        let request = MaintenanceRequest {
            id: self.next_id.take_and_advance(),
            path,
            refs,
        };
        self.last_context = Some(context);
        self.retry_at = None;
        self.started = Instant::now();
        if self.tx.send(Some(request.clone())).is_ok() {
            self.pending = Some(request);
            true
        } else {
            changed
        }
    }
}

impl Drop for MaintenanceLane {
    fn drop(&mut self) {
        self.shutdown.store(1, Ordering::Release);
        let _ = self.tx.send(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod maintenance_tests {
    use crate::ui::test_support::committed_change;

    use super::*;

    #[test]
    fn maintenance_waits_for_first_frame_and_coalesces_unchanged_context() {
        let (root, mut app) = committed_change("maintenance-scheduling");
        let path = app.active_path.clone();
        assert!(!app.maintenance.tick(Some(path.clone()), Some(1)));
        app.maintenance.enabled = true;
        assert!(app.maintenance.tick(Some(path.clone()), Some(1)));
        assert!(!app.maintenance.tick(Some(path.clone()), Some(2)));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while app.maintenance.pending.is_some() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            app.maintenance.tick(Some(path.clone()), Some(1));
        }
        assert!(app.maintenance.pending.is_none());
        assert!(!app.maintenance.tick(Some(path.clone()), Some(1)));
        assert!(!app.maintenance.tick(Some(path.clone()), Some(2)));
        app.maintenance.started = Instant::now() - MAINTENANCE_INTERVAL;
        assert!(app.maintenance.tick(Some(path), Some(2)));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
