//! Up/Down across soft-wrapped rows: a soft row's exclusive end is the next row's start.

use rand::prelude::*;
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::{ElementKind, TextArea};

fn ta_with(text: &str) -> TextArea {
    let mut t = TextArea::new();
    t.insert_str(text);
    t
}

fn cursor_row(t: &TextArea, width: u16) -> usize {
    TextArea::wrapped_line_index_by_start(&t.wrapped_lines(width), t.cursor()).unwrap()
}

#[test]
fn up_from_wide_row_lands_on_last_char_of_narrower_soft_row() {
    let width = 8;
    let mut t = ta_with("aaa bb ccccccc");
    assert_eq!(vec![0..7, 7..14], *t.wrapped_lines(width));

    t.move_cursor_up();
    assert_eq!(6, t.cursor());
    assert_eq!(0, cursor_row(&t, width));
    assert_eq!(Some((6, 0)), t.cursor_pos(Rect::new(0, 0, width, 5)));

    t.move_cursor_down();
    assert_eq!(14, t.cursor());

    t.move_cursor_up();
    assert_eq!(6, t.cursor());
    t.move_cursor_up();
    assert_eq!(0, t.cursor());
}

#[test]
fn up_onto_newline_terminated_row_lands_on_the_newline() {
    let mut t = ta_with("ab\ncdef");
    assert_eq!(vec![0..2, 3..7], *t.wrapped_lines(8));

    t.move_cursor_up();
    assert_eq!(2, t.cursor());
    assert_eq!(0, cursor_row(&t, 8));
}

#[test]
fn down_from_wide_row_lands_on_last_char_of_narrower_soft_row() {
    let mut t = ta_with("ccccccc aaa bb ddddddd");
    assert_eq!(vec![0..8, 8..15, 15..22], *t.wrapped_lines(8));
    t.set_cursor(7);

    t.move_cursor_down();
    assert_eq!(14, t.cursor());
    assert_eq!(1, cursor_row(&t, 8));

    t.move_cursor_down();
    assert_eq!(22, t.cursor());
}

#[test]
fn up_onto_soft_row_closed_by_element_lands_before_the_element() {
    let mut t = TextArea::new();
    t.insert_element("xyz", ElementKind(0), Some(Line::from("[ELEM]")));
    t.insert_str("longword");
    assert_eq!(vec![0..3, 3..11], *t.wrapped_lines(8));

    t.move_cursor_up();
    assert_eq!(0, t.cursor());
    assert_eq!(0, cursor_row(&t, 8));
}

#[test]
fn down_through_element_spanning_rows_reaches_the_end() {
    let mut t = TextArea::new();
    t.insert_element("abcdefgh", ElementKind(0), None);
    t.insert_str("z");
    assert_eq!(vec![0..3, 3..6, 6..9], *t.wrapped_lines(3));
    t.set_cursor(0);

    t.move_cursor_down();
    assert_eq!(8, t.cursor());
}

#[test]
fn up_and_down_move_one_row_at_a_time() {
    let mut rng = StdRng::seed_from_u64(7);
    for _ in 0..500 {
        let words = rng.random_range(1..12);
        let text = (0..words)
            .map(|_| "x".repeat(rng.random_range(1..9)))
            .collect::<Vec<_>>()
            .join(" ");
        let width = rng.random_range(3..12);
        let mut t = ta_with(&text);
        let rows = t.wrapped_lines(width).len();
        t.set_cursor(rng.random_range(0..=text.len()));

        let mut row = cursor_row(&t, width);
        while row + 1 < rows {
            t.move_cursor_down();
            assert_eq!(
                row + 1,
                cursor_row(&t, width),
                "down in {text:?} at width {width}"
            );
            row += 1;
        }
        while row > 0 {
            t.move_cursor_up();
            assert_eq!(
                row - 1,
                cursor_row(&t, width),
                "up in {text:?} at width {width}"
            );
            row -= 1;
        }
    }
}
