use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::commands::{CommandId, QUICK_ACTIONS};
use super::effect::RefreshScope;
use super::graph::{GRAPH_ACTIONS, GraphAction};
use super::lanes::ForegroundKind;
use super::overlay::Overlay;
use super::widgets::{self, shortcut_spans, truncate_to_width};
use super::workspaces::identity_text;
use super::{App, theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActiveTab {
    History,
    Changes,
    Files,
}

pub(super) const DEFAULT_ACTIVE_TAB: ActiveTab = ActiveTab::Changes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HeaderAction {
    History,
    Changes,
    Files,
    Commands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppShortcut {
    Changes,
    History,
    Files,
    Commands,
    Workspace,
    FilesSearch,
    Actions,
    Fetch,
    Pull,
    Commit,
    Push,
    Branch,
    Stash,
    Checkout,
    Rebase,
    CherryPick,
    Revert,
    Reset,
    CopySha,
}

const OPTION_COMPOSED_SHORTCUTS: [(char, AppShortcut); 16] = [
    ('π', AppShortcut::Commands),
    ('∑', AppShortcut::Workspace),
    ('ø', AppShortcut::FilesSearch),
    ('Ø', AppShortcut::FilesSearch),
    ('å', AppShortcut::Actions),
    ('ƒ', AppShortcut::Fetch),
    ('¬', AppShortcut::Pull),
    ('ç', AppShortcut::Commit),
    ('∫', AppShortcut::Branch),
    ('ß', AppShortcut::Stash),
    ('≈', AppShortcut::Checkout),
    ('®', AppShortcut::Rebase),
    ('¥', AppShortcut::CherryPick),
    ('√', AppShortcut::Revert),
    ('†', AppShortcut::Reset),
    ('˙', AppShortcut::CopySha),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaneFocus {
    Workspaces,
    Files,
    Diff,
    Commits,
    Details,
    Preview,
    Explorer,
    FilePreview,
}

impl PaneFocus {
    fn belongs_to(self, tab: ActiveTab) -> bool {
        match self {
            Self::Workspaces => true,
            Self::Files | Self::Diff => tab == ActiveTab::Changes,
            Self::Commits | Self::Details | Self::Preview => tab == ActiveTab::History,
            Self::Explorer | Self::FilePreview => tab == ActiveTab::Files,
        }
    }
}

pub(super) fn default_focus(tab: ActiveTab) -> PaneFocus {
    match tab {
        ActiveTab::Changes => PaneFocus::Files,
        ActiveTab::History => PaneFocus::Commits,
        ActiveTab::Files => PaneFocus::Explorer,
    }
}

#[cfg(target_os = "macos")]
const MACOS_ALT_FLAG: u64 = 1 << 19;

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    #[link_name = "CGEventSourceFlagsState"]
    fn cg_event_source_flags_state(state_id: i32) -> u64;
}

pub(super) struct ShellState {
    pub(super) invoking_path: PathBuf,
    pub(super) active_tab: ActiveTab,
    pub(super) tab_area: Rect,
    pub(super) status_bar_area: Rect,
    pub(super) status_blame_area: Rect,
    pub(super) status_horizontal_scroll: u16,
    pub(super) alt_event_held: bool,
    pub(super) shortcut_hints: bool,
    pub(super) mouse_position: Option<(u16, u16)>,
    pub(super) error: Option<String>,
}

impl ShellState {
    pub(super) fn new(invoking_path: PathBuf) -> Self {
        Self {
            invoking_path,
            active_tab: DEFAULT_ACTIVE_TAB,
            tab_area: Rect::default(),
            status_bar_area: Rect::default(),
            status_blame_area: Rect::default(),
            status_horizontal_scroll: 0,
            alt_event_held: false,
            shortcut_hints: false,
            mouse_position: None,
            error: None,
        }
    }
}

impl App {
    pub(super) fn sync_shortcut_hints(&mut self) -> bool {
        let visible = shortcut_hints_visible(self.shell.alt_event_held, system_alt_pressed());
        let changed = visible != self.shell.shortcut_hints;
        self.shell.shortcut_hints = visible;
        changed
    }

    pub(super) fn handle_alt_modifier(&mut self, input: &Event) -> bool {
        let Event::Key(key) = input else {
            return false;
        };
        let Some(visible) = alt_modifier_hint(key) else {
            return false;
        };
        self.shell.alt_event_held = visible;
        self.shell.shortcut_hints = visible;
        true
    }

    pub(super) fn track_pointer(&mut self, input: &Event) {
        match input {
            Event::Mouse(mouse) => self.shell.mouse_position = Some((mouse.column, mouse.row)),
            Event::FocusLost => {
                self.shell.mouse_position = None;
                self.shell.alt_event_held = false;
                self.shell.shortcut_hints = false;
            }
            _ => {}
        }
    }

    pub(super) fn handle_shortcut(&mut self, key: KeyEvent) -> bool {
        let shortcut = app_shortcut(&key).or_else(|| {
            self.shell
                .shortcut_hints
                .then(|| app_shortcut_code(key.code))
                .flatten()
        });
        let Some(shortcut) = shortcut else {
            return false;
        };
        match shortcut {
            AppShortcut::Changes => self.set_tab(ActiveTab::Changes),
            AppShortcut::History => self.set_tab(ActiveTab::History),
            AppShortcut::Files => self.set_tab(ActiveTab::Files),
            AppShortcut::Commands => self.open_commands(),
            AppShortcut::Workspace => self.open_workspace_picker(),
            AppShortcut::FilesSearch if self.shell.active_tab == ActiveTab::Files => {
                self.open_file_filter();
            }
            AppShortcut::FilesSearch => self.open_files_search(),
            AppShortcut::Actions if self.shell.active_tab == ActiveTab::History => {
                self.open_context_menu(None);
            }
            AppShortcut::Actions => {}
            AppShortcut::Fetch => self.run_quick_action(CommandId::Fetch),
            AppShortcut::Pull => self.run_quick_action(CommandId::Pull),
            AppShortcut::Commit => self.run_quick_action(CommandId::Commit),
            AppShortcut::Push => self.run_quick_action(CommandId::Push),
            AppShortcut::Branch => self.run_quick_action(CommandId::CreateBranchAtHead),
            AppShortcut::Stash => self.run_quick_action(CommandId::StashChanges),
            AppShortcut::Checkout if self.shell.active_tab == ActiveTab::History => {
                self.run_graph_action(GraphAction::Checkout);
            }
            AppShortcut::Rebase if self.shell.active_tab == ActiveTab::History => {
                self.run_graph_action(GraphAction::Rebase);
            }
            AppShortcut::CherryPick if self.shell.active_tab == ActiveTab::History => {
                self.run_graph_action(GraphAction::CherryPick);
            }
            AppShortcut::Revert if self.shell.active_tab == ActiveTab::History => {
                self.run_graph_action(GraphAction::Revert);
            }
            AppShortcut::Reset if self.shell.active_tab == ActiveTab::History => {
                self.run_graph_action(GraphAction::Reset);
            }
            AppShortcut::CopySha if self.shell.active_tab == ActiveTab::History => {
                self.run_graph_action(GraphAction::CopySha);
            }
            AppShortcut::Checkout
            | AppShortcut::Rebase
            | AppShortcut::CherryPick
            | AppShortcut::Revert
            | AppShortcut::Reset
            | AppShortcut::CopySha => {}
        }
        true
    }

    pub(super) fn handle_shell_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => self.escape_focus(),
            KeyCode::Tab => self.set_tab(next_tab(self.shell.active_tab)),
            _ => {}
        }
        false
    }

    fn escape_focus(&mut self) {
        match self.focus {
            PaneFocus::Workspaces => {
                self.focus = default_focus(self.shell.active_tab);
            }
            PaneFocus::Preview => self.focus = PaneFocus::Details,
            PaneFocus::Details => self.focus = PaneFocus::Commits,
            PaneFocus::Commits | PaneFocus::Explorer => {}
            PaneFocus::FilePreview => self.focus = PaneFocus::Explorer,
            PaneFocus::Diff => {
                self.focus = PaneFocus::Files;
            }
            PaneFocus::Files => {
                if self.files.tree_keyboard_selecting {
                    self.finish_tree_selection();
                } else if !self.files.tree_selection.is_empty() {
                    self.clear_tree_selection();
                }
            }
        }
    }

    pub(super) fn handle_shell_mouse(&mut self, mouse: MouseEvent) -> bool {
        let position = (mouse.column, mouse.row).into();
        let over_status = self.shell.status_bar_area.contains(position);
        let over_info = self.shell.active_tab == ActiveTab::Changes
            && self.comparison.info_area.contains(position);
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            MouseEventKind::ScrollRight if over_info => self.scroll_comparison_info(1),
            MouseEventKind::ScrollLeft if over_info => self.scroll_comparison_info(-1),
            MouseEventKind::ScrollDown if shift && over_info => self.scroll_comparison_info(1),
            MouseEventKind::ScrollUp if shift && over_info => self.scroll_comparison_info(-1),
            MouseEventKind::ScrollRight if over_status => self.scroll_status_horizontal(1),
            MouseEventKind::ScrollLeft if over_status => self.scroll_status_horizontal(-1),
            MouseEventKind::ScrollDown if shift && over_status => self.scroll_status_horizontal(1),
            MouseEventKind::ScrollUp if shift && over_status => self.scroll_status_horizontal(-1),
            MouseEventKind::Down(MouseButton::Left)
                if self.shell.status_blame_area.contains(position) =>
            {
                self.open_line_history();
            }
            MouseEventKind::Down(MouseButton::Left) if self.shell.tab_area.contains(position) => {
                if self.update.skip_area.contains(position) {
                    self.skip_update();
                    return true;
                }
                if self.update.area.contains(position) {
                    self.activate_update();
                    return true;
                }
                match header_action_at(mouse.column.saturating_sub(self.shell.tab_area.x)) {
                    Some(HeaderAction::History) => self.set_tab(ActiveTab::History),
                    Some(HeaderAction::Changes) => self.set_tab(ActiveTab::Changes),
                    Some(HeaderAction::Files) => self.set_tab(ActiveTab::Files),
                    Some(HeaderAction::Commands) => self.open_commands(),
                    None => {}
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn enter_tab(&mut self, tab: ActiveTab) {
        self.shell.active_tab = tab;
        if !self.focus.belongs_to(tab) {
            self.focus = default_focus(tab);
        }
        if tab != ActiveTab::History && matches!(self.overlay, Overlay::GraphFilter) {
            self.overlay = Overlay::None;
        }
        if tab != ActiveTab::Files && matches!(self.overlay, Overlay::FileFilter) {
            self.overlay = Overlay::None;
        }
    }

    pub(super) fn set_tab(&mut self, tab: ActiveTab) {
        if tab != ActiveTab::History {
            self.reset_commit_inspection();
        }
        self.enter_tab(tab);
        let switching = matches!(
            self.foreground.action.as_ref().map(|action| &action.kind),
            Some(ForegroundKind::Switch { .. })
        );
        if tab == ActiveTab::History && !self.graph.history_loaded && !switching {
            self.request_background_refresh("Refreshing repository", RefreshScope::History);
        }
        if tab == ActiveTab::History
            && !switching
            && self.graph.history_loaded
            && self.selected_commit_details().is_none()
            && self.inspect.pending_commit_details.is_none()
        {
            self.request_commit_details();
        }
        if tab == ActiveTab::Changes && !switching && self.repository.is_some() {
            if let Some(change) = self.files.changes.get(self.files.change_selected)
                && let Some(target) = change.section.diff_target()
            {
                self.diff.diff_target = target;
            }
            self.request_background_refresh("Loading changes", RefreshScope::Changes);
        }
        if tab == ActiveTab::Files && !switching {
            self.request_repository_files();
        }
    }

    pub(super) fn unfocus_diff(&mut self) {
        if self.focus == PaneFocus::Diff {
            self.focus = PaneFocus::Files;
        }
    }

    fn branch_status_line(&self, width: u16) -> Line<'static> {
        let context = &self.ops.command_context;
        let mut location = Vec::new();
        if context.has_repository {
            let selected = if self.shell.active_tab == ActiveTab::History && width >= 80 {
                self.graph
                    .selected_commit()
                    .filter(|commit| context.head_commit.as_deref() != Some(commit.sha.as_str()))
                    .map(|commit| {
                        format!(" · Selected: {}", super::graph::short_commit(&commit.sha))
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            match (&context.current_branch, &context.head_commit) {
                (Some(branch), head) => {
                    let suffix = format!(
                        "{}{}",
                        head.as_ref()
                            .map(|sha| format!(" · HEAD {}", super::graph::short_commit(sha)))
                            .unwrap_or_else(|| " · Uncommitted".into()),
                        selected
                    );
                    let budget = usize::from(width.saturating_sub(2))
                        .saturating_sub(8 + Line::from(suffix.as_str()).width());
                    location.push(Span::styled("Branch: ", theme::hint()));
                    location.push(Span::styled(
                        truncate_to_width(branch, budget),
                        theme::accent_bold(),
                    ));
                    location.push(Span::styled(suffix, theme::hint()));
                }
                (None, Some(head)) => {
                    location.push(Span::styled(
                        format!("Detached HEAD: {}", super::graph::short_commit(head)),
                        theme::warning_text(),
                    ));
                    location.push(Span::styled(selected, theme::hint()));
                }
                (None, None) => location.push(Span::styled("Uncommitted", theme::hint())),
            }
        } else {
            location.push(Span::styled("No repository", theme::hint()));
        }
        Line::from(location)
    }

    pub(super) fn status_bar_line(&self) -> Line<'static> {
        self.status_bar_parts().0
    }

    fn status_bar_parts(&self) -> (Line<'static>, Option<(usize, usize)>) {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(
            self.branch_status_line(self.shell.status_bar_area.width.max(40))
                .spans,
        );
        spans.push(Span::raw(format!(" │ {} │ ", self.active_path.display())));
        let blame_start = spans.iter().map(Span::width).sum::<usize>();
        let blame = self.selection_blame_status();
        let blame_columns = blame
            .as_ref()
            .map(|span| (blame_start, blame_start + span.width()));
        spans.push(blame.unwrap_or_else(|| {
            Span::raw(identity_text(
                self.local_identity.as_ref(),
                self.github_origin,
            ))
        }));
        let foreground_progress = self.foreground.action.as_ref().and_then(|action| {
            let label = match &action.kind {
                ForegroundKind::AddProject => "Choosing Project",
                ForegroundKind::RemoveProject { .. } => "Removing Project",
                ForegroundKind::Switch { .. } => "Loading repository",
                ForegroundKind::Staging { operation, .. } => {
                    crate::ui::files::staging_progress_label(operation)
                }
                ForegroundKind::ResetContext { .. }
                    if self.shell.active_tab == ActiveTab::History =>
                {
                    return None;
                }
                ForegroundKind::BranchTargets { .. } => "Loading branches",
                ForegroundKind::ResetContext { .. } => "Loading Reset targets",
                ForegroundKind::Operation { command, .. }
                    if !QUICK_ACTIONS.contains(command)
                        && *command != CommandId::StashPop
                        && !(self.shell.active_tab == ActiveTab::History
                            && GRAPH_ACTIONS
                                .iter()
                                .any(|action| action.owns_command(*command))) =>
                {
                    command.progress_label()
                }
                _ => return None,
            };
            Some((action.started.elapsed(), label))
        });
        let refresh_progress = self
            .refresh
            .active_progress()
            .map(|progress| (progress.started.elapsed(), progress.running.as_str()));
        let history_progress = self.history.pending.as_ref().map(|request| {
            (
                self.history.started.elapsed(),
                if !request.replace && self.graph.reveal.is_some() {
                    "Finding the commit in Graph"
                } else if !request.replace {
                    "Loading more commits"
                } else if self.graph.history_loaded {
                    "Refreshing commits"
                } else {
                    "Loading Graph"
                },
            )
        });
        if let Some((elapsed, label)) = foreground_progress
            .or(refresh_progress)
            .or(history_progress)
        {
            spans.push(Span::raw(" │ "));
            spans.push(theme::spinner_span(elapsed));
            spans.push(Span::raw(format!(" {label}")));
        }
        if self.maintenance.running_for(&self.active_path) {
            spans.push(Span::raw(" │ "));
            spans.push(theme::spinner_span(self.maintenance.started.elapsed()));
            spans.push(Span::raw(" Optimizing history"));
        } else if let Some((path, notice)) = &self.maintenance.notice
            && path == &self.active_path
        {
            spans.push(Span::styled(format!(" │ {notice}"), theme::error_text()));
        }
        if let Some(error) = self.history.error.as_ref().or(self.shell.error.as_ref()) {
            spans.push(Span::raw(" │ "));
            spans.push(Span::styled(format!("Error: {error}"), theme::error_text()));
        }
        spans.push(Span::raw(" "));
        (Line::from(spans), blame_columns)
    }

    #[cfg(test)]
    pub(super) fn status_bar_text(&self) -> String {
        self.status_bar_line().to_string()
    }

    pub(super) fn scroll_status_horizontal(&mut self, delta: i16) {
        let limit = horizontal_scroll_limit(
            self.status_bar_line().width(),
            self.shell.status_bar_area.width,
        );
        self.shell.status_horizontal_scroll = self
            .shell
            .status_horizontal_scroll
            .saturating_add_signed(delta)
            .min(limit);
    }

    pub(super) fn draw_header(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let margin = u16::from(frame.area().height > 13);
        let action_gap = margin;
        self.shell.tab_area = Rect::new(area.x, area.y.saturating_add(margin), area.width, 1);
        let hovered_header = self.shell.mouse_position.and_then(|(column, row)| {
            self.shell
                .tab_area
                .contains((column, row).into())
                .then(|| header_action_at(column.saturating_sub(self.shell.tab_area.x)))
                .flatten()
        });
        let mut navigation = Vec::new();
        for (index, (key, label, style)) in [
            (
                '1',
                "Changes",
                theme::hover(
                    tab_style(self.shell.active_tab == ActiveTab::Changes),
                    hovered_header == Some(HeaderAction::Changes),
                ),
            ),
            (
                '2',
                "Graph",
                theme::hover(
                    tab_style(self.shell.active_tab == ActiveTab::History),
                    hovered_header == Some(HeaderAction::History),
                ),
            ),
            (
                '3',
                "Files",
                theme::hover(
                    tab_style(self.shell.active_tab == ActiveTab::Files),
                    hovered_header == Some(HeaderAction::Files),
                ),
            ),
            (
                'P',
                "Commands",
                theme::hover(
                    theme::hint(),
                    hovered_header == Some(HeaderAction::Commands),
                ),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if index > 0 {
                navigation.push(Span::raw("  "));
            }
            if self.shell.shortcut_hints {
                navigation.extend(shortcut_spans(key, label, " ", "", style, true));
            } else {
                navigation.push(Span::styled(format!(" {label} "), style));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(navigation)), self.shell.tab_area);
        self.draw_update(frame, self.shell.tab_area);
        let quick_action_area = Rect::new(
            area.x,
            area.y.saturating_add(margin.saturating_add(2)),
            area.width,
            1,
        );
        self.draw_quick_actions(frame, quick_action_area);
        let tab_action_area = Rect::new(
            area.x,
            area.y.saturating_add(margin.saturating_add(3 + action_gap)),
            area.width,
            1,
        );
        if self.shell.active_tab == ActiveTab::History {
            self.draw_graph_actions(frame, tab_action_area);
        } else if self.shell.active_tab == ActiveTab::Files {
            self.graph.action_areas.clear();
        } else {
            self.graph.action_areas.clear();
            self.draw_comparison_controls(frame, tab_action_area);
            let info_area = Rect::new(area.x, tab_action_area.bottom() + action_gap, area.width, 1);
            self.draw_comparison_info(frame, info_area);
        }
    }

    pub(super) fn draw_status_bar(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.shell.status_bar_area = area;
        let (status, blame_columns) = self.status_bar_parts();
        self.shell.status_horizontal_scroll = self
            .shell
            .status_horizontal_scroll
            .min(horizontal_scroll_limit(status.width(), area.width));
        let scroll = usize::from(self.shell.status_horizontal_scroll);
        self.shell.status_blame_area = blame_columns
            .map(|(start, end)| {
                let start = start.saturating_sub(scroll).min(area.width as usize) as u16;
                let end = end.saturating_sub(scroll).min(area.width as usize) as u16;
                Rect::new(area.x + start, area.y, end - start, 1)
            })
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(status)
                .scroll((0, self.shell.status_horizontal_scroll))
                .style(Style::default().fg(theme::TEXT).bg(theme::SURFACE_INERT)),
            area,
        );
    }
}

pub(super) fn draw_loading(frame: &mut Frame<'_>, elapsed: Duration) {
    let inner = widgets::dialog_frame(frame, "Git", 36, 5);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            theme::spinner_span(elapsed),
            Span::raw(" Loading Git context"),
        ]))
        .alignment(Alignment::Center),
        inner,
    );
}

