use std::collections::{HashMap, HashSet};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use unicode_width::UnicodeWidthChar;

use super::theme::{
    ACCENT, DIFF_ADDED_BG, DIFF_REMOVED_BG, HINT, SURFACE_HEADER, SURFACE_INERT, WARNING,
};

pub(super) struct SyntaxHighlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

#[derive(Debug, Default)]
pub(super) struct DiffDocument {
    rows: Vec<DiffRow>,
    before_max_width: usize,
    after_max_width: usize,
    before_rows: HashMap<usize, usize>,
    after_rows: HashMap<usize, usize>,
}

#[derive(Debug)]
pub(super) struct DiffRow {
    before: DiffCell,
    after: DiffCell,
    fold: Option<FoldKey>,
}

#[derive(Debug)]
pub(super) struct DiffCell {
    rendered: Line<'static>,
    line_number: Option<usize>,
    source: Option<String>,
}

impl DiffDocument {
    #[cfg(test)]
    pub(super) fn from_rows(rows: Vec<DiffRow>) -> Self {
        let mut document = Self::default();
        for row in rows {
            document.push(row);
        }
        document
    }

    pub(super) fn plain(text: &str) -> Self {
        let mut document = Self::default();
        for line in text.lines() {
            let style = metadata_style(line);
            document.push(DiffRow::new(
                DiffCell::new(Line::styled(line.to_owned(), style), None, None),
                DiffCell::new(Line::styled(line.to_owned(), style), None, None),
                None,
            ));
        }
        document
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn before_max_width(&self) -> usize {
        self.before_max_width
    }

    pub(super) fn after_max_width(&self) -> usize {
        self.after_max_width
    }

    pub(super) fn before_row_for_line(&self, line: usize) -> Option<usize> {
        self.before_rows.get(&line).copied()
    }

    pub(super) fn after_row_for_line(&self, line: usize) -> Option<usize> {
        self.after_rows.get(&line).copied()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[cfg(test)]
    pub(super) fn before_lines(&self) -> impl Iterator<Item = &Line<'static>> + '_ {
        self.rows.iter().map(|row| row.before().rendered())
    }

    #[cfg(test)]
    pub(super) fn after_lines(&self) -> impl Iterator<Item = &Line<'static>> + '_ {
        self.rows.iter().map(|row| row.after().rendered())
    }

    pub(super) fn before_line(&self, row: usize) -> Option<&Line<'static>> {
        self.rows.get(row).map(|row| row.before().rendered())
    }

    pub(super) fn after_line(&self, row: usize) -> Option<&Line<'static>> {
        self.rows.get(row).map(|row| row.after().rendered())
    }

    pub(super) fn before_line_number(&self, row: usize) -> Option<usize> {
        self.rows
            .get(row)
            .and_then(|row| row.before().line_number())
    }

    pub(super) fn after_line_number(&self, row: usize) -> Option<usize> {
        self.rows.get(row).and_then(|row| row.after().line_number())
    }

    pub(super) fn before_source(&self, row: usize) -> Option<&str> {
        self.rows.get(row).and_then(|row| row.before().source())
    }

    pub(super) fn after_source(&self, row: usize) -> Option<&str> {
        self.rows.get(row).and_then(|row| row.after().source())
    }

    pub(super) fn fold_key(&self, row: usize) -> Option<FoldKey> {
        self.rows.get(row).and_then(DiffRow::fold_key)
    }

    pub(super) fn folds(&self) -> impl DoubleEndedIterator<Item = (usize, FoldKey)> + '_ {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(row, value)| value.fold_key().map(|key| (row, key)))
    }

    fn push(&mut self, row: DiffRow) {
        let index = self.rows.len();
        self.before_max_width = self.before_max_width.max(row.before().rendered().width());
        self.after_max_width = self.after_max_width.max(row.after().rendered().width());
        if let Some(line) = row.before().line_number() {
            self.before_rows.insert(line, index);
        }
        if let Some(line) = row.after().line_number() {
            self.after_rows.insert(line, index);
        }
        self.rows.push(row);
    }
}

