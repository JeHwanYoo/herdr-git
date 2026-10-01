use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

pub(super) const ACCENT: Color = Color::Cyan;
pub(super) const MUTED: Color = Color::DarkGray;
pub(super) const HINT: Color = Color::Rgb(220, 227, 235);
pub(super) const SECONDARY: Color = Color::Rgb(139, 148, 158);
pub(super) const ERROR: Color = Color::Red;
pub(super) const SUCCESS: Color = Color::Green;
pub(super) const WARNING: Color = Color::Yellow;
pub(super) const TEXT: Color = Color::White;
pub(super) const TEXT_INVERSE: Color = Color::Black;
pub(super) const REFERENCE: Color = Color::LightYellow;
pub(super) const STATUS_TYPE_CHANGE: Color = Color::Blue;
pub(super) const STATUS_UNMERGED: Color = Color::Magenta;
pub(super) const SURFACE_FOCUS: Color = Color::Rgb(48, 53, 61);
pub(super) const SURFACE_HOVER: Color = Color::Rgb(38, 58, 82);
pub(super) const SURFACE_SELECTION: Color = Color::Rgb(52, 74, 110);
pub(super) const SURFACE_PANEL: Color = Color::Rgb(24, 26, 30);
pub(super) const SURFACE_HEADER: Color = Color::Rgb(32, 35, 40);
pub(super) const SURFACE_INERT: Color = Color::Rgb(45, 45, 48);
pub(super) const RULE: Color = Color::Rgb(74, 78, 86);
pub(super) const DIFF_ADDED_BG: Color = Color::Rgb(31, 68, 48);
pub(super) const DIFF_REMOVED_BG: Color = Color::Rgb(72, 36, 41);
pub(super) const GRAPH_LANES: [Color; 6] = [
    Color::Rgb(255, 194, 103),
    Color::Rgb(121, 229, 242),
    Color::Rgb(184, 233, 134),
    Color::Rgb(237, 201, 255),
    Color::Rgb(255, 182, 168),
    Color::Rgb(184, 212, 255),
];
pub(super) fn graph_lane(index: usize) -> Color {
    GRAPH_LANES[index % GRAPH_LANES.len()]
}

pub(super) const BADGE_HEAD_BG: Color = Color::Rgb(76, 43, 30);
pub(super) const BADGE_LOCAL_BG: Color = Color::Rgb(25, 58, 38);
pub(super) const BADGE_REMOTE_BG: Color = Color::Rgb(20, 44, 68);
pub(super) const BADGE_REMOTE_HEAD_BG: Color = Color::Rgb(54, 36, 70);
pub(super) const BADGE_TAG_BG: Color = Color::Rgb(58, 50, 20);
pub(super) const BADGE_OTHER_BG: Color = Color::Rgb(50, 31, 55);

pub(super) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(super) const SPINNER_FRAME_DURATION: Duration = Duration::from_millis(100);
const CURSOR_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
pub(super) const CURSOR_GLYPH: &str = "▏";
pub(super) const LIST_MARKER: &str = "▶ ";
pub(super) const BRANCH_GLYPH: &str = "\u{f418}";
pub(super) const REMOTE_GLYPH: &str = "\u{f0ac}";
pub(super) const TAG_GLYPH: &str = "\u{f02b}";

pub(super) const DIALOG_NARROW: u16 = 56;
pub(super) const DIALOG_MEDIUM: u16 = 68;
pub(super) const DIALOG_WIDE: u16 = 78;
pub(super) const DIALOG_CARD: u16 = 82;

