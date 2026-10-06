use std::time::Instant;

use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation};
use unicode_width::UnicodeWidthChar;

use super::theme::{self, ACCENT, MUTED, accent, focus_row, hint, hover};
pub(super) use layout::{
    anchored, area_hovered, centered, scrolled_content_row_at, viewport_offset,
};
pub(super) use picker::{PickerEdit, PickerOutcome, PickerState, picker_edit, update_picker};

mod layout;
mod picker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TextEdit {
    Insert(char),
    Backspace,
    Paste(String),
    Newline,
}

#[derive(Clone, Debug)]
pub(super) struct TextField {
    pub(super) text: String,
    pub(super) cursor_started: Instant,
    pub(super) error: Option<String>,
}

impl Default for TextField {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor_started: Instant::now(),
            error: None,
        }
    }
}

impl TextField {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn edit(&mut self, edit: TextEdit) -> bool {
        let changed = match edit {
            TextEdit::Insert(character) => {
                self.text.push(character);
                true
            }
            TextEdit::Backspace => self.text.pop().is_some(),
            TextEdit::Paste(text) if !text.is_empty() => {
                self.text.push_str(&text);
                true
            }
            TextEdit::Paste(_) => false,
            TextEdit::Newline => {
                self.text.push('\n');
                true
            }
        };
        self.error = None;
        if changed {
            self.cursor_started = Instant::now();
        }
        changed
    }

    pub(super) fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ListCursor {
    pub(super) selected: usize,
    pub(super) scroll: usize,
}

impl ListCursor {
    pub(super) fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(len - 1);
    }

    pub(super) fn page(&mut self, delta_pages: isize, len: usize, page: usize) {
        let rows = isize::try_from(page).unwrap_or(isize::MAX);
        self.move_by(delta_pages.saturating_mul(rows), len);
    }

    pub(super) fn home(&mut self) {
        self.selected = 0;
    }

    pub(super) fn end(&mut self, len: usize) {
        self.selected = len.saturating_sub(1);
    }

    pub(super) fn clamp(&mut self, len: usize) {
        let last = len.saturating_sub(1);
        self.selected = self.selected.min(last);
        self.scroll = self.scroll.min(last);
    }

