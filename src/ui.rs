use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};
use std::{io, thread};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::git::{LocalIdentity, Repository};
use crate::project::ProjectRegistry;

use commands::{CommandContext, OperationsState};
use diff::DiffState;
#[cfg(test)]
use effect::{
    ActiveRefresh, ForegroundRequest, ReadCancellations, RefreshRequest, RefreshResult,
    load_repository_snapshot, run_foreground_job, run_refresh_job,
};
use effect::{
    BlameTarget, DiffReadTarget, ForegroundResult, KnownFingerprint, RefreshIntent, RefreshProgress,
};
use files::FilesState;
use graph::{CurveLayer, GraphState};
use inspect::InspectState;
use lanes::{ForegroundLane, RefreshLane};
use overlay::Overlay;
use review::ReviewState;
use shell::{ActiveTab, DEFAULT_ACTIVE_TAB, PaneFocus, ShellState, default_focus, is_wheel_event};
use widgets::ScrollbarDrag;
use workspaces::WorkspacesState;

mod commands;
mod comparison;
mod diff;
mod effect;
mod explorer;
mod files;
mod graph;
mod history;
mod inspect;
mod lanes;
mod line_history;
mod overlay;
mod path_tree;
mod review;
mod session;
mod settings;
mod shell;
mod syntax;
#[cfg(test)]
mod test_support;
mod theme;
mod update;
mod view;
mod widgets;
mod workspaces;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollbarOwner {
    History,
    CommitPreview,
    Diff,
}

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const AUTO_FETCH_INTERVAL: Duration = Duration::from_secs(60);
type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Debug)]
enum PendingClipboard {
    Sha(String),
    Selection(String),
}

pub fn run(path: &Path) -> Result<(), String> {
    let mut terminal = start_terminal()?;
    let path = path.to_owned();

    let result = (|| {
        let (app_tx, app_rx) = mpsc::channel();
        thread::Builder::new()
            .name("herdr-git-startup".to_owned())
            .spawn(move || {
                let result = ProjectRegistry::from_environment()
                    .and_then(|registry| App::load_path(&path, registry));
                let _ = app_tx.send(result);
            })
            .map_err(|error| format!("Could not start Git loader: {error}"))?;
        let loading_started = Instant::now();
        let mut app = loop {
            match app_rx.try_recv() {
                Ok(result) => break result?,
                Err(mpsc::TryRecvError::Disconnected) => {
                    break Err("Git loader stopped before initialization completed".to_owned())?;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            terminal
                .draw(|frame| shell::draw_loading(frame, loading_started.elapsed()))
                .map_err(|error| error.to_string())?;
            if event::poll(Duration::from_millis(100)).map_err(|error| error.to_string())?
                && matches!(
                    event::read().map_err(|error| error.to_string())?,
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('q') | KeyCode::Esc,
                        kind: KeyEventKind::Press,
                        ..
                    })
                )
            {
                return Ok(());
            }
        };
        app.curves = CurveLayer::connect(crate::herdr::pane_graphics_enabled());
        app.check_for_update();
        let mut dirty = true;
        loop {
            if sync_terminal_size(&mut terminal).map_err(|error| error.to_string())? {
                app.curves.reconnect();
                dirty = true;
            }
            dirty |= app.maybe_auto_refresh();
            dirty |= app.sync_shortcut_hints();
            if dirty || app.animating() {
                terminal
                    .draw(|frame| app.draw(frame))
                    .map_err(|error| error.to_string())?;
                dirty = app.present_graph_curves();
                app.maintenance.enabled = true;
            }
            if event::poll(app.input_poll_timeout()).map_err(|error| error.to_string())? {
                let next = event::read().map_err(|error| error.to_string())?;
                dirty = true;
                if matches!(next, Event::Resize(..)) {
                    terminal.autoresize().map_err(|error| error.to_string())?;
                    app.curves.reconnect();
                }
                let wheel_burst = is_wheel_event(&next);
                let mut close = app.handle(next)?;
                if wheel_burst {
                    let mut applied = 1;
                    for _ in 0..256 {
                        if !event::poll(Duration::ZERO).map_err(|error| error.to_string())? {
                            break;
                        }
                        let queued = event::read().map_err(|error| error.to_string())?;
                        if is_wheel_event(&queued) {
                            if applied < 4 {
                                close |= app.handle(queued)?;
                                applied += 1;
                            }
                            continue;
                        }
                        close |= app.handle(queued)?;
                        break;
                    }
                }
                if close {
                    break Ok(());
                }
                if app.update.restart_requested {
                    let executable = app.update.executable.clone().expect("restart executable");
                    let cwd = app.active_path.clone();
                    drop(app);
                    stop_terminal(&mut terminal);
                    use std::os::unix::process::CommandExt;
                    let error = std::process::Command::new(executable)
                        .current_dir(cwd)
                        .exec();
                    break Err(format!("Could not restart Herdr Git: {error}"));
                }
                if let Some(copy) = app.pending_clipboard.take() {
                    match copy {
                        PendingClipboard::Sha(value) => {
                            let result = shell::write_osc52(terminal.backend_mut(), &value);
                            app.finish_copy_sha(&value, result);
                        }
                        PendingClipboard::Selection(value) => {
                            let result = shell::write_osc52(terminal.backend_mut(), &value);
                            app.finish_copy_selection(result);
                        }
                    }
                }
            }
        }
    })();

    stop_terminal(&mut terminal);
    result
}

fn sync_terminal_size<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<bool> {
    let previous = terminal.get_frame().area();
    terminal.autoresize()?;
    Ok(terminal.get_frame().area() != previous)
}

fn start_terminal() -> Result<TuiTerminal, String> {
    enable_raw_mode().map_err(|error| error.to_string())?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags()),
        event::EnableMouseCapture,
        EnableBracketedPaste
    )
    .map_err(|error| error.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|error| error.to_string())
}

fn stop_terminal(terminal: &mut TuiTerminal) {
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        event::DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .ok();
    disable_raw_mode().ok();
    terminal.show_cursor().ok();
}

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
}

struct App {
    repository: Option<Repository>,
    active_path: PathBuf,
    local_identity: Option<LocalIdentity>,
    github_origin: bool,
    shell: ShellState,
    update: update::UpdateState,
    settings: settings::SettingsState,
    workspaces: WorkspacesState,
    graph: GraphState,
    curves: CurveLayer,
    history: history::HistoryLane,
    maintenance: history::MaintenanceLane,
    inspect: InspectState,
    files: FilesState,
    explorer: explorer::ExplorerState,
    comparison: comparison::ComparisonState,
    diff: DiffState,
    review: ReviewState,
    ops: OperationsState,
    overlay: Overlay,
    focus: PaneFocus,
    scrollbar_drag: Option<(ScrollbarOwner, ScrollbarDrag)>,
    repository_fingerprint: KnownFingerprint,
    refresh: RefreshLane,
    foreground: ForegroundLane,
    pending_clipboard: Option<PendingClipboard>,
}

