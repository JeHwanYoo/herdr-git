use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum ReviewSide {
    Before,
    After,
}

impl ReviewSide {
    pub(in crate::ui) fn label(self) -> &'static str {
        match self {
            Self::Before => "Before",
            Self::After => "After",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) struct CodeSelection {
    pub(in crate::ui) side: ReviewSide,
    rows: BTreeSet<usize>,
}

impl CodeSelection {
    pub(in crate::ui) fn from_rows(
        side: ReviewSide,
        rows: impl IntoIterator<Item = usize>,
    ) -> Self {
        Self {
            side,
            rows: rows.into_iter().collect(),
        }
    }

    pub(in crate::ui) fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().copied()
    }

    pub(in crate::ui) fn insert(&mut self, row: usize) {
        self.rows.insert(row);
    }

    pub(in crate::ui) fn contains(&self, row: usize) -> bool {
        self.rows.contains(&row)
    }

    pub(in crate::ui) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

pub(in crate::ui) fn selected_code(
    selection: &CodeSelection,
    line_numbers: &[Option<usize>],
    sources: &[Option<String>],
) -> Result<(Vec<usize>, Vec<String>), String> {
    let selected = selection
        .rows()
        .filter_map(|row| {
            Some((
                line_numbers.get(row).copied().flatten()?,
                sources.get(row)?.clone()?,
            ))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err("Select at least one code line.".to_owned());
    }
    Ok((
        selected.iter().map(|(line, _)| *line).collect(),
        selected.into_iter().map(|(_, code)| code).collect(),
    ))
}

pub(in crate::ui) fn line_numbers_text(line_numbers: &[usize]) -> String {
    let Some((&first, remaining)) = line_numbers.split_first() else {
        return String::new();
    };
    let display_range = |start: usize, end: usize| {
        if start == end {
            start.to_string()
        } else {
            format!("{start}–{end}")
        }
    };
    let mut ranges = Vec::new();
    let mut start = first;
    let mut end = first;
    for &line in remaining {
        if line == end.saturating_add(1) {
            end = line;
        } else {
            ranges.push(display_range(start, end));
            start = line;
            end = line;
        }
    }
    ranges.push(display_range(start, end));
    ranges.join(", ")
}

pub(in crate::ui) fn copy_selection_text(
    include_code: bool,
    include_line: bool,
    path: &str,
    side: Option<ReviewSide>,
    line_numbers: &[usize],
    code: &[String],
) -> String {
    debug_assert!(include_code || include_line);
    let side = side
        .map(|side| format!("\nSide: {}", side.label()))
        .unwrap_or_default();
    let context = format!(
        "File: {path}{side}\nLines: {}",
        line_numbers_text(line_numbers)
    );
    match (include_code, include_line) {
        (true, true) => format!("{context}\n\n{}", code.join("\n")),
        (true, false) => code.join("\n"),
        (false, _) => context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_formats_preserve_noncontiguous_lines_and_code_order() {
        let selection = CodeSelection::from_rows(ReviewSide::After, [2, 0]);
        let (lines, code) = selected_code(
            &selection,
            &[Some(10), None, Some(12)],
            &[Some("first".into()), None, Some("third".into())],
        )
        .unwrap();
        assert_eq!(lines, [10, 12]);
        assert_eq!(
            copy_selection_text(
                true,
                false,
                "src/a.rs",
                Some(ReviewSide::After),
                &lines,
                &code
            ),
            "first\nthird"
        );
        assert_eq!(
            copy_selection_text(
                false,
                true,
                "src/a.rs",
                Some(ReviewSide::After),
                &lines,
                &code
            ),
            "File: src/a.rs\nSide: After\nLines: 10, 12"
        );
        assert_eq!(
            copy_selection_text(
                true,
                true,
                "src/a.rs",
                Some(ReviewSide::After),
                &lines,
                &code
            ),
            "File: src/a.rs\nSide: After\nLines: 10, 12\n\nfirst\nthird"
        );
    }

    #[test]
    fn groups_adjacent_lines() {
        assert_eq!(line_numbers_text(&[3, 4, 5, 9]), "3–5, 9");
    }
}
