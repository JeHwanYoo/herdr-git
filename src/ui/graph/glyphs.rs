use ratatui::buffer::Buffer;
use ratatui::style::Style;

use super::layout::{GraphLayout, GraphViewport, LayoutRow, ROW_HEIGHT, lane_cell};
use crate::ui::theme;

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

#[derive(Clone, Copy, Default)]
struct Cell {
    joins: u8,
    color: usize,
    node: Option<&'static str>,
}

impl Cell {
    fn symbol(self) -> Option<&'static str> {
        if let Some(node) = self.node {
            return Some(node);
        }
        Some(match self.joins {
            0 => return None,
            j if j == UP | DOWN | LEFT | RIGHT => "┼",
            j if j == UP | DOWN | RIGHT => "├",
            j if j == UP | DOWN | LEFT => "┤",
            j if j == LEFT | RIGHT | DOWN => "┬",
            j if j == LEFT | RIGHT | UP => "┴",
            j if j == DOWN | RIGHT => "╭",
            j if j == DOWN | LEFT => "╮",
            j if j == UP | RIGHT => "╰",
            j if j == UP | LEFT => "╯",
            j if j & (LEFT | RIGHT) != 0 && j & (UP | DOWN) == 0 => "─",
            _ => "│",
        })
    }
}

pub(super) fn paint(buffer: &mut Buffer, layout: &GraphLayout, viewport: &GraphViewport) {
    let width = layout.width(viewport.first, viewport.rows);
    for (offset, row) in layout
        .rows
        .iter()
        .enumerate()
        .skip(viewport.first)
        .take(viewport.rows)
        .map(|(index, row)| (index - viewport.first, row))
    {
        let [node_line, link_line] = row_cells(row, width);
        let y = viewport.area.y + (offset * ROW_HEIGHT) as u16;
        put_line(buffer, viewport, y, &node_line);
        put_line(buffer, viewport, y + 1, &link_line);
    }
}

fn row_cells(row: &LayoutRow, width: usize) -> [Vec<Cell>; 2] {
    let mut node_line = vec![Cell::default(); width];
    let mut link_line = vec![Cell::default(); width];
    for (column, color) in row.above.iter().enumerate() {
        if let Some(color) = *color
            && column != row.column
        {
            join(&mut node_line, lane_cell(column), UP | DOWN, color);
        }
    }
    let node = &mut node_line[lane_cell(row.column)];
    node.node = Some(if row.uncommitted { "○" } else { "●" });
    node.color = row.color;
    for edge in &row.edges {
        let from = lane_cell(edge.from);
        let to = lane_cell(edge.to);
        if from == to {
            join(&mut link_line, from, UP | DOWN, edge.color);
            continue;
        }
        let (start, end) = if from < to {
            (RIGHT, LEFT)
        } else {
            (LEFT, RIGHT)
        };
        join(&mut link_line, from, UP | start, edge.color);
        for cell in from.min(to) + 1..from.max(to) {
            join(&mut link_line, cell, LEFT | RIGHT, edge.color);
        }
        join(&mut link_line, to, DOWN | end, edge.color);
    }
    [node_line, link_line]
}

fn join(cells: &mut [Cell], index: usize, joins: u8, color: usize) {
    if let Some(cell) = cells.get_mut(index) {
        if cell.joins & (UP | DOWN) == 0 || joins & (UP | DOWN) != 0 {
            cell.color = color;
        }
        cell.joins |= joins;
    }
}

fn put_line(buffer: &mut Buffer, viewport: &GraphViewport, y: u16, cells: &[Cell]) {
    if y >= viewport.area.bottom() {
        return;
    }
    for (index, cell) in cells.iter().enumerate().skip(viewport.scroll) {
        let x = viewport.area.x as usize + index - viewport.scroll;
        if x >= viewport.area.right() as usize {
            break;
        }
        if let Some(symbol) = cell.symbol()
            && let Some(target) = buffer.cell_mut((x as u16, y))
        {
            target
                .set_symbol(symbol)
                .set_style(Style::default().fg(theme::graph_lane(cell.color)));
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;
    use crate::ui::graph::layout::LayoutNode;

    fn draw(nodes: &[(&str, &[&str])], width: u16) -> Vec<String> {
        let parents: Vec<Vec<String>> = nodes
            .iter()
            .map(|(_, parents)| parents.iter().map(|parent| parent.to_string()).collect())
            .collect();
        let layout = GraphLayout::build(
            nodes
                .iter()
                .zip(&parents)
                .map(|((sha, _), parents)| LayoutNode { sha, parents }),
            0,
        );
        let area = Rect::new(0, 0, width, (nodes.len() * ROW_HEIGHT) as u16);
        let mut buffer = Buffer::empty(area);
        paint(
            &mut buffer,
            &layout,
            &GraphViewport::new(area, 0, 0, layout.rows.len()),
        );
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn merges_and_branches_join_with_rounded_corners() {
        let lines = draw(
            &[("m", &["a", "b"]), ("b", &["r"]), ("a", &["r"]), ("r", &[])],
            6,
        );
        assert_eq!(
            lines,
            [" ●", " ├─╮", " │ ●", " │ │", " ● │", " ├─╯", " ●", ""]
        );
    }

    #[test]
    fn lanes_crossing_a_link_use_a_cross() {
        let lines = draw(
            &[
                ("m", &["a", "c"]),
                ("n", &["a"]),
                ("a", &["r"]),
                ("c", &["r"]),
                ("r", &[]),
            ],
            8,
        );
        assert_eq!(lines[2], " │ │ ●");
        assert_eq!(lines[3], " ├─┼─╯");
    }

    #[test]
    fn scrolled_graphs_clip_to_the_viewport() {
        let parents = [vec!["p".to_owned()], Vec::new()];
        let layout = GraphLayout::build(
            [
                LayoutNode {
                    sha: "tip",
                    parents: &parents[0],
                },
                LayoutNode {
                    sha: "other",
                    parents: &parents[1],
                },
            ],
            0,
        );
        let area = Rect::new(2, 0, 2, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 4));
        paint(&mut buffer, &layout, &GraphViewport::new(area, 0, 1, 2));
        assert_eq!(buffer[(2, 2)].symbol(), "│");
        assert_eq!(buffer[(3, 2)].symbol(), " ");
        assert_eq!(buffer[(4, 2)].symbol(), " ");
    }
}