pub(super) fn horizontal_scroll_limit(content_width: usize, viewport_width: u16) -> u16 {
    content_width
        .saturating_sub(viewport_width as usize)
        .min(u16::MAX as usize) as u16
}

pub(super) fn tab_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(theme::TEXT_INVERSE)
            .bg(theme::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        theme::hint()
    }
}

const WHEEL_BURST_LIMIT: usize = 4;

impl App {
    pub(super) fn wheel_burst_limit(&self, event: &Event) -> usize {
        match event {
            Event::Mouse(mouse) if self.explorer_wheel_moves_selection(mouse) => 1,
            _ => WHEEL_BURST_LIMIT,
        }
    }
}

pub(super) fn is_wheel_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            )
    )
}

pub(super) fn app_shortcut(key: &KeyEvent) -> Option<AppShortcut> {
    if key.kind != KeyEventKind::Press || !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    app_shortcut_code(key.code)
}

pub(super) fn app_shortcut_code(code: KeyCode) -> Option<AppShortcut> {
    let KeyCode::Char(character) = code else {
        return None;
    };
    match character.to_ascii_lowercase() {
        '1' => Some(AppShortcut::Changes),
        '2' => Some(AppShortcut::History),
        '3' => Some(AppShortcut::Files),
        'p' => Some(AppShortcut::Commands),
        'w' => Some(AppShortcut::Workspace),
        'o' => Some(AppShortcut::FilesSearch),
        'a' => Some(AppShortcut::Actions),
        'f' => Some(AppShortcut::Fetch),
        'l' => Some(AppShortcut::Pull),
        'c' => Some(AppShortcut::Commit),
        'u' => Some(AppShortcut::Push),
        'b' => Some(AppShortcut::Branch),
        's' => Some(AppShortcut::Stash),
        'x' => Some(AppShortcut::Checkout),
        'r' => Some(AppShortcut::Rebase),
        'y' => Some(AppShortcut::CherryPick),
        'v' => Some(AppShortcut::Revert),
        't' => Some(AppShortcut::Reset),
        'h' => Some(AppShortcut::CopySha),
        composed => OPTION_COMPOSED_SHORTCUTS
            .iter()
            .find(|(candidate, _)| *candidate == composed)
            .map(|(_, shortcut)| *shortcut),
    }
}

