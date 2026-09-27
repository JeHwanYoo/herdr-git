use ratatui::layout::{Constraint, Direction, Layout, Rect};

use super::App;
use super::diff::{changes_pane_widths, divider_hit_area};
use super::shell::ActiveTab;

impl App {
    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let outer_header_margin = u16::from(frame.area().height > 13);
        let action_gap = outer_header_margin;
        let comparison_info_height = if self.shell.active_tab == ActiveTab::Changes {
            1 + action_gap
        } else {
            0
        };
        let header_height = 4 + action_gap + outer_header_margin * 2 + comparison_info_height;
        let constraints = if self.shell.active_tab == ActiveTab::History {
            vec![
                Constraint::Length(header_height),
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(1),
            ]
        } else {
            vec![
                Constraint::Length(header_height),
                Constraint::Min(4),
                Constraint::Length(1),
            ]
        };
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());
        self.draw_header(frame, vertical[0]);
        self.workspaces.clear_areas();
        self.inspect.clear_areas();
        self.diff.changes_body_area = Rect::default();
        self.diff.files_before_divider_area = Rect::default();
        self.diff.before_after_divider_area = Rect::default();
        if self.shell.active_tab == ActiveTab::Changes {
            let wide = vertical[1].width >= 90;
            self.diff.changes_body_area = vertical[1];
            let widths = changes_pane_widths(vertical[1].width, self.diff.changes_splits);
            let body = Layout::default()
                .direction(if wide {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints(if wide {
                    [
                        Constraint::Length(widths[0]),
                        Constraint::Length(widths[1]),
                        Constraint::Length(widths[2]),
                    ]
                } else {
                    [
                        Constraint::Percentage(25),
                        Constraint::Percentage(37),
                        Constraint::Percentage(38),
                    ]
                })
                .split(vertical[1]);
            if wide {
                self.diff.files_before_divider_area = divider_hit_area(body[1].x, vertical[1]);
                self.diff.before_after_divider_area = divider_hit_area(body[2].x, vertical[1]);
            } else {
                self.diff.resize_drag = None;
            }
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(body[0]);
            self.draw_files(frame, left[0]);
            self.draw_workspaces(frame, left[1]);
            self.draw_diff(frame, body[1], body[2]);
            if wide {
                self.draw_changes_dividers(frame);
            }
        } else {
            self.draw_graph_filter(frame, vertical[1]);
            self.draw_history(frame, vertical[2]);
        }
        self.draw_status_bar(frame, *vertical.last().expect("layout has a status bar"));
        self.draw_overlay(frame);
    }
}