impl App {
    #[cfg(test)]
    fn load(repository: Repository) -> Result<Self, String> {
        let root = repository.root().to_owned();
        Self::load_registered(&root, ProjectRegistry::load(None)?)
    }

    #[cfg(test)]
    fn load_registered(path: &Path, registry: ProjectRegistry) -> Result<Self, String> {
        let mut app = Self::load_path(path, registry)?;
        app.settle_startup();
        Ok(app)
    }

    #[cfg(test)]
    fn settle_startup(&mut self) {
        test_support::wait_for_diff(self);
        test_support::wait_for_refresh(self);
        if self.repository.is_some() {
            self.request_history();
            test_support::wait_for_history(self);
            self.load_details();
        }
    }

    fn load_path(path: &Path, registry: ProjectRegistry) -> Result<Self, String> {
        let repository = Repository::discover(path).ok();
        Self::build(repository, path.to_owned(), registry)
    }

    fn build(
        repository: Option<Repository>,
        invoking_path: PathBuf,
        project_registry: ProjectRegistry,
    ) -> Result<Self, String> {
        let syntax = Arc::new(OnceLock::new());
        let refresh = RefreshLane::start(Arc::clone(&syntax))?;
        let foreground = ForegroundLane::start(syntax)?;
        let local_identity = repository.as_ref().and_then(Repository::local_identity);
        let github_origin = repository
            .as_ref()
            .is_some_and(Repository::origin_is_github);
        let changes = repository
            .as_ref()
            .map(Repository::working_changes)
            .transpose()?
            .unwrap_or_default();
        let active_path = repository
            .as_ref()
            .map(|repository| repository.root().to_owned())
            .unwrap_or_else(|| invoking_path.clone());
        let mut app = Self {
            repository,
            active_path,
            local_identity,
            github_origin,
            shell: ShellState::new(invoking_path),
            update: update::UpdateState::from_environment(),
            settings: settings::SettingsState::default(),
            workspaces: WorkspacesState::new(project_registry),
            graph: GraphState::new(Vec::new(), false),
            curves: CurveLayer::disabled(),
            history: history::HistoryLane::start()?,
            maintenance: history::MaintenanceLane::start()?,
            inspect: InspectState::default(),
            files: FilesState::new(changes),
            explorer: explorer::ExplorerState::default(),
            comparison: comparison::ComparisonState::default(),
            diff: DiffState::default(),
            review: ReviewState::default(),
            ops: OperationsState::new(CommandContext::default()),
            overlay: Overlay::None,
            focus: default_focus(DEFAULT_ACTIVE_TAB),
            scrollbar_drag: None,
            repository_fingerprint: KnownFingerprint::default(),
            refresh,
            foreground,
            pending_clipboard: None,
        };
        app.refresh_command_selection_context();
        if let Some(change) = app.files.changes.first()
            && let Some(target) = change.section.diff_target()
        {
            app.diff.diff_target = target;
        }
        if app.repository.is_some() {
            app.request_file_diff();
        } else {
            app.show_non_repository_diff();
        }
        app.refresh.pending = true;
        app.refresh.pending_intent = RefreshIntent::Interaction;
        app.refresh.progress = Some(RefreshProgress {
            started: Instant::now(),
            running: "Refreshing repository".to_owned(),
        });
        Ok(app)
    }

