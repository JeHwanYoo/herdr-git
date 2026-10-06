use ratatui::layout::Rect;

pub(super) const LANE_CELLS: usize = 2;
pub(super) const ROW_HEIGHT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GraphViewport {
    pub(super) area: Rect,
    pub(super) first: usize,
    pub(super) scroll: usize,
    pub(super) rows: usize,
    pub(super) selected: Option<usize>,
    pub(super) hovered: Option<usize>,
    pub(super) hidden: Rect,
}

impl GraphViewport {
    pub(super) fn new(area: Rect, first: usize, scroll: usize, rows: usize) -> Self {
        Self {
            area,
            first,
            scroll,
            rows,
            selected: None,
            hovered: None,
            hidden: Rect::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Edge {
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) color: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LayoutRow {
    pub(super) column: usize,
    pub(super) color: usize,
    pub(super) uncommitted: bool,
    pub(super) above: Vec<Option<usize>>,
    pub(super) edges: Vec<Edge>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct GraphLayout {
    pub(super) rows: Vec<LayoutRow>,
    pub(super) generation: u64,
}

pub(super) struct LayoutNode<'a> {
    pub(super) sha: &'a str,
    pub(super) parents: &'a [String],
}

struct Lane<'a> {
    sha: &'a str,
    color: usize,
}

impl GraphLayout {
    pub(super) fn build<'a>(
        nodes: impl IntoIterator<Item = LayoutNode<'a>>,
        generation: u64,
    ) -> Self {
        let mut layout = Self {
            generation,
            ..Self::default()
        };
        let mut lanes: Vec<Option<Lane<'a>>> = Vec::new();
        let mut next_color = 0;
        for node in nodes {
            let column = lanes
                .iter()
                .position(|lane| lane.as_ref().is_some_and(|lane| lane.sha == node.sha))
                .unwrap_or_else(|| vacant(&mut lanes));
            let color = lanes[column].as_ref().map_or_else(
                || {
                    next_color += 1;
                    next_color - 1
                },
                |lane| lane.color,
            );
            let above: Vec<_> = lanes
                .iter()
                .map(|lane| lane.as_ref().map(|lane| lane.color))
                .collect();
            let mut edges: Vec<_> = lanes
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != column)
                .filter_map(|(index, lane)| {
                    lane.as_ref().map(|lane| Edge {
                        from: index,
                        to: index,
                        color: lane.color,
                    })
                })
                .collect();
            lanes[column] = None;
            for (index, parent) in node.parents.iter().enumerate() {
                if node.parents[..index].contains(parent) {
                    continue;
                }
                let existing = lanes
                    .iter()
                    .position(|lane| lane.as_ref().is_some_and(|lane| lane.sha == parent));
                let to = match existing {
                    Some(existing) if index == 0 && existing > column => {
                        lanes[column] = lanes[existing].take().map(|lane| Lane { color, ..lane });
                        if let Some(edge) = edges.iter_mut().find(|edge| edge.from == existing) {
                            edge.to = column;
                        }
                        column
                    }
                    Some(existing) => existing,
                    None => {
                        let to = if index == 0 && lanes[column].is_none() {
                            column
                        } else {
                            vacant(&mut lanes)
                        };
                        let lane_color = if index == 0 {
                            color
                        } else {
                            next_color += 1;
                            next_color - 1
                        };
                        lanes[to] = Some(Lane {
                            sha: parent,
                            color: lane_color,
                        });
                        to
                    }
                };
                let lane_color = lanes[to].as_ref().map_or(color, |lane| lane.color);
                edges.push(Edge {
                    from: column,
                    to,
                    color: lane_color,
                });
            }
            layout.rows.push(LayoutRow {
                column,
                color,
                uncommitted: node.sha.is_empty(),
                above,
                edges,
            });
            while lanes.last().is_some_and(Option::is_none) {
                lanes.pop();
            }
        }
        layout
    }

    pub(super) fn width(&self, first: usize, rows: usize) -> usize {
        let lanes = self
            .rows
            .iter()
            .skip(first)
            .take(rows)
            .map(|row| {
                let edges = row.edges.iter().map(|edge| edge.from.max(edge.to) + 1);
                edges
                    .chain([row.above.len(), row.column + 1])
                    .max()
                    .unwrap_or(1)
            })
            .max()
            .unwrap_or(1);
        lanes * LANE_CELLS + 1
    }
}

pub(super) fn lane_cell(column: usize) -> usize {
    column * LANE_CELLS + 1
}

fn vacant<T>(lanes: &mut Vec<Option<T>>) -> usize {
    lanes.iter().position(Option::is_none).unwrap_or_else(|| {
        lanes.push(None);
        lanes.len() - 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(nodes: &[(&str, &[&str])]) -> GraphLayout {
        let parents: Vec<Vec<String>> = nodes
            .iter()
            .map(|(_, parents)| parents.iter().map(|parent| parent.to_string()).collect())
            .collect();
        GraphLayout::build(
            nodes
                .iter()
                .zip(&parents)
                .map(|((sha, _), parents)| LayoutNode { sha, parents }),
            0,
        )
    }

    #[test]
    fn a_merge_opens_a_lane_that_returns_to_the_first_parent_column() {
        let layout = build(&[("m", &["a", "b"]), ("b", &["r"]), ("a", &["r"]), ("r", &[])]);
        assert_eq!(layout.width(0, layout.rows.len()), 5);
        assert_eq!(
            layout.rows[0].edges,
            [
                Edge {
                    from: 0,
                    to: 0,
                    color: 0
                },
                Edge {
                    from: 0,
                    to: 1,
                    color: 1
                },
            ]
        );
        assert_eq!(layout.rows[1].column, 1);
        assert_eq!(layout.rows[2].column, 0);
        assert_eq!(
            layout.rows[2].edges,
            [
                Edge {
                    from: 1,
                    to: 0,
                    color: 1
                },
                Edge {
                    from: 0,
                    to: 0,
                    color: 0
                },
            ]
        );
        assert_eq!(layout.rows[3].column, 0);
        assert_eq!(layout.rows[3].color, 0);
        assert_eq!(layout.rows[3].above, [Some(0)]);
        assert!(layout.rows[3].edges.is_empty());
    }

    #[test]
    fn a_branch_tip_joins_the_lane_that_already_holds_its_parent() {
        let layout = build(&[
            ("", &["head"]),
            ("head", &["base"]),
            ("tip", &["base"]),
            ("x", &["base"]),
            ("base", &[]),
        ]);
        assert!(layout.rows[0].uncommitted);
        assert_eq!(layout.rows[1].color, layout.rows[0].color);
        assert_eq!(layout.rows[2].column, 1);
        assert_ne!(layout.rows[2].color, layout.rows[1].color);
        assert_eq!(
            layout.rows[2].edges,
            [
                Edge {
                    from: 0,
                    to: 0,
                    color: 0
                },
                Edge {
                    from: 1,
                    to: 0,
                    color: 0
                },
            ]
        );
        assert_eq!(layout.rows[3].column, 1);
        assert_eq!(layout.rows[4].above, [Some(0)]);
        assert_eq!(layout.width(0, layout.rows.len()), 5);
    }

    #[test]
    fn duplicate_and_missing_parents_do_not_add_lanes() {
        let layout = build(&[("tip", &["p", "p"]), ("q", &[])]);
        assert_eq!(layout.rows[0].edges.len(), 1);
        assert_eq!(layout.rows[1].column, 1);
        assert_eq!(layout.rows[1].above, [Some(0), None]);
    }
}