    pub(super) fn row_at(
        &self,
        area: Rect,
        pointer: (u16, u16),
        len: usize,
        border: u16,
    ) -> Option<usize> {
        let inner = area.inner(Margin::new(border, border));
        let (column, row) = pointer;
        if !inner.contains((column, row).into()) {
            return None;
        }
        let index = usize::from(row - inner.y).checked_add(self.scroll)?;
        (index < len).then_some(index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScrollbarDrag {
    pub(super) area: Rect,
    pub(super) axis: Axis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfirmButton {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ConfirmButtons {
    pub(super) primary: Rect,
    pub(super) secondary: Rect,
}

impl ConfirmButtons {
    pub(super) fn hit(&self, pointer: (u16, u16)) -> Option<ConfirmButton> {
        let position = pointer.into();
        if self.primary.contains(position) {
            Some(ConfirmButton::Primary)
        } else if self.secondary.contains(position) {
            Some(ConfirmButton::Secondary)
        } else {
            None
        }
    }
}

pub(super) fn truncate_to_width(value: &str, max_width: usize) -> String {
    if Line::from(value).width() <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut shortened = String::new();
    for character in value.chars() {
        let candidate = format!("{shortened}{character}…");
        if Line::from(candidate.as_str()).width() > max_width {
            break;
        }
        shortened.push(character);
    }
    shortened.push('…');
    shortened
}

pub(super) fn text_display_width(text: &str) -> usize {
    text.chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or_default())
        .sum()
}

pub(super) fn counted_title(label: &str, count: usize) -> String {
    if count == 0 {
        label.to_owned()
    } else {
        format!("{label} · {count}")
    }
}

pub(super) fn shortcut_spans(
    key: char,
    label: &str,
    label_gap: &str,
    suffix: &str,
    menu_style: Style,
    enabled: bool,
) -> Vec<Span<'static>> {
    let key_style = if !enabled {
        menu_style
    } else if menu_style.bg == Some(theme::ACCENT) {
        menu_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        menu_style.fg(theme::ACCENT).add_modifier(Modifier::BOLD)
    };
    vec![
        Span::styled(key.to_string(), key_style),
        Span::styled(format!("{label_gap}{label}{suffix}"), menu_style),
    ]
}

pub(super) fn dialog_frame(frame: &mut Frame<'_>, title: &str, width: u16, height: u16) -> Rect {
    dialog_frame_at(frame, title, centered(width, height, frame.area()))
}

pub(super) fn dialog_frame_at(frame: &mut Frame<'_>, title: &str, area: Rect) -> Rect {
    styled_frame_at(frame, Span::raw(title.to_owned()), area)
}

pub(super) fn dialog_frame_titled(
    frame: &mut Frame<'_>,
    title: Span<'static>,
    width: u16,
    height: u16,
) -> Rect {
    styled_frame_at(frame, title, centered(width, height, frame.area()))
}

fn styled_frame_at(frame: &mut Frame<'_>, title: Span<'static>, area: Rect) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

pub(super) fn dialog_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    labels: [&str; 2],
    focused: Option<ConfirmButton>,
    pointer: Option<(u16, u16)>,
) -> ConfirmButtons {
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    for (cell, label, button) in [
        (cells[0], labels[0], ConfirmButton::Primary),
        (cells[1], labels[1], ConfirmButton::Secondary),
    ] {
        frame.render_widget(
            dialog_button(label, focused == Some(button), area_hovered(pointer, cell)),
            cell,
        );
    }
    ConfirmButtons {
        primary: cells[0],
        secondary: cells[1],
    }
}

pub(super) fn dialog_button(label: &str, focused: bool, hovered: bool) -> Paragraph<'static> {
    let base = if focused {
        focus_row()
    } else {
        Style::default()
    };
    Paragraph::new(label.to_owned())
        .alignment(Alignment::Center)
        .style(hover(base, hovered))
        .block(Block::default().borders(Borders::ALL))
}

pub(super) fn footer_hint(text: &str) -> Paragraph<'static> {
    Paragraph::new(text.to_owned()).style(hint())
}

pub(super) fn scrollbar(orientation: ScrollbarOrientation) -> Scrollbar<'static> {
    let (track, thumb) = match orientation {
        ScrollbarOrientation::VerticalRight | ScrollbarOrientation::VerticalLeft => ("┃", "█"),
        ScrollbarOrientation::HorizontalBottom | ScrollbarOrientation::HorizontalTop => ("─", "▄"),
    };
    Scrollbar::new(orientation)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some(track))
        .track_style(Style::default().fg(MUTED))
        .thumb_symbol(thumb)
        .thumb_style(Style::default().fg(ACCENT))
}

pub(super) fn changed_area(before: &Buffer, after: &Buffer) -> Rect {
    let mut bounds: Option<(u16, u16, u16, u16)> = None;
    for (index, (old, new)) in before.content.iter().zip(&after.content).enumerate() {
        if old == new {
            continue;
        }
        let (x, y) = after.pos_of(index);
        bounds = Some(match bounds {
            Some((left, top, right, bottom)) => {
                (left.min(x), top.min(y), right.max(x), bottom.max(y))
            }
            None => (x, y, x, y),
        });
    }
    bounds.map_or_else(Rect::default, |(left, top, right, bottom)| {
        Rect::new(left, top, right - left + 1, bottom - top + 1)
    })
}

pub(super) fn pane_block<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let block = Block::default().borders(Borders::ALL).title(title);
    if focused {
        block
            .border_style(accent())
            .title_style(Style::default().fg(Color::Reset))
    } else {
        block
    }
}