pub(super) fn focus_row() -> Style {
    Style::default()
        .bg(SURFACE_FOCUS)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn selection_row() -> Style {
    Style::default()
        .bg(SURFACE_SELECTION)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn hover(base: Style, hovered: bool) -> Style {
    if hovered {
        base.bg(SURFACE_HOVER)
    } else {
        base
    }
}

pub(super) fn hint() -> Style {
    Style::default().fg(HINT)
}

pub(super) fn secondary() -> Style {
    Style::default().fg(SECONDARY)
}

pub(super) fn disabled() -> Style {
    Style::default().fg(MUTED)
}

pub(super) fn error_text() -> Style {
    Style::default().fg(ERROR)
}

pub(super) fn error_title() -> Style {
    Style::default().fg(ERROR).add_modifier(Modifier::BOLD)
}

pub(super) fn success_title() -> Style {
    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)
}

pub(super) fn text() -> Style {
    Style::default().fg(TEXT)
}

pub(super) fn warning_text() -> Style {
    Style::default().fg(WARNING)
}

pub(super) fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub(super) fn accent_bold() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub(super) fn search_match() -> Style {
    Style::default().fg(TEXT_INVERSE).bg(WARNING)
}

pub(super) fn section_header() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub(super) fn spinner_frame(elapsed: Duration) -> &'static str {
    let frame = elapsed.as_millis() / SPINNER_FRAME_DURATION.as_millis();
    SPINNER_FRAMES[(frame as usize) % SPINNER_FRAMES.len()]
}

pub(super) fn cursor_blink_visible(elapsed: Duration) -> bool {
    (elapsed.as_millis() / CURSOR_BLINK_HALF_PERIOD.as_millis()).is_multiple_of(2)
}

pub(super) fn cursor_span(elapsed: Duration) -> Span<'static> {
    let glyph = if cursor_blink_visible(elapsed) {
        CURSOR_GLYPH
    } else {
        " "
    };
    Span::styled(glyph, accent_bold())
}

pub(super) fn spinner_span(elapsed: Duration) -> Span<'static> {
    Span::styled(spinner_frame(elapsed), accent())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::style::{Color, Modifier, Style};

    use super::{
        ACCENT, CURSOR_GLYPH, ERROR, HINT, SPINNER_FRAMES, SURFACE_FOCUS, SURFACE_HOVER,
        SURFACE_SELECTION, cursor_span, error_title, focus_row, hint, hover, selection_row,
        spinner_frame, spinner_span,
    };

    #[test]
    fn focus_and_selection_rows_are_bold_on_distinct_surfaces() {
        assert_eq!(focus_row().bg, Some(SURFACE_FOCUS));
        assert!(focus_row().add_modifier.contains(Modifier::BOLD));
        assert_eq!(selection_row().bg, Some(SURFACE_SELECTION));
        assert!(selection_row().add_modifier.contains(Modifier::BOLD));
        assert_ne!(SURFACE_SELECTION, SURFACE_HOVER);
        assert_ne!(SURFACE_SELECTION, SURFACE_FOCUS);
    }

    #[test]
    fn hover_only_changes_the_background_when_hovered() {
        let base = Style::default().fg(Color::White);
        assert_eq!(hover(base, true).bg, Some(SURFACE_HOVER));
        assert_eq!(hover(base, true).fg, Some(Color::White));
        assert_eq!(hover(base, false), base);
    }

    #[test]
    fn hint_is_readable_without_italics_and_error_title_is_bold_red() {
        assert_eq!(hint().fg, Some(HINT));
        assert!(!hint().add_modifier.contains(Modifier::ITALIC));
        assert_eq!(error_title().fg, Some(ERROR));
        assert!(error_title().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn spinner_cycles_every_hundred_milliseconds() {
        assert_eq!(spinner_frame(Duration::ZERO), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(Duration::from_millis(150)), SPINNER_FRAMES[1]);
        assert_eq!(
            spinner_frame(Duration::from_millis(1_000)),
            SPINNER_FRAMES[0]
        );
        let span = spinner_span(Duration::from_millis(250));
        assert_eq!(span.content.as_ref(), SPINNER_FRAMES[2]);
        assert_eq!(span.style.fg, Some(ACCENT));
    }

    #[test]
    fn cursor_blinks_with_a_half_second_phase_in_accent_bold() {
        let visible = cursor_span(Duration::from_millis(100));
        assert_eq!(visible.content.as_ref(), CURSOR_GLYPH);
        assert_eq!(visible.style.fg, Some(ACCENT));
        assert!(visible.style.add_modifier.contains(Modifier::BOLD));
        let hidden = cursor_span(Duration::from_millis(600));
        assert_eq!(hidden.content.as_ref(), " ");
        assert_eq!(
            cursor_span(Duration::from_millis(1_100)).content.as_ref(),
            CURSOR_GLYPH
        );
    }
}