impl DiffRow {
    pub(super) fn new(before: DiffCell, after: DiffCell, fold: Option<FoldKey>) -> Self {
        Self {
            before,
            after,
            fold,
        }
    }

    pub(super) fn before(&self) -> &DiffCell {
        &self.before
    }

    pub(super) fn after(&self) -> &DiffCell {
        &self.after
    }

    pub(super) fn fold_key(&self) -> Option<FoldKey> {
        self.fold
    }
}

impl DiffCell {
    pub(super) fn new(
        rendered: Line<'static>,
        line_number: Option<usize>,
        source: Option<String>,
    ) -> Self {
        Self {
            rendered,
            line_number,
            source,
        }
    }

    pub(super) fn rendered(&self) -> &Line<'static> {
        &self.rendered
    }

    pub(super) fn line_number(&self) -> Option<usize> {
        self.line_number
    }

    pub(super) fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum FoldKind {
    Context,
    Change,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct FoldKey {
    pub(super) kind: FoldKind,
    pub(super) old_start: usize,
    pub(super) old_end: usize,
    pub(super) new_start: usize,
    pub(super) new_end: usize,
}

impl SyntaxHighlighter {
    pub(super) fn new() -> Self {
        let syntaxes = two_face::syntax::extra_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes.themes["base16-ocean.dark"].clone();
        Self { syntaxes, theme }
    }

    pub(super) fn language_name(&self, path: &str) -> Option<&str> {
        self.syntax_for_path(path)
            .map(|syntax| syntax.name.as_str())
    }

    #[cfg(test)]
    pub(super) fn side_by_side_diff_with_folds(
        &self,
        diff: &str,
        path: Option<&str>,
        fold_toggles: &HashSet<FoldKey>,
    ) -> DiffDocument {
        self.side_by_side_diff_with_folds_cancellable(diff, path, fold_toggles, &|| false)
            .expect("non-cancellable highlighting completes")
    }

    pub(super) fn side_by_side_diff_with_folds_cancellable(
        &self,
        diff: &str,
        path: Option<&str>,
        fold_toggles: &HashSet<FoldKey>,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<DiffDocument> {
        let collapsed = collapse_context(diff, fold_toggles, cancelled)?;
        let syntax = path.and_then(|path| self.syntax_for_path(path));
        let mut before_highlighter = syntax.map(|syntax| HighlightLines::new(syntax, &self.theme));
        let mut after_highlighter = syntax.map(|syntax| HighlightLines::new(syntax, &self.theme));
        let mut document = DiffDocument::default();
        let mut old_line = 0;
        let mut new_line = 0;
        let mut removed = Vec::new();
        let mut added = Vec::new();

        for line in collapsed.lines() {
            if cancelled() {
                return None;
            }
            if let Some((old_start, new_start)) = hunk_starts(line) {
                if !flush_change_block(
                    &mut document,
                    &mut removed,
                    &mut added,
                    fold_toggles,
                    &mut old_line,
                    &mut new_line,
                    &mut before_highlighter,
                    &mut after_highlighter,
                    &self.syntaxes,
                    cancelled,
                ) {
                    return None;
                }
                old_line = old_start;
                new_line = new_start;
                let style = metadata_style(line);
                document.push(DiffRow::new(
                    DiffCell::new(Line::styled(line.to_owned(), style), None, None),
                    DiffCell::new(Line::styled(line.to_owned(), style), None, None),
                    None,
                ));
            } else if let Some((key, expanded)) = parse_fold_marker(line) {
                if !flush_change_block(
                    &mut document,
                    &mut removed,
                    &mut added,
                    fold_toggles,
                    &mut old_line,
                    &mut new_line,
                    &mut before_highlighter,
                    &mut after_highlighter,
                    &self.syntaxes,
                    cancelled,
                ) {
                    return None;
                }
                let count = key.old_end.saturating_sub(key.old_start);
                let label = fold_label(key.kind, count, expanded);
                let style = fold_style();
                document.push(DiffRow::new(
                    DiffCell::new(Line::styled(label.clone(), style), None, None),
                    DiffCell::new(Line::styled(label, style), None, None),
                    Some(key),
                ));
                if !expanded {
                    old_line = key.old_end;
                    new_line = key.new_end;
                }
            } else if line.starts_with('-') && !line.starts_with("---") {
                removed.push(line[1..].to_owned());
            } else if line.starts_with('+') && !line.starts_with("+++") {
                added.push(line[1..].to_owned());
            } else if let Some(code) = line.strip_prefix(' ') {
                if !flush_change_block(
                    &mut document,
                    &mut removed,
                    &mut added,
                    fold_toggles,
                    &mut old_line,
                    &mut new_line,
                    &mut before_highlighter,
                    &mut after_highlighter,
                    &self.syntaxes,
                    cancelled,
                ) {
                    return None;
                }
                let before = highlight_numbered_line(
                    old_line,
                    code,
                    &mut before_highlighter,
                    &self.syntaxes,
                    None,
                );
                let after = highlight_numbered_line(
                    new_line,
                    code,
                    &mut after_highlighter,
                    &self.syntaxes,
                    None,
                );
                document.push(DiffRow::new(
                    DiffCell::new(before, Some(old_line), Some(code.to_owned())),
                    DiffCell::new(after, Some(new_line), Some(code.to_owned())),
                    None,
                ));
                old_line += 1;
                new_line += 1;
            } else {
                if !flush_change_block(
                    &mut document,
                    &mut removed,
                    &mut added,
                    fold_toggles,
                    &mut old_line,
                    &mut new_line,
                    &mut before_highlighter,
                    &mut after_highlighter,
                    &self.syntaxes,
                    cancelled,
                ) {
                    return None;
                }
                let style = metadata_style(line);
                document.push(DiffRow::new(
                    DiffCell::new(Line::styled(line.to_owned(), style), None, None),
                    DiffCell::new(Line::styled(line.to_owned(), style), None, None),
                    None,
                ));
            }
        }
        if !flush_change_block(
            &mut document,
            &mut removed,
            &mut added,
            fold_toggles,
            &mut old_line,
            &mut new_line,
            &mut before_highlighter,
            &mut after_highlighter,
            &self.syntaxes,
            cancelled,
        ) {
            return None;
        }
        Some(document)
    }

    pub(super) fn highlight_file_cancellable(
        &self,
        text: &str,
        path: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<DiffDocument> {
        let mut highlighter = self
            .syntax_for_path(path)
            .map(|syntax| HighlightLines::new(syntax, &self.theme));
        let mut document = DiffDocument::default();
        for (index, code) in text.lines().enumerate() {
            if cancelled() {
                return None;
            }
            let number = index + 1;
            let rendered =
                highlight_numbered_line(number, code, &mut highlighter, &self.syntaxes, None);
            document.push(DiffRow::new(
                DiffCell::new(Line::default(), None, None),
                DiffCell::new(rendered, Some(number), Some(code.to_owned())),
                None,
            ));
        }
        Some(document)
    }

    fn syntax_for_path(&self, path: &str) -> Option<&SyntaxReference> {
        self.syntaxes.find_syntax_for_file(path).ok().flatten()
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_change_block(
    document: &mut DiffDocument,
    removed: &mut Vec<String>,
    added: &mut Vec<String>,
    fold_toggles: &HashSet<FoldKey>,
    old_line: &mut usize,
    new_line: &mut usize,
    before_highlighter: &mut Option<HighlightLines<'_>>,
    after_highlighter: &mut Option<HighlightLines<'_>>,
    syntaxes: &SyntaxSet,
    cancelled: &dyn Fn() -> bool,
) -> bool {
    let rows = removed.len().max(added.len());
    if rows == 0 {
        return true;
    }
    let key = FoldKey {
        kind: FoldKind::Change,
        old_start: *old_line,
        old_end: old_line.saturating_add(removed.len()),
        new_start: *new_line,
        new_end: new_line.saturating_add(added.len()),
    };
    let expanded = !fold_toggles.contains(&key);
    let label = fold_label(key.kind, rows, expanded);
    let style = fold_style();
    document.push(DiffRow::new(
        DiffCell::new(Line::styled(label.clone(), style), None, None),
        DiffCell::new(Line::styled(label, style), None, None),
        Some(key),
    ));
    if !expanded {
        *old_line = key.old_end;
        *new_line = key.new_end;
        removed.clear();
        added.clear();
        return true;
    }
    for row in 0..rows {
        if cancelled() {
            return false;
        }
        let before = if let Some(code) = removed.get(row) {
            let rendered = highlight_numbered_line(
                *old_line,
                code,
                before_highlighter,
                syntaxes,
                Some(DIFF_REMOVED_BG),
            );
            let cell = DiffCell::new(rendered, Some(*old_line), Some(code.clone()));
            *old_line += 1;
            cell
        } else {
            DiffCell::new(blank_line(), None, None)
        };
        let after = if let Some(code) = added.get(row) {
            let rendered = highlight_numbered_line(
                *new_line,
                code,
                after_highlighter,
                syntaxes,
                Some(DIFF_ADDED_BG),
            );
            let cell = DiffCell::new(rendered, Some(*new_line), Some(code.clone()));
            *new_line += 1;
            cell
        } else {
            DiffCell::new(blank_line(), None, None)
        };
        document.push(DiffRow::new(before, after, None));
    }
    removed.clear();
    added.clear();
    true
}

fn collapse_context(
    diff: &str,
    expanded: &HashSet<FoldKey>,
    cancelled: &dyn Fn() -> bool,
) -> Option<String> {
    let mut lines = Vec::new();
    for line in diff.lines() {
        if cancelled() {
            return None;
        }
        lines.push(line);
    }
    let mut output = Vec::new();
    let mut old_line = 0;
    let mut new_line = 0;
    let mut index = 0;
    while index < lines.len() {
        if cancelled() {
            return None;
        }
        let line = lines[index];
        if let Some((old, new)) = hunk_starts(line) {
            old_line = old;
            new_line = new;
            output.push(line.to_owned());
            index += 1;
            continue;
        }
        if line.starts_with(' ') {
            let start = index;
            while index < lines.len() && lines[index].starts_with(' ') {
                if cancelled() {
                    return None;
                }
                index += 1;
            }
            let count = index - start;
            if count > 8 {
                let key = FoldKey {
                    kind: FoldKind::Context,
                    old_start: old_line + 3,
                    old_end: old_line + count - 3,
                    new_start: new_line + 3,
                    new_end: new_line + count - 3,
                };
                let is_expanded = expanded.contains(&key);
                output.extend(
                    lines[start..start + 3]
                        .iter()
                        .map(|line| (*line).to_owned()),
                );
                output.push(fold_marker(key, is_expanded));
                if is_expanded {
                    output.extend(
                        lines[start + 3..index - 3]
                            .iter()
                            .map(|line| (*line).to_owned()),
                    );
                }
                output.extend(
                    lines[index - 3..index]
                        .iter()
                        .map(|line| (*line).to_owned()),
                );
                old_line += count;
                new_line += count;
                continue;
            }
            output.extend(lines[start..index].iter().map(|line| (*line).to_owned()));
            old_line += count;
            new_line += count;
            continue;
        }
        output.push(line.to_owned());
        if line.starts_with('-') && !line.starts_with("---") {
            old_line += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            new_line += 1;
        }
        index += 1;
    }
    Some(output.join("\n"))
}

fn fold_marker(key: FoldKey, expanded: bool) -> String {
    format!(
        "\u{1f}FOLD\tcontext\t{}\t{}\t{}\t{}\t{}",
        if expanded { "expanded" } else { "collapsed" },
        key.old_start,
        key.old_end,
        key.new_start,
        key.new_end
    )
}

fn parse_fold_marker(line: &str) -> Option<(FoldKey, bool)> {
    let mut fields = line.strip_prefix("\u{1f}FOLD\t")?.split('\t');
    let kind = match fields.next()? {
        "context" => FoldKind::Context,
        "change" => FoldKind::Change,
        _ => return None,
    };
    let expanded = match fields.next()? {
        "expanded" => true,
        "collapsed" => false,
        _ => return None,
    };
    Some((
        FoldKey {
            kind,
            old_start: fields.next()?.parse().ok()?,
            old_end: fields.next()?.parse().ok()?,
            new_start: fields.next()?.parse().ok()?,
            new_end: fields.next()?.parse().ok()?,
        },
        expanded,
    ))
}

fn fold_label(kind: FoldKind, count: usize, expanded: bool) -> String {
    let kind = match kind {
        FoldKind::Context => "unchanged",
        FoldKind::Change => "changed",
    };
    let action = if expanded { "collapse" } else { "expand" };
    format!("  ⋯ {count} {kind} lines · {action}")
}

fn fold_style() -> Style {
    Style::default().fg(ACCENT).bg(SURFACE_HEADER)
}

fn highlight_numbered_line(
    number: usize,
    code: &str,
    highlighter: &mut Option<HighlightLines<'_>>,
    syntaxes: &SyntaxSet,
    background: Option<Color>,
) -> Line<'static> {
    let expanded;
    let code = if code.contains('\t') {
        let mut text = String::with_capacity(code.len());
        let mut column = 0;
        for character in code.chars() {
            if character == '\t' {
                let spaces = 8 - column % 8;
                text.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            } else {
                text.push(character);
                column += character.width().unwrap_or_default();
            }
        }
        expanded = text;
        expanded.as_str()
    } else {
        code
    };
    let base = background.map_or_else(Style::default, |color| Style::default().bg(color));
    let mut spans = vec![Span::styled(format!("{number:>4} "), base.fg(HINT))];
    if let Some(highlighter) = highlighter {
        let source = format!("{code}\n");
        if let Ok(regions) = highlighter.highlight_line(&source, syntaxes) {
            spans.extend(regions.into_iter().filter_map(|(style, text)| {
                let text = text.trim_end_matches('\n');
                (!text.is_empty()).then(|| {
                    let mut style = syntax_style(style);
                    style.bg = background;
                    Span::styled(text.to_owned(), style)
                })
            }));
            return Line::from(spans).style(base);
        }
    }
    spans.push(Span::styled(code.to_owned(), base));
    Line::from(spans).style(base)
}

fn blank_line() -> Line<'static> {
    let style = Style::default().bg(SURFACE_INERT);
    Line::from(Span::styled(" ", style)).style(style)
}

fn hunk_starts(line: &str) -> Option<(usize, usize)> {
    if !line.starts_with("@@") {
        return None;
    }
    let mut parts = line.split_whitespace();
    parts.next()?;
    let old = parts
        .next()?
        .trim_start_matches('-')
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let new = parts
        .next()?
        .trim_start_matches('+')
        .split(',')
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

fn metadata_style(line: &str) -> Style {
    let foreground = if line.starts_with("@@") {
        WARNING
    } else if line.starts_with("---") || line.starts_with("+++") {
        ACCENT
    } else {
        HINT
    };
    Style::default().fg(foreground)
}

fn syntax_style(style: syntect::highlighting::Style) -> Style {
    let mut result = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        result = result.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        result = result.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        result = result.add_modifier(Modifier::UNDERLINED);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        ACCENT, DIFF_ADDED_BG, DIFF_REMOVED_BG, DiffCell, DiffDocument, DiffRow, FoldKind, HINT,
        SURFACE_HEADER, SURFACE_INERT, SyntaxHighlighter, WARNING,
    };
    use ratatui::text::Line;

    #[test]
    fn tabs_expand_with_row_background_and_preserve_source() {
        let highlighter = SyntaxHighlighter::new();
        for path in ["example.c", "file.unknown-extension"] {
            let raw = "\tint\tvalue_with_a_long_name;";
            let split = highlighter.side_by_side_diff_with_folds(
                &format!("@@ -1 +1 @@\n-{raw}\n+{raw}\n"),
                Some(path),
                &HashSet::new(),
            );
            let row = split.after_row_for_line(1).unwrap();
            assert_eq!(split.after_source(row), Some(raw));
            for (line, background) in [
                (split.before_line(row).unwrap(), DIFF_REMOVED_BG),
                (split.after_line(row).unwrap(), DIFF_ADDED_BG),
            ] {
                assert_eq!(
                    line.to_string(),
                    "   1         int     value_with_a_long_name;"
                );
                assert!(
                    line.spans
                        .iter()
                        .all(|span| span.style.bg == Some(background))
                );
                assert_eq!(split.after_max_width(), line.width());
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1)).unwrap();
                terminal
                    .draw(|frame| {
                        frame.render_widget(
                            ratatui::widgets::Paragraph::new(line.clone()),
                            frame.area(),
                        )
                    })
                    .unwrap();
                for x in 5..13 {
                    let cell = &terminal.backend().buffer()[(x, 0)];
                    assert_eq!(cell.symbol(), " ");
                    assert_eq!(cell.bg, background);
                }
            }
        }
    }

    #[test]
    fn fold_labels_line_numbers_and_metadata_use_theme_tokens() {
        let highlighter = SyntaxHighlighter::new();
        let diff = "diff --git a/example.txt b/example.txt\n--- a/example.txt\n+++ b/example.txt\n@@ -1,2 +1,2 @@\n unchanged\n-old\n+new\n";
        let split =
            highlighter.side_by_side_diff_with_folds(diff, Some("example.txt"), &HashSet::new());
        let styled = |needle: &str| {
            split
                .after_lines()
                .find(|line| line.to_string().starts_with(needle))
                .unwrap_or_else(|| panic!("missing row {needle:?}"))
                .style
        };
        assert_eq!(styled("diff --git").fg, Some(HINT));
        assert_eq!(styled("+++ ").fg, Some(ACCENT));
        assert_eq!(styled("@@ ").fg, Some(WARNING));
        let fold = split
            .folds()
            .find_map(|(row, key)| (key.kind == FoldKind::Change).then_some(row))
            .unwrap();
        let fold_line = split.after_line(fold).unwrap();
        assert_eq!(fold_line.style.fg, Some(ACCENT));
        assert_eq!(fold_line.style.bg, Some(SURFACE_HEADER));
        let code = split.after_line(fold + 1).unwrap();
        assert_eq!(code.spans[0].content, "   2 ");
        assert_eq!(code.spans[0].style.fg, Some(HINT));
    }

    #[test]
    fn detects_common_languages_and_falls_back() {
        let highlighter = SyntaxHighlighter::new();
        assert_eq!(highlighter.language_name("src/main.rs"), Some("Rust"));
        assert!(highlighter.language_name("src/App.swift").is_some());
        assert_eq!(highlighter.language_name("LICENSE.unknown-extension"), None);
    }

    #[test]
    fn cancellable_highlighting_publishes_no_partial_rows() {
        let highlighter = SyntaxHighlighter::new();
        let result = highlighter.side_by_side_diff_with_folds_cancellable(
            "@@ -1 +1 @@\n-old\n+new\n",
            Some("example.txt"),
            &HashSet::new(),
            &|| true,
        );
        assert!(result.is_none());
    }

    #[test]
    fn highlighting_cancels_after_parsing_has_started() {
        use std::cell::Cell;

        let highlighter = SyntaxHighlighter::new();
        let mut diff = String::from("@@ -1,1000 +1,1000 @@\n");
        for line in 0..1_000 {
            diff.push_str(&format!(" unchanged {line}\n"));
        }
        let checks = Cell::new(0_usize);
        let result = highlighter.side_by_side_diff_with_folds_cancellable(
            &diff,
            Some("example.txt"),
            &HashSet::new(),
            &|| {
                let next = checks.get() + 1;
                checks.set(next);
                next > 50
            },
        );

        assert!(checks.get() > 50);
        assert!(result.is_none());
    }

    #[test]
    fn side_by_side_diff_uses_panels_and_backgrounds_without_patch_markers() {
        let highlighter = SyntaxHighlighter::new();
        let diff = "@@ -3,2 +3,2 @@\n unchanged\n-old value\n+new value\n";
        let split =
            highlighter.side_by_side_diff_with_folds(diff, Some("example.txt"), &HashSet::new());

        assert!(!split.is_empty());
        let changed = split
            .folds()
            .find_map(|(row, key)| (key.kind == FoldKind::Change).then_some(row))
            .unwrap();
        assert!(
            split
                .before_line(changed)
                .unwrap()
                .to_string()
                .contains("changed lines · collapse")
        );
        assert_eq!(
            split.before_line(changed + 1).unwrap().spans[0].content,
            "   4 "
        );
        assert_eq!(
            split.after_line(changed + 1).unwrap().spans[0].content,
            "   4 "
        );
        assert_eq!(
            split.before_line(changed + 1).unwrap().style.bg,
            Some(DIFF_REMOVED_BG)
        );
        assert_eq!(
            split.after_line(changed + 1).unwrap().style.bg,
            Some(DIFF_ADDED_BG)
        );
        assert!(
            !split
                .before_line(changed + 1)
                .unwrap()
                .to_string()
                .contains("-old")
        );
        assert!(
            !split
                .after_line(changed + 1)
                .unwrap()
                .to_string()
                .contains("+new")
        );
    }

    #[test]
    fn side_by_side_diff_keeps_markdown_list_marker_as_source() {
        let highlighter = SyntaxHighlighter::new();
        let split = highlighter.side_by_side_diff_with_folds(
            "@@ -0,0 +1 @@\n+- Added requirement\n",
            Some("README.md"),
            &HashSet::new(),
        );

        let code_row = split
            .after_lines()
            .position(|line| line.to_string().ends_with("- Added requirement"))
            .unwrap();
        assert!(
            !split
                .after_line(code_row)
                .unwrap()
                .to_string()
                .contains("+ -")
        );
        assert_eq!(
            split.before_line(code_row).unwrap().style.bg,
            Some(SURFACE_INERT)
        );
    }

    #[test]
    fn unchanged_and_changed_runs_toggle_independently_in_both_directions() {
        let highlighter = SyntaxHighlighter::new();
        let context = (1..=12)
            .map(|line| format!(" line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = format!("@@ -1,25 +1,25 @@\n{context}\n-old\n+new\n{context}\n");
        let collapsed =
            highlighter.side_by_side_diff_with_folds(&diff, Some("example.txt"), &HashSet::new());
        let context_folds = collapsed
            .folds()
            .map(|(_, key)| key)
            .filter(|key| key.kind == FoldKind::Context)
            .collect::<Vec<_>>();
        let changed_fold = collapsed
            .folds()
            .map(|(_, key)| key)
            .find(|key| key.kind == FoldKind::Change)
            .unwrap();
        assert_eq!(context_folds.len(), 2);
        assert!(
            collapsed
                .after_lines()
                .any(|line| { line.to_string().contains("unchanged lines · expand") })
        );
        assert!(
            collapsed
                .after_lines()
                .any(|line| { line.to_string().contains("changed lines · collapse") })
        );

        let first_context_expanded = highlighter.side_by_side_diff_with_folds(
            &diff,
            Some("example.txt"),
            &HashSet::from([context_folds[0]]),
        );
        assert_eq!(first_context_expanded.folds().count(), 3);
        assert!(first_context_expanded.len() > collapsed.len());
        assert!(
            first_context_expanded
                .after_lines()
                .any(|line| { line.to_string().contains("unchanged lines · collapse") })
        );
        assert_eq!(
            highlighter
                .side_by_side_diff_with_folds(&diff, Some("example.txt"), &HashSet::new())
                .len(),
            collapsed.len()
        );

        let changed_collapsed = highlighter.side_by_side_diff_with_folds(
            &diff,
            Some("example.txt"),
            &HashSet::from([changed_fold]),
        );
        assert!(changed_collapsed.len() < collapsed.len());
        assert!(
            changed_collapsed
                .after_lines()
                .any(|line| { line.to_string().contains("changed lines · expand") })
        );
        assert_eq!(
            changed_collapsed
                .folds()
                .map(|(_, key)| key)
                .filter(|key| key.kind == FoldKind::Context)
                .count(),
            2
        );
    }

    #[test]
    fn diff_document_keeps_paired_cells_and_metadata_in_one_row() {
        let fold = super::FoldKey {
            kind: FoldKind::Change,
            old_start: 7,
            old_end: 8,
            new_start: 9,
            new_end: 10,
        };
        let document = DiffDocument::from_rows(vec![DiffRow::new(
            DiffCell::new(
                Line::from("before"),
                Some(7),
                Some("before source".to_owned()),
            ),
            DiffCell::new(
                Line::from("after"),
                Some(9),
                Some("after source".to_owned()),
            ),
            Some(fold),
        )]);

        assert_eq!(document.len(), 1);
        assert_eq!(document.before_line_number(0), Some(7));
        assert_eq!(document.after_line_number(0), Some(9));
        assert_eq!(document.before_source(0), Some("before source"));
        assert_eq!(document.after_source(0), Some("after source"));
        assert_eq!(document.fold_key(0), Some(fold));
        assert_eq!(document.folds().collect::<Vec<_>>(), vec![(0, fold)]);
        assert_eq!(document.before_lines().count(), document.len());
        assert_eq!(document.after_lines().count(), document.len());
    }

    #[test]
    fn diff_document_caches_the_widest_line_per_side_and_indexes_numbered_lines() {
        let cell = |text: &str, line: Option<usize>| {
            DiffCell::new(
                Line::from(text.to_owned()),
                line,
                line.map(|_| text.to_owned()),
            )
        };
        let document = DiffDocument::from_rows(vec![
            DiffRow::new(
                cell("@@ -1,3 +1,3 @@", None),
                cell("@@ -1,3 +1,3 @@", None),
                None,
            ),
            DiffRow::new(cell("한글 wide", Some(1)), cell("short", Some(1)), None),
            DiffRow::new(
                cell("x", Some(2)),
                cell("a much longer after line", Some(2)),
                None,
            ),
            DiffRow::new(cell(" ", None), cell("added", Some(3)), None),
        ]);

        assert_eq!(
            document.before_max_width(),
            document.before_lines().map(Line::width).max().unwrap()
        );
        assert_eq!(
            document.after_max_width(),
            document.after_lines().map(Line::width).max().unwrap()
        );
        assert_eq!(document.before_max_width(), 15);
        assert_eq!(document.after_max_width(), 24);
        assert_eq!(document.before_row_for_line(1), Some(1));
        assert_eq!(document.before_row_for_line(2), Some(2));
        assert_eq!(document.before_row_for_line(3), None);
        assert_eq!(document.after_row_for_line(3), Some(3));
        assert_eq!(document.after_row_for_line(4), None);
        let plain = DiffDocument::plain("one\nthree");
        assert_eq!(plain.before_max_width(), 5);
        assert_eq!(plain.after_row_for_line(1), None);
    }
}
