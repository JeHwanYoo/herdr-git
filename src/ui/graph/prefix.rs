use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::git::GraphPrefix;
use crate::ui::theme;

pub(in crate::ui) fn width(graph: &GraphPrefix) -> usize {
    UnicodeWidthStr::width(graph.text.as_str())
}

pub(super) fn spans(graph: &GraphPrefix, offset: usize, width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rendered_width: usize = 0;
    for (column, character) in graph.text.chars().enumerate().skip(offset) {
        let symbol = match character {
            '*' => '●',
            '|' => '│',
            '/' => '╱',
            '\\' => '╲',
            '-' | '_' => '─',
            other => other,
        };
        let symbol_width = UnicodeWidthChar::width(symbol).unwrap_or_default();
        if rendered_width.saturating_add(symbol_width) > width {
            break;
        }
        let style = if symbol.is_whitespace() {
            Style::default()
        } else {
            let color = graph
                .colors
                .get(column)
                .copied()
                .flatten()
                .map(usize::from)
                .unwrap_or(column / 2);
            Style::default().fg(theme::GRAPH_LANES[color % theme::GRAPH_LANES.len()])
        };
        spans.push(Span::styled(symbol.to_string(), style));
        rendered_width += symbol_width;
    }
    if width > rendered_width {
        spans.push(Span::raw(" ".repeat(width - rendered_width)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::*;

    #[test]
    fn preserves_git_columns_and_colors_paths() {
        let graph = GraphPrefix {
            text: "| *".to_owned(),
            colors: vec![Some(0), None, Some(1)],
        };
        let spans = spans(&graph, 0, 5);
        assert_eq!(Line::from(spans.clone()).width(), 5);
        assert_eq!(spans[0].content, "│");
        assert_eq!(spans[2].content, "●");
        assert_eq!(spans[0].style.fg, Some(theme::GRAPH_LANES[0]));
        assert_eq!(spans[2].style.fg, Some(theme::GRAPH_LANES[1]));
    }
}