    fn handle(&mut self, input: Event) -> Result<bool, String> {
        if self.handle_alt_modifier(&input) {
            return Ok(false);
        }
        self.track_pointer(&input);
        if self.skip_repeated_selection_wheel(&input) {
            return Ok(false);
        }
        if self.handle_overlay(&input)? {
            return Ok(false);
        }
        if self.handle_comparison_controls(&input) {
            return Ok(false);
        }
        match input {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if self.handle_shortcut(key) || self.handle_workspaces_key(key) {
                    return Ok(false);
                }
                if self.focus == PaneFocus::Files && self.handle_files_key(key)? {
                    return Ok(false);
                }
                if self.shell.active_tab == ActiveTab::Changes && self.handle_diff_key(key)? {
                    return Ok(false);
                }
                if self.shell.active_tab == ActiveTab::Files && self.handle_explorer_key(key) {
                    return Ok(false);
                }
                if self.handle_settings_key(key) {
                    return Ok(false);
                }
                if self.handle_review_key(key) {
                    return Ok(false);
                }
                if self.shell.active_tab == ActiveTab::History
                    && (self.handle_inspect_key(key) || self.handle_graph_key(key))
                {
                    return Ok(false);
                }
                Ok(self.handle_shell_key(key))
            }
            Event::Mouse(mouse) => {
                if self.shell.active_tab == ActiveTab::Files && self.handle_explorer_mouse(mouse) {
                    return Ok(false);
                }
                if self.handle_settings_mouse(mouse) {
                    return Ok(false);
                }
                if self.shell.active_tab == ActiveTab::Changes
                    && (self.handle_files_mouse(mouse)? || self.handle_diff_mouse(mouse)?)
                {
                    return Ok(false);
                }
                if self.shell.active_tab == ActiveTab::History
                    && (self.handle_inspect_mouse(mouse)
                        || self.handle_graph_actions_mouse(mouse)
                        || self.handle_graph_mouse(mouse))
                {
                    return Ok(false);
                }
                if self.handle_workspaces_mouse(mouse)
                    || self.handle_quick_actions_mouse(mouse)
                    || self.handle_shell_mouse(mouse)
                {
                    return Ok(false);
                }
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
                    self.release_pointer();
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn release_pointer(&mut self) {
        self.diff.resize_drag = None;
        self.scrollbar_drag = None;
        if self.review.is_dragging() {
            self.finish_visual_selection();
        }
        self.finish_tree_drag();
    }

    fn end_scrollbar_drag(&mut self, owner: ScrollbarOwner) {
        if self
            .scrollbar_drag
            .is_some_and(|(active, _)| active == owner)
        {
            self.scrollbar_drag = None;
        }
    }

    fn receive_foreground_results(&mut self) -> bool {
        let mut received = false;
        while let Ok(result) = self.foreground.result_rx.try_recv() {
            received = true;
            match result {
                ForegroundResult::RebasePlan { id, path, result } => {
                    self.apply_rebase_plan(id, path, result)
                }
                ForegroundResult::RebaseRun { id, path, result } => {
                    self.apply_rebase_run(id, path, result)
                }
                ForegroundResult::Operation {
                    id,
                    path,
                    command,
                    operation,
                    result,
                } => self.apply_operation(id, path, command, operation, result),
                ForegroundResult::ComparisonTargets { id, path, result } => {
                    self.apply_comparison_targets(id, path, result)
                }
                ForegroundResult::BranchTargets {
                    id,
                    path,
                    command,
                    result,
                } => self.apply_branch_targets(id, path, command, result),
                ForegroundResult::ResetContext { id, path, result } => {
                    self.apply_reset_context(id, path, result)
                }
                ForegroundResult::AmendMessageLoaded { id, path, result } => {
                    self.apply_amend_message(id, path, result)
                }
                ForegroundResult::AgentPanesListed { id, path, result } => {
                    self.apply_agent_panes(id, path, result)
                }
                ForegroundResult::AgentRequestSent {
                    id,
                    path,
                    agent,
                    result,
                } => self.apply_agent_request(id, path, agent, result),
                ForegroundResult::ValidateName {
                    id,
                    path,
                    command,
                    name,
                    result,
                } => self.apply_name_validation(id, path, command, name, result),
                ForegroundResult::Staging {
                    id,
                    path,
                    operation,
                    result,
                } => self.apply_staging(id, path, operation, result),
                ForegroundResult::CommitDetails {
                    id,
                    generation,
                    target,
                    result,
                } => self.apply_commit_details(id, generation, target, *result),
                ForegroundResult::Diff {
                    id,
                    generation,
                    owner,
                    path,
                    file,
                    target,
                    fold_toggles,
                    result,
                } => self.apply_diff_read(
                    id,
                    generation,
                    DiffReadTarget {
                        owner,
                        path,
                        file,
                        target,
                        fold_toggles,
                    },
                    result,
                ),
                ForegroundResult::Blame {
                    id,
                    generation,
                    path,
                    file,
                    line,
                    revision,
                    result,
                } => self.apply_blame(
                    id,
                    generation,
                    BlameTarget {
                        path,
                        file,
                        line,
                        revision,
                    },
                    result,
                ),
                ForegroundResult::SelectionBlame {
                    id,
                    generation,
                    target,
                    result,
                } => self.apply_selection_blame(id, generation, target, result),
                ForegroundResult::LineHistory {
                    id,
                    generation,
                    result,
                } => self.apply_line_history(id, generation, result),
                ForegroundResult::RepositoryFiles {
                    id,
                    generation,
                    root,
                    result,
                } => self.apply_repository_files(id, generation, root, result),
                ForegroundResult::FilePreview {
                    id,
                    generation,
                    root,
                    file,
                    result,
                } => self.apply_file_preview(id, generation, root, file, result),
                ForegroundResult::ProjectMutation { id, result } => {
                    self.apply_project_mutation(id, result)
                }
                ForegroundResult::Switch {
                    id,
                    generation,
                    path,
                    result,
                } => self.apply_switch(id, generation, path, result),
                ForegroundResult::Fetch { id } => self.apply_fetch(id),
            }
        }
        self.start_pending_switch();
        self.start_pending_blame();
        received
    }

    fn input_poll_timeout(&self) -> Duration {
        if self.foreground.action.as_ref().is_some_and(|action| {
            matches!(
                action.kind,
                crate::ui::lanes::ForegroundKind::BranchTargets { .. }
            )
        }) || self.explorer.search_pending()
        {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(100)
        }
    }

    fn maybe_auto_refresh(&mut self) -> bool {
        let mut changed = self.receive_foreground_results();
        changed |= self.poll_update();
        changed |= self.receive_history();
        changed |= self.tick_explorer_search();
        changed |= self.reveal_pending_commit();
        changed |= self.maintenance.tick(
            self.repository.as_ref().map(|repo| repo.root().to_owned()),
            self.repository_fingerprint.refs,
        );
        changed |= self.expire_result();
        changed |= self.receive_refresh_result();
        changed |= self.receive_workspace_results();
        changed |= self.apply_workspace_refresh();
        let result_can_apply = self
            .refresh
            .deferred_result
            .as_ref()
            .is_none_or(|result| !self.refresh_is_transient(result.intent));
        if result_can_apply {
            changed |= self.apply_deferred_refresh();
        }
        if self.refresh.last_check.elapsed() >= AUTO_REFRESH_INTERVAL && !self.refresh.pending {
            self.refresh.pending = true;
            self.refresh.pending_intent = RefreshIntent::Polling;
            self.refresh.scope = crate::ui::effect::RefreshScope::Repository;
            self.refresh.inspect_workspaces = true;
        }
        let pending_is_transient = self.refresh_is_transient(self.refresh.pending_intent);
        if self.refresh.pending
            && !self.refresh.in_flight
            && self.refresh.deferred_result.is_none()
            && !pending_is_transient
        {
            self.start_auto_refresh();
            changed = true;
        }
        changed |= self.request_dwelt_blame();
        changed |= self.sync_selection_blame();
        changed
    }

    fn animating(&self) -> bool {
        self.foreground.action.is_some()
            || self.history.pending.is_some()
            || self.maintenance.running_for(&self.active_path)
            || self.workspaces.pending_switch.is_some()
            || self.diff.blame_in_progress()
            || self.inspect.pending_commit_details.is_some()
            || self.pending_commit_preview().is_some()
            || self.refresh.running_progress().is_some()
            || self.overlay.animating()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{
        Arc,
        atomic::AtomicU64,
        mpsc::{self, TryRecvError},
    };
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::commands::{
        ActionDialog, AgentPicker, CommandPalette, NamePrompt, OperationResultView,
        RESULT_VISIBLE_DURATION, ResetFlow, STASH_COMMANDS,
    };
    use super::overlay::{ContextMenu, CopyShaDialog, Overlay};
    use super::shell::{ActiveTab, PaneFocus};
    use super::test_support::{
        buffer_text, git, intercept_foreground, offline_app, press, render, temp_registry,
        temp_repo, wait_for_diff, wait_for_foreground, wait_for_refresh,
    };
    use super::{
        AUTO_FETCH_INTERVAL, AUTO_REFRESH_INTERVAL, ActiveRefresh, App, ForegroundRequest,
        ForegroundResult, ReadCancellations, RefreshResult, keyboard_enhancement_flags,
        sync_terminal_size,
    };
    use crate::git::DiffTarget;
    use crate::git::GitOperation;
    use crate::git::ReadError;
    use crate::git::Repository;
    use crate::git::RepositoryFingerprint;
    use crate::git::ResetTarget;
    use crate::herdr::HerdrAgent;
    use crate::ui::commands::CommandId;
    use crate::ui::effect::KnownFingerprint;
    use crate::ui::effect::ReadGeneration;
    use crate::ui::effect::RefreshScope;
    use crate::ui::effect::RequestId;
    use crate::ui::lanes::ForegroundAction;
    use crate::ui::lanes::ForegroundKind;
    use crate::ui::syntax::DiffDocument;
    use crate::ui::syntax::SyntaxHighlighter;
    use crossterm::event::{KeyCode, KeyboardEnhancementFlags};

    fn fingerprint(value: u64) -> RepositoryFingerprint {
        RepositoryFingerprint {
            refs: value,
            worktree: value,
        }
    }

    #[test]
    fn superseded_refresh_stops_before_publishing_a_result() {
        let root = temp_repo("refresh-cancel");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let request = super::RefreshRequest {
            scope: RefreshScope::Repository,
            inspect_workspaces: true,
            id: RequestId::new(1),
            intent: super::RefreshIntent::Interaction,
            context: app.refresh_context(),
            previous_fingerprint: app.repository_fingerprint,
            previous_highlight: app.diff.highlight_key,
            previous_worktrees: app.workspaces.inspected_worktrees(),
            changes: app.files.changes.clone(),
        };
        app.advance_refresh_generation();
        let (workspace_tx, workspace_rx) = mpsc::channel();
        let result = super::run_refresh_job(
            request,
            &SyntaxHighlighter::new(),
            &app.refresh.cancellation_generation,
            &workspace_tx,
        );
        assert!(result.cancelled);
        assert!(workspace_rx.try_recv().is_err());
        assert!(result.active.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graph_changes_and_blame_reads_enqueue_without_waiting() {
        let root = temp_repo("view-async");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "one\ntwo changed\nthree\n").unwrap();

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        app.graph.history_loaded = false;

        app.set_tab(ActiveTab::History);
        let graph = refresh_rx.try_recv().expect("Graph read request");
        assert!(graph.context.wants_history);
        assert!(!graph.context.history_loaded);
        assert!(graph.previous_fingerprint.refs.is_none());
        assert!(
            graph.previous_fingerprint.worktree.is_some(),
            "opening the Graph re-reads only the refs half"
        );
        assert!(app.refresh.workspaces_pending);
        assert!(app.refresh.active_progress().is_none());

        app.refresh.in_flight = false;
        app.refresh.pending = false;
        app.refresh.progress = None;
        app.set_tab(ActiveTab::Changes);
        let changes = refresh_rx.try_recv().expect("Changes read request");
        assert!(!changes.context.wants_history);
        assert!(changes.previous_fingerprint.worktree.is_none());

        let (foreground_rx, _foreground_result_tx) = intercept_foreground(&mut app);
        let row = (0..app.diff.diff_document.len())
            .find(|&row| app.diff.diff_document.after_line_number(row).is_some())
            .expect("diff line");
        app.diff.focused_diff_row = row;
        app.load_blame_for_row(row);
        assert!(matches!(
            foreground_rx.try_recv().expect("Blame read request"),
            super::ForegroundRequest::Blame { path, file, .. }
                if path == fs::canonicalize(&root).unwrap() && file == "tracked.txt"
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_activation_enqueues_only_a_cancellable_diff_read() {
        let root = temp_repo("file-diff");
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
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        let (foreground_rx, _foreground_result_tx) = intercept_foreground(&mut app);

        let b_row = app
            .files
            .tree_rows
            .iter()
            .position(|row| row.label == "b.txt")
            .expect("b.txt row");
        app.select_tree(b_row);

        assert!(refresh_rx.try_recv().is_err());
        assert!(matches!(
            foreground_rx.try_recv().expect("file diff request"),
            super::ForegroundRequest::Diff {
                path,
                file,
                generation,
                ..
            } if path == fs::canonicalize(&root).unwrap()
                && file == "b.txt"
                && generation == app.foreground.reads.diff.generation
        ));
        let buffer = render(&mut app, 120, 30);
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Loading diff"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn superseded_file_diff_stops_before_reading_or_highlighting() {
        let cancellations = ReadCancellations {
            diff: Arc::new(AtomicU64::new(2)),
            ..ReadCancellations::default()
        };
        let request = super::ForegroundRequest::Diff {
            id: RequestId::new(7),
            generation: ReadGeneration::new(1),
            owner: super::effect::DiffOwner::Changes,
            path: PathBuf::from("/unused"),
            file: "unused.txt".to_owned(),
            target: DiffTarget::WorkingTreeAgainstIndex,
            fold_toggles: HashSet::new(),
        };

        let result = super::run_foreground_job(
            request,
            &super::effect::SharedSyntax::default(),
            &cancellations,
        );

        assert!(matches!(
            result,
            super::ForegroundResult::Diff {
                result: Err(ReadError::Cancelled),
                ..
            }
        ));
    }

    #[test]
    fn completed_fetch_starts_and_applies_local_refresh_while_result_is_visible() {
        let root = temp_repo("fetch-sync");
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (operation_rx, operation_result_tx) = intercept_foreground(&mut app);
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        app.refresh.last_remote_fetch = Instant::now() - AUTO_FETCH_INTERVAL;

        app.run_quick_action(CommandId::Fetch);
        let (id, path, operation) = match operation_rx.try_recv().unwrap() {
            ForegroundRequest::Operation {
                id,
                path,
                operation,
                ..
            } => (id, path, operation),
            other => panic!("unexpected request: {other:?}"),
        };
        operation_result_tx
            .send(ForegroundResult::Operation {
                id,
                path,
                command: CommandId::Fetch,
                operation,
                result: Ok("Git completed without output.".to_owned()),
            })
            .unwrap();

        app.receive_foreground_results();

        assert!(app.overlay.result().is_some());
        let refresh = refresh_rx
            .try_recv()
            .expect("post-Fetch refresh must start before the result card closes");
        assert_eq!(refresh.intent, super::RefreshIntent::Operation);
        assert!(
            operation_rx.try_recv().is_err(),
            "post-Fetch inspection must not run a duplicate remote Fetch"
        );
        assert!(app.refresh.fetch_in_flight.is_none());
        assert!(app.refresh.last_remote_fetch.elapsed() >= AUTO_FETCH_INTERVAL);
        let mut repository_rows = app.workspaces.repository_rows.clone();
        repository_rows[0].error = Some("Synced".to_owned());
        repository_rows[0].overview = None;
        repository_rows[0].branch_status = None;
        let (workspace_tx, workspace_rx) = mpsc::channel();
        app.refresh.workspace_rx = workspace_rx;
        workspace_tx
            .send(super::effect::WorkspaceRefresh {
                id: refresh.id,
                intent: refresh.intent,
                context: refresh.context.clone(),
                project_statuses: app.workspaces.project_statuses.clone(),
                repository_rows,
            })
            .unwrap();
        refresh_result_tx
            .send(RefreshResult {
                id: refresh.id,
                intent: refresh.intent,
                context: refresh.context,
                cancelled: false,
                active: Some(Ok(ActiveRefresh::Unchanged {
                    fingerprint: fingerprint(777),
                })),
            })
            .unwrap();

        app.maybe_auto_refresh();

        assert!(app.overlay.result().is_some());
        assert_eq!(
            app.workspaces.repository_rows[0].error.as_deref(),
            Some("Synced")
        );
        assert_eq!(
            app.repository_fingerprint,
            KnownFingerprint::of(fingerprint(777))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_refresh_supersedes_a_queued_polling_result_in_the_same_turn() {
        let root = temp_repo("refresh-race");
        git(&root, &["remote", "add", "origin", root.to_str().unwrap()]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (operation_rx, operation_result_tx) = intercept_foreground(&mut app);
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        app.refresh.in_flight = false;
        app.refresh.last_remote_fetch = Instant::now();
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;

        app.maybe_auto_refresh();
        let polling = refresh_rx.try_recv().expect("scheduled polling request");
        assert_eq!(polling.intent, super::RefreshIntent::Polling);

        app.run_quick_action(CommandId::Fetch);
        let (id, path, operation) = match operation_rx.try_recv().unwrap() {
            ForegroundRequest::Operation {
                id,
                path,
                operation,
                ..
            } => (id, path, operation),
            other => panic!("unexpected request: {other:?}"),
        };
        operation_result_tx
            .send(ForegroundResult::Operation {
                id,
                path,
                command: CommandId::Fetch,
                operation,
                result: Ok("Git completed without output.".to_owned()),
            })
            .unwrap();
        refresh_result_tx
            .send(RefreshResult {
                id: polling.id,
                intent: polling.intent,
                context: polling.context,
                cancelled: false,
                active: None,
            })
            .unwrap();

        app.maybe_auto_refresh();

        let operation_refresh = refresh_rx
            .try_recv()
            .expect("operation refresh must supersede queued polling");
        assert_eq!(operation_refresh.intent, super::RefreshIntent::Operation);
        assert!(operation_rx.try_recv().is_err());
        assert!(operation_refresh.id > polling.id);
        assert!(app.refresh.deferred_result.is_none());
        let mut repository_rows = app.workspaces.repository_rows.clone();
        repository_rows[0].error = Some("Operation synced".to_owned());
        repository_rows[0].overview = None;
        repository_rows[0].branch_status = None;
        let (workspace_tx, workspace_rx) = mpsc::channel();
        app.refresh.workspace_rx = workspace_rx;
        workspace_tx
            .send(super::effect::WorkspaceRefresh {
                id: operation_refresh.id,
                intent: operation_refresh.intent,
                context: operation_refresh.context.clone(),
                project_statuses: app.workspaces.project_statuses.clone(),
                repository_rows,
            })
            .unwrap();
        refresh_result_tx
            .send(RefreshResult {
                id: operation_refresh.id,
                intent: operation_refresh.intent,
                context: operation_refresh.context,
                cancelled: false,
                active: Some(Ok(ActiveRefresh::Unchanged {
                    fingerprint: fingerprint(991),
                })),
            })
            .unwrap();

        app.maybe_auto_refresh();

        assert!(!app.refresh.in_flight);
        assert_eq!(
            app.workspaces.repository_rows[0].error.as_deref(),
            Some("Operation synced")
        );
        assert_eq!(
            app.repository_fingerprint,
            KnownFingerprint::of(fingerprint(991))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scheduled_refresh_only_enqueues_one_background_request() {
        let root = temp_repo("refresh-queue");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;
        app.refresh.in_flight = false;
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;

        app.maybe_auto_refresh();
        let first = request_rx.try_recv().expect("background refresh request");
        assert!(app.refresh.in_flight);

        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.maybe_auto_refresh();
        assert_eq!(request_rx.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(app.refresh.latest_id, first.id);

        result_tx
            .send(RefreshResult {
                id: first.id,
                intent: first.intent,
                context: first.context,
                cancelled: false,
                active: Some(Ok(ActiveRefresh::Unchanged {
                    fingerprint: fingerprint(42),
                })),
            })
            .unwrap();
        app.maybe_auto_refresh();
        let second = request_rx
            .try_recv()
            .expect("one coalesced refresh request");
        assert!(second.id > first.id);

        drop(result_tx);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn periodic_fetch_runs_once_per_interval_on_the_mutation_lane_and_outlives_interaction_refreshes()
     {
        let root = temp_repo("auto-fetch");
        let inactive = temp_repo("auto-fetch-inactive");
        let mut registry = temp_registry("auto-fetch-projects");
        registry.add(&inactive).unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::build(Some(repository), inactive.clone(), registry).unwrap();
        let (foreground_rx, foreground_result_tx) = intercept_foreground(&mut app);
        let (request_tx, request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;
        app.refresh.in_flight = false;
        app.refresh.pending_intent = super::RefreshIntent::Polling;
        app.refresh.last_remote_fetch = Instant::now() - AUTO_FETCH_INTERVAL;

        app.start_auto_refresh();
        let ForegroundRequest::Fetch { id, roots } =
            foreground_rx.try_recv().expect("due Fetch request")
        else {
            panic!("unexpected request")
        };
        assert_eq!(roots, vec![fs::canonicalize(&root).unwrap()]);
        assert_eq!(app.refresh.fetch_in_flight, Some(id));
        let polling = request_rx.try_recv().expect("polling refresh request");
        assert_eq!(polling.intent, super::RefreshIntent::Polling);
        let fetch_generation = app.refresh.generation;

        app.request_background_refresh("Loading changes", RefreshScope::Changes);
        assert!(app.refresh.generation > fetch_generation);
        assert_eq!(
            app.refresh.fetch_in_flight,
            Some(id),
            "an interaction refresh leaves the Fetch running"
        );
        app.refresh.in_flight = false;
        app.refresh.pending = false;

        foreground_result_tx
            .send(ForegroundResult::Fetch {
                id: RequestId::new(id.get() + 1),
            })
            .unwrap();
        app.receive_foreground_results();
        assert_eq!(app.refresh.fetch_in_flight, Some(id));
        assert!(!app.refresh.pending);

        foreground_result_tx
            .send(ForegroundResult::Fetch { id })
            .unwrap();
        app.receive_foreground_results();
        assert!(app.refresh.fetch_in_flight.is_none());
        assert!(app.refresh.pending, "a finished Fetch schedules a refresh");
        assert_eq!(app.refresh.pending_intent, super::RefreshIntent::Polling);

        app.start_auto_refresh();
        assert_eq!(
            request_rx.try_recv().map(|request| request.intent),
            Ok(super::RefreshIntent::Polling)
        );
        assert!(
            foreground_rx.try_recv().is_err(),
            "no second Fetch before the interval elapses"
        );

        app.repository = None;
        app.refresh.last_remote_fetch = Instant::now() - AUTO_FETCH_INTERVAL;
        app.start_auto_refresh();
        assert!(
            foreground_rx.try_recv().is_err(),
            "without an active repository, invoking and registered repositories are not fetched"
        );
        assert!(app.refresh.fetch_in_flight.is_none());

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(inactive).unwrap();
    }

    #[test]
    fn build_defers_history_command_context_and_workspaces_to_the_lanes() {
        let root = temp_repo("startup-lanes");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("tracked.txt"), "base\nedited\n").unwrap();
        let projects = (0..3)
            .map(|index| temp_repo(&format!("startup-project-{index}")))
            .collect::<Vec<_>>();
        let mut registry = temp_registry("startup-projects");
        for project in &projects {
            registry.add(project).unwrap();
        }
        let roots = registry.roots().to_vec();
        assert_eq!(roots.len(), 3);
        let repository = Repository::discover(&root).unwrap();

        let mut app = App::build(Some(repository), root.clone(), registry).unwrap();

        assert!(app.workspaces.repository_rows.is_empty());
        assert!(!app.graph.history_loaded);
        assert!(app.graph.commits.is_empty());
        assert!(app.ops.command_context.head_commit.is_none());
        assert!(app.ops.command_context.has_repository);
        assert_eq!(app.files.changes.len(), 1);
        assert!(
            app.diff.pending_diff.is_some(),
            "the first diff goes through the read lane"
        );
        assert!(app.refresh.pending);
        assert_eq!(
            app.refresh.pending_intent,
            super::RefreshIntent::Interaction
        );

        let (request_tx, request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;
        app.maybe_auto_refresh();
        let first = request_rx.try_recv().expect("startup refresh request");
        assert_eq!(first.intent, super::RefreshIntent::Interaction);
        assert_eq!(first.context.project_roots, roots);
        assert_eq!(first.previous_fingerprint, KnownFingerprint::default());
        assert!(!first.context.wants_history);
        assert!(app.refresh.workspaces_pending);
        assert!(buffer_text(&render(&mut app, 120, 40)).contains("Loading Workspaces"));

        fs::remove_dir_all(root).unwrap();
        for project in projects {
            fs::remove_dir_all(project).unwrap();
        }
    }

    #[test]
    fn a_mutation_waiting_on_its_lane_never_delays_a_diff_read() {
        let root = temp_repo("lane-split");
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
        let (mutation_tx, mutation_rx) = mpsc::channel();
        app.foreground.mutation_tx = mutation_tx;

        app.add_project_from_picker();
        assert!(matches!(
            mutation_rx.try_recv(),
            Ok(ForegroundRequest::AddProject { .. })
        ));
        assert!(matches!(
            app.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::AddProject)
        ));

        let b_row = app
            .files
            .tree_rows
            .iter()
            .position(|row| row.label == "b.txt")
            .expect("b.txt row");
        app.select_tree(b_row);
        wait_for_diff(&mut app);

        assert!(app.diff.diff_text.contains("after b"));
        assert!(
            app.foreground.action.is_some(),
            "the Project picker is still waiting on the mutation lane"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scheduled_refresh_waits_for_transient_ui_then_starts_immediately() {
        let root = temp_repo("refresh-defer");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;
        app.refresh.in_flight = false;
        app.overlay = Overlay::Commands(CommandPalette::new());
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;

        app.maybe_auto_refresh();
        assert_eq!(request_rx.try_recv(), Err(TryRecvError::Empty));
        assert!(app.refresh.pending);

        app.overlay = Overlay::None;
        app.maybe_auto_refresh();
        assert!(request_rx.try_recv().is_ok());
        assert!(app.refresh.in_flight);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scheduled_refresh_still_waits_behind_an_operation_result() {
        let root = temp_repo("result-poll");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;
        app.show_result(OperationResultView::operation(
            &GitOperation::Fetch,
            "done",
            false,
        ));
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;

        app.maybe_auto_refresh();

        assert_eq!(request_rx.try_recv(), Err(TryRecvError::Empty));
        assert!(app.refresh.pending);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_scheduled_refresh_never_replaces_current_repository_rows() {
        let root = temp_repo("refresh-stale");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let original_rows = app.workspaces.repository_rows.clone();
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.workspace_rx = result_rx;
        app.refresh.in_flight = false;
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.maybe_auto_refresh();
        let request = request_rx.recv().unwrap();
        let mut stale_rows = original_rows.clone();
        stale_rows[0].path = root.join("stale");
        app.shell.invoking_path = root.join("other-invocation");
        result_tx
            .send(super::effect::WorkspaceRefresh {
                id: request.id,
                intent: request.intent,
                context: request.context,
                project_statuses: Vec::new(),
                repository_rows: stale_rows,
            })
            .unwrap();

        app.receive_workspace_results();
        app.apply_workspace_refresh();
        assert_eq!(app.workspaces.repository_rows, original_rows);
        assert!(app.refresh.pending || app.refresh.in_flight);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_scheduled_refresh_preserves_visible_content() {
        let root = temp_repo("refresh-unchanged");
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        app.refresh.request_tx = request_tx;
        app.refresh.result_rx = result_rx;
        app.refresh.in_flight = false;
        app.diff.diff_text = "keep this diff".to_owned();
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.maybe_auto_refresh();
        let request = request_rx.recv().unwrap();
        result_tx
            .send(RefreshResult {
                id: request.id,
                intent: request.intent,
                context: request.context,
                cancelled: false,
                active: Some(Ok(ActiveRefresh::Unchanged {
                    fingerprint: fingerprint(42),
                })),
            })
            .unwrap();

        app.maybe_auto_refresh();
        assert_eq!(app.diff.diff_text, "keep this diff");
        assert_eq!(
            app.repository_fingerprint,
            KnownFingerprint::of(fingerprint(42))
        );
        assert!(!app.refresh.in_flight);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_refresh_applies_an_external_worktree_change() {
        let root = temp_repo("refresh-change");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        fs::write(root.join("tracked.txt"), "base\nexternal\n").unwrap();
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;

        app.maybe_auto_refresh();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .files
            .changes
            .iter()
            .any(|change| change.path == "tracked.txt")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
            app.maybe_auto_refresh();
        }

        assert!(
            app.files
                .changes
                .iter()
                .any(|change| change.path == "tracked.txt")
        );
        assert!(app.diff.diff_text.contains("external"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worktree_only_refresh_keeps_graph_rows_and_the_unchanged_diff_document() {
        let root = temp_repo("refresh-halves-app");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("a.txt"), "a\n").unwrap();
        fs::write(root.join("b.txt"), "b\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "Base"]);
        fs::write(root.join("a.txt"), "a\nchanged\n").unwrap();
        fs::write(root.join("b.txt"), "b\nchanged\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        assert_eq!(app.files.changes[app.files.change_selected].path, "a.txt");
        let known = app.repository_fingerprint;
        assert!(known.refs.is_some() && known.worktree.is_some());
        app.graph.commits[0].subject = "kept across a worktree-only refresh".to_owned();
        app.diff.diff_document = DiffDocument::plain("kept document");
        let document_text = |app: &App| {
            app.diff.diff_document.after_line(0).map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
        };

        fs::write(root.join("b.txt"), "b\nchanged\nagain\n").unwrap();
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.maybe_auto_refresh();
        wait_for_refresh(&mut app);

        assert_eq!(
            app.graph.commits[0].subject,
            "kept across a worktree-only refresh"
        );
        assert_eq!(document_text(&app).as_deref(), Some("kept document"));
        assert_eq!(app.repository_fingerprint.refs, known.refs);
        assert_ne!(app.repository_fingerprint.worktree, known.worktree);
        let unstaged = app
            .files
            .change_summaries
            .iter()
            .find(|(section, _)| *section == crate::git::ChangeSection::Unstaged)
            .map(|(_, summary)| summary.additions);
        assert_eq!(unstaged, Some(3));

        git(&root, &["commit", "-am", "External commit"]);
        app.refresh.last_check = Instant::now() - AUTO_REFRESH_INTERVAL;
        app.maybe_auto_refresh();
        wait_for_refresh(&mut app);

        assert_eq!(app.graph.commits[0].subject, "External commit");
        assert_eq!(app.files.changes.len(), 2);
        assert!(
            app.files
                .changes
                .iter()
                .all(|change| change.section == crate::git::ChangeSection::Commit)
        );
        assert_ne!(app.repository_fingerprint.refs, known.refs);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn switching_changes_defers_history_and_keeps_the_repository_snapshot() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-ui-fast-switch-{unique}"));
        let current = base.join("current");
        let registered = base.join("registered");
        for root in [&current, &registered] {
            fs::create_dir_all(root).unwrap();
            git(root, &["init", "-b", "main"]);
            git(root, &["config", "user.name", "Test Author"]);
            git(root, &["config", "user.email", "test@example.com"]);
            fs::write(root.join("tracked.txt"), "base\n").unwrap();
            git(root, &["add", "tracked.txt"]);
            git(root, &["commit", "-m", "Base"]);
        }
        let mut registry = temp_registry("removed-current");
        registry.add(&registered).unwrap();
        let mut app = App::load_registered(&current, registry).unwrap();
        assert!(app.graph.history_loaded);
        assert!(app.workspaces.repository_rows[1].error.is_none());

        fs::remove_dir_all(&current).unwrap();
        app.switch_repository_row(1);
        wait_for_foreground(&mut app);

        assert!(!app.graph.history_loaded);
        assert!(app.graph.commits.is_empty());
        assert!(app.workspaces.repository_rows[0].error.is_none());
        assert_eq!(
            app.repository.as_ref().unwrap().root(),
            fs::canonicalize(&registered).unwrap()
        );
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);
        assert!(app.graph.history_loaded);
        assert_eq!(app.graph.commits.len(), 1);

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn switch_completed_while_graph_is_open_loads_the_new_history_in_background() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("herdr-git-graph-switch-{unique}"));
        let current = base.join("current");
        let registered = base.join("registered");
        for (root, subject) in [(&current, "Current"), (&registered, "Registered")] {
            fs::create_dir_all(root).unwrap();
            git(root, &["init", "-b", "main"]);
            git(root, &["config", "user.name", "Test Author"]);
            git(root, &["config", "user.email", "test@example.com"]);
            fs::write(root.join("tracked.txt"), format!("{subject}\n")).unwrap();
            git(root, &["add", "tracked.txt"]);
            git(root, &["commit", "-m", subject]);
        }
        let mut registry = temp_registry("history-after-switch");
        registry.add(&registered).unwrap();
        let mut app = App::load_registered(&current, registry).unwrap();

        app.switch_repository_row(1);
        app.set_tab(ActiveTab::History);
        wait_for_foreground(&mut app);
        assert!(!app.graph.history_loaded);
        assert!(app.refresh.pending);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.graph.history_loaded && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.maybe_auto_refresh();
        }
        assert!(app.graph.history_loaded);
        assert_eq!(app.graph.commits[0].subject, "Registered");

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn terminal_size_changes_without_resize_events_redraw_the_entire_layout() {
        let mut app = offline_app();
        let mut terminal = Terminal::new(TestBackend::new(240, 60)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(!sync_terminal_size(&mut terminal).unwrap());

        for (width, height) in [(120, 30), (120, 45), (240, 60)] {
            terminal.backend_mut().resize(width, height);
            assert!(sync_terminal_size(&mut terminal).unwrap());
            terminal.draw(|frame| app.draw(frame)).unwrap();

            let screen = buffer_text(terminal.backend().buffer());
            for label in [
                "Fetch", "Pull", "Commit", "Push", "Branch", "Stash", "After",
            ] {
                assert!(
                    screen.contains(label),
                    "{label} is visible at {width}x{height}"
                );
            }
            assert_eq!(app.ops.quick_action_areas.len(), 6);
            assert_eq!(app.diff.after_diff_area.right(), width);
            assert_eq!(app.shell.status_bar_area.bottom(), height);
            assert!(!sync_terminal_size(&mut terminal).unwrap());
        }
    }

    #[test]
    fn idle_ticks_leave_the_frame_alone_while_pending_work_and_cursors_animate() {
        let mut app = offline_app();
        let (_foreground_rx, foreground_result_tx) = intercept_foreground(&mut app);
        let (refresh_tx, _refresh_rx) = mpsc::channel();
        let (_refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;
        app.refresh.last_check = Instant::now();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let idle_frame = terminal.backend().buffer().clone();

        for _ in 0..2 {
            assert!(!sync_terminal_size(&mut terminal).unwrap());
            assert!(!app.maybe_auto_refresh(), "an idle tick changes nothing");
            assert!(!app.animating());
        }
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(terminal.backend().buffer(), &idle_frame);

        foreground_result_tx
            .send(ForegroundResult::Fetch {
                id: RequestId::new(99),
            })
            .unwrap();
        assert!(
            app.maybe_auto_refresh(),
            "a delivered completion marks the frame dirty"
        );

        app.foreground.action = Some(ForegroundAction {
            id: RequestId::new(1),
            kind: ForegroundKind::AddProject,
            started: Instant::now(),
        });
        assert!(app.animating(), "a pending foreground action spins");
        app.foreground.action = None;
        assert!(!app.animating());
        app.overlay = Overlay::Commands(CommandPalette::new());
        assert!(app.animating(), "a blinking cursor animates");
        app.overlay = Overlay::None;
        app.show_result(OperationResultView::operation(
            &GitOperation::Fetch,
            "done",
            true,
        ));
        assert!(app.animating(), "a result card animates until it closes");
        app.overlay.result_mut().unwrap().shown_at = Instant::now() - RESULT_VISIBLE_DURATION;
        assert!(
            app.maybe_auto_refresh(),
            "an expired card marks the frame dirty"
        );
        assert!(matches!(app.overlay, Overlay::None));
        assert!(!app.animating());
    }

    #[test]
    fn keyboard_protocol_preserves_the_terminal_ime_text_path() {
        let flags = keyboard_enhancement_flags();
        assert!(flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
        assert!(flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }

    #[test]
    fn global_branch_action_uses_the_active_repository_head_after_switching() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("herdr-git-branch-switch-{unique}"));
        let first = parent.join("first");
        let second = parent.join("second");
        for (root, content) in [(&first, "first\n"), (&second, "second\n")] {
            fs::create_dir_all(root).unwrap();
            git(root, &["init", "-b", "main"]);
            git(root, &["config", "user.name", "Test Author"]);
            git(root, &["config", "user.email", "test@example.com"]);
            fs::write(root.join("tracked.txt"), content).unwrap();
            git(root, &["add", "tracked.txt"]);
            git(root, &["commit", "-m", content.trim()]);
        }
        let second_head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&second)
            .output()
            .unwrap();
        let second_head = String::from_utf8(second_head.stdout)
            .unwrap()
            .trim()
            .to_owned();
        git(&second, &["checkout", "--detach", &second_head]);

        let repository = Repository::discover(&first).unwrap();
        let mut app = App::load(repository).unwrap();
        assert_eq!(app.inspect.details_cache.len(), 1);
        let snapshot =
            super::load_repository_snapshot(&second, &SyntaxHighlighter::new(), &|| false).unwrap();
        app.apply_repository_snapshot(snapshot);
        assert_eq!(
            app.inspect.details_cache.len(),
            0,
            "a repository switch drops the cached details"
        );
        assert_eq!(app.inspect.preview_cache.len(), 0);
        assert_eq!(
            app.ops.command_context.head_commit.as_deref(),
            Some(second_head.as_str())
        );
        assert!(app.ops.command_context.current_branch.is_none());
        assert!(app.ops.command_context.selected_commit.is_none());
        app.run_quick_action(CommandId::CreateBranchAtHead);
        let Overlay::Name(prompt) = &mut app.overlay else {
            panic!("Branch opens the name prompt from detached HEAD");
        };
        assert_eq!(prompt.command, CommandId::CreateBranchAtHead);
        prompt.input.text = "feature/second".to_owned();
        app.submit_name();
        wait_for_foreground(&mut app);
        assert_eq!(
            app.overlay.confirm_operation(),
            Some(&GitOperation::CreateBranch {
                name: "feature/second".to_owned(),
                commit: second_head,
            })
        );

        fs::remove_dir_all(parent).unwrap();
    }

    fn review_agent() -> HerdrAgent {
        HerdrAgent {
            pane_id: "pane-1".to_owned(),
            label: "Review agent".to_owned(),
            cwd: "/tmp/project".to_owned(),
            ..Default::default()
        }
    }

    fn reset_context() -> crate::git::ResetContext {
        crate::git::ResetContext {
            current_branch: "main".to_owned(),
            current_head: "0123456789abcdef".to_owned(),
            targets: vec![
                ResetTarget {
                    name: "main".to_owned(),
                    reference: "refs/heads/main".to_owned(),
                    commit: "0123456789abcdef".to_owned(),
                },
                ResetTarget {
                    name: "origin/main".to_owned(),
                    reference: "refs/remotes/origin/main".to_owned(),
                    commit: "fedcba9876543210".to_owned(),
                },
            ],
        }
    }

    type OverlayCase = (&'static str, fn(&mut App), &'static str);

    #[test]
    fn every_overlay_draws_its_title_and_escape_closes_it() {
        let cases: [OverlayCase; 14] = [
            (
                "workspace",
                |app| app.open_workspace_picker(),
                "Select Workspace",
            ),
            ("commit", |app| app.open_commit_dialog(), "Description"),
            (
                "reset",
                |app| app.overlay = Overlay::Reset(ResetFlow::new(reset_context())),
                "Reset",
            ),
            (
                "line jump",
                |app| app.overlay = Overlay::LineJump(super::diff::LineJump::default()),
                "Go to After line",
            ),
            ("files search", |app| app.open_files_search(), "Find Files"),
            (
                "action",
                |app| {
                    app.overlay = Overlay::Action(ActionDialog::new(
                        "Stash",
                        &STASH_COMMANDS,
                        "Choose a stash operation".to_owned(),
                    ))
                },
                "Choose a stash operation",
            ),
            (
                "copy SHA",
                |app| {
                    app.overlay = Overlay::CopySha(CopyShaDialog {
                        sha: "0123456789abcdef".to_owned(),
                        subject: "Base".to_owned(),
                        buttons: Default::default(),
                    })
                },
                "Copy commit SHA",
            ),
            (
                "copy selection",
                |app| app.overlay = Overlay::CopySelection(Default::default()),
                "Copy Selection",
            ),
            (
                "name",
                |app| app.overlay = Overlay::Name(NamePrompt::new(CommandId::CreateTag)),
                "Tag…",
            ),
            (
                "confirm",
                |app| {
                    app.overlay = Overlay::Confirm {
                        command: CommandId::Fetch,
                        operation: GitOperation::Fetch,
                        buttons: Default::default(),
                        push_controls: Default::default(),
                    }
                },
                "Confirm Git operation",
            ),
            (
                "context menu",
                |app| app.overlay = Overlay::ContextMenu(ContextMenu::default()),
                "Commit actions",
            ),
            (
                "commands",
                |app| app.overlay = Overlay::Commands(CommandPalette::new()),
                "[Enter] Run",
            ),
            (
                "graph filter",
                |app| {
                    app.shell.active_tab = ActiveTab::History;
                    app.focus = PaneFocus::Commits;
                    app.overlay = Overlay::GraphFilter;
                },
                "Filter",
            ),
            (
                "result",
                |app| {
                    app.show_result(OperationResultView::operation(
                        &GitOperation::Fetch,
                        "fatal: offline",
                        false,
                    ))
                },
                "Fetch failed",
            ),
        ];
        for (name, open, title) in cases {
            let mut app = offline_app();
            open(&mut app);
            assert!(!matches!(app.overlay, Overlay::None), "{name} opens");
            let text = buffer_text(&render(&mut app, 100, 32));
            assert!(text.contains(title), "{name} draws {title:?}");
            press(&mut app, KeyCode::Esc);
            assert!(matches!(app.overlay, Overlay::None), "Esc closes {name}");
            let text = buffer_text(&render(&mut app, 100, 32));
            if name != "graph filter" {
                assert!(!text.contains(title), "{name} disappears after Esc");
            }
        }

        let mut app = offline_app();
        app.open_commit_dialog();
        app.overlay.commit_mut().unwrap().agent_picker =
            Some(AgentPicker::new(vec![review_agent()]));
        let text = buffer_text(&render(&mut app, 100, 32));
        assert!(text.contains("Send Agent"));
        assert!(text.contains("Review agent"));
        press(&mut app, KeyCode::Esc);
        let dialog = app
            .overlay
            .commit()
            .expect("Esc in Send Agent returns to Commit");
        assert!(dialog.agent_picker.is_none());
        assert!(buffer_text(&render(&mut app, 100, 32)).contains("Description"));
    }
}