pub(super) fn draw_filter_bar(frame: &mut Frame<'_>, area: Rect, query: &TextField, active: bool) {
    let mut spans = vec![Span::raw(format!("/ {}", query.text))];
    if active {
        spans.push(theme::cursor_span(query.cursor_started.elapsed()));
    }
    let style = if active {
        theme::warning_text()
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(style)
            .block(Block::default().borders(Borders::ALL).title("Filter")),
        area,
    );
}

pub(super) fn chord(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
}

pub(super) fn left_click(input: &Event) -> Option<(u16, u16)> {
    match input {
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) => {
            Some((mouse.column, mouse.row))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::widgets::Widget;

    use crate::ui::theme::{ACCENT, ERROR, HINT, MUTED, SURFACE_FOCUS, SURFACE_HOVER, error_title};

    use super::*;

    #[test]
    fn changed_area_bounds_every_cell_a_dialog_drew() {
        let area = Rect::new(0, 0, 20, 10);
        let before = Buffer::empty(area);
        let mut after = before.clone();
        assert_eq!(super::changed_area(&before, &after), Rect::default());
        after[(4, 2)].set_symbol("┌");
        after[(11, 6)].set_symbol("┘");
        assert_eq!(super::changed_area(&before, &after), Rect::new(4, 2, 8, 5));
    }

    #[test]
    fn footer_hint_uses_the_hint_style() {
        let buffer = rendered(footer_hint("[Enter] Save"), 14, 1);
        assert_eq!(buffer[(0, 0)].fg, HINT);
        assert_eq!(buffer[(1, 0)].symbol(), "E");
    }

    #[test]
    fn scrollbar_symbols_follow_the_orientation_without_bold() {
        let vertical = scrollbar(ScrollbarOrientation::VerticalRight);
        let horizontal = scrollbar(ScrollbarOrientation::HorizontalBottom);
        let mut state = ratatui::widgets::ScrollbarState::new(10).position(0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 4));
        ratatui::widgets::StatefulWidget::render(vertical, buffer.area, &mut buffer, &mut state);
        assert_eq!(buffer[(0, 0)].symbol(), "█");
        assert_eq!(buffer[(0, 0)].fg, ACCENT);
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 3)].symbol(), "┃");
        assert_eq!(buffer[(0, 3)].fg, MUTED);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        ratatui::widgets::StatefulWidget::render(horizontal, buffer.area, &mut buffer, &mut state);
        assert_eq!(buffer[(0, 0)].symbol(), "▄");
        assert_eq!(buffer[(3, 0)].symbol(), "─");
    }

    fn rendered(widget: impl Widget, width: u16, height: u16) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        widget.render(buffer.area, &mut buffer);
        buffer
    }

    #[test]
    fn dialog_frame_clears_and_returns_the_inner_area() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut inner = Rect::default();
        terminal
            .draw(|frame| inner = dialog_frame(frame, "Title", 20, 6))
            .unwrap();
        assert_eq!(inner, Rect::new(11, 4, 18, 4));
        let buffer = terminal.backend().buffer();
        let top = (10..30)
            .map(|column| buffer[(column, 3)].symbol())
            .collect::<String>();
        assert!(top.contains("Title"));
        assert_eq!(buffer[(10, 4)].symbol(), "│");

        terminal
            .draw(|frame| {
                inner = dialog_frame_titled(frame, Span::styled("Failed", error_title()), 20, 6)
            })
            .unwrap();
        assert_eq!(inner, Rect::new(11, 4, 18, 4));
        let buffer = terminal.backend().buffer();
        let title_column = (10..30)
            .find(|column| buffer[(*column, 3)].symbol() == "F")
            .unwrap();
        assert_eq!(buffer[(title_column, 3)].fg, ERROR);
        assert!(buffer[(title_column, 3)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dialog_footer_splits_the_area_and_marks_the_focused_button() {
        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        let mut buttons = None;
        terminal
            .draw(|frame| {
                buttons = Some(dialog_footer(
                    frame,
                    frame.area(),
                    ["[Enter] Run", "[Esc] Close"],
                    Some(ConfirmButton::Secondary),
                    Some((2, 1)),
                ));
            })
            .unwrap();
        let buttons = buttons.unwrap();
        assert_eq!(buttons.primary, Rect::new(0, 0, 20, 3));
        assert_eq!(buttons.secondary, Rect::new(20, 0, 20, 3));
        let buffer = terminal.backend().buffer();
        let row = (0..40)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        assert!(row.contains("[Enter] Run"));
        assert!(row.contains("[Esc] Close"));
        assert_eq!(buffer[(2, 1)].bg, SURFACE_HOVER);
        assert_eq!(buffer[(25, 1)].bg, SURFACE_FOCUS);
        assert!(buffer[(25, 1)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn dialog_button_is_bordered_centered_and_shows_focus_and_hover() {
        let plain = rendered(dialog_button("OK", false, false), 10, 3);
        let text = (0..10)
            .map(|column| plain[(column, 1)].symbol())
            .collect::<String>();
        assert_eq!(text, "│   OK   │");
        assert_eq!(plain[(4, 1)].bg, Color::Reset);
        let focused = rendered(dialog_button("OK", true, false), 10, 3);
        assert_eq!(focused[(4, 1)].bg, SURFACE_FOCUS);
        assert!(focused[(4, 1)].modifier.contains(Modifier::BOLD));
        let hovered = rendered(dialog_button("OK", false, true), 10, 3);
        assert_eq!(hovered[(4, 1)].bg, SURFACE_HOVER);
    }

    #[test]
    fn pane_block_marks_focus_with_an_accent_border_only() {
        let focused = rendered(pane_block("Files", true), 12, 3);
        assert_eq!(focused[(0, 0)].fg, ACCENT);
        assert_eq!(focused[(1, 0)].symbol(), "F");
        assert_eq!(focused[(1, 0)].fg, Color::Reset);
        let unfocused = rendered(pane_block("Files", false), 12, 3);
        assert_eq!(unfocused[(0, 0)].fg, Color::Reset);
        assert_eq!(unfocused[(1, 0)].symbol(), "F");
    }

    #[test]
    fn list_counts_appear_only_when_the_list_has_items() {
        assert_eq!(counted_title("Files", 0), "Files");
        assert_eq!(counted_title("Files", 3), "Files · 3");
    }

    fn aged_field(text: &str) -> (TextField, Instant) {
        let started = Instant::now() - Duration::from_secs(1);
        let field = TextField {
            text: text.to_owned(),
            cursor_started: started,
            error: Some("stale".to_owned()),
        };
        (field, started)
    }

    #[test]
    fn text_edits_report_changes_and_reset_the_cursor() {
        let (mut field, started) = aged_field("");
        assert!(field.edit(TextEdit::Insert('a')));
        assert_eq!(field.text, "a");
        assert!(field.cursor_started > started);

        let (mut field, started) = aged_field("ab");
        assert!(field.edit(TextEdit::Backspace));
        assert_eq!(field.text, "a");
        assert!(field.cursor_started > started);

        let (mut field, started) = aged_field("a");
        assert!(field.edit(TextEdit::Paste("bc".to_owned())));
        assert_eq!(field.text, "abc");
        assert!(field.cursor_started > started);

        let (mut field, started) = aged_field("a");
        assert!(field.edit(TextEdit::Newline));
        assert_eq!(field.text, "a\n");
        assert!(field.cursor_started > started);
        assert!(!field.is_empty());
        assert!(TextField::new().is_empty());
    }

    #[test]
    fn rejected_text_edits_keep_the_cursor_but_clear_the_error() {
        let (mut field, started) = aged_field("");
        assert!(!field.edit(TextEdit::Backspace));
        assert_eq!(field.cursor_started, started);
        assert_eq!(field.error, None);

        let (mut field, started) = aged_field("a");
        assert!(!field.edit(TextEdit::Paste(String::new())));
        assert_eq!(field.text, "a");
        assert_eq!(field.cursor_started, started);
    }

    #[test]
    fn text_field_errors_are_set_and_cleared_by_the_next_edit() {
        let mut field = TextField::new();
        field.set_error("Enter a name");
        assert_eq!(field.error.as_deref(), Some("Enter a name"));
        field.edit(TextEdit::Insert('a'));
        assert_eq!(field.error, None);
    }

    #[test]
    fn move_by_clamps_at_both_ends() {
        let mut cursor = ListCursor::default();
        cursor.move_by(-3, 5);
        assert_eq!(cursor.selected, 0);
        cursor.move_by(2, 5);
        assert_eq!(cursor.selected, 2);
        cursor.move_by(10, 5);
        assert_eq!(cursor.selected, 4);
        cursor.move_by(-1, 5);
        assert_eq!(cursor.selected, 3);
        cursor.move_by(1, 0);
        assert_eq!(cursor.selected, 0);
    }

    #[test]
    fn page_home_end_and_clamp_stay_in_range() {
        let mut cursor = ListCursor::default();
        cursor.page(1, 30, 10);
        assert_eq!(cursor.selected, 10);
        cursor.page(5, 30, 10);
        assert_eq!(cursor.selected, 29);
        cursor.page(-1, 30, 10);
        assert_eq!(cursor.selected, 19);
        cursor.page(-9, 30, 10);
        assert_eq!(cursor.selected, 0);

        cursor.end(30);
        assert_eq!(cursor.selected, 29);
        cursor.home();
        assert_eq!(cursor.selected, 0);
        cursor.end(0);
        assert_eq!(cursor.selected, 0);

        let mut cursor = ListCursor {
            selected: 12,
            scroll: 9,
        };
        cursor.clamp(4);
        assert_eq!(
            cursor,
            ListCursor {
                selected: 3,
                scroll: 3
            }
        );
        cursor.clamp(0);
        assert_eq!(cursor, ListCursor::default());
    }

    #[test]
    fn row_at_accounts_for_scroll_and_border() {
        let area = Rect::new(10, 5, 20, 6);
        let cursor = ListCursor {
            selected: 0,
            scroll: 3,
        };

        assert_eq!(cursor.row_at(area, (12, 6), 10, 1), Some(3));
        assert_eq!(cursor.row_at(area, (12, 9), 10, 1), Some(6));
        assert_eq!(cursor.row_at(area, (12, 5), 10, 1), None);
        assert_eq!(cursor.row_at(area, (12, 10), 10, 1), None);
        assert_eq!(cursor.row_at(area, (10, 6), 10, 1), None);
        assert_eq!(cursor.row_at(area, (29, 6), 10, 1), None);
        assert_eq!(cursor.row_at(area, (12, 9), 6, 1), None);

        assert_eq!(cursor.row_at(area, (10, 5), 10, 0), Some(3));
        assert_eq!(cursor.row_at(area, (29, 10), 10, 0), Some(8));
        assert_eq!(cursor.row_at(area, (30, 5), 10, 0), None);
        assert_eq!(cursor.row_at(area, (12, 6), 10, 3), None);
    }

    #[test]
    fn confirm_buttons_hit_test_each_area() {
        let buttons = ConfirmButtons {
            primary: Rect::new(2, 8, 10, 3),
            secondary: Rect::new(14, 8, 10, 3),
        };
        assert_eq!(buttons.hit((2, 8)), Some(ConfirmButton::Primary));
        assert_eq!(buttons.hit((11, 10)), Some(ConfirmButton::Primary));
        assert_eq!(buttons.hit((14, 9)), Some(ConfirmButton::Secondary));
        assert_eq!(buttons.hit((12, 9)), None);
        assert_eq!(buttons.hit((5, 11)), None);
        assert_eq!(ConfirmButtons::default().hit((0, 0)), None);
    }
}
