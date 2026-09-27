use ratatui::layout::Rect;

pub(in crate::ui) fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub(in crate::ui) fn anchored(anchor: (u16, u16), width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = anchor
        .0
        .clamp(area.x, area.right().saturating_sub(width).max(area.x));
    let y = anchor
        .1
        .clamp(area.y, area.bottom().saturating_sub(height).max(area.y));
    Rect::new(x, y, width, height)
}

pub(in crate::ui) fn area_hovered(pointer: Option<(u16, u16)>, area: Rect) -> bool {
    pointer.is_some_and(|position| area.contains(position.into()))
}

fn content_row_at(pointer: Option<(u16, u16)>, area: Rect, row_count: usize) -> Option<usize> {
    let (column, row) = pointer?;
    if !area.contains((column, row).into()) {
        return None;
    }
    let index = row.saturating_sub(area.y) as usize;
    (index < row_count).then_some(index)
}

pub(in crate::ui) fn scrolled_content_row_at(
    pointer: Option<(u16, u16)>,
    area: Rect,
    row_count: usize,
    offset: usize,
) -> Option<usize> {
    content_row_at(pointer, area, row_count)
        .and_then(|row| row.checked_add(offset))
        .filter(|row| *row < row_count)
}

pub(in crate::ui) fn viewport_offset(
    offset: usize,
    selected: usize,
    len: usize,
    height: usize,
) -> usize {
    let offset = offset.min(len.saturating_sub(1));
    if len == 0 || height == 0 {
        return offset;
    }
    let selected = selected.min(len - 1);
    if selected < offset {
        selected
    } else if selected >= offset.saturating_add(height) {
        selected.saturating_add(1).saturating_sub(height)
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_is_clamped_to_terminal() {
        assert_eq!(
            centered(50, 20, Rect::new(0, 0, 40, 10)),
            Rect::new(0, 0, 40, 10)
        );
    }

    #[test]
    fn viewport_offset_moves_only_as_far_as_the_selection_needs() {
        assert_eq!(viewport_offset(0, 3, 30, 8), 0);
        assert_eq!(viewport_offset(0, 8, 30, 8), 1);
        assert_eq!(viewport_offset(5, 20, 30, 8), 13);
        assert_eq!(viewport_offset(13, 20, 30, 8), 13);
        assert_eq!(viewport_offset(13, 2, 30, 8), 2);
        assert_eq!(viewport_offset(40, 29, 30, 8), 29);
        assert_eq!(viewport_offset(4, 7, 30, 0), 4);
        assert_eq!(viewport_offset(4, 0, 0, 8), 0);
    }
}