pub(super) fn alt_modifier_hint(key: &KeyEvent) -> Option<bool> {
    if !matches!(
        key.code,
        KeyCode::Modifier(ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt)
    ) {
        return None;
    }
    match key.kind {
        KeyEventKind::Press | KeyEventKind::Repeat => Some(true),
        KeyEventKind::Release => Some(false),
    }
}

pub(super) fn write_osc52(writer: &mut impl Write, value: &str) -> Result<(), String> {
    writer
        .write_all(osc52_sequence(value).as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| error.to_string())
}

fn osc52_sequence(value: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", base64(value.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        encoded.push(TABLE[(first >> 2) as usize] as char);
        encoded.push(TABLE[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[(third & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

pub(super) fn shortcut_hints_visible(event_state: bool, system_state: Option<bool>) -> bool {
    event_state || system_state.unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn alt_pressed_from_flags(flags: u64) -> bool {
    flags & MACOS_ALT_FLAG != 0
}

#[cfg(target_os = "macos")]
fn system_alt_pressed() -> Option<bool> {
    let flags = unsafe { cg_event_source_flags_state(0) };
    Some(alt_pressed_from_flags(flags))
}

#[cfg(not(target_os = "macos"))]
fn system_alt_pressed() -> Option<bool> {
    None
}

pub(super) fn header_action_at(column: u16) -> Option<HeaderAction> {
    match column {
        0..=8 => Some(HeaderAction::Changes),
        11..=17 => Some(HeaderAction::History),
        20..=26 => Some(HeaderAction::Files),
        29..=38 => Some(HeaderAction::Commands),
        _ => None,
    }
}

pub(super) fn next_tab(active: ActiveTab) -> ActiveTab {
    match active {
        ActiveTab::Changes => ActiveTab::History,
        ActiveTab::History => ActiveTab::Files,
        ActiveTab::Files => ActiveTab::Changes,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseEvent,
        MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use crate::git::{ReadError, Repository, RepositoryFingerprint};
    use crate::project::ProjectRegistry;
    use crate::ui::commands::CommandId;
    use crate::ui::effect::{
        ActiveRefresh, ChangedRefresh, ChangesRefresh, ForegroundRequest, ForegroundResult,
        HighlightedDiff, RefreshProgress, RefreshRequest, RefreshResult, RefreshScope, RequestId,
    };
    use crate::ui::lanes::{ForegroundAction, ForegroundKind};
    use crate::ui::overlay::Overlay;
    use crate::ui::review::ReviewSide;
    use crate::ui::syntax::DiffDocument;
    use crate::ui::test_support::{
        buffer_text, committed_change, find_text, find_text_in_row, git, intercept_foreground,
        offline_app, press, render, temp_repo,
    };
    use crate::ui::widgets::scrolled_content_row_at;
    use crate::ui::{App, theme};

    use super::{
        ActiveTab, AppShortcut, DEFAULT_ACTIVE_TAB, HeaderAction, PaneFocus, alt_modifier_hint,
        app_shortcut, app_shortcut_code, draw_loading, header_action_at, horizontal_scroll_limit,
        next_tab, osc52_sequence, shortcut_hints_visible, tab_style, write_osc52,
    };

    fn diff_result(
        request: ForegroundRequest,
        result: Result<(String, HighlightedDiff), ReadError>,
    ) -> ForegroundResult {
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
            panic!("unexpected request: {request:?}")
        };
        ForegroundResult::Diff {
            id,
            generation,
            owner,
            path,
            file,
            target,
            fold_toggles,
            result,
        }
    }

    fn changes_refresh_result(
        app: &App,
        request: RefreshRequest,
        changes: Result<ChangesRefresh, String>,
    ) -> RefreshResult {
        RefreshResult {
            id: request.id,
            intent: request.intent,
            context: request.context,
            cancelled: false,
            active: Some(Ok(ActiveRefresh::Changed(Box::new(ChangedRefresh {
                fingerprint: RepositoryFingerprint {
                    refs: 1,
                    worktree: 1,
                },
                history: false,
                changes: Some(changes),
                command_context: app.ops.command_context.clone(),
            })))),
        }
    }

    #[test]
    fn statusline_identifies_checked_out_branch_independently_of_selection() {
        use crate::ui::test_support::wait_for_refresh;
        let root = temp_repo("header-branch");
        git(&root, &["commit", "--allow-empty", "-m", "Base"]);
        git(&root, &["branch", "feature/login"]);
        git(&root, &["commit", "--allow-empty", "-m", "Next"]);
        git(&root, &["branch", "release"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();
        app.set_tab(ActiveTab::History);
        wait_for_refresh(&mut app);
        let header = |app: &mut App| {
            let buffer = render(app, 100, 30);
            (0..100)
                .map(|x| buffer[(x, 29)].symbol())
                .collect::<String>()
        };
        let head = app.ops.command_context.head_commit.clone().unwrap();
        assert!(header(&mut app).contains(&format!("Branch: main · HEAD {}", &head[..8])));
        assert!(!header(&mut app).contains("release"));
        app.select(1);
        let selected = app.graph.selected_commit().unwrap().sha.clone();
        let text = header(&mut app);
        assert!(text.contains("Branch: main"));
        assert!(text.contains(&format!("Selected: {}", &selected[..8])));
        git(&root, &["switch", "feature/login"]);
        app.request_operation_refresh("Refreshing", RefreshScope::Repository);
        wait_for_refresh(&mut app);
        assert!(header(&mut app).contains("Branch: feature/login"));
        app.set_tab(ActiveTab::Changes);
        assert!(header(&mut app).contains("Branch: feature/login"));
        git(&root, &["checkout", "--detach"]);
        app.request_operation_refresh("Refreshing", RefreshScope::Repository);
        wait_for_refresh(&mut app);
        let text = header(&mut app);
        assert!(text.contains("Detached HEAD:"));
        assert!(!text.contains("Branch:"));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn statusline_branch_feedback_fits_compact_and_unborn_repositories() {
        let mut app = offline_app();
        app.ops.command_context.has_repository = true;
        app.ops.command_context.current_branch = Some("feature/a-very-long-branch-name".into());
        app.ops.command_context.head_commit = Some("1234567890abcdef".into());
        let buffer = render(&mut app, 40, 12);
        let text = (0..40)
            .map(|x| buffer[(x, 11)].symbol())
            .collect::<String>();
        assert!(text.contains("Branch: feature/"));
        assert!(text.contains("HEAD 12345678"));
        assert!(text.contains('…'));
        app.ops.command_context.current_branch = Some("main".into());
        app.ops.command_context.head_commit = None;
        let screen = buffer_text(&render(&mut app, 40, 12));
        assert!(screen.contains("Branch: main · Uncommitted"));
    }

    #[test]
    fn alt_is_the_only_app_shortcut_prefix() {
        let alt = KeyModifiers::ALT;
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('1'), alt)),
            Some(AppShortcut::Changes)
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('2'), alt)),
            Some(AppShortcut::History)
        );
        assert_eq!(app_shortcut(&KeyEvent::new(KeyCode::Char('n'), alt)), None);
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('p'), alt)),
            Some(AppShortcut::Commands)
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('w'), alt)),
            Some(AppShortcut::Workspace)
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('o'), alt)),
            Some(AppShortcut::FilesSearch)
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('c'), alt)),
            Some(AppShortcut::Commit)
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('U'), alt)),
            Some(AppShortcut::Push)
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('s'), alt)),
            Some(AppShortcut::Stash)
        );
        assert!(app_shortcut(&KeyEvent::new(KeyCode::Char('a'), alt)).is_some());
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            app_shortcut(&KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn option_composed_characters_map_to_the_same_shortcuts() {
        for (character, shortcut) in [
            ('π', AppShortcut::Commands),
            ('∑', AppShortcut::Workspace),
            ('ø', AppShortcut::FilesSearch),
            ('Ø', AppShortcut::FilesSearch),
            ('å', AppShortcut::Actions),
            ('ƒ', AppShortcut::Fetch),
            ('¬', AppShortcut::Pull),
            ('ç', AppShortcut::Commit),
            ('∫', AppShortcut::Branch),
            ('ß', AppShortcut::Stash),
            ('≈', AppShortcut::Checkout),
            ('®', AppShortcut::Rebase),
            ('¥', AppShortcut::CherryPick),
            ('√', AppShortcut::Revert),
            ('†', AppShortcut::Reset),
            ('˙', AppShortcut::CopySha),
        ] {
            assert_eq!(
                app_shortcut_code(KeyCode::Char(character)),
                Some(shortcut),
                "{character}"
            );
            assert_eq!(
                app_shortcut(&KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)),
                Some(shortcut),
                "Alt+{character}"
            );
        }
        assert_eq!(app_shortcut_code(KeyCode::Char('¨')), None);
        assert_eq!(
            app_shortcut_code(KeyCode::Char('u')),
            Some(AppShortcut::Push)
        );

        let mut app = offline_app();
        app.shell.shortcut_hints = true;
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('π'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::Commands(_)));
    }

    #[test]
    fn alt_a_opens_graph_commit_actions_and_shift_f10_is_inert() {
        let root = temp_repo("ui-actions");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::ContextMenu(_)));

        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!matches!(app.overlay, Overlay::ContextMenu(_)));
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::F(10),
            KeyModifiers::SHIFT,
        )))
        .unwrap();
        assert!(!matches!(app.overlay, Overlay::ContextMenu(_)));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stash_is_global_and_commit_shortcuts_are_graph_only() {
        let root = temp_repo("scoped-action-shortcuts");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-m", "Base"]);
        let mut app = App::load(Repository::discover(&root).unwrap()).unwrap();

        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::Action(_)));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::None));

        app.set_tab(ActiveTab::History);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        crate::ui::test_support::wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Target(_)));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        crate::ui::test_support::wait_for_foreground(&mut app);
        assert!(matches!(app.overlay, Overlay::Target(_)));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(
            app.overlay.confirm_operation(),
            Some(crate::git::GitOperation::CherryPick(_))
        ));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(
            app.overlay.confirm_operation(),
            Some(crate::git::GitOperation::Revert(_))
        ));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::Reset(_)));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::ALT,
        )))
        .unwrap();
        assert!(matches!(app.overlay, Overlay::CopySha(_)));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn osc52_copy_encodes_and_flushes_the_selected_sha() {
        assert_eq!(osc52_sequence("abc"), "\u{1b}]52;c;YWJj\u{7}");
        assert_eq!(osc52_sequence("ab"), "\u{1b}]52;c;YWI=\u{7}");
        let mut bytes = Vec::new();
        write_osc52(&mut bytes, "abc").unwrap();
        assert_eq!(bytes, b"\x1b]52;c;YWJj\x07");
    }

    #[test]
    fn alt_press_and_release_control_shortcut_hints() {
        let press = KeyEvent::new_with_kind(
            KeyCode::Modifier(ModifierKeyCode::LeftAlt),
            KeyModifiers::ALT,
            KeyEventKind::Press,
        );
        let release = KeyEvent::new_with_kind(
            KeyCode::Modifier(ModifierKeyCode::LeftAlt),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(alt_modifier_hint(&press), Some(true));
        assert_eq!(alt_modifier_hint(&release), Some(false));
        assert_eq!(
            alt_modifier_hint(&KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
            None
        );
        assert!(shortcut_hints_visible(false, Some(true)));
        assert!(shortcut_hints_visible(true, Some(false)));
        assert!(shortcut_hints_visible(true, None));
        let mut app = offline_app();
        assert!(app.handle_alt_modifier(&Event::Key(press)));
        assert!(app.shell.alt_event_held && app.shell.shortcut_hints);
        assert!(app.handle_alt_modifier(&Event::Key(release)));
        assert!(!app.shell.alt_event_held && !app.shell.shortcut_hints);
        app.shell.shortcut_hints = true;
        app.track_pointer(&Event::FocusLost);
        assert!(!app.shell.shortcut_hints);
        assert_eq!(app.shell.mouse_position, None);
    }

    #[test]
    fn top_navigation_keeps_a_two_cell_gap_between_actions() {
        assert_eq!(header_action_at(8), Some(HeaderAction::Changes));
        assert_eq!(header_action_at(9), None);
        assert_eq!(header_action_at(10), None);
        assert_eq!(header_action_at(11), Some(HeaderAction::History));
        assert_eq!(header_action_at(17), Some(HeaderAction::History));
        assert_eq!(header_action_at(18), None);
        assert_eq!(header_action_at(19), None);
        assert_eq!(header_action_at(20), Some(HeaderAction::Files));
        assert_eq!(header_action_at(26), Some(HeaderAction::Files));
        assert_eq!(header_action_at(27), None);
        assert_eq!(header_action_at(28), None);
        assert_eq!(header_action_at(29), Some(HeaderAction::Commands));
        assert_eq!(header_action_at(38), Some(HeaderAction::Commands));
        assert_eq!(header_action_at(39), None);
    }

    #[test]
    fn holding_alt_overlays_shortcuts_without_moving_navigation() {
        assert_eq!(CommandId::Fetch.quick_action_shortcut(), 'f');
        assert_eq!(CommandId::Pull.quick_action_shortcut(), 'l');
        assert_eq!(CommandId::Commit.quick_action_shortcut(), 'c');
        assert_eq!(CommandId::Push.quick_action_shortcut(), 'u');
        assert_eq!(CommandId::CreateBranchAtHead.quick_action_shortcut(), 'b');
        assert_eq!(CommandId::StashChanges.quick_action_shortcut(), 's');
        assert_eq!(header_action_at(8), Some(HeaderAction::Changes));
        assert_eq!(header_action_at(20), Some(HeaderAction::Files));
        assert_eq!(header_action_at(29), Some(HeaderAction::Commands));

        let mut app = offline_app();
        let plain = render(&mut app, 80, 16);
        app.shell.shortcut_hints = true;
        let hinted = render(&mut app, 80, 16);
        let plain_row = (0..80)
            .map(|column| plain[(column, 1)].symbol())
            .collect::<String>();
        let hinted_row = (0..80)
            .map(|column| hinted[(column, 1)].symbol())
            .collect::<String>();
        assert!(plain_row.starts_with(" Changes    Graph    Files    Commands "));
        assert!(hinted_row.starts_with("1 Changes  2 Graph  3 Files  P Commands"));
        let key = &hinted[(0, 1)];
        assert_eq!(key.bg, theme::ACCENT);
        assert!(key.modifier.contains(Modifier::BOLD | Modifier::UNDERLINED));
        let graph_key = &hinted[(11, 1)];
        assert_eq!(graph_key.fg, theme::ACCENT);
        assert!(graph_key.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn navigation_and_status_use_graph_as_the_public_label() {
        let mut app = offline_app();
        let buffer = render(&mut app, 80, 16);
        let navigation = (0..80)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        assert!(navigation.starts_with(" Changes    Graph    Files    Commands "));
        assert!(!navigation.contains("History"));

        app.set_tab(ActiveTab::History);
        let status = app.status_bar_text();
        assert!(!status.contains("Graph ·"));
        assert!(!status.contains("History"));
    }

    #[test]
    fn repository_refresh_spinner_lives_only_on_the_status_bar() {
        let mut app = offline_app();
        app.refresh.in_flight = true;
        app.refresh.workspaces_pending = false;
        app.refresh.running_scope = RefreshScope::Repository;
        app.refresh.progress = Some(RefreshProgress {
            started: Instant::now(),
            running: "Refreshing repository".into(),
        });
        let buffer = render(&mut app, 120, 40);
        let text = buffer_text(&buffer);
        assert_eq!(text.matches("Refreshing repository").count(), 1);
        assert!(app.status_bar_text().contains("Refreshing repository"));
        let files_title: String = (app.files.files_area.x..app.files.files_area.right())
            .map(|x| buffer[(x, app.files.files_area.y)].symbol())
            .collect();
        assert!(!files_title.contains("Refreshing repository"));
        let after = app.diff.after_diff_area;
        let after_title: String = (after.x..after.right())
            .map(|x| buffer[(x, after.y)].symbol())
            .collect();
        assert!(!after_title.contains("Refreshing repository"));
    }

    #[test]
    fn status_bar_keeps_completion_notices_off_the_inert_surface() {
        let mut app = offline_app();
        app.show_result(crate::ui::commands::OperationResultView::named_message(
            "Copy Selection",
            true,
            "Selection copied",
        ));
        let buffer = render(&mut app, 120, 16);
        let row = app.shell.status_bar_area.y;
        assert!(find_text_in_row(&buffer, row, "Selection copied").is_none());
        assert_eq!(buffer[(0, row)].bg, theme::SURFACE_INERT);

        app.foreground.action = Some(ForegroundAction {
            id: RequestId::new(1),
            kind: ForegroundKind::AddProject,
            started: Instant::now(),
        });
        let buffer = render(&mut app, 120, 16);
        let column = find_text_in_row(&buffer, row, "Choosing Project").expect("progress label");
        assert!(find_text_in_row(&buffer, row, "Selection copied").is_none());
        let spinner = &buffer[(column - 2, row)];
        assert!(theme::SPINNER_FRAMES.contains(&spinner.symbol()));
        assert_eq!(spinner.fg, theme::ACCENT);
        assert_eq!(spinner.bg, theme::SURFACE_INERT);
        assert_eq!(buffer[(column, row)].fg, theme::TEXT);
    }

    #[test]
    fn status_bar_shows_a_diff_read_error_until_a_later_diff_applies() {
        let (root, mut app) = committed_change("status-bar-diff-error");
        let (request_rx, result_tx) = intercept_foreground(&mut app);
        assert_eq!(app.shell.active_tab, ActiveTab::Changes);

        app.request_file_diff();
        let request = request_rx.try_recv().expect("diff request");
        result_tx
            .send(diff_result(
                request,
                Err(ReadError::Diagnostic("git diff failed".to_owned())),
            ))
            .unwrap();
        app.receive_foreground_results();

        assert_eq!(app.shell.error.as_deref(), Some("git diff failed"));
        render(&mut app, 120, 16);
        app.scroll_status_horizontal(i16::MAX);
        let buffer = render(&mut app, 120, 16);
        let row = app.shell.status_bar_area.y;
        let column =
            find_text_in_row(&buffer, row, "Error: git diff failed").expect("error segment");
        assert_eq!(buffer[(column, row)].fg, theme::ERROR);
        assert_eq!(buffer[(column, row)].bg, theme::SURFACE_INERT);
        assert_eq!(buffer[(column - 2, row)].symbol(), "│");
        assert!(
            app.status_bar_text()
                .ends_with(" │ Error: git diff failed ")
        );

        app.request_file_diff();
        let request = request_rx.try_recv().expect("second diff request");
        result_tx
            .send(diff_result(
                request,
                Ok((
                    "diff text".to_owned(),
                    HighlightedDiff {
                        language: "Plain text".to_owned(),
                        split: DiffDocument::plain("diff text"),
                        key: 0,
                    },
                )),
            ))
            .unwrap();
        app.receive_foreground_results();

        assert_eq!(app.shell.error, None);
        assert_eq!(app.diff.diff_text, "diff text");
        assert!(!app.status_bar_text().contains("Error:"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_bar_shows_a_failed_changes_refresh_until_the_next_one_succeeds() {
        let (root, mut app) = committed_change("status-bar-refresh-error");
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;

        app.request_background_refresh("Loading changes", RefreshScope::Changes);
        let request = refresh_rx.try_recv().expect("changes refresh request");
        refresh_result_tx
            .send(changes_refresh_result(
                &app,
                request,
                Err("git status failed".to_owned()),
            ))
            .unwrap();
        app.maybe_auto_refresh();

        assert_eq!(app.shell.error.as_deref(), Some("git status failed"));
        let status = app.status_bar_text();
        assert!(status.ends_with(" │ Error: git status failed "), "{status}");
        render(&mut app, 120, 16);
        app.scroll_status_horizontal(i16::MAX);
        let buffer = render(&mut app, 120, 16);
        let row = app.shell.status_bar_area.y;
        let column =
            find_text_in_row(&buffer, row, "Error: git status failed").expect("error segment");
        assert_eq!(buffer[(column, row)].fg, theme::ERROR);

        app.request_background_refresh("Loading changes", RefreshScope::Changes);
        let request = refresh_rx
            .try_recv()
            .expect("second changes refresh request");
        let changes = ChangesRefresh {
            changes: app.files.changes.clone(),
            selected: app.files.change_selected,
            target: app.diff.diff_target.clone(),
            commit_titles: app.comparison.commit_titles.clone(),
            summaries: app.files.change_summaries.clone(),
            diff_text: app.diff.diff_text.clone(),
            highlighted: None,
        };
        refresh_result_tx
            .send(changes_refresh_result(&app, request, Ok(changes)))
            .unwrap();
        app.maybe_auto_refresh();

        assert_eq!(app.shell.error, None);
        let status = app.status_bar_text();
        assert!(!status.contains("Changes loaded"), "{status}");
        assert!(!status.contains("Error:"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_bar_drops_a_whole_refresh_error_when_the_repository_reads_unchanged() {
        let (root, mut app) = committed_change("status-bar-whole-refresh-error");
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let (refresh_result_tx, refresh_result_rx) = mpsc::channel();
        app.refresh.request_tx = refresh_tx;
        app.refresh.result_rx = refresh_result_rx;

        app.request_background_refresh("Loading changes", RefreshScope::Changes);
        let request = refresh_rx.try_recv().expect("first refresh request");
        let mut failed = changes_refresh_result(&app, request, Err(String::new()));
        failed.active = Some(Err("fatal: not a git repository".to_owned()));
        refresh_result_tx.send(failed).unwrap();
        app.maybe_auto_refresh();
        assert_eq!(
            app.shell.error.as_deref(),
            Some("fatal: not a git repository")
        );
        assert!(
            app.status_bar_text()
                .contains("Error: fatal: not a git repository")
        );

        app.request_background_refresh("Loading changes", RefreshScope::Changes);
        let request = refresh_rx.try_recv().expect("second refresh request");
        let mut unchanged = changes_refresh_result(&app, request, Err(String::new()));
        unchanged.active = Some(Ok(ActiveRefresh::Unchanged {
            fingerprint: RepositoryFingerprint {
                refs: 1,
                worktree: 1,
            },
        }));
        refresh_result_tx.send(unchanged).unwrap();
        app.maybe_auto_refresh();

        assert_eq!(app.shell.error, None);
        assert!(!app.status_bar_text().contains("Error:"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loading_frame_is_a_dialog_with_an_accent_spinner() {
        let mut terminal = Terminal::new(TestBackend::new(60, 9)).unwrap();
        terminal
            .draw(|frame| draw_loading(frame, Duration::ZERO))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = find_text(buffer, "Loading Git context").expect("loading label");
        let spinner = &buffer[(x - 2, y)];
        assert_eq!(spinner.symbol(), theme::SPINNER_FRAMES[0]);
        assert_eq!(spinner.fg, theme::ACCENT);
        assert!(find_text_in_row(buffer, y - 1, "Git").is_some());
        assert_eq!(buffer[(12, y)].symbol(), "│");
        assert_eq!(buffer[(47, y)].symbol(), "│");
    }

    #[test]
    fn status_bar_scrolls_horizontally_without_rendering_a_scrollbar() {
        let mut app = App::load_registered(
            &PathBuf::from("/a/very/long/repository/path/that/exceeds/the/status/viewport"),
            ProjectRegistry::load(None).unwrap(),
        )
        .unwrap();
        render(&mut app, 40, 12);
        let area = app.shell.status_bar_area;
        let limit = horizontal_scroll_limit(app.status_bar_line().width(), area.width);
        assert!(limit > 0);
        let wheel = |kind, modifiers| {
            Event::Mouse(MouseEvent {
                kind,
                column: area.x,
                row: area.y,
                modifiers,
            })
        };

        app.handle(wheel(MouseEventKind::ScrollRight, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.shell.status_horizontal_scroll, 1);
        for _ in 0..1_000 {
            app.handle(wheel(MouseEventKind::ScrollRight, KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.shell.status_horizontal_scroll, limit);
        app.handle(wheel(MouseEventKind::ScrollUp, KeyModifiers::SHIFT))
            .unwrap();
        assert_eq!(app.shell.status_horizontal_scroll, limit - 1);

        let buffer = render(&mut app, 40, 12);
        assert!(
            (area.x..area.right())
                .all(|column| !matches!(buffer[(column, area.y)].symbol(), "─" | "▄"))
        );
    }

    #[test]
    fn command_hint_is_readable_without_italics() {
        let style = theme::hint();
        assert_eq!(style.fg, Some(theme::HINT));
        assert!(!style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn shell_keys_are_quit_escape_and_tab_only() {
        let (root, mut app) = committed_change("shell-keys");
        render(&mut app, 120, 30);
        assert_eq!(app.focus, PaneFocus::Files);
        let tree_selected = app.files.tree_selected;
        let before = buffer_text(&render(&mut app, 120, 30));
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
        ] {
            assert!(!app.handle_shell_key(KeyEvent::new(code, KeyModifiers::NONE)));
        }
        assert_eq!(app.files.tree_selected, tree_selected);
        assert_eq!(app.focus, PaneFocus::Files);
        assert_eq!(app.shell.active_tab, ActiveTab::Changes);
        assert_eq!(buffer_text(&render(&mut app, 120, 30)), before);
        assert!(app.handle_shell_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_clickable_uses_the_same_hover_background() {
        let base = ratatui::style::Style::default().fg(theme::TEXT);
        assert_eq!(theme::hover(base, true).bg, Some(theme::SURFACE_HOVER));
        assert_eq!(theme::hover(base, false), base);
        assert_eq!(
            scrolled_content_row_at(Some((4, 3)), Rect::new(2, 1, 20, 6), 20, 7),
            Some(9)
        );
    }

    #[test]
    fn legacy_refresh_and_focus_shortcuts_are_inert() {
        let root = temp_repo("ui-shortcuts");
        git(&root, &["config", "user.name", "Test Author"]);
        git(&root, &["config", "user.email", "test@example.com"]);

        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(app.files.changes.is_empty());

        for key in ['d', 'f'] {
            app.handle(Event::Key(KeyEvent::new(
                KeyCode::Char(key),
                KeyModifiers::NONE,
            )))
            .unwrap();
        }
        assert_ne!(app.focus, PaneFocus::Diff);
        assert!(!app.review.changes.keyboard_selecting);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_tab_has_strong_feedback_and_inactive_tab_is_muted() {
        let active = tab_style(true);
        let inactive = tab_style(false);
        assert_eq!(active.bg, Some(theme::ACCENT));
        assert_eq!(active.fg, Some(theme::TEXT_INVERSE));
        assert!(active.add_modifier.contains(Modifier::BOLD));
        assert!(!active.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(inactive.fg, Some(theme::HINT));
        assert_eq!(inactive.bg, None);

        let mut app = offline_app();
        let buffer = render(&mut app, 80, 16);
        let changes = &buffer[(1, 1)];
        assert_eq!(changes.bg, theme::ACCENT);
        assert_eq!(changes.fg, theme::TEXT_INVERSE);
        let graph = &buffer[(12, 1)];
        assert_eq!(graph.fg, theme::HINT);
        assert_eq!(graph.bg, Color::Reset);
    }

    #[test]
    fn changes_is_first_and_tab_cycles_through_every_tab() {
        assert_eq!(DEFAULT_ACTIVE_TAB, ActiveTab::Changes);
        assert_eq!(next_tab(ActiveTab::Changes), ActiveTab::History);
        assert_eq!(next_tab(ActiveTab::History), ActiveTab::Files);
        assert_eq!(next_tab(ActiveTab::Files), ActiveTab::Changes);

        let mut app = offline_app();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.shell.active_tab, ActiveTab::History);
        assert_eq!(app.focus, PaneFocus::Commits);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.shell.active_tab, ActiveTab::Files);
        assert_eq!(app.focus, PaneFocus::Explorer);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.shell.active_tab, ActiveTab::Changes);
        assert_eq!(app.focus, PaneFocus::Files);
        assert!(
            app.status_bar_text()
                .starts_with(" No repository │ /tmp/project │ ")
        );
        assert!(!app.status_bar_text().contains("Changes · 0 files"));
    }

    #[test]
    fn escape_clears_code_and_file_selections_without_confirmation() {
        let root = temp_repo("escape-selection");
        fs::write(root.join("alpha.txt"), "alpha\n").unwrap();
        fs::write(root.join("beta.txt"), "beta\n").unwrap();
        let repository = Repository::discover(&root).unwrap();
        let mut app = App::load(repository).unwrap();

        app.focus = PaneFocus::Diff;
        app.review.changes.code_selection = Some(crate::ui::review::CodeSelection::from_rows(
            ReviewSide::After,
            [0],
        ));
        press(&mut app, KeyCode::Esc);
        assert!(app.review.changes.code_selection.is_none());
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.focus, PaneFocus::Diff);

        app.focus = PaneFocus::Files;
        let file_row = app
            .files
            .tree_rows
            .iter()
            .position(|row| matches!(row.kind, crate::ui::files::TreeRowKind::File { .. }))
            .unwrap();
        app.files
            .tree_selection
            .insert(app.tree_selection_key(file_row).unwrap());
        press(&mut app, KeyCode::Esc);
        assert!(app.files.tree_selection.is_empty());
        assert!(matches!(app.overlay, Overlay::None));

        app.focus = PaneFocus::Diff;
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, PaneFocus::Files);

        fs::remove_dir_all(root).unwrap();
    }
}
