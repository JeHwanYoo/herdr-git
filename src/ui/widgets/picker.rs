use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::{ListCursor, TextEdit, TextField, chord};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) enum PickerEdit {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Insert(char),
    Paste(String),
    Backspace,
    Activate,
    Cancel,
    WheelUp,
    WheelDown,
    ClickRow(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum PickerOutcome {
    Unchanged,
    Moved,
    Filtered,
    Activate,
    Cancel,
}

pub(in crate::ui) struct PickerState<'a> {
    pub(in crate::ui) query: Option<&'a mut TextField>,
    pub(in crate::ui) cursor: &'a mut ListCursor,
    pub(in crate::ui) len: usize,
    pub(in crate::ui) page: usize,
}

pub(in crate::ui) fn update_picker(state: PickerState<'_>, edit: PickerEdit) -> PickerOutcome {
    let PickerState {
        query,
        cursor,
        len,
        page,
    } = state;
    let before = *cursor;
    let moved = |cursor: &ListCursor| {
        if *cursor == before {
            PickerOutcome::Unchanged
        } else {
            PickerOutcome::Moved
        }
    };
    match edit {
        PickerEdit::Up | PickerEdit::WheelUp => {
            cursor.move_by(-1, len);
            moved(cursor)
        }
        PickerEdit::Down | PickerEdit::WheelDown => {
            cursor.move_by(1, len);
            moved(cursor)
        }
        PickerEdit::PageUp => {
            cursor.page(-1, len, page.max(1));
            moved(cursor)
        }
        PickerEdit::PageDown => {
            cursor.page(1, len, page.max(1));
            moved(cursor)
        }
        PickerEdit::Home => {
            cursor.home();
            moved(cursor)
        }
        PickerEdit::End => {
            cursor.end(len);
            moved(cursor)
        }
        PickerEdit::Insert(character) => filter_picker(query, cursor, TextEdit::Insert(character)),
        PickerEdit::Paste(text) => filter_picker(query, cursor, TextEdit::Paste(text)),
        PickerEdit::Backspace => filter_picker(query, cursor, TextEdit::Backspace),
        PickerEdit::Activate => PickerOutcome::Activate,
        PickerEdit::Cancel => PickerOutcome::Cancel,
        PickerEdit::ClickRow(row) => {
            if row < len {
                cursor.selected = row;
                PickerOutcome::Activate
            } else {
                PickerOutcome::Unchanged
            }
        }
    }
}

fn filter_picker(
    query: Option<&mut TextField>,
    cursor: &mut ListCursor,
    edit: TextEdit,
) -> PickerOutcome {
    let Some(query) = query else {
        return PickerOutcome::Unchanged;
    };
    if !query.edit(edit) {
        return PickerOutcome::Unchanged;
    }
    *cursor = ListCursor::default();
    PickerOutcome::Filtered
}

pub(in crate::ui) fn picker_edit(
    event: &Event,
    list_area: Rect,
    scroll: usize,
    has_text_field: bool,
) -> Option<PickerEdit> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let chord = chord(key.modifiers);
            match key.code {
                KeyCode::Esc => Some(PickerEdit::Cancel),
                KeyCode::Enter => Some(PickerEdit::Activate),
                KeyCode::Up => Some(PickerEdit::Up),
                KeyCode::Down => Some(PickerEdit::Down),
                KeyCode::PageUp => Some(PickerEdit::PageUp),
                KeyCode::PageDown => Some(PickerEdit::PageDown),
                KeyCode::Home => Some(PickerEdit::Home),
                KeyCode::End => Some(PickerEdit::End),
                KeyCode::Char('j') if !has_text_field && !chord => Some(PickerEdit::Down),
                KeyCode::Char('k') if !has_text_field && !chord => Some(PickerEdit::Up),
                KeyCode::Backspace if has_text_field => Some(PickerEdit::Backspace),
                KeyCode::Char(character) if has_text_field && !chord => {
                    Some(PickerEdit::Insert(character))
                }
                _ => None,
            }
        }
        Event::Paste(text) if has_text_field => Some(PickerEdit::Paste(text.clone())),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollDown => Some(PickerEdit::WheelDown),
            MouseEventKind::ScrollUp => Some(PickerEdit::WheelUp),
            MouseEventKind::Down(MouseButton::Left)
                if list_area.contains((mouse.column, mouse.row).into()) =>
            {
                Some(PickerEdit::ClickRow(
                    usize::from(mouse.row - list_area.y).saturating_add(scroll),
                ))
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn picker_edit_maps_keys_by_text_field_presence_and_ignores_chords() {
        let area = Rect::new(10, 5, 20, 4);
        assert_eq!(
            picker_edit(&key(KeyCode::Char('j'), KeyModifiers::NONE), area, 0, false),
            Some(PickerEdit::Down)
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Char('k'), KeyModifiers::NONE), area, 0, false),
            Some(PickerEdit::Up)
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Char('j'), KeyModifiers::NONE), area, 0, true),
            Some(PickerEdit::Insert('j'))
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Char('x'), KeyModifiers::NONE), area, 0, false),
            None
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Char('p'), KeyModifiers::ALT), area, 0, true),
            None
        );
        assert_eq!(
            picker_edit(
                &key(KeyCode::Char('p'), KeyModifiers::CONTROL),
                area,
                0,
                true
            ),
            None
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Backspace, KeyModifiers::NONE), area, 0, false),
            None
        );
        assert_eq!(
            picker_edit(&Event::Paste("ab".to_owned()), area, 0, true),
            Some(PickerEdit::Paste("ab".to_owned()))
        );
        assert_eq!(
            picker_edit(&Event::Paste("ab".to_owned()), area, 0, false),
            None
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Esc, KeyModifiers::NONE), area, 0, true),
            Some(PickerEdit::Cancel)
        );
        assert_eq!(
            picker_edit(&key(KeyCode::Enter, KeyModifiers::NONE), area, 0, true),
            Some(PickerEdit::Activate)
        );
    }

    #[test]
    fn picker_edit_click_rows_are_scroll_aware_and_wheel_moves_anywhere() {
        let area = Rect::new(10, 5, 20, 4);
        let mouse = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(
            picker_edit(
                &mouse(
                    MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    12,
                    6
                ),
                area,
                7,
                false
            ),
            Some(PickerEdit::ClickRow(8))
        );
        assert_eq!(
            picker_edit(
                &mouse(
                    MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    12,
                    9
                ),
                area,
                0,
                false
            ),
            None
        );
        assert_eq!(
            picker_edit(&mouse(MouseEventKind::ScrollDown, 0, 0), area, 0, false),
            Some(PickerEdit::WheelDown)
        );
        assert_eq!(
            picker_edit(&mouse(MouseEventKind::ScrollUp, 0, 0), area, 0, true),
            Some(PickerEdit::WheelUp)
        );
        assert_eq!(
            picker_edit(&mouse(MouseEventKind::Moved, 12, 6), area, 0, false),
            None
        );
    }

    fn state(cursor: &mut ListCursor) -> PickerState<'_> {
        PickerState {
            query: None,
            cursor,
            len: 12,
            page: 5,
        }
    }

    #[test]
    fn picker_navigation_moves_and_clamps_the_cursor() {
        let mut cursor = ListCursor::default();
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::Down),
            PickerOutcome::Moved
        );
        assert_eq!(cursor.selected, 1);
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::WheelDown),
            PickerOutcome::Moved
        );
        assert_eq!(cursor.selected, 2);
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::PageDown),
            PickerOutcome::Moved
        );
        assert_eq!(cursor.selected, 7);
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::End),
            PickerOutcome::Moved
        );
        assert_eq!(cursor.selected, 11);
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::Down),
            PickerOutcome::Unchanged
        );
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::PageUp),
            PickerOutcome::Moved
        );
        assert_eq!(cursor.selected, 6);
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::Home),
            PickerOutcome::Moved
        );
        assert_eq!(cursor.selected, 0);
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::Up),
            PickerOutcome::Unchanged
        );
        assert_eq!(
            update_picker(state(&mut cursor), PickerEdit::WheelUp),
            PickerOutcome::Unchanged
        );
    }

    #[test]
    fn picker_text_edits_reset_the_cursor_and_report_filtering() {
        let mut query = TextField::new();
        let mut cursor = ListCursor {
            selected: 4,
            scroll: 3,
        };
        let outcome = update_picker(
            PickerState {
                query: Some(&mut query),
                cursor: &mut cursor,
                len: 9,
                page: 4,
            },
            PickerEdit::Insert('g'),
        );
        assert_eq!(outcome, PickerOutcome::Filtered);
        assert_eq!(query.text, "g");
        assert_eq!(cursor, ListCursor::default());

        cursor.selected = 2;
        let outcome = update_picker(
            PickerState {
                query: Some(&mut query),
                cursor: &mut cursor,
                len: 9,
                page: 4,
            },
            PickerEdit::Paste("it".to_owned()),
        );
        assert_eq!(outcome, PickerOutcome::Filtered);
        assert_eq!(query.text, "git");
        assert_eq!(cursor.selected, 0);

        cursor.selected = 5;
        assert_eq!(
            update_picker(
                PickerState {
                    query: Some(&mut query),
                    cursor: &mut cursor,
                    len: 9,
                    page: 4,
                },
                PickerEdit::Paste(String::new()),
            ),
            PickerOutcome::Unchanged
        );
        assert_eq!(cursor.selected, 5);

        assert_eq!(
            update_picker(
                PickerState {
                    query: Some(&mut query),
                    cursor: &mut cursor,
                    len: 9,
                    page: 4,
                },
                PickerEdit::Backspace,
            ),
            PickerOutcome::Filtered
        );
        assert_eq!(query.text, "gi");

        assert_eq!(
            update_picker(
                PickerState {
                    query: None,
                    cursor: &mut cursor,
                    len: 9,
                    page: 4,
                },
                PickerEdit::Insert('x'),
            ),
            PickerOutcome::Unchanged
        );
    }

    #[test]
    fn picker_activation_click_and_cancel_are_reported() {
        let mut cursor = ListCursor {
            selected: 1,
            scroll: 0,
        };
        assert_eq!(
            update_picker(
                PickerState {
                    query: None,
                    cursor: &mut cursor,
                    len: 3,
                    page: 3,
                },
                PickerEdit::ClickRow(2),
            ),
            PickerOutcome::Activate
        );
        assert_eq!(cursor.selected, 2);
        assert_eq!(
            update_picker(
                PickerState {
                    query: None,
                    cursor: &mut cursor,
                    len: 3,
                    page: 3,
                },
                PickerEdit::ClickRow(3),
            ),
            PickerOutcome::Unchanged
        );
        assert_eq!(cursor.selected, 2);
        assert_eq!(
            update_picker(
                PickerState {
                    query: None,
                    cursor: &mut cursor,
                    len: 3,
                    page: 3,
                },
                PickerEdit::Activate,
            ),
            PickerOutcome::Activate
        );
        assert_eq!(
            update_picker(
                PickerState {
                    query: None,
                    cursor: &mut cursor,
                    len: 3,
                    page: 3,
                },
                PickerEdit::Cancel,
            ),
            PickerOutcome::Cancel
        );
    }
}
