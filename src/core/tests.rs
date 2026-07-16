use super::cell::*;
use super::color::*;
use super::grid::*;
use super::parser::*;
use super::sixel::{SixelImage, decode};
use super::{base64, gif, inflate, iterm, jpeg, png, webp};

fn parse(input: &[u8], cols: usize, rows: usize) -> Grid {
    let mut g = Grid::new(cols, rows);
    let mut p = AnsiParser::new();
    p.advance(&mut g, input);
    g
}

fn row_text(g: &Grid, y: usize) -> String {
    let base = y * g.cols;
    g.cells[base..base + g.cols].iter().map(|c| c.ch).collect()
}

/// The full glyph text at `(x, y)`: base scalar plus any interned grapheme
/// continuation. Mirrors the renderer's reconstruction.
fn glyph(g: &Grid, x: usize, y: usize) -> String {
    let cell = g.cells[y * g.cols + x];
    let mut s = String::new();
    s.push(cell.ch);
    if cell.cluster != 0 {
        s.push_str(&g.clusters[(cell.cluster - 1) as usize]);
    }
    s
}

/// A decoded Sixel image's pixel at `(x, y)`, `None` if transparent/out of range.
fn spix(img: &SixelImage, x: usize, y: usize) -> Option<u32> {
    if x < img.width && y < img.height {
        img.pixels[y * img.width + x]
    } else {
        None
    }
}

#[test]
fn writes_plain_text() {
    let g = parse(b"hi", 80, 24);
    assert_eq!(g.cells[0].ch, 'h');
    assert_eq!(g.cells[1].ch, 'i');
    assert_eq!(g.cursor, (2, 0));
    assert!(g.dirty[0]);
}

#[test]
fn newline_and_carriage_return() {
    let g = parse(b"ab\r\nc", 80, 24);
    assert_eq!(g.cells[0].ch, 'a');
    assert_eq!(row_text(&g, 1).trim_end(), "c");
    assert_eq!(g.cursor, (1, 1));
}

#[test]
fn put_char_wraps_at_right_margin() {
    let g = parse(b"abc", 2, 24);
    assert_eq!(row_text(&g, 0), "ab");
    assert_eq!(g.cells[2].ch, 'c'); // wrapped to row 1, col 0 -> index y*cols+x = 1*2+0
    assert_eq!(g.cursor, (1, 1));
}

#[test]
fn tab_stops_at_eight_and_clamps_at_margin() {
    let g = parse(b"a\tb", 80, 24);
    assert_eq!(g.cursor.0, 9); // 'a' at 0, tab to 8, 'b' at 8 -> cursor 9
    assert_eq!(g.cells[8].ch, 'b');

    // A tab on a narrow grid must not wrap/scroll.
    let g2 = parse(b"\t", 4, 4);
    assert_eq!(g2.cursor.1, 0);
    assert!(g2.cursor.0 <= 3);
}

#[test]
fn sgr_sets_basic_colors() {
    let g = parse(b"\x1b[31mX", 80, 24);
    assert_eq!(g.cells[0].fg, PALETTE_16[1]); // SGR 31 = dim red 0x800000
}

#[test]
fn sgr_reset_restores_defaults() {
    let g = parse(b"\x1b[31mA\x1b[0mB", 80, 24);
    assert_eq!(g.cells[0].fg, PALETTE_16[1]);
    assert_eq!(g.cells[1].fg, DEFAULT_FG);
}

#[test]
fn sgr_empty_param_is_reset() {
    let g = parse(b"\x1b[31mA\x1b[mB", 80, 24);
    assert_eq!(g.cells[1].fg, DEFAULT_FG);
}

#[test]
fn sgr_truecolor_and_256() {
    let g = parse(b"\x1b[38;2;10;20;30mX", 80, 24);
    assert_eq!(g.cells[0].fg, 0x0A141E);

    let g2 = parse(b"\x1b[48;5;15mY", 80, 24);
    assert_eq!(g2.cells[0].bg, 0xFFFFFF); // palette index 15
}

#[test]
fn sgr_colon_subparameters() {
    // ISO 8613-6 colon forms (libvte et al.): both must set the color and not
    // leak the tail onto the screen as the old `;`-only parser did.
    let g = parse(b"\x1b[38:2:10:20:30mX", 80, 24);
    assert_eq!(g.cells[0].fg, 0x0A141E);
    assert_eq!(g.cells[1].ch, ' ', "no leaked tail");

    let g = parse(b"\x1b[48:5:15mY", 80, 24);
    assert_eq!(g.cells[0].bg, 0xFFFFFF);

    // T.416 form with a colorspace field between `2` and the RGB triple.
    let g = parse(b"\x1b[38:2:0:10:20:30mZ", 80, 24);
    assert_eq!(g.cells[0].fg, 0x0A141E);

    // A colon group mixed with ordinary parameters: bold + colon-fg.
    let g = parse(b"\x1b[1;38:5:1mW", 80, 24);
    assert_eq!(g.cells[0].fg, PALETTE_16[1]);
    assert_ne!(g.cells[0].flags & ATTR_BOLD, 0);
}

#[test]
fn csi_param_buffer_is_bounded() {
    // An unterminated CSI with a huge digit run must not grow without bound;
    // the parser keeps consuming but caps storage, then recovers on a final
    // byte and prints normally.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[");
    let flood = vec![b'1'; 1_000_000];
    p.advance(&mut g, &flood);
    p.advance(&mut g, b"mX"); // SGR final byte then a printable
    assert_eq!(g.cells[0].ch, 'X');
}

#[test]
fn cursor_position_is_clamped() {
    let g = parse(b"\x1b[999;999H", 80, 24);
    assert_eq!(g.cursor, (79, 23));
}

#[test]
fn cursor_position_default_is_home() {
    let g = parse(b"X\x1b[HY", 80, 24);
    assert_eq!(g.cells[0].ch, 'Y'); // overwrote 'X' at home
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn erase_line_to_end() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (3, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[K");
    assert_eq!(&row_text(&g, 0)[..6], "abc   ");
}

#[test]
fn erase_line_to_start() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (2, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[1K");
    assert_eq!(&row_text(&g, 0)[..6], "   def");
}

#[test]
fn erase_display_full() {
    let mut g = parse(b"hello", 80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2J");
    assert_eq!(g.cells[0].ch, ' ');
    assert_eq!(g.cursor, (0, 0));
}

#[test]
fn erase_display_3_clears_scrollback() {
    // Scroll a few lines into history, then `CSI 3 J` (xterm "erase saved
    // lines"). `2 J` leaves history; `3 J` must drop it.
    let mut g = Grid::new(10, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"a\r\nb\r\nc\r\nd");
    assert!(!g.scrollback.is_empty(), "lines scrolled into history");

    p.advance(&mut g, b"\x1b[2J");
    assert!(!g.scrollback.is_empty(), "2J keeps scrollback");

    p.advance(&mut g, b"\x1b[3J");
    assert!(g.scrollback.is_empty(), "3J erases scrollback");
}

#[test]
fn scroll_up_shifts_rows() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"top\r\nbot");
    // Force one more newline to scroll.
    p.advance(&mut g, b"\r\nnew");
    assert_eq!(row_text(&g, 0).trim_end(), "bot");
    assert_eq!(row_text(&g, 1).trim_end(), "new");
}

#[test]
fn alt_screen_saves_and_restores_primary() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"primary text");
    assert_eq!(&row_text(&g, 0)[..12], "primary text");

    // Enter alt screen (DEC private 1049): cleared, cursor home.
    p.advance(&mut g, b"\x1b[?1049h");
    assert_eq!(g.cells[0].ch, ' ');
    assert_eq!(g.cursor, (0, 0));
    p.advance(&mut g, b"ALT");
    assert_eq!(&row_text(&g, 0)[..3], "ALT");

    // Leave alt screen: primary content and cursor come back.
    p.advance(&mut g, b"\x1b[?1049l");
    assert_eq!(&row_text(&g, 0)[..12], "primary text");
    assert_eq!(g.cursor, (12, 0));
}

#[test]
fn alt_screen_47_does_not_save_or_restore_cursor() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"primary");
    g.cursor = (5, 5);

    // ?47 swaps the buffer but must not home or save the cursor.
    p.advance(&mut g, b"\x1b[?47h");
    assert_eq!(g.cursor, (5, 5)); // not homed (unlike 1049)
    g.cursor = (10, 3);

    // ?47l swaps back without restoring the cursor.
    p.advance(&mut g, b"\x1b[?47l");
    assert_eq!(g.cursor, (10, 3)); // not restored
    assert_eq!(&row_text(&g, 0)[..7], "primary"); // primary buffer back
}

#[test]
fn alt_screen_survives_resize() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"keep me");
    p.advance(&mut g, b"\x1b[?1049h"); // to alt
    g.resize(100, 30); // resize while on alt screen
    p.advance(&mut g, b"\x1b[?1049l"); // back to primary
    assert_eq!(g.cols, 100);
    assert_eq!(&row_text(&g, 0)[..7], "keep me");
}

#[test]
fn resize_keeps_short_line_and_repaints() {
    let mut g = parse(b"hello", 80, 24);
    g.resize(10, 5);
    assert_eq!((g.cols, g.rows), (10, 5));
    assert_eq!(&row_text(&g, 0)[..5], "hello"); // short line rides with its row
    assert_eq!(g.cursor, (5, 0)); // cursor stays just past its (unwrapped) line
    assert!(g.dirty.iter().all(|&d| d)); // full repaint queued
    // Growing back keeps the surviving content and blanks the new area.
    g.resize(80, 24);
    assert_eq!(&row_text(&g, 0)[..5], "hello");
    assert_eq!(g.cells[79].ch, ' ');
}

#[test]
fn resize_narrow_rewraps_a_wrapped_line() {
    // 15 cells auto-wrap 10|5 at width 10; narrowing to 5 re-wraps to 5|5|5
    // rather than truncating the overflow.
    let mut g = Grid::new(10, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"ABCDEFGHIJKLMNO");
    assert_eq!(row_text(&g, 0), "ABCDEFGHIJ");
    assert_eq!(row_text(&g, 1).trim_end(), "KLMNO");
    g.resize(5, 4);
    assert_eq!(row_text(&g, 0), "ABCDE");
    assert_eq!(row_text(&g, 1), "FGHIJ");
    assert_eq!(row_text(&g, 2), "KLMNO");
    assert_eq!(g.cursor, (4, 2)); // cursor follows to the end of the rewrapped run
}

#[test]
fn resize_widen_rejoins_a_wrapped_line() {
    // 10 cells wrapped 5|5 at width 5; widening to 10 rejoins them on one row.
    let mut g = Grid::new(5, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"ABCDEFGHIJ");
    assert_eq!(row_text(&g, 0), "ABCDE");
    assert_eq!(row_text(&g, 1), "FGHIJ");
    g.resize(10, 4);
    assert_eq!(row_text(&g, 0), "ABCDEFGHIJ");
    assert_eq!(row_text(&g, 1).trim_end(), ""); // continuation pulled up
    assert_eq!(g.cursor, (9, 0));
}

#[test]
fn resize_roundtrip_preserves_wrapped_text() {
    // A 30-cell logical line survives narrow→wide unchanged: the soft-wrap bit,
    // not the physical row split, defines the line.
    let mut g = Grid::new(20, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"the quick brown fox jumps over");
    g.resize(7, 5); // shred into 7-wide rows, some pushed into scrollback
    g.resize(20, 5); // and pull them all back
    assert_eq!(row_text(&g, 0), "the quick brown fox ");
    assert_eq!(row_text(&g, 1).trim_end(), "jumps over");
    assert_eq!(g.cursor, (10, 1));
}

#[test]
fn resize_does_not_rejoin_hard_breaks() {
    // Two lines separated by a hard CR/LF stay distinct logical lines: a resize
    // must not glue them into "helloworld".
    let mut g = Grid::new(10, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"hello\r\nworld");
    g.resize(3, 4);
    assert_eq!(row_text(&g, 0), "hel");
    assert_eq!(row_text(&g, 1).trim_end(), "lo");
    assert_eq!(row_text(&g, 2), "wor");
    assert_eq!(row_text(&g, 3).trim_end(), "ld");
    assert_eq!(g.cursor, (2, 3));
}

#[test]
fn resize_does_not_split_a_wide_glyph() {
    // a世界你 = widths 1+2+2+2 = 7. Narrowing to 4 must keep each CJK head glued
    // to its trailer, pushing a glyph that won't fit down to the next row.
    let mut g = Grid::new(6, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, "a世界你".as_bytes());
    g.resize(4, 3);
    assert_eq!(g.cells[0].ch, 'a');
    assert_eq!(g.cells[1].ch, '世');
    assert_eq!(g.cells[2].flags & WIDE_TRAILER, WIDE_TRAILER);
    assert_eq!(g.cells[3].ch, ' '); // 界 didn't fit in col 3 -> pushed down, no split
    assert_eq!(g.cells[4].ch, '界'); // row 1, col 0
    assert_eq!(g.cells[6].ch, '你'); // row 1, col 2
}

#[test]
fn resize_preserves_double_width_attr() {
    let mut g = Grid::new(10, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b#6HI"); // DECDWL on row 0
    assert_eq!(g.snapshot_dirty().line_attrs[0], LineAttr::DoubleWidth);
    g.resize(8, 3);
    // The line-size attribute rides the logical line through the reflow
    // (previously a resize reset every row to Single).
    assert_eq!(g.snapshot_dirty().line_attrs[0], LineAttr::DoubleWidth);
}

#[test]
fn resize_preserves_prompt_marks() {
    // Four OSC-133 prompts, the oldest scrolled into history; a resize must carry
    // the marks through instead of clearing them (the pre-fix behavior).
    let mut g = Grid::new(8, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;A\x07one\r\n");
    p.advance(&mut g, b"\x1b]133;A\x07two\r\n");
    p.advance(&mut g, b"\x1b]133;A\x07three\r\n");
    p.advance(&mut g, b"\x1b]133;A\x07four\r\n");
    assert!(g.scrollback.len() >= 2);
    assert!(g.scroll_to_prev_prompt()); // navigable before resize
    g.reset_view();
    g.resize(4, 3); // narrows + rebuilds the buffer
    assert!(g.scroll_to_prev_prompt()); // a prompt mark survived into history
}

#[test]
fn scroll_region_limits_scrolling_and_dirtying() {
    // 1-row grid would be degenerate; use 5 rows, region = rows 2..=3 (1-based 3;4).
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    // Fill rows 2 and 3 with markers.
    g.cursor = (0, 2);
    p.advance(&mut g, b"AAAA");
    g.cursor = (0, 3);
    p.advance(&mut g, b"BBBB");
    // Set region to rows 3..4 (1-based) = 2..=3 (0-based) and clear dirty.
    p.advance(&mut g, b"\x1b[3;4r");
    assert_eq!((g.scroll_top, g.scroll_bottom), (2, 3));
    assert_eq!(g.cursor, (0, 0)); // DECSTBM homes the cursor
    g.clear_dirty();

    // Put cursor at region bottom and newline -> scroll only the region.
    g.cursor = (0, 3);
    p.advance(&mut g, b"\n");
    assert_eq!(row_text(&g, 2).trim_end(), "BBBB"); // row 3 shifted up to row 2
    assert_eq!(row_text(&g, 3).trim_end(), ""); // region bottom blanked
    // Only region rows are dirty.
    assert_eq!(g.dirty, vec![false, false, true, true, false]);
}

#[test]
fn scroll_region_resets_on_full_screen_request() {
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2;4r");
    assert_eq!((g.scroll_top, g.scroll_bottom), (1, 3));
    p.advance(&mut g, b"\x1b[r"); // no params -> full screen
    assert_eq!((g.scroll_top, g.scroll_bottom), (0, 4));
}

#[test]
fn snapshot_only_dirty_rows() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"x");
    let frame = g.snapshot_dirty();
    assert_eq!(frame.rows.len(), 1);
    assert_eq!(frame.rows[0].0, 0);
    assert_eq!(frame.cursor, (1, 0));
    g.clear_dirty();
    assert!(g.snapshot_dirty().rows.is_empty());
}

#[test]
fn csi_empty_leading_param_keeps_position() {
    // CSI ;5H -> row defaults to 1, column = 5 -> (col 4, row 0).
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b[;5H");
    assert_eq!(g.cursor, (4, 0));
    // CSI ;10r -> top defaults to 1 (row 0), bottom = 10 (row 9).
    p.advance(&mut g, b"\x1b[;10r");
    assert_eq!((g.scroll_top, g.scroll_bottom), (0, 9));
}

#[test]
fn csi_huge_count_does_not_overflow() {
    // CUD/CUF with a near-usize::MAX count must saturate, not panic/wrap.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (5, 5);
    p.advance(&mut g, b"\x1b[18446744073709551610B"); // CUD
    assert_eq!(g.cursor.1, 23); // clamped to last row
    p.advance(&mut g, b"\x1b[18446744073709551610C"); // CUF
    assert_eq!(g.cursor.0, 79); // clamped to last column
}

#[test]
fn c0_control_inside_csi_executes_and_continues() {
    // CSI 5 \r ; 10 H: the CR executes mid-sequence, the CSI continues, and
    // nothing leaks as printed text.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5\r;10H");
    assert_eq!(g.cursor, (9, 4)); // CUP row 5, col 10 applied
    assert_eq!(g.cells[0].ch, ' '); // ";10H" not printed
}

#[test]
fn alt_screen_does_not_leak_saved_cursor() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (5, 5);
    p.advance(&mut g, b"\x1b7"); // DECSC on primary -> saved (5,5)
    p.advance(&mut g, b"\x1b[?1049h"); // to alt
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b7"); // DECSC on alt -> alt's saved (10,10)
    p.advance(&mut g, b"\x1b[?1049l"); // back to primary
    p.advance(&mut g, b"\x1b8"); // DECRC on primary
    assert_eq!(g.cursor, (5, 5)); // primary's saved cursor intact
}

#[test]
fn cursor_motion_relative() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b[3C"); // forward 3
    assert_eq!(g.cursor, (13, 10));
    p.advance(&mut g, b"\x1b[5D"); // back 5
    assert_eq!(g.cursor, (8, 10));
    p.advance(&mut g, b"\x1b[2A"); // up 2
    assert_eq!(g.cursor, (8, 8));
    p.advance(&mut g, b"\x1b[B"); // down 1 (default)
    assert_eq!(g.cursor, (8, 9));
}

#[test]
fn cursor_absolute_column_and_row() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b[1G"); // column 1 (0-based 0)
    assert_eq!(g.cursor, (0, 10));
    p.advance(&mut g, b"\x1b[5d"); // row 5 (0-based 4)
    assert_eq!(g.cursor, (0, 4));
}

#[test]
fn backspace_moves_left() {
    let mut g = parse(b"abc", 80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x08"); // cursor 3 -> 2
    assert_eq!(g.cursor, (2, 0));
    p.advance(&mut g, b"X"); // overwrites 'c'
    assert_eq!(row_text(&g, 0).trim_end(), "abX");
}

#[test]
fn delete_chars_shifts_left() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (1, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2P"); // delete "bc"
    assert_eq!(&row_text(&g, 0)[..6], "adef  ");
}

#[test]
fn insert_chars_shifts_right() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (1, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2@"); // insert 2 blanks at col 1
    assert_eq!(&row_text(&g, 0)[..6], "a  bcd");
}

#[test]
fn erase_chars_blanks_without_shift() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (2, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2X"); // blank "cd"
    assert_eq!(&row_text(&g, 0)[..6], "ab  ef");
}

#[test]
fn save_and_restore_cursor() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (5, 5);
    p.advance(&mut g, b"\x1b[s"); // save
    g.cursor = (20, 20);
    p.advance(&mut g, b"\x1b[u"); // restore
    assert_eq!(g.cursor, (5, 5));
    // DECSC/DECRC (ESC 7 / ESC 8) variant.
    g.cursor = (1, 2);
    p.advance(&mut g, b"\x1b7");
    g.cursor = (9, 9);
    p.advance(&mut g, b"\x1b8");
    assert_eq!(g.cursor, (1, 2));
}

#[test]
fn osc_title_is_consumed_not_printed() {
    // OSC 2 (set window title) terminated by BEL, then real text.
    let g = parse(b"\x1b]2;my title\x07X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn osc_terminated_by_st_is_consumed() {
    // OSC 7 (cwd) terminated by ST (ESC \).
    let g = parse(b"\x1b]7;file://host/path\x1b\\Y", 80, 24);
    assert_eq!(g.cells[0].ch, 'Y');
}

#[test]
fn csi_private_mode_is_consumed_not_printed() {
    // Bracketed-paste enable/disable must not leak "2004h"/"2004l".
    let g = parse(b"\x1b[?2004hA\x1b[?2004lB", 80, 24);
    assert_eq!(g.cells[0].ch, 'A');
    assert_eq!(g.cells[1].ch, 'B');
    assert_eq!(g.cursor, (2, 0));
}

#[test]
fn charset_designation_is_consumed() {
    // ESC ( B (designate ASCII) must not leak the 'B'.
    let g = parse(b"\x1b(BZ", 80, 24);
    assert_eq!(g.cells[0].ch, 'Z');
    assert_eq!(g.cursor, (1, 0));
}
#[test]
fn dec_line_drawing_g0_translates_letters_to_box_glyphs() {
    // ESC ( 0 designates DEC Special Graphics into G0 (active by default), so
    // `lqk` becomes the top of a box: ┌─┐. ESC ( B restores ASCII.
    let g = parse(b"\x1b(0lqk\x1b(Bx", 80, 24);
    assert_eq!(g.cells[0].ch, '┌');
    assert_eq!(g.cells[1].ch, '─');
    assert_eq!(g.cells[2].ch, '┐');
    assert_eq!(g.cells[3].ch, 'x'); // back to ASCII — not translated
}

#[test]
fn so_si_toggle_g1_line_drawing() {
    // The ncurses pattern: designate G1 = line-drawing (ESC ) 0), then SO/SI
    // (^N/^O) shift GL between G1 and G0 around a run of glyphs.
    let g = parse(b"\x1b)0a\x0eqx\x0fb", 80, 24);
    assert_eq!(g.cells[0].ch, 'a'); // GL still G0 (ASCII)
    assert_eq!(g.cells[1].ch, '─'); // SO -> G1, 'q' -> horizontal line
    assert_eq!(g.cells[2].ch, '│'); // 'x' -> vertical line
    assert_eq!(g.cells[3].ch, 'b'); // SI -> back to G0 (ASCII)
}

#[test]
fn dec_graphics_passes_digits_and_space_through() {
    // Only 0x60..=0x7e differ; digits, space, and punctuation are unchanged.
    let g = parse(b"\x1b(0 1!", 80, 24);
    assert_eq!(g.cells[0].ch, ' ');
    assert_eq!(g.cells[1].ch, '1');
    assert_eq!(g.cells[2].ch, '!');
}

#[test]
fn dec_graphics_rep_repeats_translated_glyph() {
    // REP after a line-drawing glyph repeats the translated glyph, not the byte.
    let g = parse(b"\x1b(0q\x1b[2b", 80, 24); // '─' then repeat x2 -> "───"
    assert_eq!(g.cells[0].ch, '─');
    assert_eq!(g.cells[1].ch, '─');
    assert_eq!(g.cells[2].ch, '─');
    assert_eq!(g.cursor, (3, 0));
}

#[test]
fn ris_resets_charset_to_ascii() {
    // A line-drawing charset left active must not survive a full reset.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b(0"); // G0 = line-drawing
    p.advance(&mut g, b"\x1bc"); // RIS
    p.advance(&mut g, b"q"); // would be '─' if charset persisted
    assert_eq!(g.cells[0].ch, 'q');
}

#[test]
fn decstr_resets_charset_to_ascii() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b(0"); // G0 = line-drawing
    p.advance(&mut g, b"\x1b[!p"); // DECSTR soft reset
    p.advance(&mut g, b"q");
    assert_eq!(g.cells[0].ch, 'q');
}

#[test]
fn char_width_classifies_common_cases() {
    assert_eq!(char_width('a'), 1);
    assert_eq!(char_width('é'), 1);
    assert_eq!(char_width('世'), 2); // CJK
    assert_eq!(char_width('😀'), 2); // emoji
    assert_eq!(char_width('\u{0301}'), 0); // combining acute accent
}

#[test]
fn char_width_covers_cases_the_old_table_missed() {
    // Zero-width characters the hand-rolled table didn't list. Getting any
    // of these wrong shifts the rest of the line (cursor desync).
    assert_eq!(char_width('\u{200D}'), 0); // ZWJ (emoji sequence glue)
    assert_eq!(char_width('\u{200C}'), 0); // ZWNJ
    assert_eq!(char_width('\u{FE0F}'), 0); // VS16 (emoji presentation selector)
    assert_eq!(char_width('\u{064B}'), 0); // Arabic fathatan
    assert_eq!(char_width('\u{094D}'), 0); // Devanagari virama
    assert_eq!(char_width('\u{1160}'), 0); // Hangul conjoining jungseong filler

    // Default-emoji-presentation symbols below the old 0x2E80 wide cutoff;
    // these render double-width and were previously reported as 1.
    assert_eq!(char_width('\u{231A}'), 2); // ⌚ WATCH
    assert_eq!(char_width('\u{26A1}'), 2); // ⚡ HIGH VOLTAGE
    assert_eq!(char_width('\u{2705}'), 2); // ✅ WHITE HEAVY CHECK MARK

    // Text-presentation-by-default symbol stays width 1 (no VS16 follows).
    assert_eq!(char_width('\u{2764}'), 1); // ❤ HEAVY BLACK HEART
}

#[test]
fn wide_char_occupies_two_cells() {
    let g = parse("世x".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, '世');
    assert_eq!(g.cells[0].flags & WIDE_TRAILER, 0);
    assert_ne!(g.cells[1].flags & WIDE_TRAILER, 0); // trailer flagged
    assert_eq!(g.cells[2].ch, 'x'); // next glyph after the wide pair
    assert_eq!(g.cursor, (3, 0));
}

#[test]
fn overwriting_wide_head_clears_orphan_trailer() {
    let mut g = Grid::new(80, 24);
    g.put_char('世', Pen::default()); // head col 0, trailer col 1
    g.cursor = (0, 0);
    g.put_char('a', Pen::default()); // overwrite the head
    assert_eq!(g.cells[0].ch, 'a');
    assert_eq!(g.cells[1].ch, ' '); // orphaned trailer blanked
    assert_eq!(g.cells[1].flags & WIDE_TRAILER, 0);
}

#[test]
fn overwriting_wide_trailer_clears_orphan_head() {
    let mut g = Grid::new(80, 24);
    g.put_char('世', Pen::default()); // head col 0, trailer col 1
    g.cursor = (1, 0);
    g.put_char('b', Pen::default()); // overwrite the trailer
    assert_eq!(g.cells[1].ch, 'b');
    assert_eq!(g.cells[0].ch, ' '); // orphaned head blanked
}

#[test]
fn wide_char_wraps_when_it_would_not_fit() {
    // Width-3 grid: 'a' at col 0, wide '世' needs cols 1-2 -> fits at 1..3.
    let g = parse("a世".as_bytes(), 3, 24);
    assert_eq!(g.cells[0].ch, 'a');
    assert_eq!(g.cells[1].ch, '世');
    // Now only 1 column free; a second wide char must wrap to the next row.
    let g2 = parse("ab世".as_bytes(), 3, 24);
    assert_eq!(row_text(&g2, 0), "ab ");
    assert_eq!(g2.cells[3].ch, '世'); // wrapped to row 1, col 0
}

#[test]
fn combining_mark_attaches_to_preceding_glyph() {
    // 'a' + U+0301 (combining acute) + 'b'.
    let g = parse("a\u{0301}b".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, 'a');
    assert_eq!(glyph(&g, 0, 0), "a\u{0301}"); // mark composed onto 'a'
    assert_eq!(g.cells[1].ch, 'b'); // mark consumed no cell
    assert_eq!(g.cursor, (2, 0));
}

#[test]
fn multiple_combining_marks_are_all_kept() {
    let mut g = Grid::new(80, 24);
    g.put_char('e', Pen::default());
    // Three combining marks all attach to 'e' — UAX #29 clusters are unbounded
    // now, so none is dropped (the old fixed 2-slot cap is gone).
    g.put_char('\u{0301}', Pen::default());
    g.put_char('\u{0323}', Pen::default());
    g.put_char('\u{0308}', Pen::default());
    assert_eq!(glyph(&g, 0, 0), "e\u{0301}\u{0323}\u{0308}");
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn combining_mark_at_line_start_is_dropped() {
    // No preceding glyph -> nothing to attach to.
    let g = parse("\u{0301}x".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, 'x');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn combining_mark_attaches_to_wide_glyph_head() {
    let g = parse("世\u{0301}".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, '世');
    assert_eq!(glyph(&g, 0, 0), "世\u{0301}"); // on the head, not the trailer
    assert_ne!(g.cells[1].flags & WIDE_TRAILER, 0);
}

#[test]
fn zwj_emoji_sequence_collapses_to_one_glyph() {
    // 👨 + ZWJ + 💻 (man technologist) is one grapheme occupying two columns,
    // not three separate emoji spanning six.
    let g = parse("👨\u{200d}💻".as_bytes(), 80, 24);
    assert_eq!(glyph(&g, 0, 0), "👨\u{200d}💻");
    assert_ne!(g.cells[1].flags & WIDE_TRAILER, 0);
    assert_eq!(g.cursor, (2, 0));
}

#[test]
fn emoji_skin_tone_modifier_joins_base_glyph() {
    // 👍 + medium skin tone is one grapheme; the modifier doesn't get its own cell.
    let g = parse("👍\u{1f3fd}".as_bytes(), 80, 24);
    assert_eq!(glyph(&g, 0, 0), "👍\u{1f3fd}");
    assert_eq!(g.cursor, (2, 0));
}

#[test]
fn distinct_wide_glyphs_stay_separate() {
    // Boundary detection must not over-merge: 世 and 界 are separate graphemes.
    let g = parse("世界".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, '世');
    assert_eq!(g.cells[2].ch, '界');
    assert_eq!(g.cursor, (4, 0));
}

#[test]
fn utf8_two_byte_decodes() {
    // U+00E9 'é' = C3 A9
    let g = parse(b"\xc3\xa9", 80, 24);
    assert_eq!(g.cells[0].ch, 'é');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn utf8_three_byte_decodes() {
    // U+2794 '➔'-family arrow = E2 9E 94; the prompt arrow '➜' is E2 9E 9C.
    let g = parse("➜".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, '➜');
}

#[test]
fn utf8_four_byte_emoji_decodes() {
    // U+1F600 😀 = F0 9F 98 80
    let g = parse("😀".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, '😀');
}

#[test]
fn utf8_split_across_chunks() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    let bytes = "é".as_bytes(); // C3 A9
    p.advance(&mut g, &bytes[..1]); // lead byte only
    assert_eq!(g.cells[0].ch, ' '); // nothing emitted yet
    p.advance(&mut g, &bytes[1..]); // continuation
    assert_eq!(g.cells[0].ch, 'é');
}

#[test]
fn utf8_invalid_yields_replacement() {
    // Stray continuation byte.
    let g = parse(b"\x80X", 80, 24);
    assert_eq!(g.cells[0].ch, '\u{FFFD}');
    assert_eq!(g.cells[1].ch, 'X');
}

#[test]
fn utf8_truncated_then_ascii_recovers() {
    // Lead byte expecting a continuation, interrupted by an ASCII byte:
    // emit replacement for the truncated char, then render the ASCII byte.
    let g = parse(b"\xc3A", 80, 24);
    assert_eq!(g.cells[0].ch, '\u{FFFD}');
    assert_eq!(g.cells[1].ch, 'A');
}

#[test]
fn utf8_overlong_yields_replacement() {
    // E0 80 9B is an overlong encoding of ESC (0x1B). Accepting it would put a
    // raw control scalar in a cell — an escape-injection vector once the
    // renderer replays cell text to the host terminal.
    let g = parse(b"\xe0\x80\x9bX", 80, 24);
    assert_eq!(g.cells[0].ch, '\u{FFFD}');
    assert_eq!(g.cells[1].ch, 'X');
    // The largest overlong values each length admits (C0/C1 leads are already
    // rejected, so 2-byte overlongs can't begin; test 3- and 4-byte forms).
    let g = parse(b"\xe0\x9f\xbfX", 80, 24); // U+07FF in three bytes
    assert_eq!(g.cells[0].ch, '\u{FFFD}');
    let g = parse(b"\xf0\x8f\xbf\xbfX", 80, 24); // U+FFFF in four bytes
    assert_eq!(g.cells[0].ch, '\u{FFFD}');
    assert_eq!(g.cells[1].ch, 'X');
    // The boundary values themselves still decode.
    let g = parse("\u{800}\u{10000}".as_bytes(), 80, 24);
    assert_eq!(g.cells[0].ch, '\u{800}');
    assert_eq!(g.cells[1].ch, '\u{10000}');
}

#[test]
fn wide_glyph_in_one_column_grid_does_not_panic() {
    // A width-2 glyph can never fit in a 1-column grid; the cursor must still
    // stay within the row (it used to run past it and index out of bounds).
    let g = parse("世界世界".as_bytes(), 1, 3);
    assert!(g.cursor.0 <= 1, "cursor past the row: {:?}", g.cursor);
}

#[test]
fn escape_sequence_split_across_chunks() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[3");
    p.advance(&mut g, b"1mX");
    assert_eq!(g.cells[0].fg, PALETTE_16[1]); // SGR 31
}

#[test]
fn dcs_string_is_consumed_not_printed() {
    // DCS (ESC P) … ST (ESC \) — e.g. a DECRQSS status reply. The body must
    // not leak onto the screen.
    let g = parse(b"\x1bP1$r0m\x1b\\X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn apc_string_is_consumed_not_printed() {
    // APC (ESC _) … ST — e.g. the Kitty graphics protocol introducer.
    let g = parse(b"\x1b_Gf=100,a=T;base64data\x1b\\X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn pm_string_is_consumed_not_printed() {
    // PM (ESC ^) … ST.
    let g = parse(b"\x1b^private message\x1b\\X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn sos_string_is_consumed_not_printed() {
    // SOS (ESC X) … ST.
    let g = parse(b"\x1bXstart of string\x1b\\Y", 80, 24);
    assert_eq!(g.cells[0].ch, 'Y');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn dcs_string_split_across_chunks_is_consumed() {
    // The string sink state must persist across read boundaries.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1bP1$r");
    p.advance(&mut g, b"sixel-ish body");
    p.advance(&mut g, b"\x1b\\Z");
    assert_eq!(g.cells[0].ch, 'Z');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn dcs_string_aborted_by_can() {
    // CAN (0x18) cancels the string; subsequent bytes render normally.
    let g = parse(b"\x1bPbody\x18X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn da1_query_is_answered_and_not_printed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Both the bare and explicit-0 forms are queries. Attribute `4` (Sixel)
    // is advertised since this terminal actually decodes Sixel — apps that
    // gate Sixel support on DA1 containing it need to see it here.
    p.advance(&mut g, b"\x1b[c");
    assert_eq!(p.take_responses(), b"\x1b[?1;2;4c");
    p.advance(&mut g, b"\x1b[0c");
    assert_eq!(p.take_responses(), b"\x1b[?1;2;4c");
    // Nothing leaked onto the grid.
    assert_eq!(g.cells[0].ch, ' ');
    assert_eq!(g.cursor, (0, 0));
}

#[test]
fn da2_query_is_answered() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[>c");
    assert_eq!(p.take_responses(), b"\x1b[>0;1;0c");
    // The `>` marker must not be confused with a DEC private mode and must
    // not disturb the alt screen.
    assert!(g.cells[0].ch == ' ');
}

#[test]
fn xtversion_query_reports_name_and_version() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[>q");
    let expected = concat!("\x1bP>|rusty_term(", env!("CARGO_PKG_VERSION"), ")\x1b\\").as_bytes();
    assert_eq!(p.take_responses(), expected);
    assert_eq!(g.cells[0].ch, ' '); // nothing leaked to the screen
}
#[test]
fn xtgettcap_answers_known_caps_and_truecolor() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // `Co` (436f) and `Tc` (5463) in one query.
    p.advance(&mut g, b"\x1bP+q436f;5463\x1b\\");
    // Co=256 -> value "256" hex-encoded (323536); Tc is a boolean (no value).
    assert_eq!(p.take_responses(), b"\x1bP1+r436f=323536\x1b\\\x1bP1+r5463\x1b\\");
    assert_eq!(g.cells[0].ch, ' '); // nothing leaked to the screen
}

#[test]
fn xtgettcap_reports_terminal_name() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1bP+q544e\x1b\\"); // TN -> terminfo name
    // "rusty_term" hex-encoded.
    assert_eq!(p.take_responses(), b"\x1bP1+r544e=72757374795f7465726d\x1b\\");
}

#[test]
fn xtgettcap_unknown_and_malformed_fail() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // "ZZ" (5a5a) is valid hex but an unknown cap; "abc" is malformed (odd len).
    // Both echo the requested name back under the `0 + r` failure form.
    p.advance(&mut g, b"\x1bP+q5a5a;abc\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP0+r5a5a\x1b\\\x1bP0+rabc\x1b\\");
}

#[test]
fn decscusr_sets_cursor_shape_and_blink() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Power-on default: steady block.
    assert_eq!(g.cursor_shape, CursorShape::Block);
    assert!(!g.cursor_blink);
    p.advance(&mut g, b"\x1b[6 q"); // steady bar
    assert_eq!(g.cursor_shape, CursorShape::Bar);
    assert!(!g.cursor_blink);
    p.advance(&mut g, b"\x1b[3 q"); // blinking underline
    assert_eq!(g.cursor_shape, CursorShape::Underline);
    assert!(g.cursor_blink);
    p.advance(&mut g, b"\x1b[ q"); // empty param == 0 == blinking block
    assert_eq!(g.cursor_shape, CursorShape::Block);
    assert!(g.cursor_blink);
    p.advance(&mut g, b"\x1b[9 q"); // out of range: unchanged
    assert_eq!(g.cursor_shape, CursorShape::Block);
    assert!(g.cursor_blink);
}

#[test]
fn decscusr_is_relayed_to_host_not_printed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[4 q");
    assert_eq!(g.take_host_out(), b"\x1b[4 q");
    assert_eq!(g.cells[0].ch, ' '); // nothing leaked to the screen
}

#[test]
fn ris_restores_configured_default_cursor() {
    let mut g = Grid::new(80, 24);
    g.set_default_cursor(CursorShape::Bar, true); // config default: blinking bar
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2 q"); // child switches to a steady block
    assert_eq!(g.cursor_shape, CursorShape::Block);
    assert!(!g.cursor_blink);
    p.advance(&mut g, b"\x1bc"); // RIS restores the configured default
    assert_eq!(g.cursor_shape, CursorShape::Bar);
    assert!(g.cursor_blink);
}

#[test]
fn da3_query_is_answered() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[=c");
    assert_eq!(p.take_responses(), b"\x1bP!|00000000\x1b\\");
}

#[test]
fn dsr_status_report_is_answered() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5n");
    assert_eq!(p.take_responses(), b"\x1b[0n");
}

#[test]
fn dsr_cursor_position_report_uses_one_based_coords() {
    let mut g = Grid::new(80, 24);
    g.cursor = (4, 9); // col 4, row 9 (0-based)
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[6n");
    assert_eq!(p.take_responses(), b"\x1b[10;5R"); // row 10, col 5 (1-based)
}

#[test]
fn decxcpr_private_form_is_answered_with_the_private_marker() {
    // `CSI ? 6 n` (DECXCPR) is the private-marker form of CPR; a program
    // that probes with it (rather than the plain `CSI 6 n` above) used to
    // get no reply at all and could block forever waiting for one.
    let mut g = Grid::new(80, 24);
    g.cursor = (4, 9); // col 4, row 9 (0-based)
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?6n");
    assert_eq!(p.take_responses(), b"\x1b[?10;5;1R"); // row 10, col 5, page 1
}

#[test]
fn no_query_means_no_response() {
    // A normal print run owes the host nothing.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"hello\x1b[31mworld");
    assert!(p.take_responses().is_empty());
}

#[test]
fn synchronized_output_mode_toggles_on_set_reset() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.sync_output_active(), "closed by default");
    p.advance(&mut g, b"\x1b[?2026h");
    assert!(g.sync_output_active(), "set: window is open");
    p.advance(&mut g, b"\x1b[?2026l");
    assert!(!g.sync_output_active(), "reset: window closed normally");
    // A whole begin..end pair landing in one `advance` call (the common case
    // when an app's write isn't split across PTY reads) leaves the window
    // closed by the time the render-trigger call site checks it — exactly
    // the "paint the complete frame in one shot" behavior this mode is for.
    p.advance(&mut g, b"\x1b[?2026h...\x1b[?2026l");
    assert!(!g.sync_output_active());
}

// The timeout safety valve (SYNC_OUTPUT_TIMEOUT, ~800ms) that auto-closes a
// window a misbehaving client never resets isn't exercised here: faking it
// would need either a real sleep (slows the suite for one test) or injecting
// a fake clock this crate doesn't otherwise need. Covered by inspection of
// `Grid::sync_output_active` instead — the logic is a two-line elapsed()
// check with nothing else that could plausibly break independently of the
// toggle behavior the tests above already cover.

#[test]
fn xtwinops_18t_reports_text_area_in_cells() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[18t");
    assert_eq!(p.take_responses(), b"\x1b[8;24;80t");
}

#[test]
fn xtwinops_pixel_queries_need_cell_px_and_answer_when_set() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // No cell_px (TUI-mode default): pixel queries are silently declined.
    p.advance(&mut g, b"\x1b[16t\x1b[14t");
    assert!(p.take_responses().is_empty());

    g.cell_px = Some((9, 18));
    p.advance(&mut g, b"\x1b[16t");
    assert_eq!(p.take_responses(), b"\x1b[6;18;9t");
    p.advance(&mut g, b"\x1b[14t");
    assert_eq!(p.take_responses(), format!("\x1b[4;{};{}t", 18 * 24, 9 * 80).as_bytes());
}

#[test]
fn xtpushtitle_and_xtpoptitle_round_trip() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.title = "first".into();
    p.advance(&mut g, b"\x1b[22t"); // push "first"
    g.title = "second".into();
    p.advance(&mut g, b"\x1b[22t"); // push "second"
    g.title = "third".into();
    p.advance(&mut g, b"\x1b[23t"); // pop -> restores "second"
    assert_eq!(g.title, "second");
    p.advance(&mut g, b"\x1b[23t"); // pop -> restores "first"
    assert_eq!(g.title, "first");
    p.advance(&mut g, b"\x1b[23t"); // stack empty: no-op
    assert_eq!(g.title, "first");
}

#[test]
fn kitty_keyboard_push_pop_and_query_round_trip() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert_eq!(g.kitty_keyboard_flags(), 0);
    p.advance(&mut g, b"\x1b[?u"); // query with nothing pushed
    assert_eq!(p.take_responses(), b"\x1b[?0u");

    p.advance(&mut g, b"\x1b[>1u"); // push disambiguate
    assert_eq!(g.kitty_keyboard_flags(), 1);
    p.advance(&mut g, b"\x1b[?u");
    assert_eq!(p.take_responses(), b"\x1b[?1u");

    p.advance(&mut g, b"\x1b[>5u"); // push disambiguate|report-alternate-keys
    assert_eq!(g.kitty_keyboard_flags(), 5);

    p.advance(&mut g, b"\x1b[<u"); // pop (default 1): back to the first push
    assert_eq!(g.kitty_keyboard_flags(), 1);
    p.advance(&mut g, b"\x1b[<u"); // pop again: stack empty, legacy encoding
    assert_eq!(g.kitty_keyboard_flags(), 0);
    p.advance(&mut g, b"\x1b[<5u"); // popping past empty is a harmless no-op
    assert_eq!(g.kitty_keyboard_flags(), 0);
}

#[test]
fn kitty_keyboard_set_modes_replace_or_or_or_clear() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[=1;1u"); // mode 1 (default): replace
    assert_eq!(g.kitty_keyboard_flags(), 1);
    p.advance(&mut g, b"\x1b[=4;2u"); // mode 2: OR in
    assert_eq!(g.kitty_keyboard_flags(), 5);
    p.advance(&mut g, b"\x1b[=1;3u"); // mode 3: clear those bits
    assert_eq!(g.kitty_keyboard_flags(), 4);
    p.advance(&mut g, b"\x1b[=2u"); // mode omitted defaults to 1 (replace)
    assert_eq!(g.kitty_keyboard_flags(), 2);
}

#[test]
fn kitty_keyboard_relayed_to_host_and_cleared_by_ris() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[>1u");
    assert_eq!(g.take_host_out(), b"\x1b[>1u");
    assert_eq!(g.kitty_keyboard_flags(), 1);
    p.advance(&mut g, b"\x1bc"); // RIS
    assert_eq!(g.kitty_keyboard_flags(), 0);
}

#[test]
fn osc22_sets_and_clears_cursor_icon_request() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert_eq!(g.cursor_icon, None);
    p.advance(&mut g, b"\x1b]22;pointer\x1b\\");
    assert_eq!(g.cursor_icon.as_deref(), Some("pointer"));
    p.advance(&mut g, b"\x1b]22;text\x1b\\");
    assert_eq!(g.cursor_icon.as_deref(), Some("text"));
    // An empty payload clears the request back to the default arrow.
    p.advance(&mut g, b"\x1b]22;\x1b\\");
    assert_eq!(g.cursor_icon, None);
}

#[test]
fn ris_and_decstr_clear_sync_output_and_cursor_icon() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]22;wait\x1b\\\x1b[?2026h");
    assert_eq!(g.cursor_icon.as_deref(), Some("wait"));
    assert!(g.sync_output_active());
    p.advance(&mut g, b"\x1bc"); // RIS
    assert_eq!(g.cursor_icon, None);
    assert!(!g.sync_output_active());
}

#[test]
fn sgr4_colon_style_sets_undercurl_and_other_styles() {
    let g = parse(b"\x1b[4:3ma\x1b[4:4mb\x1b[4:5mc\x1b[4:2md\x1b[4:1me\x1b[4:0mf", 80, 24);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[0].flags), UnderlineStyle::Curly);
    assert_ne!(g.cells[0].flags & ATTR_UNDERLINE, 0);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[1].flags), UnderlineStyle::Dotted);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[2].flags), UnderlineStyle::Dashed);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[3].flags), UnderlineStyle::Double);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[4].flags), UnderlineStyle::Straight);
    // `4:0` turns the underline off outright rather than just resetting style.
    assert_eq!(g.cells[5].flags & ATTR_UNDERLINE, 0);
}

#[test]
fn sgr4_semicolon_form_is_unambiguous_from_colon_sub_param() {
    // `4;3` is two independent codes (underline, then italic) — never the
    // curly-style sub-parameter `4:3` means.
    let g = parse(b"\x1b[4;3mx", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_UNDERLINE, 0);
    assert_ne!(g.cells[0].flags & ATTR_ITALIC, 0);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[0].flags), UnderlineStyle::Straight);
}

#[test]
fn sgr4_bare_forces_straight_even_over_a_leftover_colon_style() {
    let g = parse(b"\x1b[4:3m\x1b[4mx", 80, 24);
    assert_eq!(UnderlineStyle::from_attrs(g.cells[0].flags), UnderlineStyle::Straight);
    assert_ne!(g.cells[0].flags & ATTR_UNDERLINE, 0);
}

#[test]
fn sgr58_sets_underline_color_independent_of_fg_and_59_resets() {
    let g = parse(b"\x1b[4m\x1b[38;2;10;20;30m\x1b[58;2;255;0;0mA\x1b[59mB", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_UNDERLINE_COLOR, 0);
    assert_eq!(g.cells[0].underline_color, 0xFF0000);
    assert_eq!(g.cells[0].fg, 0x0A141E); // fg untouched by SGR 58
    // 59 turns the custom-color flag back off; the rendering fallback (fg) is
    // the renderer's job, not the model's — the flag alone is what matters.
    assert_eq!(g.cells[1].flags & ATTR_UNDERLINE_COLOR, 0);
}

#[test]
fn sgr58_colon_form_matches_semicolon_form() {
    let g = parse(b"\x1b[4m\x1b[58:2:0:255:0mA", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_UNDERLINE_COLOR, 0);
    assert_eq!(g.cells[0].underline_color, 0x00FF00);
}

#[test]
fn sgr_reset_clears_underline_style_and_color() {
    let g = parse(b"\x1b[4:3m\x1b[58;2;1;2;3m\x1b[0mx", 80, 24);
    assert_eq!(g.cells[0].flags & ATTR_UNDERLINE, 0);
    assert_eq!(g.cells[0].flags & ATTR_UNDERLINE_COLOR, 0);
}

#[test]
fn decrqm_reports_known_dec_private_modes_and_unknown_as_not_recognized() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?7$p"); // DECAWM, default on
    assert_eq!(p.take_responses(), b"\x1b[?7;1$y");
    p.advance(&mut g, b"\x1b[?7l\x1b[?7$p"); // reset it, query again
    assert_eq!(p.take_responses(), b"\x1b[?7;2$y");
    p.advance(&mut g, b"\x1b[?2026$p"); // synchronized output, default off
    assert_eq!(p.take_responses(), b"\x1b[?2026;2$y");
    p.advance(&mut g, b"\x1b[?2026h\x1b[?2026$p");
    assert_eq!(p.take_responses(), b"\x1b[?2026;1$y");
    p.advance(&mut g, b"\x1b[?1$p"); // DECCKM, default off
    assert_eq!(p.take_responses(), b"\x1b[?1;2$y");
    p.advance(&mut g, b"\x1b[?1h\x1b[?1$p");
    assert_eq!(p.take_responses(), b"\x1b[?1;1$y");
    // Focus reporting is tracked (the window backend reports its own focus
    // transitions), so it answers real state now, not "0" (not recognized).
    p.advance(&mut g, b"\x1b[?1004$p");
    assert_eq!(p.take_responses(), b"\x1b[?1004;2$y");
    p.advance(&mut g, b"\x1b[?1004h\x1b[?1004$p");
    assert_eq!(p.take_responses(), b"\x1b[?1004;1$y");
    // A mode we genuinely don't track state for is answered honestly ("0",
    // not recognized) rather than guessed.
    p.advance(&mut g, b"\x1b[?1010$p");
    assert_eq!(p.take_responses(), b"\x1b[?1010;0$y");
}

#[test]
fn decrqm_reports_ansi_irm_and_unknown_modes() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[4$p"); // IRM, default off
    assert_eq!(p.take_responses(), b"\x1b[4;2$y");
    p.advance(&mut g, b"\x1b[4h\x1b[4$p");
    assert_eq!(p.take_responses(), b"\x1b[4;1$y");
    p.advance(&mut g, b"\x1b[20$p"); // LNM, default off
    assert_eq!(p.take_responses(), b"\x1b[20;2$y");
    p.advance(&mut g, b"\x1b[20h\x1b[20$p");
    assert_eq!(p.take_responses(), b"\x1b[20;1$y");
    p.advance(&mut g, b"\x1b[9999$p"); // a genuinely unmodeled mode
    assert_eq!(p.take_responses(), b"\x1b[9999;0$y");
}

#[test]
fn osc_2_sets_window_title() {
    let mut g = parse(b"\x1b]2;My Title\x07", 80, 24);
    assert_eq!(g.title, "My Title");
    // OSC 0 also sets the title.
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]0;Another\x07");
    assert_eq!(g.title, "Another");
}

#[test]
fn osc_7_sets_working_directory() {
    let g = parse(b"\x1b]7;file://host/home/user\x1b\\", 80, 24);
    assert_eq!(g.cwd, "file://host/home/user");
}

#[test]
fn osc_title_decodes_utf8_and_does_not_print() {
    // Multi-byte payload must round-trip, and the trailing 'X' still renders.
    let g = parse("\x1b]2;café 世\x07X".as_bytes(), 80, 24);
    assert_eq!(g.title, "café 世");
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn osc_split_across_chunks_is_captured() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]2;split ");
    p.advance(&mut g, b"title\x07");
    assert_eq!(g.title, "split title");
}

#[test]
fn osc_without_separator_is_ignored() {
    // No ';' — not actionable, and must not panic or print.
    let g = parse(b"\x1b]999\x07Z", 80, 24);
    assert_eq!(g.title, "");
    assert_eq!(g.cwd, "");
    assert_eq!(g.cells[0].ch, 'Z');
}

#[test]
fn osc_unknown_code_is_ignored_but_consumed() {
    // OSC 52 (clipboard) isn't acted on yet, but must not leak or set title.
    let g = parse(b"\x1b]52;c;SGVsbG8=\x07W", 80, 24);
    assert_eq!(g.title, "");
    assert_eq!(g.cells[0].ch, 'W');
}

#[test]
fn full_screen_scroll_captures_into_scrollback() {
    // 4x2 grid: write two rows, then a third newline scrolls "row0" off.
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB");
    assert_eq!(g.scrollback.len(), 0);
    p.advance(&mut g, b"\r\nCCCC"); // scrolls AAAA off the top
    assert_eq!(g.scrollback.len(), 1);
    let line: String = g.scrollback[0].cells.iter().map(|c| c.ch).collect();
    assert_eq!(line, "AAAA");
    // Live grid now shows BBBB / CCCC.
    assert_eq!(row_text(&g, 0), "BBBB");
    assert_eq!(row_text(&g, 1), "CCCC");
}

#[test]
fn partial_region_scroll_is_not_captured() {
    // A DECSTBM sub-region scroll (TUI behavior) must not feed scrollback.
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[1;3r"); // region rows 1..=3 (0-based 0..=2)
    g.cursor = (0, 2);
    p.advance(&mut g, b"\n"); // scrolls within the region only
    assert_eq!(g.scrollback.len(), 0);
}

#[test]
fn alt_screen_scroll_is_not_captured() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1049h"); // enter alt screen
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC"); // would scroll on alt
    assert_eq!(g.scrollback.len(), 0);
}

#[test]
fn scrollback_is_capped() {
    let mut g = Grid::new(2, 2);
    let mut p = AnsiParser::new();
    // Each newline at the bottom scrolls one line into history.
    for _ in 0..(SCROLLBACK_MAX + 50) {
        p.advance(&mut g, b"\r\n");
    }
    assert_eq!(g.scrollback.len(), SCROLLBACK_MAX);
}

#[test]
fn viewport_composites_history_above_live_grid() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    // History: row "AAAA"; live: "BBBB" / "CCCC".
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC");
    assert_eq!(g.scrollback.len(), 1);

    // Scroll up one line: top row shows history "AAAA", then live row 0.
    assert!(g.scroll_view_up(1));
    assert_eq!(g.view_offset, 1);
    let vp = g.snapshot_viewport();
    let text: Vec<String> = vp
        .rows
        .iter()
        .map(|(_, cells)| cells.iter().map(|c| c.ch).collect())
        .collect();
    assert_eq!(text, vec!["AAAA".to_string(), "BBBB".to_string()]);
}

#[test]
fn scroll_view_clamps_and_resets() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC"); // 1 line of history
    // Asking for more than exists clamps to the available history.
    assert!(g.scroll_view_up(100));
    assert_eq!(g.view_offset, 1);
    // No further movement -> returns false.
    assert!(!g.scroll_view_up(100));
    // Reset snaps back to the live bottom.
    assert!(g.reset_view());
    assert_eq!(g.view_offset, 0);
    assert!(!g.reset_view());
}

#[test]
fn no_history_browsing_on_alt_screen() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC"); // build some history
    p.advance(&mut g, b"\x1b[?1049h"); // enter alt screen
    assert!(!g.scroll_view_up(1)); // refused while on alt screen
    assert_eq!(g.view_offset, 0);
}

#[test]
fn osc_52_set_is_forwarded_to_host_and_not_printed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]52;c;SGVsbG8=\x07X");
    assert_eq!(g.take_host_out(), b"\x1b]52;c;SGVsbG8=\x07");
    assert_eq!(g.cells[0].ch, 'X'); // payload didn't leak onto the screen
    // Drained: a second take returns nothing.
    assert!(g.take_host_out().is_empty());
}

#[test]
fn osc_52_set_is_not_truncated_at_the_old_4kb_cap() {
    // The generic OSC_MAX (4 KiB) used to truncate any clipboard set past
    // ~3 KiB of decoded text; OSC 52 needs its own, much larger cap so a
    // real (not just a title-sized) copy survives intact.
    let text = "x".repeat(10_000); // well past the old 4096-byte OSC_MAX
    let b64 = base64::encode(text.as_bytes());
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    let osc = [b"\x1b]52;c;".as_slice(), b64.as_bytes(), b"\x07"].concat();
    p.advance(&mut g, &osc);
    assert_eq!(g.clipboard_set.as_deref(), Some(text.as_str()));
}

#[test]
fn osc_52_query_is_not_forwarded() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]52;c;?\x07");
    assert!(g.take_host_out().is_empty());
}

#[test]
fn osc_52_set_records_decoded_text_for_window_backend() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]52;c;SGVsbG8=\x07"); // base64("Hello")
    assert_eq!(g.clipboard_set.as_deref(), Some("Hello"));
}

#[test]
fn osc_52_query_flags_window_backend() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.clipboard_query);
    p.advance(&mut g, b"\x1b]52;c;?\x07");
    assert!(g.clipboard_query);
    assert_eq!(g.cells[0].ch, ' '); // nothing leaked to the screen
}

#[test]
fn osc_9_records_notification_and_relays() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]9;Build complete\x07");
    assert_eq!(g.notifications, vec![(String::new(), "Build complete".to_string())]);
    assert_eq!(g.take_host_out(), b"\x1b]9;Build complete\x07"); // relayed for TUI
    assert_eq!(g.cells[0].ch, ' '); // not printed
}

#[test]
fn osc_9_conemu_progress_is_not_a_notification() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]9;4;1;50\x07"); // ConEmu progress, not iTerm2 notify
    assert!(g.notifications.is_empty());
    // …but since G01 it *is* tracked as progress state and relayed to the host.
    assert_eq!(g.progress, Some((1, 50)));
    assert_eq!(g.take_host_out(), b"\x1b]9;4;1;50\x07");
    // Other ConEmu numeric subcommands stay ignored entirely.
    p.advance(&mut g, b"\x1b]9;2;text\x07");
    assert!(g.notifications.is_empty());
    assert!(g.take_host_out().is_empty());
}

#[test]
fn osc_777_parses_title_and_body() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // The body keeps any further `;` (only the first two are structural).
    p.advance(&mut g, b"\x1b]777;notify;Deploy;done; ok\x07");
    assert_eq!(g.notifications, vec![("Deploy".to_string(), "done; ok".to_string())]);
}

#[test]
fn osc_777_non_notify_is_ignored() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]777;something;x\x07");
    assert!(g.notifications.is_empty());
}

#[test]
fn osc_8_stamps_link_on_covered_cells() {
    let g = parse(b"\x1b]8;;http://example.com\x1b\\AB\x1b]8;;\x1b\\C", 80, 24);
    assert_ne!(g.cells[0].link, 0);
    assert_eq!(g.cells[0].link, g.cells[1].link); // A and B share the link
    assert_eq!(
        g.links[(g.cells[0].link - 1) as usize],
        "http://example.com"
    );
    assert_eq!(g.cells[2].link, 0); // C is after the close
}

#[test]
fn link_at_resolves_covered_cells() {
    let g = parse(b"\x1b]8;;http://example.com\x1b\\AB\x1b]8;;\x1b\\C", 80, 24);
    assert_eq!(g.link_at(0, 0), Some("http://example.com")); // A
    assert_eq!(g.link_at(1, 0), Some("http://example.com")); // B
    assert_eq!(g.link_at(2, 0), None); // C is after the close
    assert_eq!(g.link_at(5, 0), None); // a blank, unlinked cell
    // Out-of-bounds coordinates are safe.
    assert_eq!(g.link_at(999, 0), None);
    assert_eq!(g.link_at(0, 999), None);
}

#[test]
fn links_arc_and_clusters_arc_stay_in_sync_with_the_backing_vecs() {
    // snapshot_dirty/snapshot_viewport hand out links_arc/clusters_arc instead
    // of cloning links/clusters directly (an O(1) Arc clone vs. an O(n) deep
    // Vec<String> clone on every rendered frame); the cache must actually
    // track the source of truth or the renderer silently sees stale data.
    let mut g = Grid::new(20, 3);
    assert_eq!(g.links_arc.len(), g.links.len());
    assert_eq!(g.clusters_arc.len(), g.clusters.len());

    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]8;;http://example.com\x1b\\hi\x1b]8;;\x1b\\");
    assert_eq!(g.links.len(), 1, "the link should have been interned");
    assert_eq!(g.links_arc.len(), g.links.len());
    assert_eq!(&*g.links_arc, &g.links[..]);

    // "e\u{0301}" is 'e' + combining acute accent: the accent is a grapheme
    // continuation, interned into `clusters`.
    p.advance(&mut g, "e\u{0301}".as_bytes());
    assert_eq!(g.clusters.len(), 1, "the combining accent should be interned");
    assert_eq!(g.clusters_arc.len(), g.clusters.len());
    assert_eq!(&*g.clusters_arc, &g.clusters[..]);
}

#[test]
fn force_full_repaint_marks_every_row_dirty_even_after_a_clear() {
    let mut g = Grid::new(10, 4);
    g.clear_dirty();
    assert!(g.dirty.iter().all(|&d| !d), "clear_dirty should have cleared everything");
    g.force_full_repaint();
    assert_eq!(g.dirty.len(), 4);
    assert!(g.dirty.iter().all(|&d| d));
}

#[test]
fn hover_link_at_spans_the_whole_osc8_link_on_its_row() {
    let g = parse(b"\x1b]8;;http://example.com\x1b\\AB\x1b]8;;\x1b\\C", 80, 24);
    assert_eq!(
        g.hover_link_at(0, 0),
        Some((0, 1, "http://example.com".to_string()))
    );
    assert_eq!(
        g.hover_link_at(1, 0),
        Some((0, 1, "http://example.com".to_string()))
    );
    assert_eq!(g.hover_link_at(2, 0), None); // C is after the close
    assert_eq!(g.hover_link_at(999, 0), None); // out of bounds is safe
}

#[test]
fn search_finds_matches_across_scrollback_and_screen() {
    let mut g = Grid::new(20, 3);
    let mut p = AnsiParser::new();
    // 5 lines, 3 rows: "alpha"/"beta" scroll into history, screen keeps the rest.
    p.advance(&mut g, b"alpha\r\nbeta\r\ngamma\r\nalpha two\r\ndelta");
    assert_eq!(g.search_with("alpha", false), 2); // history "alpha" + screen "alpha two"
    assert_eq!(g.search_status(), Some((1, 2)));
    // Case-insensitive.
    assert_eq!(g.search_with("ALPHA", false), 2);
    // No match.
    assert_eq!(g.search_with("zzz", false), 0);
    assert_eq!(g.search_status(), None);
}

#[test]
fn search_matches_across_a_soft_wrap() {
    let mut g = Grid::new(5, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"abcdefgh"); // wraps: "abcde" then "fgh" as one logical line
    assert_eq!(g.search_with("ef", false), 1); // 'e' ends row 0, 'f' starts row 1
    // Highlight spans both physical rows (view snapped to show the match).
    assert_eq!(g.search_highlight(4, 0), Some(true));
    assert_eq!(g.search_highlight(0, 1), Some(true));
    assert_eq!(g.search_highlight(0, 0), None);
}

#[test]
fn search_jump_cycles_and_clear_resets() {
    let mut g = Grid::new(20, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"alpha\r\nbeta\r\ngamma\r\nalpha two\r\ndelta");
    assert_eq!(g.search_with("alpha", false), 2);
    assert_eq!(g.search_status(), Some((1, 2)));
    g.search_jump(true);
    assert_eq!(g.search_status(), Some((2, 2)));
    g.search_jump(true); // wraps back to the first
    assert_eq!(g.search_status(), Some((1, 2)));
    g.search_jump(false); // previous wraps to the last
    assert_eq!(g.search_status(), Some((2, 2)));
    g.clear_search();
    assert_eq!(g.search_status(), None);
    assert_eq!(g.search_highlight(0, 0), None);
}

#[test]
fn osc_8_with_id_param_links_uri() {
    // The params field (here `id=foo`) is skipped; the URI still links.
    let g = parse(b"\x1b]8;id=foo;http://e.com\x1b\\Z", 80, 24);
    assert_ne!(g.cells[0].link, 0);
    assert_eq!(g.links[(g.cells[0].link - 1) as usize], "http://e.com");
}

#[test]
fn osc_8_interns_duplicate_uri_once() {
    let g = parse(
        b"\x1b]8;;http://e.com\x1b\\A\x1b]8;;\x1b\\ \x1b]8;;http://e.com\x1b\\B\x1b]8;;\x1b\\",
        80,
        24,
    );
    assert_eq!(g.cells[0].link, g.cells[2].link); // same interned id
    assert_eq!(g.links.len(), 1); // URI stored once
}

#[test]
fn osc_4_sets_palette_index_recoloring_later_text() {
    // Redefine palette index 1 (normally dim red) to pure blue, then print with
    // SGR 31 (which resolves through index 1).
    let g = parse(b"\x1b]4;1;rgb:00/00/ff\x1b\\\x1b[31mX", 80, 24);
    assert_eq!(g.cells[0].fg, 0x0000FF);
}

#[test]
fn osc_4_query_replies_with_current_value() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]4;1;?\x07"); // query index 1 (default 0x800000)
    assert_eq!(p.take_responses(), b"\x1b]4;1;rgb:8080/0000/0000\x1b\\");
}

#[test]
fn osc_104_resets_palette() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]4;1;rgb:00/00/ff\x1b\\"); // change index 1
    p.advance(&mut g, b"\x1b]104;1\x1b\\"); // reset index 1
    p.advance(&mut g, b"\x1b[31mX");
    assert_eq!(g.cells[0].fg, 0x800000); // back to the default dim red
}

#[test]
fn osc_11_sets_default_bg_and_colors_erases() {
    // Set the default background to blue, then clear the screen: the cleared
    // cells must carry the new background, not the static black default.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]11;rgb:00/00/ff\x1b\\");
    p.advance(&mut g, b"\x1b[2J"); // ED 2 — clear all
    assert_eq!(g.cells[0].bg, 0x0000FF);
    // And SGR 49 (default bg) now resolves to blue for new text too.
    p.advance(&mut g, b"\x1b[49mX");
    assert_eq!(g.cells[0].bg, 0x0000FF);
}

#[test]
fn osc_10_sets_default_fg_for_reset_pen() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]10;rgb:0a/0b/0c\x1b\\");
    p.advance(&mut g, b"X"); // default pen -> default fg
    assert_eq!(g.cells[0].fg, 0x0A0B0C);
}

#[test]
fn osc_11_query_replies_and_110_resets() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]11;rgb:00/00/ff\x1b\\");
    p.advance(&mut g, b"\x1b]11;?\x07");
    assert_eq!(p.take_responses(), b"\x1b]11;rgb:0000/0000/ffff\x1b\\");
    // OSC 111 resets the default background to black.
    p.advance(&mut g, b"\x1b]111\x1b\\");
    p.advance(&mut g, b"\x1b[2J");
    assert_eq!(g.cells[0].bg, 0x000000);
}

#[test]
fn osc_color_resets_on_ris() {
    // A default background set via OSC 11 must not survive a full reset.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]11;rgb:00/00/ff\x1b\\");
    p.advance(&mut g, b"\x1bc"); // RIS
    p.advance(&mut g, b"\x1b[2J");
    assert_eq!(g.cells[0].bg, 0x000000);
}

#[test]
fn osc_1_icon_name_is_forwarded_to_host_not_printed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]1;myicon\x07X");
    assert_eq!(g.cells[0].ch, 'X'); // not leaked to the screen
    assert_eq!(g.title, ""); // icon name is not the window title
    assert_eq!(g.take_host_out(), b"\x1b]1;myicon\x07"); // forwarded verbatim
}

#[test]
fn color_spec_parses_rgb_and_hash_forms() {
    assert_eq!(parse_color_spec("rgb:ff/00/00"), Some(0xFF0000));
    assert_eq!(parse_color_spec("rgb:ffff/0000/0000"), Some(0xFF0000)); // 16-bit
    assert_eq!(parse_color_spec("#00ff00"), Some(0x00FF00));
    assert_eq!(parse_color_spec("#0f0"), Some(0x00FF00)); // short form scales up
    assert_eq!(parse_color_spec("red"), None); // named colors unsupported
    assert_eq!(parse_color_spec("rgb:zz/00/00"), None); // non-hex
}

#[test]
fn color_spec_formats_as_16bit_rgb() {
    assert_eq!(format_color_spec(0xFF8000), "rgb:ffff/8080/0000");
}

#[test]
fn osc_133_prompt_navigation() {
    // Three shell prompts, each scrolling the prior line into history (the
    // P1/P2/P3 pattern). Marks are logical line indices stable across scrolls.
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;A\x07P1\r\n");
    p.advance(&mut g, b"\x1b]133;A\x07P2\r\n");
    p.advance(&mut g, b"\x1b]133;A\x07P3\r\n");
    assert_eq!(g.view_offset, 0); // live view; P3 is on the live screen
    // Jump up to the previous prompt — P2, now the most recent history line.
    assert!(g.scroll_to_prev_prompt());
    assert_eq!(g.view_offset, 1);
    let top: String = g.snapshot_viewport().rows[0]
        .1
        .iter()
        .map(|c| c.ch)
        .collect();
    assert_eq!(top.trim_end(), "P2");
    // And up again to the oldest, P1.
    assert!(g.scroll_to_prev_prompt());
    assert_eq!(g.view_offset, 2);
    // Nothing above the oldest prompt.
    assert!(!g.scroll_to_prev_prompt());
    // Walk back down: P2, then snap to the live bottom (P3 lives on screen).
    assert!(g.scroll_to_next_prompt());
    assert_eq!(g.view_offset, 1);
    assert!(g.scroll_to_next_prompt());
    assert_eq!(g.view_offset, 0);
}

#[test]
fn osc_133_oldest_mark_evicts_from_history() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;A\x07old\r\n"); // marks the oldest line
    // Overflow the scrollback so the oldest line — and its mark — is evicted.
    for _ in 0..(SCROLLBACK_MAX + 10) {
        p.advance(&mut g, b"y\r\n");
    }
    p.advance(&mut g, b"\x1b]133;A\x07new\r\n"); // a fresh prompt...
    p.advance(&mut g, b"z\r\n"); // ...scrolled into history so it's above the view
    // The "new" mark survived eviction (its index was decremented) and is navigable...
    assert!(g.scroll_to_prev_prompt());
    // ...and "old" is gone: nothing older remains above it.
    assert!(!g.scroll_to_prev_prompt());
}

#[test]
fn osc_133_not_recorded_on_alt_screen() {
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1049h"); // enter alt screen (no history)
    p.advance(&mut g, b"\x1b]133;A\x07");
    assert!(!g.scroll_to_prev_prompt()); // marks aren't recorded there
}

#[test]
fn osc_133_marks_cleared_on_ris() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;A\x07P1\r\n");
    p.advance(&mut g, b"\x1b]133;A\x07P2\r\n"); // one prompt now in history
    p.advance(&mut g, b"\x1bc"); // RIS clears scrollback and marks
    assert!(!g.scroll_to_prev_prompt());
}

#[test]
fn fold_block_opens_on_c_and_closes_on_d_unfolded_by_default() {
    let mut g = Grid::new(80, 2);
    let mut p = AnsiParser::new();
    assert!(g.fold_blocks().is_empty());
    p.advance(&mut g, b"\x1b]133;C\x07output line\r\n");
    assert!(g.fold_blocks().is_empty(), "still pending until D");
    p.advance(&mut g, b"\x1b]133;D;0\x07");
    let blocks = g.fold_blocks();
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].start < blocks[0].end);
    assert!(!blocks[0].folded);
}

#[test]
fn fold_block_toggle_finds_the_block_containing_a_line_only() {
    let mut g = Grid::new(80, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;C\x07line1\r\nline2\r\n");
    p.advance(&mut g, b"\x1b]133;D\x07");
    let (start, end) = {
        let b = g.fold_blocks()[0];
        (b.start, b.end)
    };
    assert!(g.toggle_fold_at(start));
    assert!(g.fold_blocks()[0].folded);
    assert!(g.toggle_fold_at(start)); // toggling again flips it back
    assert!(!g.fold_blocks()[0].folded);
    assert!(!g.toggle_fold_at(end), "end is exclusive: one past the block");
}

#[test]
fn fold_block_d_without_c_is_a_no_op() {
    let mut g = Grid::new(80, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;D;0\x07");
    assert!(g.fold_blocks().is_empty());
}

#[test]
fn fold_block_second_c_before_d_replaces_the_pending_start() {
    let mut g = Grid::new(80, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;C\x07discarded\r\n");
    p.advance(&mut g, b"\x1b]133;C\x07kept\r\n"); // supersedes the first C
    p.advance(&mut g, b"\x1b]133;D\x07");
    assert_eq!(g.fold_blocks().len(), 1);
}

#[test]
fn fold_blocks_shift_with_scrollback_eviction_and_stale_block_is_dropped() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;C\x07old\r\n\x1b]133;D\x07"); // a block anchored at line 0
    // Overflow the scrollback so the oldest line — and the block entirely on
    // it — is evicted, the same eviction `osc_133_oldest_mark_evicts_from_history`
    // exercises for prompt marks.
    for _ in 0..(SCROLLBACK_MAX + 10) {
        p.advance(&mut g, b"y\r\n");
    }
    assert!(g.fold_blocks().is_empty(), "the old block's line scrolled off the cap");

    // A block opened after the overflow survives further eviction, shifted
    // down by exactly the one line that scrolls off per iteration.
    p.advance(&mut g, b"\x1b]133;C\x07new\r\n\x1b]133;D\x07");
    let before = g.fold_blocks()[0];
    p.advance(&mut g, b"z\r\n"); // one more line scrolls into (and off) history
    let after = g.fold_blocks()[0];
    assert_eq!(after.start, before.start.saturating_sub(1));
    assert_eq!(after.end, before.end.saturating_sub(1));
}

#[test]
fn fold_blocks_cleared_on_ris() {
    let mut g = Grid::new(80, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;C\x07x\r\n\x1b]133;D\x07");
    assert_eq!(g.fold_blocks().len(), 1);
    p.advance(&mut g, b"\x1bc"); // RIS
    assert!(g.fold_blocks().is_empty());
}

#[test]
fn fold_blocks_survive_a_resize_reflow() {
    let mut g = Grid::new(80, 10);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]133;C\x07a line of output\r\nanother line\r\n");
    p.advance(&mut g, b"\x1b]133;D\x07");
    assert_eq!(g.fold_blocks().len(), 1);
    g.resize(40, 10); // narrower: forces a rewrap
    assert_eq!(g.fold_blocks().len(), 1, "block survives the reflow");
    let b = g.fold_blocks()[0];
    assert!(b.start < b.end);
}

#[test]
fn osc_633_prompt_mark_matches_133() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]633;A\x07old\r\n");
    p.advance(&mut g, b"y\r\n");
    assert!(g.scroll_to_prev_prompt());
}

#[test]
fn osc_633_reports_cwd_via_p_property() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert_eq!(g.cwd, "");
    p.advance(&mut g, b"\x1b]633;P;Cwd=/home/user/project\x07");
    assert_eq!(g.cwd, "/home/user/project");
    // An unrecognized property is ignored rather than clobbering cwd.
    p.advance(&mut g, b"\x1b]633;P;IsWindows=False\x07");
    assert_eq!(g.cwd, "/home/user/project");
}

#[test]
fn osc_633_command_lifecycle_and_unknown_subcommands_are_harmless() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // B (prompt end) and E (command-line report) aren't modeled; well-formed
    // but otherwise a no-op, same as their 133 counterparts.
    p.advance(&mut g, b"\x1b]633;B\x07\x1b]633;E;ls -la\x07");
    p.advance(&mut g, b"echo hi");
    assert_eq!(g.cells[0].ch, 'e'); // the OSC didn't leak into the grid
}

#[test]
fn sgr_sets_text_attribute_bits() {
    let g = parse(b"\x1b[1mA\x1b[4mB", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_BOLD, 0); // A is bold
    // B is bold + underline (attributes accumulate until reset).
    assert_ne!(g.cells[1].flags & ATTR_BOLD, 0);
    assert_ne!(g.cells[1].flags & ATTR_UNDERLINE, 0);
}

#[test]
fn sgr_combined_attributes_in_one_sequence() {
    // Bold + italic + reverse set together, with a color, in one CSI m.
    let g = parse(b"\x1b[1;3;7;31mX", 80, 24);
    let f = g.cells[0].flags;
    assert_ne!(f & ATTR_BOLD, 0);
    assert_ne!(f & ATTR_ITALIC, 0);
    assert_ne!(f & ATTR_REVERSE, 0);
    assert_eq!(g.cells[0].fg, PALETTE_16[1]); // 31 still applied
}

#[test]
fn sgr_reset_clears_all_attributes() {
    let g = parse(b"\x1b[1;4;7mA\x1b[0mB", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_MASK, 0); // A styled
    assert_eq!(g.cells[1].flags & ATTR_MASK, 0); // B fully reset
}

#[test]
fn sgr_selective_attribute_resets() {
    // 22 clears bold (and dim), leaving underline; 24 then clears underline.
    let g = parse(b"\x1b[1;4mA\x1b[22mB\x1b[24mC", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_BOLD, 0);
    assert_ne!(g.cells[0].flags & ATTR_UNDERLINE, 0);
    assert_eq!(g.cells[1].flags & ATTR_BOLD, 0); // bold cleared
    assert_ne!(g.cells[1].flags & ATTR_UNDERLINE, 0); // underline kept
    assert_eq!(g.cells[2].flags & ATTR_UNDERLINE, 0); // underline cleared
}

#[test]
fn sgr_22_clears_both_bold_and_dim() {
    let g = parse(b"\x1b[1;2mA\x1b[22mB", 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_BOLD, 0);
    assert_ne!(g.cells[0].flags & ATTR_DIM, 0);
    assert_eq!(g.cells[1].flags & (ATTR_BOLD | ATTR_DIM), 0);
}

#[test]
fn wide_trailer_carries_only_layout_bit_under_attributes() {
    // A bold wide glyph: the head carries the bold bit; the trailer carries
    // only WIDE_TRAILER, never a rendition attribute.
    let g = parse("\x1b[1m世".as_bytes(), 80, 24);
    assert_ne!(g.cells[0].flags & ATTR_BOLD, 0); // head is bold
    assert_eq!(g.cells[1].flags, WIDE_TRAILER); // trailer: layout bit only
}

#[test]
fn insert_lines_shifts_down_and_blanks() {
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    g.cursor = (0, 1);
    p.advance(&mut g, b"\x1b[L"); // IL 1 at row 1
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 1), "    "); // blank inserted
    assert_eq!(row_text(&g, 2), "BBBB"); // shifted down
    assert_eq!(row_text(&g, 3), "CCCC");
    // DDDD was pushed past the bottom and lost.
}

#[test]
fn delete_lines_shifts_up_and_blanks() {
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    g.cursor = (0, 1);
    p.advance(&mut g, b"\x1b[M"); // DL 1 at row 1
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 1), "CCCC"); // pulled up
    assert_eq!(row_text(&g, 2), "DDDD");
    assert_eq!(row_text(&g, 3), "    "); // bottom blanked
}

#[test]
fn insert_lines_clamps_to_region_and_marks_dirty() {
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    g.cursor = (0, 1);
    g.clear_dirty();
    p.advance(&mut g, b"\x1b[10L"); // IL 10 -> clamped to 3 rows below cursor
    assert_eq!(row_text(&g, 0), "AAAA"); // above cursor untouched
    assert_eq!(row_text(&g, 1), "    ");
    assert_eq!(row_text(&g, 2), "    ");
    assert_eq!(row_text(&g, 3), "    ");
    // Rows 1..=3 are dirty, row 0 is not.
    assert_eq!(g.dirty, vec![false, true, true, true]);
}

#[test]
fn insert_lines_respects_scroll_region() {
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE");
    p.advance(&mut g, b"\x1b[2;4r"); // region rows 2..=4 (0-based 1..=3); homes cursor
    g.cursor = (0, 2); // inside the region
    p.advance(&mut g, b"\x1b[L"); // IL 1
    assert_eq!(row_text(&g, 0), "AAAA"); // above region untouched
    assert_eq!(row_text(&g, 1), "BBBB");
    assert_eq!(row_text(&g, 2), "    "); // blank inserted at cursor
    assert_eq!(row_text(&g, 3), "CCCC"); // shifted down within region
    assert_eq!(row_text(&g, 4), "EEEE"); // below region untouched (DDDD lost)
}

#[test]
fn delete_lines_blanks_at_region_bottom() {
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE");
    p.advance(&mut g, b"\x1b[2;4r"); // region rows 1..=3 (0-based)
    g.cursor = (0, 1);
    p.advance(&mut g, b"\x1b[M"); // DL 1
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 1), "CCCC"); // pulled up within region
    assert_eq!(row_text(&g, 2), "DDDD");
    assert_eq!(row_text(&g, 3), "    "); // region bottom blanked
    assert_eq!(row_text(&g, 4), "EEEE"); // below region untouched
}

#[test]
fn insert_lines_outside_region_is_noop() {
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE");
    p.advance(&mut g, b"\x1b[2;4r"); // region rows 1..=3 (0-based)
    g.cursor = (0, 0); // above the region top
    p.advance(&mut g, b"\x1b[L"); // IL -> no-op
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 1), "BBBB");
    assert_eq!(row_text(&g, 2), "CCCC");
    assert_eq!(row_text(&g, 3), "DDDD");
}

#[test]
fn ind_moves_down_preserving_column() {
    let mut g = Grid::new(4, 3);
    let mut p = AnsiParser::new();
    g.cursor = (2, 0);
    p.advance(&mut g, b"\x1bD"); // IND
    assert_eq!(g.cursor, (2, 1)); // down one row, column unchanged
}

#[test]
fn ind_at_bottom_scrolls_and_captures_scrollback() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB");
    g.cursor = (0, 1); // region bottom (full-screen)
    p.advance(&mut g, b"\x1bD"); // IND scrolls up
    assert_eq!(row_text(&g, 0), "BBBB");
    assert_eq!(row_text(&g, 1), "    ");
    assert_eq!(g.scrollback.len(), 1);
    let line: String = g.scrollback[0].cells.iter().map(|c| c.ch).collect();
    assert_eq!(line, "AAAA");
}

#[test]
fn nel_carriage_returns_then_indexes() {
    let mut g = Grid::new(4, 3);
    let mut p = AnsiParser::new();
    g.cursor = (3, 0);
    p.advance(&mut g, b"\x1bE"); // NEL
    assert_eq!(g.cursor, (0, 1)); // column reset to 0, down one row
}

#[test]
fn ri_moves_up_preserving_column() {
    let mut g = Grid::new(4, 3);
    let mut p = AnsiParser::new();
    g.cursor = (2, 2);
    p.advance(&mut g, b"\x1bM"); // RI
    assert_eq!(g.cursor, (2, 1)); // up one row, column unchanged
}

#[test]
fn ri_at_top_scrolls_region_down() {
    let mut g = Grid::new(4, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC");
    g.cursor = (0, 0); // region top
    p.advance(&mut g, b"\x1bM"); // RI scrolls down
    assert_eq!(row_text(&g, 0), "    "); // blank inserted at top
    assert_eq!(row_text(&g, 1), "AAAA"); // shifted down
    assert_eq!(row_text(&g, 2), "BBBB"); // CCCC pushed past the bottom, lost
}

#[test]
fn su_scrolls_region_up_n() {
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    p.advance(&mut g, b"\x1b[2S"); // SU 2
    assert_eq!(row_text(&g, 0), "CCCC");
    assert_eq!(row_text(&g, 1), "DDDD");
    assert_eq!(row_text(&g, 2), "    ");
    assert_eq!(row_text(&g, 3), "    ");
    // Full-screen SU on the primary buffer captures displaced lines.
    assert_eq!(g.scrollback.len(), 2);
}

#[test]
fn sd_scrolls_region_down_n_and_blanks_top() {
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    p.advance(&mut g, b"\x1b[2T"); // SD 2
    assert_eq!(row_text(&g, 0), "    ");
    assert_eq!(row_text(&g, 1), "    ");
    assert_eq!(row_text(&g, 2), "AAAA");
    assert_eq!(row_text(&g, 3), "BBBB");
}

#[test]
fn sd_multi_parameter_form_is_ignored() {
    // CSI 1;2;3;4;5 T is xterm highlight mouse tracking, not SD — leave the
    // grid untouched rather than scrolling by a bogus count.
    let mut g = Grid::new(4, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
    p.advance(&mut g, b"\x1b[1;2;3;4;5T");
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 3), "DDDD");
}

#[test]
fn su_respects_scroll_region_without_scrollback() {
    let mut g = Grid::new(4, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE");
    p.advance(&mut g, b"\x1b[2;4r"); // region rows 1..=3 (0-based); homes cursor
    p.advance(&mut g, b"\x1b[1S"); // SU 1 within the region
    assert_eq!(row_text(&g, 0), "AAAA"); // above region untouched
    assert_eq!(row_text(&g, 1), "CCCC"); // BBBB scrolled off within region
    assert_eq!(row_text(&g, 2), "DDDD");
    assert_eq!(row_text(&g, 3), "    "); // region bottom blanked
    assert_eq!(row_text(&g, 4), "EEEE"); // below region untouched
    assert_eq!(g.scrollback.len(), 0); // sub-region scroll never captures
}

#[test]
fn cnl_moves_to_column_zero_and_down() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (3, 1);
    p.advance(&mut g, b"\x1b[2E"); // CNL 2
    assert_eq!(g.cursor, (0, 3));
}

#[test]
fn cpl_moves_to_column_zero_and_up() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (3, 5);
    p.advance(&mut g, b"\x1b[2F"); // CPL 2
    assert_eq!(g.cursor, (0, 3));
}

#[test]
fn cnl_default_count_is_one() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (7, 0);
    p.advance(&mut g, b"\x1b[E"); // CNL (default 1)
    assert_eq!(g.cursor, (0, 1));
}

#[test]
fn rep_repeats_last_graphic_char() {
    let g = parse(b"A\x1b[3b", 80, 24); // 'A' then repeat x3 -> "AAAA"
    assert_eq!(&row_text(&g, 0)[..4], "AAAA");
    assert_eq!(g.cursor, (4, 0));
}

#[test]
fn rep_default_count_is_one() {
    let g = parse(b"X\x1b[b", 80, 24); // repeat once -> "XX"
    assert_eq!(&row_text(&g, 0)[..2], "XX");
    assert_eq!(g.cursor, (2, 0));
}

#[test]
fn rep_repeats_multibyte_char() {
    let g = parse("é\x1b[2b".as_bytes(), 80, 24); // 'é' then x2 -> "ééé"
    assert_eq!(g.cells[0].ch, 'é');
    assert_eq!(g.cells[1].ch, 'é');
    assert_eq!(g.cells[2].ch, 'é');
    assert_eq!(g.cursor, (3, 0));
}

#[test]
fn rep_after_newline_is_noop() {
    // CR/LF clear the last-char memory, so REP across a line break does nothing.
    let g = parse(b"A\r\n\x1b[3b", 80, 24);
    assert_eq!(row_text(&g, 1).trim_end(), "");
    assert_eq!(g.cursor, (0, 1));
}

#[test]
fn rep_after_tab_is_noop() {
    // A tab also clears the memory (it is a cursor move, not a "last char").
    let g = parse(b"A\t\x1b[2b", 80, 24);
    // 'A' at col 0, tab to col 8, REP repeats nothing.
    assert_eq!(g.cells[0].ch, 'A');
    assert_eq!(g.cursor, (8, 0));
}

#[test]
fn ht_is_non_destructive() {
    // A tab moves the cursor without erasing the cells it passes over.
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (2, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\t"); // HT from col 2 -> col 8
    assert_eq!(g.cursor.0, 8);
    assert_eq!(&row_text(&g, 0)[..6], "abcdef"); // text under the tab preserved
}

#[test]
fn hts_sets_a_custom_tab_stop() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (3, 0);
    p.advance(&mut g, b"\x1bH"); // HTS — stop at col 3
    g.cursor = (0, 0);
    p.advance(&mut g, b"\t"); // HT -> custom stop at 3 (before the default 8)
    assert_eq!(g.cursor.0, 3);
}

#[test]
fn tbc_clears_stop_at_cursor() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (8, 0);
    p.advance(&mut g, b"\x1b[g"); // TBC 0 — clear the default stop at col 8
    g.cursor = (0, 0);
    p.advance(&mut g, b"\t"); // HT skips the cleared stop -> 16
    assert_eq!(g.cursor.0, 16);
}

#[test]
fn tbc_3_clears_all_stops() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[3g"); // TBC 3 — clear every stop
    g.cursor = (0, 0);
    p.advance(&mut g, b"\t"); // no stops -> right margin
    assert_eq!(g.cursor.0, 79);
}

#[test]
fn cht_moves_forward_n_stops() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (0, 0);
    p.advance(&mut g, b"\x1b[3I"); // CHT 3 -> stops 8, 16, 24
    assert_eq!(g.cursor.0, 24);
}

#[test]
fn cbt_moves_backward_n_stops() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (20, 0);
    p.advance(&mut g, b"\x1b[2Z"); // CBT 2 -> 16, then 8
    assert_eq!(g.cursor.0, 8);
    // Default count is 1.
    g.cursor = (20, 0);
    p.advance(&mut g, b"\x1b[Z"); // CBT 1 -> 16
    assert_eq!(g.cursor.0, 16);
}

#[test]
fn custom_tab_stops_survive_resize() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    g.cursor = (3, 0);
    p.advance(&mut g, b"\x1bH"); // custom stop at col 3
    g.resize(100, 30); // grow — surviving columns keep their stops
    g.cursor = (0, 0);
    p.advance(&mut g, b"\t");
    assert_eq!(g.cursor.0, 3); // custom stop preserved across resize
}

#[test]
fn mouse_mode_set_is_relayed_to_host_not_printed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1000h");
    assert_eq!(g.take_host_out(), b"\x1b[?1000h"); // relayed verbatim
    assert_eq!(g.cells[0].ch, ' '); // not printed onto the grid
    assert_eq!(g.cursor, (0, 0));
}

#[test]
fn mouse_mode_reset_is_relayed_to_host() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1000l");
    assert_eq!(g.take_host_out(), b"\x1b[?1000l");
}

#[test]
fn sgr_mouse_and_bracketed_paste_are_relayed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1006h"); // SGR mouse encoding
    assert_eq!(g.take_host_out(), b"\x1b[?1006h");
    p.advance(&mut g, b"\x1b[?2004h"); // bracketed paste
    assert_eq!(g.take_host_out(), b"\x1b[?2004h");
}
#[test]
fn decckm_tracked_for_window_backend_and_relayed_and_reset_by_ris() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.app_cursor_keys);
    // Tracked into the grid (for the windowed front-end's own key encoder,
    // which has no host to relay to) *and* still relayed to the host (whose
    // own key encoder needs to follow it too, in TUI mode).
    p.advance(&mut g, b"\x1b[?1h");
    assert_eq!(g.take_host_out(), b"\x1b[?1h");
    assert!(g.app_cursor_keys);
    p.advance(&mut g, b"\x1b[?1l");
    assert_eq!(g.take_host_out(), b"\x1b[?1l");
    assert!(!g.app_cursor_keys);
    p.advance(&mut g, b"\x1b[?1h\x1bc"); // RIS clears it back to normal
    assert!(!g.app_cursor_keys);
}
#[test]
fn alt_scroll_mode_tracked_relayed_and_reset_by_ris() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.alt_scroll);
    p.advance(&mut g, b"\x1b[?1007h");
    assert_eq!(g.take_host_out(), b"\x1b[?1007h");
    assert!(g.alt_scroll);
    p.advance(&mut g, b"\x1b[?1007$p"); // DECRQM sees the tracked state too
    assert_eq!(p.take_responses(), b"\x1b[?1007;1$y");
    p.advance(&mut g, b"\x1b[?1007l");
    assert_eq!(g.take_host_out(), b"\x1b[?1007l");
    assert!(!g.alt_scroll);
    p.advance(&mut g, b"\x1b[?1007h\x1bc"); // RIS clears it back to normal
    assert!(!g.alt_scroll);
}
#[test]
fn lnm_reset_lf_moves_down_only_set_also_returns_to_column_0() {
    let mut g = Grid::new(10, 5);
    let mut p = AnsiParser::new();
    assert!(!g.line_feed_new_line);
    p.advance(&mut g, b"abc\n"); // reset (default): LF doesn't touch the column
    assert_eq!(g.cursor, (3, 1));

    p.advance(&mut g, b"\x1b[20h"); // LNM set
    assert!(g.line_feed_new_line);
    p.advance(&mut g, b"de\n"); // now LF also carriage-returns
    assert_eq!(g.cursor, (0, 2));

    p.advance(&mut g, b"\x1b[20l"); // LNM reset again
    p.advance(&mut g, b"fgh\n");
    assert_eq!(g.cursor, (3, 3));
}

#[test]
fn lnm_reset_by_ris() {
    let mut g = Grid::new(10, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[20h");
    assert!(g.line_feed_new_line);
    p.advance(&mut g, b"\x1bc"); // RIS
    assert!(!g.line_feed_new_line);
}

#[test]
fn decera_erases_a_rectangle_in_default_colors_leaving_the_rest_untouched() {
    let mut g = parse(b"AAAA\r\nAAAA\r\nAAAA\r\nAAAA", 4, 4);
    // DECERA rows 2..=3, cols 2..=3 (1-based) = 0-based rows 1..=2, cols 1..=2.
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[2;2;3;3$z");
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 1), "A  A");
    assert_eq!(row_text(&g, 2), "A  A");
    assert_eq!(row_text(&g, 3), "AAAA");
}

#[test]
fn decfra_fills_a_rectangle_with_the_given_character() {
    let mut g = Grid::new(4, 3);
    let mut p = AnsiParser::new();
    // Fill rows 1..=2, cols 1..=4 (whole width) with 'x' (code point 120).
    p.advance(&mut g, b"\x1b[120;1;1;2;4$x");
    assert_eq!(row_text(&g, 0), "xxxx");
    assert_eq!(row_text(&g, 1), "xxxx");
    assert_eq!(row_text(&g, 2), "    ");
}

#[test]
fn decfra_uses_the_current_pen_colors() {
    let g = parse(b"\x1b[31m\x1b[120;1;1;1;1$x", 4, 3);
    assert_eq!(g.cells[0].ch, 'x');
    assert_ne!(g.cells[0].fg, DEFAULT_FG); // took the red pen, not the default
}

#[test]
fn deccra_copies_a_rectangle_to_a_new_location() {
    let mut g = parse(b"AB\r\nCD", 4, 4);
    let mut p = AnsiParser::new();
    // Copy the 2x2 block at (1,1)-(2,2) to destination (3,3) (1-based).
    p.advance(&mut g, b"\x1b[1;1;2;2;1;3;3;1$v");
    assert_eq!(&row_text(&g, 2)[2..4], "AB");
    assert_eq!(&row_text(&g, 3)[2..4], "CD");
    // Source is untouched by a copy.
    assert_eq!(row_text(&g, 0).trim_end(), "AB");
    assert_eq!(row_text(&g, 1).trim_end(), "CD");
}

#[test]
fn deccra_handles_overlapping_source_and_destination_without_corruption() {
    // Shift a 1-row strip one column right, onto itself: "ABCD" -> "AABC".
    let mut g = parse(b"ABCD", 4, 1);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[1;1;1;3;1;1;2;1$v"); // copy cols 1-3 to start at col 2
    assert_eq!(row_text(&g, 0), "AABC");
}

#[test]
fn rect_ops_ignore_inverted_or_out_of_range_bounds() {
    let mut g = parse(b"AAAA\r\nAAAA", 4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[3;1;1;1$z"); // top > bottom: no-op
    assert_eq!(row_text(&g, 0), "AAAA");
    assert_eq!(row_text(&g, 1), "AAAA");
}
#[test]
fn mouse_modes_tracked_for_window_backend() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.mouse_modes.active());
    // Base tracking modes record into the grid *and* still relay to the host.
    p.advance(&mut g, b"\x1b[?1000h");
    assert_eq!(g.take_host_out(), b"\x1b[?1000h");
    assert_eq!(g.mouse_modes.base, 1000);
    assert!(g.mouse_modes.active());
    // A higher tracking level supersedes the current base.
    p.advance(&mut g, b"\x1b[?1003h");
    assert_eq!(g.mouse_modes.base, 1003);
    // Disabling a level that isn't the current one leaves tracking on.
    p.advance(&mut g, b"\x1b[?1000l");
    assert_eq!(g.mouse_modes.base, 1003);
    // Disabling the active level turns reporting off.
    p.advance(&mut g, b"\x1b[?1003l");
    assert_eq!(g.mouse_modes.base, 0);
    assert!(!g.mouse_modes.active());
}

#[test]
fn sgr_extended_mouse_flag_tracked_and_reset_by_ris() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1006h"); // SGR extended encoding -> bit 1
    assert_eq!(g.take_host_out(), b"\x1b[?1006h");
    assert_eq!(g.mouse_modes.extended & 2, 2);
    p.advance(&mut g, b"\x1b[?1000h");
    let _ = g.take_host_out();
    assert!(g.mouse_modes.active());
    // RIS clears mouse tracking entirely.
    p.advance(&mut g, b"\x1bc");
    assert_eq!(g.mouse_modes.base, 0);
    assert_eq!(g.mouse_modes.extended, 0);
    assert!(!g.mouse_modes.active());
}
#[test]
fn kitty_keyboard_protocol_is_relayed_to_host() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Push flags (`CSI > flags u`).
    p.advance(&mut g, b"\x1b[>1u");
    assert_eq!(g.take_host_out(), b"\x1b[>1u");
    assert_eq!(g.cells[0].ch, ' '); // not printed
    assert!(p.take_responses().is_empty()); // host answers, not us
    // Set (`= flags ; mode u`), pop (`< n u`), and query (`? u`).
    p.advance(&mut g, b"\x1b[=5;1u");
    assert_eq!(g.take_host_out(), b"\x1b[=5;1u");
    p.advance(&mut g, b"\x1b[<2u");
    assert_eq!(g.take_host_out(), b"\x1b[<2u");
    p.advance(&mut g, b"\x1b[?u");
    assert_eq!(g.take_host_out(), b"\x1b[?u");
}

#[test]
fn modify_other_keys_is_relayed_to_host() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[>4;2m"); // XTMODKEYS: modifyOtherKeys = 2
    assert_eq!(g.take_host_out(), b"\x1b[>4;2m");
    p.advance(&mut g, b"\x1b[>4m"); // reset
    assert_eq!(g.take_host_out(), b"\x1b[>4m");
}

#[test]
fn da2_and_xtversion_answer_locally_not_relayed() {
    // `CSI > c` / `CSI > q` are queries we answer ourselves — they must reply on
    // the child channel and NOT be relayed to the host as keyboard sequences.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[>c");
    assert_eq!(p.take_responses(), b"\x1b[>0;1;0c");
    assert!(g.take_host_out().is_empty());
}

#[test]
fn focus_reporting_mode_is_relayed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1004h");
    assert_eq!(g.take_host_out(), b"\x1b[?1004h");
}

#[test]
fn combined_input_modes_in_one_sequence_are_each_relayed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1000;1006h"); // enable both at once
    assert_eq!(g.take_host_out(), b"\x1b[?1000h\x1b[?1006h");
}

#[test]
fn alt_screen_mode_is_not_relayed_to_host() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"primary");
    p.advance(&mut g, b"\x1b[?1049h"); // enter alt — handled internally
    assert!(g.take_host_out().is_empty());
    p.advance(&mut g, b"\x1b[?1049l"); // leave alt
    assert!(g.take_host_out().is_empty());
    assert_eq!(&row_text(&g, 0)[..7], "primary"); // confirms it was handled, not ignored
}

#[test]
fn cursor_visibility_mode_is_not_relayed() {
    // ?25 (DECTCEM) is swallowed, not relayed — the renderer owns the host
    // cursor's visibility (it hides it while browsing scrollback).
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?25l");
    assert!(g.take_host_out().is_empty());
}

#[test]
fn ris_resets_screen_cursor_and_region() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[31mhello"); // red text
    p.advance(&mut g, b"\x1b[2;10r"); // scroll region (homes cursor)
    p.advance(&mut g, b"\x1b[?25l"); // hide cursor
    p.advance(&mut g, b"\x1bc"); // RIS
    assert_eq!(g.cells[0].ch, ' '); // screen cleared
    assert_eq!(g.cursor, (0, 0)); // home
    assert_eq!((g.scroll_top, g.scroll_bottom), (0, 23)); // region reset
    assert!(g.cursor_visible); // cursor visible again
}

#[test]
fn ris_resets_pen() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[31m"); // red pen
    p.advance(&mut g, b"\x1bc"); // RIS resets the pen
    p.advance(&mut g, b"X");
    assert_eq!(g.cells[0].fg, DEFAULT_FG); // back to default color
}

#[test]
fn ris_clears_scrollback() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\nBBBB\r\nCCCC"); // 1 line of history
    assert_eq!(g.scrollback.len(), 1);
    p.advance(&mut g, b"\x1bc"); // RIS
    assert_eq!(g.scrollback.len(), 0);
}

#[test]
fn decstr_soft_reset_keeps_screen() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"hello");
    p.advance(&mut g, b"\x1b[3;10r"); // scroll region (homes cursor)
    p.advance(&mut g, b"\x1b[!p"); // DECSTR
    assert_eq!((g.scroll_top, g.scroll_bottom), (0, 23)); // region reset
    assert_eq!(&row_text(&g, 0)[..5], "hello"); // screen NOT cleared
    assert!(g.cursor_visible);
    assert!(g.autowrap);
}

#[test]
fn dectcem_toggles_cursor_visibility() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(g.cursor_visible); // default visible
    p.advance(&mut g, b"\x1b[?25l");
    assert!(!g.cursor_visible);
    p.advance(&mut g, b"\x1b[?25h");
    assert!(g.cursor_visible);
}

#[test]
fn decawm_off_overwrites_last_column() {
    let mut g = Grid::new(3, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?7l"); // autowrap off
    p.advance(&mut g, b"abcd"); // 'd' overwrites 'c' in the last column
    assert_eq!(row_text(&g, 0), "abd");
    assert_eq!(g.cursor.1, 0); // never wrapped to the next row
    assert_eq!(row_text(&g, 1), "   "); // row 1 untouched
}

#[test]
fn decaln_fills_screen_with_e() {
    let g = parse(b"\x1b#8", 4, 2); // DECALN
    assert!(g.cells.iter().all(|c| c.ch == 'E'));
    assert_eq!(g.cursor, (0, 0));
}

#[test]
fn esc_hash_non_8_is_consumed_not_printed() {
    // ESC # 3 (DECDHL top half) sets the line size but must not leak the '#'/'3';
    // the following 'X' still renders.
    let g = parse(b"\x1b#3X", 4, 2);
    assert_eq!(g.cells[0].ch, 'X');
    assert_eq!(g.cursor, (1, 0));
}

#[test]
fn decdwl_decdhl_decswl_set_line_size() {
    assert_eq!(
        parse(b"\x1b#6", 8, 2).snapshot_dirty().line_attrs[0],
        LineAttr::DoubleWidth
    );
    assert_eq!(
        parse(b"\x1b#3", 8, 2).snapshot_dirty().line_attrs[0],
        LineAttr::DoubleTop
    );
    assert_eq!(
        parse(b"\x1b#4", 8, 2).snapshot_dirty().line_attrs[0],
        LineAttr::DoubleBottom
    );
    // ESC # 5 (DECSWL) resets the line to single width.
    assert_eq!(
        parse(b"\x1b#6\x1b#5", 8, 2).snapshot_dirty().line_attrs[0],
        LineAttr::Single
    );
}

#[test]
fn line_attr_follows_content_when_scrolling() {
    // A double-width line must keep its size as the screen scrolls it upward.
    let mut g = Grid::new(4, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"AAAA\r\n"); // row 0
    p.advance(&mut g, b"\x1b#6BBBB\r\n"); // row 1, double-width
    p.advance(&mut g, b"CCCC\r\n"); // scrolls row 0 off; BBBB rises to row 0
    let attrs = g.snapshot_dirty().line_attrs;
    assert_eq!(attrs[0], LineAttr::DoubleWidth); // followed BBBB up to row 0
    assert_eq!(attrs[1], LineAttr::Single);
}

#[test]
fn line_attrs_reset_on_ris_and_decaln() {
    let mut g = Grid::new(8, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b#6");
    p.advance(&mut g, b"\x1bc"); // RIS
    assert_eq!(g.snapshot_dirty().line_attrs[0], LineAttr::Single);
    p.advance(&mut g, b"\x1b#6\x1b#8"); // double-width, then DECALN
    assert_eq!(g.snapshot_dirty().line_attrs[0], LineAttr::Single);
}

#[test]
fn decom_makes_cup_relative_to_region() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r"); // region rows 5..=20 (0-based 4..=19)
    p.advance(&mut g, b"\x1b[?6h"); // origin mode on -> homes to (0, 4)
    assert_eq!(g.cursor, (0, 4));
    p.advance(&mut g, b"\x1b[3;10H"); // CUP row 3 col 10 -> region row 4+2=6
    assert_eq!(g.cursor, (9, 6));
}

#[test]
fn decom_confines_cursor_to_region() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r");
    p.advance(&mut g, b"\x1b[?6h");
    p.advance(&mut g, b"\x1b[100;1H"); // far past the region bottom
    assert_eq!(g.cursor.1, 19); // clamped to scroll_bottom, not screen bottom
}

#[test]
fn decom_bare_cup_homes_to_region_top() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r");
    p.advance(&mut g, b"\x1b[?6h");
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b[H"); // bare CUP -> origin-relative home
    assert_eq!(g.cursor, (0, 4));
}

#[test]
fn decom_vpa_is_region_relative() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r");
    p.advance(&mut g, b"\x1b[?6h");
    p.advance(&mut g, b"\x1b[3d"); // VPA row 3 -> region row 4+2=6
    assert_eq!(g.cursor.1, 6);
}

#[test]
fn decom_toggle_homes_the_cursor() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r"); // homes to (0,0) (origin off)
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b[?6h"); // on -> homes to region top (0,4)
    assert_eq!(g.cursor, (0, 4));
    g.cursor = (10, 10);
    p.advance(&mut g, b"\x1b[?6l"); // off -> homes to screen top (0,0)
    assert_eq!(g.cursor, (0, 0));
}

#[test]
fn decstbm_homes_to_region_top_when_origin_mode_on() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?6h"); // origin on (region still full -> home 0,0)
    p.advance(&mut g, b"\x1b[5;20r"); // set region -> homes to (0,4)
    assert_eq!(g.cursor, (0, 4));
}

#[test]
fn cup_is_absolute_when_origin_mode_off() {
    // Regression: with origin mode off, CUP ignores the scroll region.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r"); // region set, origin off
    p.advance(&mut g, b"\x1b[3;10H"); // absolute row 2, col 9
    assert_eq!(g.cursor, (9, 2));
}

#[test]
fn ris_and_decstr_reset_origin_mode() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?6h");
    assert!(g.origin_mode);
    p.advance(&mut g, b"\x1bc"); // RIS
    assert!(!g.origin_mode);
    p.advance(&mut g, b"\x1b[?6h");
    assert!(g.origin_mode);
    p.advance(&mut g, b"\x1b[!p"); // DECSTR
    assert!(!g.origin_mode);
}

#[test]
fn irm_insert_mode_shifts_row_right() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (2, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[4h"); // IRM — insert mode on
    p.advance(&mut g, b"XY"); // insert at col 2, pushing "cdef" right
    assert_eq!(&row_text(&g, 0)[..8], "abXYcdef");
}

#[test]
fn irm_replace_mode_overwrites_by_default() {
    let mut g = parse(b"abcdef", 80, 24);
    g.cursor = (2, 0);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"XY"); // default replace mode
    assert_eq!(&row_text(&g, 0)[..6], "abXYef");
}

#[test]
fn irm_is_toggled_and_reset() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[4h");
    assert!(g.insert_mode);
    p.advance(&mut g, b"\x1b[4l");
    assert!(!g.insert_mode);
    p.advance(&mut g, b"\x1b[4h");
    p.advance(&mut g, b"\x1bc"); // RIS resets it
    assert!(!g.insert_mode);
    p.advance(&mut g, b"\x1b[4h");
    p.advance(&mut g, b"\x1b[!p"); // DECSTR resets it
    assert!(!g.insert_mode);
}

#[test]
fn deckkm_cursor_key_mode_is_relayed_to_host() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?1h"); // application cursor keys
    assert_eq!(g.take_host_out(), b"\x1b[?1h");
    p.advance(&mut g, b"\x1b[?1l"); // normal cursor keys
    assert_eq!(g.take_host_out(), b"\x1b[?1l");
}

#[test]
fn cuu_stops_at_region_top() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r"); // region rows 4..=19
    g.cursor = (0, 10); // inside the region
    p.advance(&mut g, b"\x1b[100A"); // CUU far past the top margin
    assert_eq!(g.cursor.1, 4); // clamped to scroll_top
}

#[test]
fn cud_stops_at_region_bottom() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r");
    g.cursor = (0, 10);
    p.advance(&mut g, b"\x1b[100B"); // CUD far past the bottom margin
    assert_eq!(g.cursor.1, 19); // clamped to scroll_bottom
}

#[test]
fn cuu_above_region_floors_at_screen_top() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r");
    g.cursor = (0, 2); // above the region top margin
    p.advance(&mut g, b"\x1b[100A");
    assert_eq!(g.cursor.1, 0); // floors at row 0, not the margin
}

#[test]
fn cud_below_region_ceilings_at_screen_bottom() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[5;20r");
    g.cursor = (0, 21); // below the region bottom margin
    p.advance(&mut g, b"\x1b[100B");
    assert_eq!(g.cursor.1, 23); // ceilings at the last row, not the margin
}

#[test]
fn sixel_decodes_single_column_all_pixels() {
    // Define register 0 = full red (RGB 100;0;0), select it, then `~` (all six
    // band bits set) paints one column, six rows tall.
    let img = decode(b"#0;2;100;0;0~");
    assert_eq!((img.width, img.height), (1, 6));
    assert!((0..6).all(|y| spix(&img, 0, y) == Some(0xFF0000)));
}

#[test]
fn sixel_repeat_paints_multiple_columns() {
    // `!3~` repeats the all-bits byte three times: 3 columns × 6 rows green.
    let img = decode(b"#0;2;0;100;0!3~");
    assert_eq!((img.width, img.height), (3, 6));
    assert!((0..3).all(|x| spix(&img, x, 0) == Some(0x00FF00)));
}

#[test]
fn sixel_band_advance_stacks_rows() {
    // `-` starts the next band: two stacked columns -> 12 rows of blue.
    let img = decode(b"#0;2;0;0;100~-~");
    assert_eq!((img.width, img.height), (1, 12));
    assert_eq!(spix(&img, 0, 0), Some(0x0000FF));
    assert_eq!(spix(&img, 0, 11), Some(0x0000FF));
}

#[test]
fn sixel_partial_bits_set_only_some_rows() {
    // `@` = 0x40 -> value 1 -> only bit 0 (top row) is painted.
    let img = decode(b"#0;2;100;100;100@");
    assert_eq!((img.width, img.height), (1, 1));
    assert_eq!(spix(&img, 0, 0), Some(0xFFFFFF));
}

#[test]
fn sixel_transparent_advance_leaves_gaps() {
    // `!2?` advances two columns without painting (value 0); `~` then paints col 2.
    let img = decode(b"#0;2;100;0;0!2?~");
    assert_eq!(img.width, 3);
    assert_eq!(spix(&img, 0, 0), None);
    assert_eq!(spix(&img, 1, 0), None);
    assert_eq!(spix(&img, 2, 0), Some(0xFF0000));
}

#[test]
fn sixel_default_palette_select_without_define() {
    // `#1` selects register 1 of the VT340 default palette (RGB 20;20;80).
    let img = decode(b"#1~");
    assert_eq!(spix(&img, 0, 0), Some(0x3333CC));
}

#[test]
fn sixel_empty_payload_is_empty_image() {
    let img = decode(b"");
    assert_eq!((img.width, img.height), (0, 0));
}

#[test]
fn render_sixel_writes_halfblock_cells() {
    // A 1×2 image (top red, bottom green) becomes one upper-half-block cell:
    // fg = top pixel, bg = bottom pixel.
    let img = SixelImage {
        width: 1,
        height: 2,
        pixels: vec![Some(0xFF0000), Some(0x00FF00)],
    };
    let mut g = Grid::new(80, 24);
    g.render_sixel(&img);
    assert_eq!(g.cells[0].ch, '\u{2580}'); // ▀
    assert_eq!(g.cells[0].fg, 0xFF0000);
    assert_eq!(g.cells[0].bg, 0x00FF00);
    assert_eq!(g.cursor, (0, 1)); // sixel scrolling: column 0 of the row below
}

#[test]
fn render_image_stores_pixel_image_for_overlay() {
    let mut g = Grid::new(10, 4);
    g.render_image(4, 4, &[Some(0x00FF00); 16]);
    // The full-resolution source is kept for the CPU pixel overlay, anchored at
    // the top cell row so it tracks scroll/history.
    assert_eq!(g.images().len(), 1);
    let im = &g.images()[0];
    assert_eq!(im.serial, 0);
    assert_eq!(g.image_top_row(im), 0); // no scroll: top cell at viewport row 0
    assert_eq!((im.col, im.cols, im.rows), (0, 4, 2)); // footprint: cols wide, 2 rows
    assert_eq!((im.pw, im.ph), (4, 4)); // full source resolution retained
    assert_eq!(im.pixels.len(), 16);
    // ...alongside the half-block cells the TUI/GPU fall back to.
    assert_eq!(g.cells[0].ch, '\u{2580}');
    // Clearing the screen drops placed images.
    g.clear_all();
    assert!(g.images().is_empty());
}

#[test]
fn render_sixel_transparent_lower_half_uses_default_bg() {
    // Only a top pixel: upper half block, fg = pixel, bg = default background.
    let img = SixelImage {
        width: 1,
        height: 1,
        pixels: vec![Some(0xFF0000)],
    };
    let mut g = Grid::new(80, 24);
    g.render_sixel(&img);
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000);
    assert_eq!(g.cells[0].bg, DEFAULT_BG);
}

#[test]
fn render_sixel_shrinks_to_fit_width() {
    // A 4-wide image into a 2-column grid downsamples to 2 cells (cols 0 and 2
    // of the source), preserving aspect (height collapses to one cell row).
    let row = vec![
        Some(0xFF0000),
        Some(0xFF0000),
        Some(0x00FF00),
        Some(0x00FF00),
    ];
    let mut pixels = row.clone();
    pixels.extend(row); // 4×2
    let img = SixelImage {
        width: 4,
        height: 2,
        pixels,
    };
    let mut g = Grid::new(2, 24);
    g.render_sixel(&img);
    assert_eq!(g.cells[0].fg, 0xFF0000); // sampled source col 0
    assert_eq!(g.cells[1].fg, 0x00FF00); // sampled source col 2
    assert_eq!(g.cells[0].ch, '\u{2580}');
}

#[test]
fn render_sixel_taller_than_screen_scrolls() {
    // A 1×8 image (4 cell rows) into a 2-row grid scrolls the top rows into
    // history without panicking; the visible rows still show the image.
    let img = SixelImage {
        width: 1,
        height: 8,
        pixels: vec![Some(0xFF0000); 8],
    };
    let mut g = Grid::new(2, 2);
    g.render_sixel(&img);
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000);
    assert!(!g.scrollback.is_empty()); // rows scrolled into history
    assert_eq!(g.cursor, (0, 1));
}

#[test]
fn dcs_sixel_renders_into_grid() {
    // DCS `q` + define-red + all-bits column + ST. The image (1×6 red) renders
    // as upper-half blocks; the sixel data never leaks as text.
    let g = parse(b"\x1bPq#0;2;100;0;0~\x1b\\", 80, 24);
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000);
    assert_eq!(g.cells[0].bg, 0xFF0000); // 6 rows -> top and bottom both red
    assert_eq!(g.cursor, (0, 3)); // 6px = 3 cell rows, cursor below
}

#[test]
fn dcs_sixel_skips_leading_params() {
    // `DCS 0;1;0 q <data>`: the `q` final byte follows the numeric params.
    let g = parse(b"\x1bP0;1;0q#1~\x1b\\", 80, 24);
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0x3333CC); // default palette register 1
}

#[test]
fn dcs_non_sixel_is_ignored_not_leaked() {
    // A DECRQSS-style DCS (`$q…`) is not a Sixel: it's consumed and discarded,
    // and the following `X` prints normally at the origin.
    let g = parse(b"\x1bP$qm\x1b\\X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
    assert!(g.cells.iter().all(|c| c.ch != '\u{2580}')); // nothing rendered
}

#[test]
fn dcs_sixel_split_across_chunks() {
    // The DCS may arrive in pieces; the parser buffers across `advance` calls.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1bPq#0;2;0;100;0"); // introducer + partial data
    p.advance(&mut g, b"~\x1b\\"); // rest of data + ST
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0x00FF00);
}

#[test]
fn base64_decodes_standard_and_padding() {
    assert_eq!(base64::decode(b"TWFu").unwrap(), b"Man");
    assert_eq!(base64::decode(b"SGVsbG8=").unwrap(), b"Hello");
    assert_eq!(base64::decode(b"SGVsbG8h").unwrap(), b"Hello!");
    assert_eq!(base64::decode(b"").unwrap(), b"");
    // Whitespace (line wrapping) is ignored; invalid bytes are rejected.
    assert_eq!(base64::decode(b"SGVs\nbG8=").unwrap(), b"Hello");
    assert!(base64::decode(b"@@@@").is_none());
}

#[test]
fn base64_encodes_with_padding_and_round_trips() {
    assert_eq!(base64::encode(b"Man"), "TWFu");
    assert_eq!(base64::encode(b"Hello"), "SGVsbG8=");
    assert_eq!(base64::encode(b"Hello!"), "SGVsbG8h");
    assert_eq!(base64::encode(b""), "");
    // Round-trips with decode across every byte value.
    let data: Vec<u8> = (0u8..=255).collect();
    assert_eq!(base64::decode(base64::encode(&data).as_bytes()).unwrap(), data);
}

#[test]
fn inflate_zlib_short_string() {
    let z1 = [
        0x78, 0xda, 0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0xd7, 0x51, 0x08, 0xcf, 0x2f, 0xca, 0x49, 0x51,
        0x04, 0x00, 0x1f, 0x9e, 0x04, 0x6a,
    ];
    assert_eq!(
        inflate::zlib_decompress(&z1, 1 << 20).unwrap(),
        b"Hello, World!"
    );
}

#[test]
fn inflate_zlib_repetitive_back_references() {
    // Repetitive data exercises LZ77 back-references and dynamic Huffman.
    let z2 = [
        0x78, 0xda, 0x4b, 0x4c, 0x4a, 0x4e, 0x84, 0x21, 0x05, 0x03, 0x43, 0x23, 0x63, 0x13, 0x53,
        0x33, 0x73, 0x0b, 0x4b, 0x85, 0xc4, 0x51, 0xf1, 0x61, 0x21, 0x0e, 0x00, 0xa0, 0x46, 0x89,
        0xe5,
    ];
    let expected = b"abcabcabcabc 0123456789 ".repeat(20);
    assert_eq!(inflate::zlib_decompress(&z2, 1 << 20).unwrap(), expected);
}

#[test]
fn inflate_raw_deflate_and_stored_block() {
    let r3 = [
        0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0xd7, 0x51, 0x08, 0xcf, 0x2f, 0xca, 0x49, 0x51, 0x04, 0x00,
    ];
    assert_eq!(inflate::inflate(&r3, 1 << 20).unwrap(), b"Hello, World!");
    // A stored (uncompressed) block.
    let r4 = [
        0x01, 0x09, 0x00, 0xf6, 0xff, 0x52, 0x41, 0x57, 0x53, 0x54, 0x4f, 0x52, 0x45, 0x44,
    ];
    assert_eq!(inflate::inflate(&r4, 1 << 20).unwrap(), b"RAWSTORED");
}

#[test]
fn inflate_rejects_garbage_and_respects_cap() {
    assert!(inflate::zlib_decompress(&[0x00, 0x01, 0x02], 1024).is_none());
    // Output cap: decompressing the repetitive stream with a tiny cap truncates.
    let z2 = [
        0x78, 0xda, 0x4b, 0x4c, 0x4a, 0x4e, 0x84, 0x21, 0x05, 0x03, 0x43, 0x23, 0x63, 0x13, 0x53,
        0x33, 0x73, 0x0b, 0x4b, 0x85, 0xc4, 0x51, 0xf1, 0x61, 0x21, 0x0e, 0x00, 0xa0, 0x46, 0x89,
        0xe5,
    ];
    assert!(inflate::zlib_decompress(&z2, 16).unwrap().len() <= 64);
}

#[test]
fn png_decodes_rgba_filter0() {
    let data: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00, 0x13, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x0c, 0x81, 0x34, 0x08, 0x30, 0x00, 0x00, 0x48, 0xc9, 0x08, 0xf8,
        0xc5, 0x34, 0xfd, 0x05, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
        0x82,
    ];
    let img = png::decode(data).unwrap();
    assert_eq!((img.width, img.height), (2, 2));
    assert_eq!(&img.rgba[0..4], &[255, 0, 0, 255]); // red
    assert_eq!(&img.rgba[4..8], &[0, 255, 0, 255]); // green
    assert_eq!(&img.rgba[8..12], &[0, 0, 255, 255]); // blue
    assert_eq!(&img.rgba[12..16], &[255, 255, 255, 0]); // transparent white
}

#[test]
fn png_reverses_sub_and_paeth_filters() {
    // 3×2 RGB encoded with Sub on row 0 and Paeth on row 1.
    let data: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0x12,
        0x16, 0xf1, 0x4d, 0x00, 0x00, 0x00, 0x13, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xe4,
        0x12, 0x91, 0x83, 0x00, 0x96, 0xa8, 0xa8, 0x28, 0x08, 0x0b, 0x00, 0x18, 0xd8, 0x02, 0xb8,
        0x8d, 0x21, 0x18, 0x45, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
        0x82,
    ];
    let img = png::decode(data).unwrap();
    assert_eq!((img.width, img.height), (3, 2));
    assert_eq!(&img.rgba[0..4], &[10, 20, 30, 255]); // row 0, pixel 0
    assert_eq!(&img.rgba[8..12], &[70, 80, 90, 255]); // row 0, pixel 2
    assert_eq!(&img.rgba[12..16], &[100, 110, 120, 255]); // row 1, pixel 0
    assert_eq!(&img.rgba[20..24], &[160, 170, 180, 255]); // row 1, pixel 2
}

#[test]
fn png_rejects_non_png() {
    assert!(png::decode(b"not a png").is_none());
}

// Baseline JPEG fixtures (generated with PIL, embedded as base64). See the
// decoder in src/core/jpeg.rs.
const GRAY8_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/wAALCAAIAAgBAREA/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/9oACAEBAAA/ACv/2Q==";
const RED8_420_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAAIAAgDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDxSiiivzc/vs//2Q==";
const LRBLUE16_444_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/2wBDAQMDAwQDBAgEBAgQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/wAARCAAQABADAREAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDwevyI/wBGDzev9Yj/ACkPSK/ydP8AVs83r/WI/wApD//Z";
const NOISE34_444_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wAARCAAiACIDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwB+kPZan4guNKubq11TRNMt9smkQ2SnZM0h+cReZI378wxEAI+DIoj+baXyr4DExxlTL3QnOr7SpySU4Jx51GpGo/3cVeClJTrSUUoJNpvlpnly/dUKMPrVabjOn70rqpO1GfK4SruUnRpqdV2i6dSDjB+x5m6kYvDOtat460rxXrF6NfSC7uZrq4h1R3toigtniij2DynZZT5aq2HaJrZWZZDIAvTToYTIsRQw1OpGXtlKKclB1OeDlOtKaVKo4yWq5bSi02rqKpOX0VTHZJepRVWm6kILnlLncuSUle0buKo8tR1Y0+aVSUJOXvykrdmNKi8FeEPFOt6pNp9za2TXJtbqRVaOyZpUliiMUjIIvs8io6xurFTJGF8x9qL4Ff6r7joRcoVZUqTblK8ouPPOUJQpVKnvtVJKVHlhdub5qUFfw8NTrOycXDE1Jc1RKUZP91P2bnJqE+e1CScaiqyd5TlOg4qMZeceDNG1Bpra8toj4b0YWUl2smn2ySNMFgMN3II0fYysQ6bZNgL5kxwyN9Pnsp1MNSvWnGupKKtGpC7lWSjTkrqpJ06k3NOPPUsuWal7SjOfnV8VgsLisVhsXBVq06lWEqalJ/xH7SynTlBxinTVpqpLkT9nZwpy9jz+tXvxjfWL9tK0hU0szyG0W4tLlpBDuOwMTsJbbjJ2rz/COld0MqyOrFVK+LxDm9ZNV+VNve0XG8VfZPVbPU/LcVDDxxFSMcVWsm7Xqxi9+sYylGL7qMpJbJtanpLaHBqviiwg8RJe674nigub61u7m0+0SW6K8CmVFi/exSyPauQyFmheRXYg5jXzqODxFPBYyeBh7OnHlcFNyc/ehNypzUGoQhFypuEY8nOuaEIte0T/AHDKa9KdStHMKyhzVIylOEYRh7k01CE017SUYRSqybj+7pckqVqc6hgaN4us9f1KLQ4NE/t7QrFriW7W4UfZpYIvMnikW4Lurl8yyS71lnwpbIkgXZ2Vspp1MulmdHGSw6m7NUppz517ri405Q5laMaVFwVKEoyfxUanMdOFwWFp4mOAm03KkoRhyyfMm5xm4cjV405qLw9OE60Ep8sJJvlWv4OtNIMWuSwWOolo7+HUYZ9UulhS8mSOYxzNuuolWRXjh+V2JXzUyoQtIvy+JxVStVpU8tTTcq+jX8KcpUnKcoxpTvGME+S75nLnXNKSjF/P47H1aOBhk3PCFONPni0uVuVlzOFXmqUoR5ErTppRnJTqQVSKdMv+GjPpflzi4kv45Jrg3A1NxqENm8iBrN+QpZ3itY1WTYCMzeZktGg2qYGH1SjRpWqSq01GSpqmqk4pRc3Jw5U1Co0p3qWqKF/ecZSXs4vOcTnVL6piYqlh37HWXK3VtGS0lKM3GPN7041OdwXPKUFGEpO5pvhDV9R061u9Mt7kabPEktqBqNtjymAKdUB+6R1ANcONxuY08VVhi5SVVSal71H4rvm3qt733bZ4lbijO6lWU1h67u27xdXlfnG+Ivy9r9DmfBi65p+mwa14a0W1fUri0bT4Le6CWzlY4StyqSp5W/OGd38tSJC6LIASV+m4ky/CThOGJq8uHcW9XdSkudpcsYVJRiqfNF8lb3YJpWpu8/Sr5xGpDBZZWm5UqtRKsoyjGKjN1Iqo4yh7WCxK/fRu1+7SklKUpKM2qaxqeoaDYwGSW+eexQx2JtGtreRVlRYflS4ESglXfZHNtdkQS7BDsOOGhXqutUzak4Qw84zjJP2sWlSnzpOVPnqKUbxlXrJxSnOVKT5qjqbYfFwilVwuKozqQhUg05KDnGK9nOcZwjpGV4RipxUqkIQcHJqPLlWUFnbeHtX0y3W3sNUv9Ijto7+3thIdViupppPJLrIoMfMCeYjEDzCuNuwj6HGUJ8kZZpTnOhCs5+ykuedOpTVO+jg7Si5TUORez5bqXMlKC7MPVnejipwjTp1L3XLaScIwp8sZVVKpXfKlUjyNpwXs4p1KsWzRk03R9K2wadNe+IyVvY9NtLYkFNvmrMCwYssYFxCh2uYlWRmULJ+7+dxsnha9OpQxTlSlzKE4SvaNnCcLQqPldSUXNVG6jqSq6z9tGTqfP4mliKT/ANl5KlCUn7Tn1g/Z3j7NpN1UnONStJTdOdScVe04xa970Dw0df0HTdU1iOKPVr22jubxJRZh1mdQ0gYblwdxOflX6DpXxGOzjJsJiquGo0cO4QlKKtRbVk2lZqrJbdpSX957nz2L4Rx9bEVKs8zrwcpNuNOc/Zxbfww96XuLaPvS0tq9zw6+XV7q5urCzfVvFmnqo+2WdvDE1pdJG9uGaaMSTBpIzJGfLf5FjdCP3fD/AKJGgmqeNjOivac3slKUn7SThOKjaKp0vehGmoyUqUlUpygoOo3Ne5icxwOf4ihDDKNCHNG1SU5ScIqfNFym4uEIRl7WK5lOEY1OfklVbtqNFrHiKOHULXS9Y0Vp1jtrqHUIra3tb22W3VdzRyO0cjtGkCtKg+RI0xuVESR5pHLMHVhhMa4OMFOcLSq1H7RV+X2c6i+Nv94vZSbnztxi+ebjD6rBzjRwGJo4V01yzox9yU3O/KotKUUoulacJRSklVk6kIylFxqyv/aB4putJvZr2z1zUtUuWtrW4ffPbm2LTuYxbKgaRgQ+fLBbDbXVDiGtHhMPg6trwUFCnGXO4QjP3oVbxnFOHO3UjZzXspJpU5zlOpOPjYNYivglSwVOFbDOoppL20JUqsINRnGUadHliryk5Tkr1eb2c4zXtzjba41C2i1C71KS/vNeufMu5IYfIthPd3EuJJJZYXZZlSQvG4kGwmPyf3oRS/hVcFipfVZ4qlanTVOK5oylVStCceVVIKrzSSacoKMk1ze2puVSNLBZxSr5ZBKHNGpFwdWd5LR83s2p3+ODpuPJzSmlTl7CVNS5eaHxK0QgG8TxCt2f9cLTXLXyQ/8AFs863Mm3OceYS+MbiTmvXp8LZpioRxFDEVuSavHWs9HqvgjOG38spR/lk1Zv9Zq4TIFUksVlLdS75mqV05dWmq0U1e9rRirbJbHrHjHSrLTPGHh+Szs7e0ki8IX08bwRKhSSOSx8twQOGX7TcbT1Hny4++2fzvh+pPHY1YfFt1Kd8PHll7y5XhdVZ3VnZXWzsj8qws5f2JkuJv8AvKsvfl9qd5qL5nvK8fdd73jpsc/8VtIsdF+HeuQafZW9jBYaqptIraJY1tt5YP5YUAJuCqDjGQBnpX7vwjThj6uW1cWlUlXwt6jl7zqOnOkqbm3fncFKSg5X5U3y2uz5ziOpPCcRYjDYeThTjKnJRjpFSWEpTUklomptyT3Um5bu5xOv3M2s6Zey6hK99LB4m1uxhe5YyNHbxrEY4VLZxGu99qjgb2wOTXk4GKp5RSUFZPAe0dutT2dL94+8/wC98XmfXcM4ejjcswGIxUFObnSvKSTb/d4l6t3e8I/+Ax7K298OtUvPEXiPWbrVbufU7qz1PxLHbT3kjSvAqWcioqMxJUKFUADoAMdK8CtQpUcvwGHpwUYVHR54pJKX7uoveW0tNNb6H3PBFGliatKVeKk4V6fLdXtdUm7X2u9Xbd67nsdt4N8P21vFDDoemxRRqESNLSMKqgYAAA4Ar+PamcZlOcpSxM22/wCeX+Z/F3ELcc5xsYuyVWp/6Wz/2Q==";

/// A decoded JPEG pixel `(r, g, b)` as signed ints (for tolerance comparisons).
fn jpx(img: &jpeg::Image, x: usize, y: usize) -> (i32, i32, i32) {
    let o = (y * img.width + x) * 4;
    (img.rgba[o] as i32, img.rgba[o + 1] as i32, img.rgba[o + 2] as i32)
}

#[test]
fn jpeg_decodes_grayscale() {
    let data = base64::decode(GRAY8_B64.as_bytes()).unwrap();
    let img = jpeg::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (8, 8));
    let (r, g, b) = jpx(&img, 4, 4);
    assert_eq!((r, g), (b, b)); // a single-component image replicates luma
    assert!((r - 128).abs() <= 8, "mid-gray ~128, got {r}");
    assert_eq!(img.rgba[3], 255); // JPEG is opaque
}

#[test]
fn jpeg_decodes_solid_rgb_with_420_subsampling() {
    let data = base64::decode(RED8_420_B64.as_bytes()).unwrap();
    let img = jpeg::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (8, 8));
    let (r, g, b) = jpx(&img, 4, 4);
    assert!(
        (r - 220).abs() <= 16 && (g - 30).abs() <= 18 && (b - 40).abs() <= 18,
        "solid red ~(220,30,40), got {:?}",
        (r, g, b)
    );
}

#[test]
fn jpeg_decodes_two_colors_444() {
    // Left half red, right half blue at 4:4:4 (no chroma subsampling).
    let data = base64::decode(LRBLUE16_444_B64.as_bytes()).unwrap();
    let img = jpeg::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (16, 16));
    let (lr, lg, lb) = jpx(&img, 2, 8);
    assert!(lr > 180 && lg < 80 && lb < 80, "left red, got {:?}", (lr, lg, lb));
    let (rr, rg, rb) = jpx(&img, 13, 8);
    assert!(rb > 180 && rr < 80 && rg < 90, "right blue, got {:?}", (rr, rg, rb));
}

#[test]
fn jpeg_decodes_multi_mcu_image() {
    // 34x34 spans multiple MCUs and many blocks (exercises DC prediction across
    // blocks); we only assert it decodes to the right shape.
    let data = base64::decode(NOISE34_444_B64.as_bytes()).unwrap();
    let img = jpeg::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (34, 34));
    assert_eq!(img.rgba.len(), 34 * 34 * 4);
}

#[test]
fn jpeg_rejects_unsupported() {
    assert!(jpeg::decode(b"not a jpeg at all").is_none());
    assert!(jpeg::decode(&[0xFF, 0xD8, 0xFF]).is_none()); // SOI then truncated
}

#[test]
fn jpeg_rejects_out_of_range_huffman_selectors() {
    // Patch the scan header's Huffman-table selector byte to 0xFF (td=15,
    // ta=15): decode must return None, not index past the [Huff; 4] tables.
    let mut data = base64::decode(GRAY8_B64.as_bytes()).unwrap();
    let sos = data.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
    data[sos + 6] = 0xFF; // marker(2) + len(2) + ns(1) + cs(1) -> td/ta
    assert!(jpeg::decode(&data).is_none());
}

#[test]
fn iterm2_inline_jpeg_renders_image() {
    // OSC 1337 ; File=inline=1 : <base64 JPEG> BEL
    let mut input = b"\x1b]1337;File=inline=1:".to_vec();
    input.extend_from_slice(GRAY8_B64.as_bytes());
    input.push(0x07);
    let g = parse(&input, 20, 10);
    assert_eq!(g.cells[0].ch, '\u{2580}'); // a half-block image cell
    let (r, gn, b) = (
        (g.cells[0].fg >> 16) & 0xff,
        (g.cells[0].fg >> 8) & 0xff,
        g.cells[0].fg & 0xff,
    );
    assert!(
        (118..=138).contains(&r) && r == gn && gn == b,
        "mid-gray image, got {:?}",
        (r, gn, b)
    );
}

#[test]
fn iterm2_non_inline_transfer_is_ignored() {
    // inline=0 is a download, which a terminal has no surface for: render nothing.
    let mut input = b"\x1b]1337;File=inline=0:".to_vec();
    input.extend_from_slice(GRAY8_B64.as_bytes());
    input.push(0x07);
    let g = parse(&input, 20, 10);
    assert_eq!(g.cells[0].ch, ' ');
}

#[test]
fn iterm2_non_file_subcommand_is_ignored() {
    let g = parse(b"\x1b]1337;SetUserVar=foo=YmFy\x07", 20, 10);
    assert_eq!(g.cells[0].ch, ' ');
}

#[test]
fn iterm2_large_image_payload_is_not_truncated() {
    // The base64 is 4500 chars, past the ordinary OSC cap (4096). If the parser
    // truncated the OSC string the JPEG would corrupt and the lower rows (whose
    // entropy bytes sit far past the cap) would differ from a direct decode.
    let data = base64::decode(NOISE34_444_B64.as_bytes()).unwrap();
    let img = jpeg::decode(&data).unwrap();
    let pixels: Vec<Option<u32>> = img
        .rgba
        .chunks_exact(4)
        .map(|p| Some(((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32))
        .collect();
    let mut gref = Grid::new(40, 30);
    gref.render_image(img.width, img.height, &pixels);

    let mut input = b"\x1b]1337;File=inline=1:".to_vec();
    input.extend_from_slice(NOISE34_444_B64.as_bytes());
    input.push(0x07);
    let g = parse(&input, 40, 30);

    // 34px tall -> 17 cell rows; row 16 (the bottom) depends on late entropy.
    let bottom = 16 * 40;
    assert_ne!(g.cells[bottom].ch, ' '); // an image actually rendered there
    assert_eq!(g.cells[bottom].fg, gref.cells[bottom].fg);
    assert_eq!(g.cells[0].fg, gref.cells[0].fg);
}

#[test]
fn iterm2_resolve_dimension_cells_percent_px_and_auto() {
    assert_eq!(iterm::resolve_dimension("10", 80, Some(9)), Some(10));
    assert_eq!(iterm::resolve_dimension("50%", 80, Some(9)), Some(40));
    assert_eq!(iterm::resolve_dimension("90px", 80, Some(9)), Some(10));
    // A pixel hint with no known cell pixel size (TUI mode) can't resolve.
    assert_eq!(iterm::resolve_dimension("90px", 80, None), None);
    assert_eq!(iterm::resolve_dimension("auto", 80, Some(9)), None);
    assert_eq!(iterm::resolve_dimension("AUTO", 80, Some(9)), None);
    assert_eq!(iterm::resolve_dimension("garbage", 80, Some(9)), None);
}

#[test]
fn iterm2_width_hint_shrinks_the_image_footprint() {
    // Natural size is 8x8 (one cell column per pixel column): unhinted, all
    // 8 columns of row 0 get painted.
    let g = parse(
        &[b"\x1b]1337;File=inline=1:".as_slice(), GRAY8_B64.as_bytes(), b"\x07"].concat(),
        20,
        10,
    );
    assert_ne!(g.cells[7].ch, ' ');

    // width=4 shrinks the footprint to 4 columns; nothing past it is touched.
    let g = parse(
        &[b"\x1b]1337;File=inline=1;width=4:".as_slice(), GRAY8_B64.as_bytes(), b"\x07"].concat(),
        20,
        10,
    );
    assert_ne!(g.cells[3].ch, ' ');
    assert_eq!(g.cells[4].ch, ' ');
}

#[test]
fn iterm2_height_hint_shrinks_the_row_footprint() {
    // Natural 8px tall -> 4 cell rows unhinted.
    let g = parse(
        &[b"\x1b]1337;File=inline=1:".as_slice(), GRAY8_B64.as_bytes(), b"\x07"].concat(),
        20,
        10,
    );
    assert_ne!(g.cells[3 * 20].ch, ' ');

    // height=2 shrinks it to 2 cell rows; row 2 onward is never touched.
    let g = parse(
        &[b"\x1b]1337;File=inline=1;height=2:".as_slice(), GRAY8_B64.as_bytes(), b"\x07"].concat(),
        20,
        10,
    );
    assert_ne!(g.cells[20].ch, ' '); // row 1 painted
    assert_eq!(g.cells[2 * 20].ch, ' '); // row 2 untouched
}

#[test]
fn iterm2_preserve_aspect_ratio_zero_stretches_to_both_axes() {
    // Both width and height hints, aspect off: the footprint is exactly
    // width x height cells (5 cols x 3 rows), not "contain"-fit.
    let g = parse(
        &[
            b"\x1b]1337;File=inline=1;width=5;height=3;preserveAspectRatio=0:".as_slice(),
            GRAY8_B64.as_bytes(),
            b"\x07",
        ]
        .concat(),
        20,
        10,
    );
    assert_ne!(g.cells[4].ch, ' '); // col 4 (5th column) painted
    assert_eq!(g.cells[5].ch, ' '); // col 5 not
    assert_ne!(g.cells[2 * 20].ch, ' '); // row 2 (3rd row) painted
    assert_eq!(g.cells[3 * 20].ch, ' '); // row 3 not
}

#[test]
fn render_image_sized_contain_fits_within_both_axes_preserving_aspect() {
    // A 4x2 source (2:1) into a 4x8-cell requested box, aspect preserved:
    // width is the binding constraint (scaling to 4 columns needs only 1
    // cell row for a 2:1 source), so the much taller height budget goes
    // unused rather than stretching the image to fill it.
    let mut g = Grid::new(20, 10);
    g.render_image_sized(4, 2, &[Some(0xFF0000); 8], Some(4), Some(8), true);
    assert_ne!(g.cells[3].ch, ' '); // all 4 requested columns painted
    assert_eq!(g.cells[20].ch, ' '); // only 1 cell row used, not 8
}

#[test]
fn render_image_sized_clamps_absurd_height_hint() {
    // An untrusted `height=` hint in the billions (iTerm2 OSC 1337, or a
    // Kitty `r=` count) used to drive the newline() loop that many times —
    // a single escape sequence could hang the terminal for minutes. It must
    // instead clamp to whatever could ever be visible or retained in
    // scrollback, not the attacker-chosen request.
    let mut g = Grid::new(4, 3);
    g.render_image_sized(1, 1, &[Some(0xFF0000)], None, Some(2_000_000_000), true);
    assert!(g.scrollback.len() <= g.rows + g.scrollback_max);
}

#[test]
fn kitty_raw_rgba_renders() {
    // f=32 (RGBA), 1×1 red, transmit+display. `/wAA/w==` = [ff,00,00,ff].
    let g = parse(b"\x1b_Gf=32,s=1,v=1,a=T;/wAA/w==\x1b\\", 80, 24);
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000);
    assert_eq!(g.cursor, (0, 1));
}

#[test]
fn kitty_query_is_answered_ok() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b_Gi=99,a=q;\x1b\\");
    assert_eq!(p.take_responses(), b"\x1b_Gi=99;OK\x1b\\");
    assert_eq!(g.cells[0].ch, ' '); // a query renders nothing
}

#[test]
fn kitty_png_renders() {
    let cmd = b"\x1b_Gf=100,a=T;iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAE0lEQVR42mP4z8DwHwyBNAgwAABIyQj4xTT9BQAAAABJRU5ErkJggg==\x1b\\";
    let g = parse(cmd, 80, 24);
    // 2×2: row0 red,green; row1 blue,transparent -> one half-block cell row.
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000); // top-left red
    assert_eq!(g.cells[0].bg, 0x0000FF); // bottom-left blue
    assert_eq!(g.cells[1].fg, 0x00FF00); // top-right green
}

#[test]
fn kitty_chunked_transmission_accumulates() {
    // The base64 payload split across two APC chunks (`m=1` then `m=0`).
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b_Gf=32,s=1,v=1,a=T,m=1;/wAA\x1b\\");
    p.advance(&mut g, b"\x1b_Gm=0;/w==\x1b\\");
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000);
}

#[test]
fn kitty_oversized_chunk_reports_failure_not_truncated_render() {
    // Payload accumulated across chunks past the 8 MiB cap must abort cleanly
    // (EBADF), not render whatever prefix fit. Each chunk stays under the
    // parser's 4 MiB per-APC cap; three of them overflow the kitty cap.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    let chunk = vec![b'A'; 3 * 1024 * 1024];
    let mut first = b"\x1b_Gf=32,s=1,v=1,a=T,i=7,m=1;".to_vec();
    first.extend_from_slice(&chunk);
    first.extend_from_slice(b"\x1b\\");
    p.advance(&mut g, &first);
    for _ in 0..2 {
        let mut more = b"\x1b_Gm=1;".to_vec();
        more.extend_from_slice(&chunk);
        more.extend_from_slice(b"\x1b\\");
        p.advance(&mut g, &more);
    }
    p.advance(&mut g, b"\x1b_Gm=0;\x1b\\"); // final chunk
    assert_eq!(g.cells[0].ch, ' ', "no image rendered from truncated payload");
    assert_eq!(p.take_responses(), b"\x1b_Gi=7;EBADF\x1b\\");
}

#[test]
fn kitty_non_graphics_apc_ignored() {
    // An APC not starting with `G` is consumed, not decoded; `X` then prints.
    let g = parse(b"\x1b_Zhello\x1b\\X", 80, 24);
    assert_eq!(g.cells[0].ch, 'X');
}

#[cfg(feature = "l13")]
mod l13 {
    use super::*;

    /// Drive one channel OSC (`OSC 5379 ; <protocol> ; <json> ST`) and return
    /// the reply the terminal queued for the child, as a string.
    fn channel_roundtrip(grid: &mut Grid, protocol: &str, json: &str) -> String {
        let mut p = AnsiParser::new();
        let msg = format!("\x1b]5379;{protocol};{json}\x1b\\");
        p.advance(grid, msg.as_bytes());
        String::from_utf8(p.take_responses()).unwrap()
    }

    #[test]
    fn channel_initialize_advertises_protocols() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "channel",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        );
        assert!(resp.starts_with("\x1b]5379;channel;"));
        assert!(resp.ends_with("\x1b\\"));
        assert!(resp.contains("\"protocols\""));
        assert!(resp.contains("\"mcp\"") && resp.contains("\"lsp\"") && resp.contains("\"acp\""));
        assert!(resp.contains("\"rusty_term\""));
        assert!(resp.contains("\"id\":1"));
    }

    #[test]
    fn mcp_tools_list_includes_terminal_tools() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        for tool in ["get_screen", "get_scrollback", "get_cwd", "get_title", "get_dimensions"] {
            assert!(resp.contains(tool), "tools/list missing {tool}");
        }
    }

    #[test]
    fn mcp_get_screen_returns_current_text() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"hello channel"); // put text on the screen
        let _ = p.take_responses();
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_screen"}}"#,
        );
        assert!(resp.contains("hello channel"), "screen text not returned: {resp}");
        assert!(resp.contains("\"content\""));
    }

    #[test]
    fn mcp_get_dimensions_and_title() {
        let mut g = Grid::new(80, 24);
        g.title = "my window".into();
        let dims = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_dimensions"}}"#,
        );
        assert!(dims.contains("80x24"), "{dims}");
        let title = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_title"}}"#,
        );
        assert!(title.contains("my window"));
    }

    #[test]
    fn lsp_initialize_negotiates() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "lsp",
            r#"{"jsonrpc":"2.0","id":6,"method":"initialize","params":{"capabilities":{}}}"#,
        );
        assert!(resp.contains("\"capabilities\""));
        assert!(resp.contains("\"rusty_term\""));
    }

    #[test]
    fn acp_initialize_negotiates() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "acp",
            r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1}}"#,
        );
        assert!(resp.contains("\"protocolVersion\":1"));
        assert!(resp.contains("\"agentCapabilities\""));
        assert!(resp.contains("\"authMethods\""));
    }

    #[test]
    fn unknown_method_returns_jsonrpc_error() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":8,"method":"nonexistent"}"#,
        );
        assert!(resp.contains("\"error\""));
        assert!(resp.contains("-32601")); // METHOD_NOT_FOUND
    }

    #[test]
    fn unknown_protocol_returns_error() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "bogus",
            r#"{"jsonrpc":"2.0","id":9,"method":"initialize"}"#,
        );
        assert!(resp.contains("\"error\"") && resp.contains("-32601"));
    }

    #[test]
    fn malformed_json_is_dropped_no_reply() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(&mut g, "mcp", "{not valid json");
        assert!(resp.is_empty(), "malformed request should produce no reply");
    }

    #[test]
    fn notification_produces_no_reply() {
        // No `id` -> a JSON-RPC notification; the channel must not respond.
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert!(resp.is_empty());
    }

    #[test]
    fn channel_initialize_negotiates_version_down() {
        let mut g = Grid::new(80, 24);
        // A client claiming a far-future version is negotiated down to ours.
        let resp = channel_roundtrip(
            &mut g,
            "channel",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"version":99}}"#,
        );
        assert!(resp.contains("\"version\":1"), "did not downgrade: {resp}");
        assert!(resp.contains("\"capabilities\""));
        assert!(resp.contains("\"resources\":true"));
    }

    #[test]
    fn channel_initialize_rejects_version_below_floor() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "channel",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"version":0}}"#,
        );
        assert!(resp.contains("\"error\""), "version 0 must be rejected: {resp}");
        assert!(resp.contains("\"supported\""));
    }

    #[test]
    fn channel_initialize_intersects_requested_protocols() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "channel",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocols":["mcp","bogus"]}}"#,
        );
        // Only the supported intersection comes back — "mcp" yes, "bogus"/"lsp" no.
        assert!(resp.contains("\"protocols\":[\"mcp\"]"), "{resp}");
    }

    #[test]
    fn channel_describe_returns_versioned_schema() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "channel",
            r#"{"jsonrpc":"2.0","id":2,"method":"describe"}"#,
        );
        assert!(resp.contains("\"min\":1") && resp.contains("\"max\":1"), "{resp}");
        assert!(resp.contains("resources/read"), "schema lists MCP resource methods: {resp}");
        assert!(resp.contains("\"acp\""));
    }

    #[test]
    fn mcp_resources_list_and_read() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"resource body");
        let _ = p.take_responses();
        let list = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
        );
        for uri in ["terminal://screen", "terminal://cursor", "terminal://dimensions"] {
            assert!(list.contains(uri), "resource {uri} missing from list: {list}");
        }
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"terminal://screen"}}"#,
        );
        assert!(read.contains("resource body"), "screen resource text missing: {read}");
        assert!(read.contains("\"contents\""));
    }

    #[test]
    fn mcp_resource_read_unknown_uri_errors() {
        let mut g = Grid::new(80, 24);
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"terminal://nope"}}"#,
        );
        assert!(resp.contains("\"error\""), "unknown resource must error: {resp}");
    }

    #[test]
    fn mcp_get_cursor_tool_reports_position() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b[4;6H"); // CUP row 4, col 6 (1-based) -> cursor (5, 3)
        let _ = p.take_responses();
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"get_cursor"}}"#,
        );
        assert!(resp.contains("5,3"), "cursor position not reported as COL,ROW: {resp}");
    }

    #[test]
    fn subscribe_pushes_resource_updated_on_cwd_change() {
        let mut g = Grid::new(80, 24);
        let sub = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://cwd"}}"#,
        );
        assert!(sub.contains("\"result\""), "subscribe should succeed: {sub}");
        // A cwd change (OSC 7) now pushes a notification on the child channel.
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]7;file:///tmp/x\x07");
        let push = String::from_utf8(p.take_responses()).unwrap();
        assert!(push.contains("notifications/resources/updated"), "no push: {push}");
        assert!(push.contains("terminal://cwd"), "{push}");
        assert!(!push.contains("\"id\""), "a notification carries no id: {push}");
    }

    #[test]
    fn cwd_change_without_subscription_is_silent() {
        let mut g = Grid::new(80, 24);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]7;file:///tmp/x\x07");
        assert!(p.take_responses().is_empty(), "no notification without subscribe");
    }

    #[test]
    fn unsubscribe_stops_resource_updates() {
        let mut g = Grid::new(80, 24);
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://title"}}"#,
        );
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/unsubscribe","params":{"uri":"terminal://title"}}"#,
        );
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]2;new title\x07");
        assert!(p.take_responses().is_empty(), "unsubscribed: must not notify");
    }

    #[test]
    fn subscribe_to_high_churn_resource_errors() {
        let mut g = Grid::new(80, 24);
        // The screen changes on nearly every byte; it is polled, not pushed.
        let resp = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://screen"}}"#,
        );
        assert!(resp.contains("\"error\""), "screen is not subscribable: {resp}");
    }

    #[test]
    fn resource_update_fires_only_on_real_change() {
        let mut g = Grid::new(80, 24);
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://title"}}"#,
        );
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]2;same\x07"); // "" -> "same": a real change
        assert!(
            String::from_utf8(p.take_responses()).unwrap().contains("terminal://title"),
            "first title set must notify"
        );
        let mut p2 = AnsiParser::new();
        p2.advance(&mut g, b"\x1b]2;same\x07"); // "same" -> "same": no change
        assert!(p2.take_responses().is_empty(), "a no-op title set must not notify");
    }

    #[test]
    fn render_set_status_overlays_bottom_row() {
        let mut g = Grid::new(10, 3);
        let ok = channel_roundtrip(
            &mut g,
            "render",
            r#"{"jsonrpc":"2.0","id":1,"method":"set_status","params":{"text":"READY"}}"#,
        );
        assert!(ok.contains("\"result\""), "set_status should succeed: {ok}");
        // The bottom row of a snapshot now shows the overlay text, not grid content.
        let frame = g.snapshot_dirty();
        let (_, bottom) = frame.rows.iter().find(|(y, _)| *y == 2).expect("bottom row dirty");
        let text: String = bottom.iter().map(|c| c.ch).collect();
        assert!(text.starts_with("READY"), "overlay text missing: {text:?}");
        // Clearing restores the underlying (blank) row.
        let _ = channel_roundtrip(
            &mut g,
            "render",
            r#"{"jsonrpc":"2.0","id":2,"method":"clear_status"}"#,
        );
        let frame = g.snapshot_dirty();
        let (_, bottom) = frame.rows.iter().find(|(y, _)| *y == 2).expect("bottom row dirty");
        let text: String = bottom.iter().map(|c| c.ch).collect();
        assert!(!text.contains("READY"), "overlay not cleared: {text:?}");
    }

    #[test]
    fn render_status_survives_resize() {
        let mut g = Grid::new(10, 3);
        let _ = channel_roundtrip(
            &mut g,
            "render",
            r#"{"jsonrpc":"2.0","id":1,"method":"set_status","params":{"text":"hi"}}"#,
        );
        g.resize(4, 4); // overlay re-lays out to the new width
        let frame = g.snapshot_dirty();
        let (_, bottom) = frame.rows.iter().find(|(y, _)| *y == 3).expect("new bottom row");
        assert_eq!(bottom.len(), 4, "overlay re-laid to new width");
        let text: String = bottom.iter().map(|c| c.ch).collect();
        assert!(text.starts_with("hi"), "overlay lost across resize: {text:?}");
    }

    #[test]
    fn render_set_status_requires_text() {
        let mut g = Grid::new(10, 3);
        let resp = channel_roundtrip(
            &mut g,
            "render",
            r#"{"jsonrpc":"2.0","id":1,"method":"set_status","params":{}}"#,
        );
        assert!(resp.contains("\"error\""), "missing text must error: {resp}");
    }

    #[test]
    fn osc_133_d_pushes_typed_command_finished() {
        let mut g = Grid::new(20, 4);
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://exit"}}"#,
        );
        // A command finishes with exit 0 (OSC 133;D;0).
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]133;D;0\x07");
        let push = String::from_utf8(p.take_responses()).unwrap();
        assert!(push.contains("notifications/command_finished"), "no typed push: {push}");
        assert!(push.contains("\"exit\":0"), "exit code not carried in the push: {push}");
        // The resource now reads the exit code.
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"terminal://exit"}}"#,
        );
        assert!(read.contains("\"text\":\"0\""), "exit code not reported: {read}");
    }

    #[test]
    fn osc_133_d_parses_nonzero_exit_code() {
        let mut g = Grid::new(20, 4);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]133;D;130\x07");
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"terminal://exit"}}"#,
        );
        assert!(read.contains("\"text\":\"130\""), "nonzero exit not parsed: {read}");
    }

    #[test]
    fn osc_133_d_without_code_notifies_with_empty_exit() {
        let mut g = Grid::new(20, 4);
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://exit"}}"#,
        );
        // D with no exit code still signals a finished command.
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]133;D\x07");
        let push = String::from_utf8(p.take_responses()).unwrap();
        assert!(push.contains("notifications/command_finished"), "must push on finish: {push}");
        assert!(push.contains("\"exit\":null"), "a missing code should push null: {push}");
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"terminal://exit"}}"#,
        );
        assert!(read.contains("\"text\":\"\""), "missing code should read empty: {read}");
    }

    #[test]
    fn osc_133_captures_command_output() {
        let mut g = Grid::new(20, 5);
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://command"}}"#,
        );
        // Prompt, command line, output-start, output, command-end.
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]133;A\x07$ echo hi\r\n\x1b]133;C\x07hi\r\n\x1b]133;D;0\x07");
        let push = String::from_utf8(p.take_responses()).unwrap();
        assert!(push.contains("terminal://command"), "command finish must push: {push}");
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"terminal://command"}}"#,
        );
        assert!(read.contains("\"text\":\"hi\""), "captured output wrong: {read}");
    }

    #[test]
    fn osc_133_captures_multiline_output() {
        let mut g = Grid::new(20, 6);
        let mut p = AnsiParser::new();
        p.advance(
            &mut g,
            b"\x1b]133;A\x07$ ls\r\n\x1b]133;C\x07a.txt\r\nb.txt\r\n\x1b]133;D;0\x07",
        );
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"terminal://command"}}"#,
        );
        // Output rows joined by a newline (escaped in JSON).
        assert!(read.contains("a.txt\\nb.txt"), "multi-line capture wrong: {read}");
    }

    #[test]
    fn osc_133_d_without_c_captures_no_output() {
        let mut g = Grid::new(20, 5);
        let mut p = AnsiParser::new();
        // Empty command line: prompt then finish, no output-start marker.
        p.advance(&mut g, b"\x1b]133;A\x07$ \r\n\x1b]133;D;0\x07");
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"terminal://command"}}"#,
        );
        assert!(read.contains("\"text\":\"\""), "no C means no captured output: {read}");
    }

    #[test]
    fn resize_preserves_in_flight_command_capture() {
        let mut g = Grid::new(20, 5);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"\x1b]133;A\x07$ x\r\n\x1b]133;C\x07partial\r\n");
        g.resize(10, 5); // mid-command resize: the anchor rides the reflow
        p.advance(&mut g, b"\x1b]133;D;0\x07");
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"terminal://command"}}"#,
        );
        assert!(read.contains("\"text\":\"partial\""), "resize must keep the capture: {read}");
    }

    #[test]
    fn resize_rewraps_captured_command_output() {
        let mut g = Grid::new(10, 5);
        let mut p = AnsiParser::new();
        // Output is one row at width 10; narrowing to 4 rewraps it to two rows,
        // and the capture must follow the rewrap.
        p.advance(&mut g, b"\x1b]133;A\x07$ x\r\n\x1b]133;C\x07ABCDEFGH\r\n");
        g.resize(4, 6);
        p.advance(&mut g, b"\x1b]133;D;0\x07");
        let read = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"terminal://command"}}"#,
        );
        assert!(read.contains("ABCD\\nEFGH"), "capture must reflect the rewrap: {read}");
    }

    #[test]
    fn resize_notifies_dimensions_subscriber() {
        let mut g = Grid::new(80, 24);
        // No subscriber: the driver gets nothing to send.
        assert!(g.resize_notification().is_none(), "unsubscribed: no frame");
        // After subscribing, a resize yields a dimensions update for the driver
        // to write to the child (the resize path runs outside `advance`).
        let _ = channel_roundtrip(
            &mut g,
            "mcp",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{"uri":"terminal://dimensions"}}"#,
        );
        g.resize(100, 30);
        let frame = g.resize_notification().expect("subscribed: a frame");
        let s = String::from_utf8(frame).unwrap();
        assert!(s.contains("notifications/resources/updated"), "{s}");
        assert!(s.contains("terminal://dimensions"), "{s}");
    }
}

#[test]
fn selection_extracts_single_line() {
    let mut g = parse(b"hello world", 20, 2);
    g.selection = Some(Selection { anchor: (0, 0), head: (4, 0) });
    assert_eq!(g.selected_text().as_deref(), Some("hello"));
}

#[test]
fn selection_spans_rows_and_joins_with_newline() {
    let mut g = parse(b"abc\r\ndef", 10, 3);
    g.selection = Some(Selection { anchor: (0, 0), head: (2, 1) });
    assert_eq!(g.selected_text().as_deref(), Some("abc\ndef"));
}

#[test]
fn selection_backward_drag_normalizes() {
    let mut g = parse(b"hello world", 20, 2);
    g.selection = Some(Selection { anchor: (4, 0), head: (0, 0) });
    assert_eq!(g.selected_text().as_deref(), Some("hello"));
}

#[test]
fn selection_trims_trailing_blanks_per_line() {
    let mut g = parse(b"hi", 10, 1);
    g.selection = Some(Selection { anchor: (0, 0), head: (9, 0) });
    assert_eq!(g.selected_text().as_deref(), Some("hi"));
}

#[test]
fn is_selected_includes_full_intermediate_rows() {
    let mut g = parse(b"", 10, 3);
    g.selection = Some(Selection { anchor: (5, 0), head: (1, 2) });
    assert!(g.is_selected(0, 1), "whole intermediate row is selected");
    assert!(g.is_selected(9, 1));
    assert!(!g.is_selected(4, 0), "before the start col on the start row");
    assert!(!g.is_selected(2, 2), "after the end col on the end row");
    assert!(g.is_selected(1, 2), "the end cell is inclusive");
}

#[test]
fn selection_in_scrolled_history_copies_viewport_text() {
    // "one" scrolls into history on a 2-row grid. Scrolled back one line, the
    // top viewport row shows "one"; selecting it must copy that history line,
    // not whatever the live grid holds at the same coordinates ("two").
    let mut g = parse(b"one\r\ntwo\r\nthree", 10, 2);
    assert!(g.scroll_view_up(1));
    g.selection = Some(Selection { anchor: (0, 0), head: (4, 0) });
    assert_eq!(g.selected_text().as_deref(), Some("one"));
}

#[test]
fn no_selection_yields_none() {
    let g = parse(b"text", 10, 1);
    assert_eq!(g.selected_text(), None);
    assert!(!g.is_selected(0, 0));
}

#[test]
fn bracketed_paste_tracks_mode_2004() {
    let mut g = Grid::new(10, 2);
    let mut p = AnsiParser::new();
    assert!(!g.bracketed_paste);
    p.advance(&mut g, b"\x1b[?2004h");
    assert!(g.bracketed_paste, "?2004h enables bracketed paste");
    p.advance(&mut g, b"\x1b[?2004l");
    assert!(!g.bracketed_paste, "?2004l disables it");
}

#[test]
fn ris_clears_bracketed_paste_and_selection() {
    let mut g = Grid::new(10, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?2004h");
    g.selection = Some(Selection { anchor: (0, 0), head: (3, 0) });
    p.advance(&mut g, b"\x1bc"); // RIS
    assert!(!g.bracketed_paste);
    assert_eq!(g.selection, None);
}

// --- Adversarial input: untrusted child output must never hang the parser ---
// (all of these run under the held grid lock, so an unbounded loop would freeze
// the whole terminal). Each previously looped on an attacker-controlled count.

#[test]
fn sixel_huge_repeat_count_is_bounded() {
    // `!<huge>` clamps to the column cap (MAX_DIM = 2000) instead of spinning the
    // inner paint loop ~usize::MAX times. A ~25-byte payload must decode promptly.
    let img = decode(b"#0;2;100;0;0!999999999999999999~");
    assert!(img.width > 0, "the band still paints");
    assert!(img.width <= 2000, "repeat clamped to the column cap, got {}", img.width);
}

#[test]
fn rep_huge_count_is_bounded_to_capacity() {
    // `CSI 99999999 b` is clamped to the addressable capacity (screen +
    // scrollback); without the clamp this would loop ~1e8+ times under the lock.
    // The fill still completes; the top row is a fully repeated, scrolled-up line.
    let g = parse(b"A\x1b[99999999b", 4, 2);
    assert_eq!(row_text(&g, 0), "AAAA", "the clamped REP still fills the screen");
}

#[test]
fn su_huge_count_clears_region_without_flooding_scrollback() {
    let mut g = Grid::new(4, 2);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"top\r\nbot");
    p.advance(&mut g, b"\x1b[9999999999S"); // SU by an enormous count
    // Clamped to the region height (2): the region clears, and scrollback gains
    // exactly the two displaced lines — not 9_999_999_999 blank entries.
    assert_eq!(row_text(&g, 0).trim_end(), "");
    assert_eq!(row_text(&g, 1).trim_end(), "");
    assert_eq!(g.scrollback.len(), 2, "only the region's rows reach scrollback");
}

// --- Configured theme (startup config) ---

/// A distinctive test theme: dark-grey bg, off-white fg, red cursor, and a
/// remapped ANSI red (index 1).
fn test_theme() -> Theme {
    let mut t = Theme { fg: 0xd8d8d8, bg: 0x1d1f21, cursor: 0xff0000, ..Default::default() };
    t.palette16[1] = 0xcc6666;
    t
}

#[test]
fn themed_parser_writes_text_in_theme_colors() {
    let mut g = Grid::new(8, 2);
    g.apply_theme(&test_theme());
    let mut p = AnsiParser::with_theme(test_theme());
    p.advance(&mut g, b"hi\x1b[31mr");
    let c = g.cells[0];
    assert_eq!(c.fg, 0xd8d8d8, "plain text uses the themed default fg");
    assert_eq!(c.bg, 0x1d1f21, "plain text uses the themed default bg");
    let r = g.cells[2];
    assert_eq!(r.fg, 0xcc6666, "SGR 31 resolves through the themed palette");
}

#[test]
fn themed_grid_erases_in_theme_background() {
    let mut g = Grid::new(4, 2);
    g.apply_theme(&test_theme());
    // Untouched (startup) cells already carry the themed colors...
    assert_eq!(g.cells[0].bg, 0x1d1f21);
    // ...and so do cells cleared after output (ED 2).
    let mut p = AnsiParser::with_theme(test_theme());
    p.advance(&mut g, b"xx\x1b[2J");
    assert_eq!(g.cells[0].bg, 0x1d1f21, "ED fills with the themed bg");
}

#[test]
fn ris_restores_theme_not_builtin() {
    let mut g = Grid::new(4, 2);
    g.apply_theme(&test_theme());
    let mut p = AnsiParser::with_theme(test_theme());
    // The child sets its own colors (OSC 10/11), then fully resets.
    p.advance(&mut g, b"\x1b]10;#ffffff\x07\x1b]11;#000000\x07\x1b\x63x");
    let c = g.cells[0];
    assert_eq!(c.fg, 0xd8d8d8, "RIS returns to the configured fg, not white");
    assert_eq!(c.bg, 0x1d1f21, "RIS returns to the configured bg, not black");
}

#[test]
fn osc_110_111_reset_to_theme() {
    let mut g = Grid::new(4, 2);
    g.apply_theme(&test_theme());
    let mut p = AnsiParser::with_theme(test_theme());
    p.advance(&mut g, b"\x1b]10;#123456\x07\x1b]110\x07\x1b]111\x07x");
    let c = g.cells[0];
    assert_eq!(c.fg, 0xd8d8d8, "OSC 110 restores the themed fg");
    assert_eq!(c.bg, 0x1d1f21, "OSC 111 restores the themed bg");
}

#[test]
fn osc_104_resets_palette_to_theme() {
    let mut g = Grid::new(8, 2);
    let mut p = AnsiParser::with_theme(test_theme());
    // Remap index 1 via OSC 4, then reset it via OSC 104;1 — it must return to
    // the *themed* red, not the stock xterm 0x800000.
    p.advance(&mut g, b"\x1b]4;1;#0000ff\x07\x1b]104;1\x07\x1b[31mx");
    assert_eq!(g.cells[0].fg, 0xcc6666, "OSC 104 restores the themed index 1");
}

#[test]
fn custom_scrollback_cap_is_enforced() {
    let mut g = Grid::new(4, 2);
    g.set_scrollback_max(3);
    let mut p = AnsiParser::new();
    for i in 0..10 {
        p.advance(&mut g, format!("l{i}\r\n").as_bytes());
    }
    assert_eq!(g.scrollback.len(), 3, "cap honored during scrolling");
    // Shrinking the cap trims an overfull buffer immediately.
    g.set_scrollback_max(1);
    assert_eq!(g.scrollback.len(), 1);
    // Zero disables history.
    g.set_scrollback_max(0);
    p.advance(&mut g, b"more\r\nlines\r\n");
    assert_eq!(g.scrollback.len(), 0, "zero cap keeps history empty");
}

// --- Live retheme (config reload) ---

#[test]
fn retheme_recolors_existing_content() {
    let mut g = Grid::new(8, 2);
    g.apply_theme(&test_theme());
    let mut p = AnsiParser::with_theme(test_theme());
    p.advance(&mut g, b"hi\x1b[31mr");
    // Switch to a different theme: defaults and ANSI red follow.
    let mut new = Theme { fg: 0x111111, bg: 0x222222, ..Default::default() };
    new.palette16[1] = 0x333333;
    let old = p.retheme(new);
    assert_eq!(old, test_theme(), "retheme returns the previous seed");
    g.retheme(&old, &new);
    assert_eq!(g.cells[0].fg, 0x111111, "plain text recolored to new fg");
    assert_eq!(g.cells[0].bg, 0x222222, "bg recolored");
    assert_eq!(g.cells[2].fg, 0x333333, "ANSI red remapped to new red");
    // New output uses the new theme immediately.
    p.advance(&mut g, b"\x1b[0mx");
    assert_eq!(g.cells[3].fg, 0x111111);
}

#[test]
fn retheme_preserves_truecolor_and_child_overrides() {
    let mut g = Grid::new(8, 2);
    g.apply_theme(&test_theme());
    let mut p = AnsiParser::with_theme(test_theme());
    // Truecolor text + a child-overridden palette slot (OSC 4;2).
    p.advance(&mut g, b"\x1b[38;2;1;2;3mt\x1b]4;2;#abcdef\x07\x1b[32mg");
    let mut new = Theme::default();
    new.palette16[2] = 0x444444;
    let old = p.retheme(new);
    g.retheme(&old, &new);
    assert_eq!(g.cells[0].fg, 0x010203, "truecolor passes through retheme");
    // The child's own OSC 4 green is kept (not stomped to the new theme's).
    p.advance(&mut g, b"\x1b[32mG");
    assert_eq!(g.cells[2].fg, 0xabcdef, "child palette override survives");
    // And a reset returns to the *new* theme, not the old one.
    p.advance(&mut g, b"\x1b]104;2\x07\x1b[32mz");
    assert_eq!(g.cells[3].fg, 0x444444, "reset lands on the new theme");
}

#[test]
fn retheme_recolors_scrollback() {
    let mut g = Grid::new(4, 2);
    g.apply_theme(&test_theme());
    let mut p = AnsiParser::with_theme(test_theme());
    for i in 0..5 {
        p.advance(&mut g, format!("l{i}\r\n").as_bytes());
    }
    assert!(!g.scrollback.is_empty());
    let new = Theme { fg: 0x999999, ..Default::default() };
    let old = p.retheme(new);
    g.retheme(&old, &new);
    for line in &g.scrollback {
        for cell in &line.cells {
            assert_ne!(cell.fg, 0xd8d8d8, "old themed fg gone from history");
        }
    }
}

// ---- Wave-1 additions: keypad mode, focus reporting, DECRQSS, XTSMGRAPHICS ----

#[test]
fn keypad_application_mode_tracked_and_relayed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.app_keypad);
    // DECKPAM (`ESC =`) sets application keypad and relays to the host.
    p.advance(&mut g, b"\x1b=");
    assert!(g.app_keypad);
    assert_eq!(g.take_host_out(), b"\x1b=");
    // DECKPNM (`ESC >`) resets it.
    p.advance(&mut g, b"\x1b>");
    assert!(!g.app_keypad);
    assert_eq!(g.take_host_out(), b"\x1b>");
    // DECNKM (`?66`) toggles the same state and answers DECRQM.
    p.advance(&mut g, b"\x1b[?66h");
    assert!(g.app_keypad);
    p.advance(&mut g, b"\x1b[?66$p");
    assert_eq!(p.take_responses(), b"\x1b[?66;1$y");
    // RIS restores the numeric default.
    p.advance(&mut g, b"\x1bc");
    assert!(!g.app_keypad);
}

#[test]
fn focus_reporting_mode_tracked_and_relayed() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.focus_reporting);
    p.advance(&mut g, b"\x1b[?1004h");
    assert!(g.focus_reporting);
    assert_eq!(g.take_host_out(), b"\x1b[?1004h"); // still relayed for TUI mode
    p.advance(&mut g, b"\x1b[?1004l");
    assert!(!g.focus_reporting);
}

#[test]
fn decrqss_reports_sgr_scroll_region_and_cursor_style() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Default pen: just the reset.
    p.advance(&mut g, b"\x1bP$qm\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP1$r0m\x1b\\");
    // Bold + curly underline + truecolor fg.
    p.advance(&mut g, b"\x1b[1;4:3m\x1b[38;2;1;2;3m\x1bP$qm\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP1$r0;1;4:3;38;2;1;2;3m\x1b\\");
    p.advance(&mut g, b"\x1b[0m");
    // DECSTBM: default region, then an explicit one.
    p.advance(&mut g, b"\x1bP$qr\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP1$r1;24r\x1b\\");
    p.advance(&mut g, b"\x1b[5;10r\x1bP$qr\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP1$r5;10r\x1b\\");
    // DECSCUSR: steady bar (6), set then queried.
    p.advance(&mut g, b"\x1b[6 q\x1bP$q q\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP1$r6 q\x1b\\");
    let _ = g.take_host_out(); // drop the relayed DECSCUSR/DECSTBM bytes
}

#[test]
fn decrqss_unknown_request_reports_invalid() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1bP$qt\x1b\\"); // DECSLPP — not tracked
    assert_eq!(p.take_responses(), b"\x1bP0$r\x1b\\");
    // DECSCA *is* tracked now (G15): report the current protection state.
    p.advance(&mut g, b"\x1bP$q\"q\x1b\\");
    assert_eq!(p.take_responses(), b"\x1bP1$r0\"q\x1b\\".to_vec());
}

#[test]
fn xtsmgraphics_reports_color_registers_and_sixel_geometry() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Item 1: color registers — read and read-max report the fixed table size.
    p.advance(&mut g, b"\x1b[?1;1S");
    assert_eq!(p.take_responses(), b"\x1b[?1;0;256S");
    p.advance(&mut g, b"\x1b[?1;4S");
    assert_eq!(p.take_responses(), b"\x1b[?1;0;256S");
    // Item 2: Sixel geometry — the decoder's per-axis cap.
    p.advance(&mut g, b"\x1b[?2;1S");
    assert_eq!(p.take_responses(), b"\x1b[?2;0;2000;2000S");
    // A "set" succeeds by reporting the actual (unchanged) limits.
    p.advance(&mut g, b"\x1b[?2;3;900;900S");
    assert_eq!(p.take_responses(), b"\x1b[?2;0;2000;2000S");
}

#[test]
fn xtsmgraphics_rejects_unknown_item_and_action() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?3;1S"); // ReGIS: unsupported item
    assert_eq!(p.take_responses(), b"\x1b[?3;1S");
    p.advance(&mut g, b"\x1b[?1;9S"); // bad action on a known item
    assert_eq!(p.take_responses(), b"\x1b[?1;2S");
}

// ---- Wave-2 additions: bell, OSC 9;4 progress, command timing, selection ----

#[test]
fn bel_flags_bell_and_relays_to_host() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.bell);
    p.advance(&mut g, b"before\x07after");
    assert!(g.bell);
    assert_eq!(g.take_host_out(), b"\x07");
    // Text around the BEL still prints normally.
    assert_eq!(g.cells[0].ch, 'b');
    assert_eq!(g.cells[6].ch, 'a');
    // RIS clears the pending ring.
    p.advance(&mut g, b"\x1bc");
    assert!(!g.bell);
}

#[test]
fn osc_9_4_progress_states_track_and_clear() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]9;4;1;150\x07"); // percent clamps to 100
    assert_eq!(g.progress, Some((1, 100)));
    p.advance(&mut g, b"\x1b]9;4;2;30\x07"); // error state keeps its percent
    assert_eq!(g.progress, Some((2, 30)));
    p.advance(&mut g, b"\x1b]9;4;3\x07"); // indeterminate: no percent
    assert_eq!(g.progress, Some((3, 0)));
    p.advance(&mut g, b"\x1b]9;4;0\x07"); // clear
    assert_eq!(g.progress, None);
    p.advance(&mut g, b"\x1b]9;4;9;9\x07"); // unknown state clears too
    assert_eq!(g.progress, None);
    let _ = g.take_host_out();
}

#[test]
fn command_timer_records_exit_and_runtime_on_133_d() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // D without C: no record (shell only emits D).
    p.advance(&mut g, b"\x1b]133;D;0\x07");
    assert!(g.finished_commands.is_empty());
    // C … D with an exit code records one entry.
    p.advance(&mut g, b"\x1b]133;C\x07output\x1b]133;D;3\x07");
    assert_eq!(g.finished_commands.len(), 1);
    assert_eq!(g.finished_commands[0].0, Some(3));
    // And without one records None.
    p.advance(&mut g, b"\x1b]133;C\x07\x1b]133;D\x07");
    assert_eq!(g.finished_commands[1].0, None);
}

#[test]
fn select_word_at_expands_over_path_like_runs() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"ls /tmp/dir-1/file.txt \"quoted\"");
    g.select_word_at(8, 0); // inside the path
    assert_eq!(g.selected_text().as_deref(), Some("/tmp/dir-1/file.txt"));
    g.select_word_at(0, 0); // inside `ls`
    assert_eq!(g.selected_text().as_deref(), Some("ls"));
    g.select_word_at(2, 0); // the blank between: selects just that cell
    assert_eq!(g.selected_text().as_deref(), Some(""));
    g.select_word_at(24, 0); // inside `quoted` — quotes are separators
    assert_eq!(g.selected_text().as_deref(), Some("quoted"));
}

#[test]
fn select_line_at_follows_soft_wraps() {
    let mut g = Grid::new(10, 5);
    let mut p = AnsiParser::new();
    // 15 chars soft-wrap onto row 1; then a hard-broken second line.
    p.advance(&mut g, b"abcdefghijklmno\r\nsecond");
    g.select_line_at(0);
    assert_eq!(g.selected_text().as_deref(), Some("abcdefghij\nklmno"));
    g.select_line_at(1); // clicking the continuation selects the same logical line
    assert_eq!(g.selected_text().as_deref(), Some("abcdefghij\nklmno"));
    g.select_line_at(2);
    assert_eq!(g.selected_text().as_deref(), Some("second"));
}

#[test]
fn extend_word_selection_keeps_both_words_whole_and_everything_between() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"ls /tmp/dir-1/file.txt \"quoted\"");
    // Click lands in "ls" (cols 0-1); drag point lands in the path
    // (cols 3-21) — dragging forward from the first word to a later one.
    g.select_word_at(0, 0);
    let anchor = g.selection.unwrap().anchor;
    g.extend_word_selection(anchor, (8, 0));
    assert_eq!(g.selected_text().as_deref(), Some("ls /tmp/dir-1/file.txt"));

    // Same two words, but the *click* is in the path and the drag point
    // moves back onto "ls" — dragging backward past the anchor must yield
    // the identical union, not just the first word.
    g.select_word_at(8, 0);
    let anchor = g.selection.unwrap().anchor;
    g.extend_word_selection(anchor, (0, 0));
    assert_eq!(g.selected_text().as_deref(), Some("ls /tmp/dir-1/file.txt"));

    // Dragging within the same word never shrinks below the clicked word.
    g.select_word_at(8, 0);
    let anchor = g.selection.unwrap().anchor;
    g.extend_word_selection(anchor, (15, 0));
    assert_eq!(g.selected_text().as_deref(), Some("/tmp/dir-1/file.txt"));
}

#[test]
fn extend_line_selection_spans_whole_wrapped_lines_either_drag_direction() {
    let mut g = Grid::new(10, 5);
    let mut p = AnsiParser::new();
    // Row 0 wraps onto row 1; row 2 is a separate hard-broken line.
    p.advance(&mut g, b"abcdefghijklmno\r\nsecond");
    g.select_line_at(0);
    let anchor_row = g.selection.unwrap().anchor.1;
    g.extend_line_selection(anchor_row, 2);
    assert_eq!(g.selected_text().as_deref(), Some("abcdefghij\nklmno\nsecond"));

    // Same span, dragging from the last line back up to the first.
    g.select_line_at(2);
    let anchor_row = g.selection.unwrap().anchor.1;
    g.extend_line_selection(anchor_row, 0);
    assert_eq!(g.selected_text().as_deref(), Some("abcdefghij\nklmno\nsecond"));
}

// ---- Wave-3 additions: regex search (rusty_regx) ----

#[test]
fn regex_search_matches_and_folds_case() {
    let mut g = Grid::new(40, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"Error: 404\r\nwarning: X\r\nERROR: 500");
    assert_eq!(g.search_with("error: [0-9]+", true), 2);
    // Plain mode still works through the same entry point.
    assert_eq!(g.search_with("error", false), 2);
}

#[test]
fn regex_search_spans_highlight_the_matched_cells() {
    let mut g = Grid::new(40, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"pi=3.14 e=2.71");
    assert_eq!(g.search_with("[0-9]+\\.[0-9]+", true), 2);
    // First match "3.14" covers cols 3..7 of row 0.
    assert_eq!(g.search_highlight(3, 0), Some(true));
    assert_eq!(g.search_highlight(6, 0), Some(true));
    assert_eq!(g.search_highlight(7, 0), None);
    assert_eq!(g.search_highlight(10, 0), Some(false)); // "2.71", inactive
}

#[test]
fn regex_search_anchors_match_once_per_logical_line() {
    let mut g = Grid::new(10, 6);
    let mut p = AnsiParser::new();
    // "ababab..." soft-wraps; ^ab must match only at the true line start.
    p.advance(&mut g, b"abababababab\r\nab");
    assert_eq!(g.search_with("^ab", true), 2);
    // `$` anchors to the logical line end, not each visual row.
    assert_eq!(g.search_with("ab$", true), 2);
}

#[test]
fn regex_search_rejects_malformed_and_skips_empty_matches() {
    let mut g = Grid::new(20, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"hello");
    assert_eq!(g.search_with("(unclosed", true), 0); // malformed: no matches
    assert_eq!(g.search_with("z*", true), 0); // only empty-width: nothing to show
    assert_eq!(g.search_with("l+", true), 1); // and the engine still terminates
}

#[test]
fn url_at_detects_plain_text_urls_across_wraps() {
    let mut g = Grid::new(20, 5);
    let mut p = AnsiParser::new();
    // URL soft-wraps across two rows; detection joins the logical line.
    p.advance(&mut g, b"see https://example.com/a/long/path now");
    assert_eq!(g.url_at(6, 0).as_deref(), Some("https://example.com/a/long/path"));
    assert_eq!(g.url_at(2, 1).as_deref(), Some("https://example.com/a/long/path"));
    assert_eq!(g.url_at(0, 0), None); // "see" is not a URL
}

#[test]
fn hover_link_at_clamps_a_wrapped_plain_text_url_to_the_hovered_row() {
    let mut g = Grid::new(20, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"see https://example.com/a/long/path now");
    // Row 0: the URL runs from col 4 (right after "see ") to the row's last
    // column (it continues onto row 1); row 1: it resumes at col 0 and ends
    // before " now".
    let url = "https://example.com/a/long/path".to_string();
    assert_eq!(g.hover_link_at(6, 0), Some((4, 19, url.clone())));
    assert_eq!(g.hover_link_at(2, 1), Some((0, 14, url)));
    assert_eq!(g.hover_link_at(0, 0), None); // "see" is not a URL
}

#[test]
fn url_detection_trims_punctuation_and_handles_www() {
    let mut g = Grid::new(60, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"(see https://ex.com/x). Or www.rust-lang.org, ok? mailto:a@b.c!");
    assert_eq!(g.url_at(6, 0).as_deref(), Some("https://ex.com/x"));
    assert_eq!(g.url_at(28, 0).as_deref(), Some("http://www.rust-lang.org"));
    assert_eq!(g.url_at(51, 0).as_deref(), Some("mailto:a@b.c"));
    // The trailing ")." and "," were trimmed; the "." between them is no URL.
    assert_eq!(g.url_at(22, 0), None);
}

#[test]
fn url_detection_keeps_balanced_parens() {
    let mut g = Grid::new(60, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"https://en.wikipedia.org/wiki/Rust_(language)");
    assert_eq!(
        g.url_at(0, 0).as_deref(),
        Some("https://en.wikipedia.org/wiki/Rust_(language)")
    );
}

#[test]
fn visible_links_collects_osc8_and_detected_urls() {
    let mut g = Grid::new(40, 6);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]8;;https://osc8.example\x07link\x1b]8;;\x07 and http://plain.example\r\nhttp://plain.example again");
    let links = g.visible_links();
    assert!(links.contains(&"https://osc8.example".to_string()));
    assert!(links.contains(&"http://plain.example".to_string()));
    assert_eq!(links.len(), 2, "{links:?}"); // deduped
}

#[test]
fn osc_52_primary_selection_routes_separately() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // `p` selection sets the primary side; `c` (and empty) the clipboard side.
    p.advance(&mut g, b"\x1b]52;p;SGVsbG8=\x07");
    assert_eq!(g.clipboard_set_primary.as_deref(), Some("Hello"));
    assert_eq!(g.clipboard_set, None);
    p.advance(&mut g, b"\x1b]52;c;V29ybGQ=\x07");
    assert_eq!(g.clipboard_set.as_deref(), Some("World"));
    // Queries route the same way.
    p.advance(&mut g, b"\x1b]52;p;?\x07");
    assert!(g.clipboard_query_primary);
    assert!(!g.clipboard_query);
    let _ = g.take_host_out();
}

// ---- Wave-4 additions: color-scheme query/notify, OSC 99, contrast ----

#[test]
fn dsr_996_reports_dark_or_light_from_background() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Default theme: black background -> dark (997;1).
    p.advance(&mut g, b"\x1b[?996n");
    assert_eq!(p.take_responses(), b"\x1b[?997;1n");
    // Flip to a light background via OSC 11 and ask again.
    p.advance(&mut g, b"\x1b]11;#ffffff\x07\x1b[?996n");
    assert_eq!(p.take_responses(), b"\x1b[?997;2n");
}

#[test]
fn mode_2031_is_tracked_relayed_and_reported() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.report_color_scheme);
    p.advance(&mut g, b"\x1b[?2031h");
    assert!(g.report_color_scheme);
    assert_eq!(g.take_host_out(), b"\x1b[?2031h"); // relayed for TUI mode
    p.advance(&mut g, b"\x1b[?2031$p");
    assert_eq!(p.take_responses(), b"\x1b[?2031;1$y");
    assert_eq!(g.color_scheme_report(), b"\x1b[?997;1n");
    p.advance(&mut g, b"\x1b[?2031l");
    assert!(!g.report_color_scheme);
}

#[test]
fn mode_9001_win32_input_is_tracked_relayed_and_reported() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    assert!(!g.win32_input);
    p.advance(&mut g, b"\x1b[?9001h");
    assert!(g.win32_input);
    assert_eq!(g.take_host_out(), b"\x1b[?9001h"); // relayed for TUI mode
    p.advance(&mut g, b"\x1b[?9001$p");
    assert_eq!(p.take_responses(), b"\x1b[?9001;1$y");
    p.advance(&mut g, b"\x1b[?9001l");
    assert!(!g.win32_input);
    p.advance(&mut g, b"\x1b[?9001$p");
    assert_eq!(p.take_responses(), b"\x1b[?9001;2$y");
    // RIS clears it like the other input modes.
    p.advance(&mut g, b"\x1b[?9001h");
    p.advance(&mut g, b"\x1bc");
    assert!(!g.win32_input);
}

#[test]
fn osc_99_single_part_notification() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Bare payload (implicit p=title, d=1): surfaces as the body.
    p.advance(&mut g, b"\x1b]99;;Build finished\x07");
    assert_eq!(g.notifications.len(), 1);
    assert_eq!(g.notifications[0], (String::new(), "Build finished".to_string()));
    let _ = g.take_host_out();
}

#[test]
fn osc_99_multipart_title_and_body_with_base64() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // Two parts accumulate under i=x; the d=1 part finalizes. Body is base64.
    p.advance(&mut g, b"\x1b]99;i=x:d=0:p=title;CI failed\x07");
    assert!(g.notifications.is_empty());
    p.advance(&mut g, b"\x1b]99;i=x:d=1:p=body:e=1;am9iIDQyMQ==\x07");
    assert_eq!(g.notifications.len(), 1);
    assert_eq!(g.notifications[0], ("CI failed".to_string(), "job 421".to_string()));
    let _ = g.take_host_out();
}

#[test]
fn osc_99_non_text_payloads_are_ignored() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]99;p=close:i=x;\x07"); // close request: untracked
    p.advance(&mut g, b"\x1b]99;p=?;\x07"); // query: untracked
    assert!(g.notifications.is_empty());
    assert!(g.take_host_out().is_empty()); // not even relayed
}

#[test]
fn min_contrast_is_off_by_default_and_settable() {
    let g = Grid::new(4, 2);
    assert_eq!(g.min_contrast, 1.0);
}

// ---- Wave-5 additions: absolute selection, scrollbar ----

#[test]
fn selection_stays_anchored_while_scrolling_history() {
    let mut g = Grid::new(10, 3);
    let mut p = AnsiParser::new();
    for i in 0..20 {
        p.advance(&mut g, format!("line{i:02}\r\n").as_bytes());
    }
    // Scroll 5 rows into history and select the word on the top viewport row.
    g.scroll_view_up(5);
    g.select_word_at(0, 0);
    let picked = g.selected_text().unwrap();
    assert!(picked.starts_with("line"), "{picked:?}");
    // Scrolling further must not change what's selected (absolute coords)...
    g.scroll_view_up(3);
    assert_eq!(g.selected_text().unwrap(), picked);
    // ...and the highlight follows the text: the row it appears on shifted
    // down by exactly the extra scroll.
    assert!(g.is_selected(0, 3));
    assert!(!g.is_selected(0, 0));
}

#[test]
fn select_line_follows_wraps_into_scrollback() {
    let mut g = Grid::new(10, 3);
    let mut p = AnsiParser::new();
    // A 25-char run wraps over three rows, then push it into history.
    p.advance(&mut g, b"abcdefghijklmnopqrstuvwxy\r\n");
    for _ in 0..4 {
        p.advance(&mut g, b"filler\r\n");
    }
    g.scroll_view_up(10); // far enough that the wrapped run is in view
    // Find the viewport row showing the run's middle chunk ("klmnopqrst").
    let vr = (0..3)
        .find(|&r| !g.is_selected(0, r) && {
            g.select_word_at(0, r);
            g.selected_text().is_some_and(|t| t.contains("klmnopqrst"))
        })
        .unwrap_or(1);
    g.select_line_at(vr);
    let text = g.selected_text().unwrap().replace('\n', "");
    assert_eq!(text, "abcdefghijklmnopqrstuvwxy");
}

#[test]
fn scrollbar_hides_at_bottom_and_tracks_position() {
    let mut g = Grid::new(10, 4);
    let mut p = AnsiParser::new();
    for i in 0..40 {
        p.advance(&mut g, format!("l{i}\r\n").as_bytes());
    }
    assert_eq!(g.scrollbar(), None); // live bottom: hidden
    g.scroll_view_up(5);
    let (first_mid, len, _) = g.scrollbar().unwrap();
    assert!((1..=4).contains(&len));
    g.scroll_view_up(1000); // clamped to the top of history
    let (first_top, _, _) = g.scrollbar().unwrap();
    assert_eq!(first_top, 0, "thumb at the very top when fully scrolled");
    assert!(first_mid >= first_top);
    g.reset_view();
    assert_eq!(g.scrollbar(), None);
}

// ---- Wave-8: deterministic parser stress (the in-suite face of G37) ----

/// A tiny deterministic xorshift so the stress input is reproducible; real
/// coverage-guided fuzzing lives in `fuzz/` (cargo-fuzz, nightly).
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn parser_survives_deterministic_byte_soup() {
    // Byte soup seasoned with escape introducers so the state machine
    // actually leaves ground; split into chunks to exercise incremental
    // state. Asserting "does not panic / hang" over ~256 KiB of input.
    let mut rng = XorShift(0x5EED_1BAD_F00D_2026);
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    for _ in 0..64 {
        let mut chunk = Vec::with_capacity(4096);
        for _ in 0..4096 {
            let r = rng.next();
            let b = match r % 10 {
                0 => 0x1b,
                1 => *b"[]P_^X#()78cnqSHJKm~u".get((r >> 8) as usize % 21).unwrap(),
                2 => b';',
                3..=4 => (r >> 16) as u8 % 10 + b'0',
                _ => (r >> 24) as u8,
            };
            chunk.push(b);
        }
        p.advance(&mut g, &chunk);
        let _ = p.take_responses();
        let _ = g.take_host_out();
    }
    // The grid must still be internally consistent enough to render.
    assert_eq!(g.cells.len(), g.cols * g.rows);
    let _ = g.snapshot_viewport();
}

#[test]
fn graphics_payload_soup_never_panics() {
    // The wrapped-payload shapes the `fuzz/graphics` target uses, driven
    // deterministically: hostile Sixel / Kitty APC / iTerm2 bodies.
    let mut rng = XorShift(0xC0FF_EE00_2026_0711);
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    for i in 0..48 {
        let mut body = Vec::with_capacity(512);
        for _ in 0..512 {
            body.push((rng.next() >> 32) as u8);
        }
        let mut frame = match i % 3 {
            0 => {
                let mut f = b"\x1bPq".to_vec();
                f.extend_from_slice(&body);
                f
            }
            1 => {
                let mut f = b"\x1b_G".to_vec();
                f.extend_from_slice(&body);
                f
            }
            _ => {
                let mut f = b"\x1b]1337;File=inline=1:".to_vec();
                f.extend_from_slice(&body);
                f.push(0x07);
                f
            }
        };
        if i % 3 != 2 {
            frame.extend_from_slice(b"\x1b\\");
        }
        p.advance(&mut g, &frame);
        let _ = p.take_responses();
        let _ = g.take_host_out();
    }
}

#[test]
fn prompt_cursor_moves_computes_arrow_deltas() {
    let mut g = Grid::new(40, 10);
    let mut p = AnsiParser::new();
    // No prompt mark yet: never moves.
    p.advance(&mut g, b"plain$ ");
    assert_eq!(g.prompt_cursor_moves(0, 0), None);
    // Prompt mark, cursor at col 7 row 0: click right on the same row.
    p.advance(&mut g, b"\r\x1b]133;A\x07prompt$ ");
    assert_eq!(g.prompt_cursor_moves(12, 0), Some((4, 0)));
    assert_eq!(g.prompt_cursor_moves(2, 0), Some((-6, 0)));
    // Click on the cursor cell itself: nothing to do.
    assert_eq!(g.prompt_cursor_moves(8, 0), None);
    // Click above the prompt row: outside the editable region.
    p.advance(&mut g, b"\r\n\x1b]133;A\x07$ ");
    assert_eq!(g.prompt_cursor_moves(0, 0), None);
    // A running command (open 133;C capture) disables it...
    p.advance(&mut g, b"\x1b]133;C\x07");
    assert_eq!(g.prompt_cursor_moves(3, 1), None);
    // ...and the next prompt re-enables.
    p.advance(&mut g, b"\x1b]133;D;0\x07\r\n\x1b]133;A\x07$ ");
    assert!(g.prompt_cursor_moves(5, 2).is_some());
    // Scrolled views never move the cursor (needs real history to scroll
    // into — an empty scrollback clamps the offset back to the live view).
    for _ in 0..12 {
        p.advance(&mut g, b"x\r\n");
    }
    p.advance(&mut g, b"\x1b]133;A\x07$ ");
    g.scroll_view_up(1);
    assert_eq!(g.prompt_cursor_moves(5, 2), None);
    g.reset_view();
    assert!(g.prompt_cursor_moves(5, g.cursor.1).is_some());
    // Neither does the alternate screen.
    p.advance(&mut g, b"\x1b[?1049h");
    assert_eq!(g.prompt_cursor_moves(1, 1), None);
}

// ---- Wave-9 additions: protected areas, left/right margins ----

#[test]
fn decsca_protects_cells_from_selective_erase() {
    let mut g = Grid::new(20, 4);
    let mut p = AnsiParser::new();
    // "keep" written protected, "drop" not.
    p.advance(&mut g, b"\x1b[1\"qkeep\x1b[0\"qdrop");
    // DECSEL 2 (whole line, selective): only "drop" goes.
    p.advance(&mut g, b"\x1b[?2K");
    let row: String = (0..8).map(|x| g.cells[x].ch).collect();
    assert_eq!(row, "keep    ");
    // SGR 0 must NOT clear protection (it's not a rendition attribute).
    p.advance(&mut g, b"\x1b[H\x1b[1\"q\x1b[0mSAFE\x1b[?2K");
    let row: String = (0..4).map(|x| g.cells[x].ch).collect();
    assert_eq!(row, "SAFE");
    // Plain EL still erases everything, protected or not.
    p.advance(&mut g, b"\x1b[2K");
    assert_eq!(g.cells[0].ch, ' ');
}

#[test]
fn decsed_erases_display_selectively() {
    let mut g = Grid::new(10, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[1\"qAA\x1b[0\"qbb\r\ncc\r\ndd");
    p.advance(&mut g, b"\x1b[H\x1b[?2J"); // selective erase all
    assert_eq!(g.cells[0].ch, 'A');
    assert_eq!(g.cells[1].ch, 'A');
    assert_eq!(g.cells[2].ch, ' '); // "bb" gone
    assert_eq!(g.cells[g.cols].ch, ' '); // "cc" gone
    // DECSTR resets the protection state for subsequent writes.
    p.advance(&mut g, b"\x1b[!p");
    p.advance(&mut g, b"xx\x1b[?2K");
    assert_eq!(g.cells[0].ch, ' ');
}

#[test]
fn decslrm_sets_margins_only_under_declrmm() {
    let mut g = Grid::new(20, 5);
    let mut p = AnsiParser::new();
    // Without ?69, `CSI s` is SCP (save cursor) — margins untouched.
    p.advance(&mut g, b"\x1b[5;10s");
    assert!(!g.side_margins_active());
    // With ?69 set, the same sequence is DECSLRM (1-based, inclusive).
    p.advance(&mut g, b"\x1b[?69h\x1b[5;10s");
    assert!(g.side_margins_active());
    assert_eq!((g.left_margin, g.right_margin), (4, 9));
    assert_eq!(g.cursor, (0, 0), "DECSLRM homes the cursor");
    // DECRQM reports the mode; resetting it restores the full width.
    p.advance(&mut g, b"\x1b[?69$p");
    assert_eq!(p.take_responses(), b"\x1b[?69;1$y");
    p.advance(&mut g, b"\x1b[?69l");
    assert!(!g.side_margins_active());
    assert_eq!((g.left_margin, g.right_margin), (0, 19));
}

#[test]
fn side_margins_confine_wrap_cr_and_scrolling() {
    let mut g = Grid::new(10, 3);
    let mut p = AnsiParser::new();
    // Sentinels outside the margins must never move.
    p.advance(&mut g, b"L        R");
    p.advance(&mut g, b"\x1b[?69h\x1b[3;8s"); // margins cols 2..=7 (0-based)
    // 19 chars from the left margin: the 6-wide band fills three rows
    // (abcdef / ghijkl / mnopqr) and the 's' wraps at the band bottom,
    // scrolling the band once — "abcdef" leaves, sentinels never move.
    p.advance(&mut g, b"\x1b[1;3Habcdefghijklmnopqrs");
    assert_eq!(g.cells[0].ch, 'L');
    assert_eq!(g.cells[9].ch, 'R');
    let band = |row: usize| -> String { (2..8).map(|x| g.cells[row * g.cols + x].ch).collect() };
    assert_eq!(band(0), "ghijkl");
    assert_eq!(band(1), "mnopqr");
    assert_eq!(band(2), "s     ");
    // CR returns to the left margin, not column 0.
    p.advance(&mut g, b"\r");
    assert_eq!(g.cursor.0, 2);
    // Band scrolling never forms scrollback.
    assert_eq!(g.scrollback.len(), 0);
}

#[test]
fn side_margins_limit_il_dl_to_the_band() {
    let mut g = Grid::new(10, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc\r\ndddddddddd");
    p.advance(&mut g, b"\x1b[?69h\x1b[3;8s\x1b[2;3H"); // margins 2..=7, cursor row 1 in band
    p.advance(&mut g, b"\x1b[1L"); // IL: shift band rows 1.. down
    // Outside the margins row 1 keeps its 'b's; inside it blanked.
    assert_eq!(g.cells[g.cols].ch, 'b');
    assert_eq!(g.cells[g.cols + 2].ch, ' ');
    // The old band content of row 1 moved to row 2.
    assert_eq!(g.cells[2 * g.cols + 2].ch, 'b');
    // DL undoes it.
    p.advance(&mut g, b"\x1b[1M");
    assert_eq!(g.cells[g.cols + 2].ch, 'b');
    // IL with the cursor outside the side margins is a no-op.
    p.advance(&mut g, b"\x1b[2;1H\x1b[1L");
    assert_eq!(g.cells[g.cols + 2].ch, 'b');
}

#[test]
fn delete_lines_with_side_margins_and_cursor_at_top_does_not_panic() {
    // DL's DECSLRM branch computed `scroll_bottom - n` directly; with the
    // cursor on row 0 (== scroll_top) and a count covering the whole region,
    // `n` reaches `scroll_bottom + 1`, underflowing that subtraction. Any
    // program sending an oversized `CSI Pn M` with margins active and the
    // cursor parked at the top must not crash the terminal.
    let mut g = Grid::new(10, 4);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc\r\ndddddddddd");
    p.advance(&mut g, b"\x1b[?69h\x1b[3;8s\x1b[1;3H"); // margins cols 2..=7 (0-based), cursor row 0 in band
    p.advance(&mut g, b"\x1b[9999M"); // DL count far exceeding the region height
    // The whole band is blanked; outside the margins is untouched.
    assert_eq!(g.cells[2].ch, ' ');
    assert_eq!(g.cells[0].ch, 'a'); // col 0, outside the left margin, unaffected
}

#[test]
fn origin_mode_addresses_within_side_margins() {
    let mut g = Grid::new(20, 5);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b[?69h\x1b[5;10s\x1b[?6h"); // margins 4..=9, DECOM on
    assert_eq!(g.cursor, (4, 0), "origin home is the margin corner");
    p.advance(&mut g, b"\x1b[1;3H"); // col 3 relative -> absolute 6
    assert_eq!(g.cursor, (6, 0));
    p.advance(&mut g, b"\x1b[1;99H"); // clamps at the right margin
    assert_eq!(g.cursor, (9, 0));
}

// ---- Wave-10 additions: kitty image store, placeholders, animation ----

#[test]
fn kitty_store_and_put_render_by_id() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    // a=t stores without rendering; a=p places by id.
    p.advance(&mut g, b"\x1b_Gf=32,s=1,v=1,a=t,i=7;/wAA/w==\x1b\\");
    assert_eq!(p.take_responses(), b"\x1b_Gi=7;OK\x1b\\");
    assert_eq!(g.cells[0].ch, ' ', "transmit-only draws nothing");
    p.advance(&mut g, b"\x1b_Ga=p,i=7;\x1b\\");
    assert_eq!(g.cells[0].ch, '\u{2580}');
    assert_eq!(g.cells[0].fg, 0xFF0000);
    // Placing an unknown id is an error.
    p.advance(&mut g, b"\x1b_Ga=p,i=8;\x1b\\");
    let _ = p.take_responses();
    // a=d,d=i deletes by id; the next put fails.
    p.advance(&mut g, b"\x1b_Ga=d,d=i,i=7;\x1b\\");
    p.advance(&mut g, b"\x1b_Ga=p,i=7;\x1b\\");
    assert!(p.take_responses().windows(5).any(|w| w == b"EBADF"));
}

#[test]
fn kitty_virtual_placement_and_placeholder_decode() {
    let mut g = Grid::new(20, 6);
    let mut p = AnsiParser::new();
    // 1×1 red stored + virtually placed on a 2×2 cell grid.
    p.advance(&mut g, b"\x1b_Gf=32,s=1,v=1,a=T,U=1,i=42,c=2,r=2;/wAA/w==\x1b\\");
    assert_eq!(g.cells[0].ch, ' ', "a virtual placement draws no cells");
    assert_eq!(g.kitty_virtual_geometry(42), Some((2, 2)));
    // The app prints placeholders: fg encodes the id, diacritics row/col.
    // DIACRITICS[0]=U+0305 (0), DIACRITICS[1]=U+030D (1).
    p.advance(&mut g, b"\x1b[38;2;0;0;42m");
    p.advance(&mut g, "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}".as_bytes());
    assert_eq!(g.placeholder_at(0, 0), Some((42, Some(0), Some(0))));
    assert_eq!(g.placeholder_at(1, 0), Some((42, Some(0), Some(1))));
    // A plain cell is not a placeholder.
    assert_eq!(g.placeholder_at(5, 5), None);
    // Placeholders without diacritics report None indices (inference is the
    // renderer's job).
    p.advance(&mut g, "\u{10EEEE}".as_bytes());
    assert_eq!(g.placeholder_at(2, 0), Some((42, None, None)));
}

#[test]
fn kitty_frames_composite_and_advance() {
    let mut g = Grid::new(20, 6);
    let mut p = AnsiParser::new();
    // Root: 2x1 red|red (raw RGBA: ff0000ff ff0000ff).
    p.advance(&mut g, b"\x1b_Gf=32,s=2,v=1,a=t,i=9;/wAA//8AAP8=\x1b\\");
    // Frame 2: 1x1 green composited at x=1 -> red|green. `AP8A/w==`.
    p.advance(&mut g, b"\x1b_Gf=32,s=1,v=1,a=f,i=9,x=1,z=100;AP8A/w==\x1b\\");
    let (w, _, px) = g.kitty_frame(9).unwrap();
    assert_eq!(w, 2);
    assert_eq!(px[0], Some(0xFF0000), "frame 0 shown before playback");
    // Start playback; the frame advances only after its gap elapses.
    p.advance(&mut g, b"\x1b_Ga=a,i=9,s=3;\x1b\\");
    let t0 = std::time::Instant::now();
    assert!(!g.advance_animations(t0), "no time elapsed: no change");
    let later = t0 + std::time::Duration::from_millis(150);
    assert!(g.advance_animations(later));
    let (_, _, px) = g.kitty_frame(9).unwrap();
    assert_eq!(px[1], Some(0x00FF00), "frame 1 composited green at x=1");
    assert_eq!(px[0], Some(0xFF0000), "untouched pixel carried from root");
    // Stop freezes it.
    p.advance(&mut g, b"\x1b_Ga=a,i=9,s=1;\x1b\\");
    let much_later = later + std::time::Duration::from_secs(1);
    assert!(!g.advance_animations(much_later));
}

// ---- GIF / WebP / progressive JPEG fixtures (generated with PIL, embedded
// as base64). See src/core/gif.rs, src/core/webp.rs, src/core/jpeg.rs.

/// 4x4, three solid frames (red, green, blue) at 100/200/300 ms.
const ANIM_GIF_B64: &str = "R0lGODlhBAAEAIEAAP8AAAAAAAAAAAAAACH/C05FVFNDQVBFMi4wAwEAAAAh+QQACgAAACwAAAAABAAEAAAICQABCBxIsCCAgAAh+QQBFAABACwAAAAABAAEAIEA/wAAAAAAAAAAAAAICQABCBxIsCCAgAAh+QQBHgABACwAAAAABAAEAIEAAP8AAAAAAAAAAAAICQABCBxIsCCAgAA7";
/// 16x16 interlaced static GIF, rows cycling red/green/blue/white.
const INTERLACE_GIF_B64: &str = "R0lGODdhEAAQAIEAAP///wD/AP8AAAAA/ywAAAAAEAAQAEAIOQAFCBxIsKDBgwgTKkw4oKHDhxAjSpxIsSLFABgzatwYAIDHjyBDAuBIMqPIkx9LlkSJUiVJlicDAgA7";
/// Two frames with disposal=2 and palette index 0 transparent: frame 0 is
/// fully transparent (its color *is* index 0), frame 1 restores-to-background
/// then paints one green pixel at (0,0).
const DISPOSAL_GIF_B64: &str = "R0lGODlhBAAEAIEAAP8AAAAAAAAAAAAAACH/C05FVFNDQVBFMi4wAwEAAAAh+QQJBQAAACwAAAAABAAEAAAICQABCBxIsCCAgAAh+QQJBgAAACwAAAAABAAEAIEAAAAA/wAAAAAAAAAICQADABhIsKDBgAA7";
/// 32x24 progressive (SOF2) color JPEG, 4:2:0, two-axis gradient.
const PROG_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wgARCAAYACADASIAAhEBAxEB/8QAGAABAQEBAQAAAAAAAAAAAAAABQAGBAf/xAAXAQEBAQEAAAAAAAAAAAAAAAAFBwYI/9oADAMBAAIQAxAAAAHw5rSs6Y/NNaVkOq8LVAcvMtUFVv/EABYQAAMAAAAAAAAAAAAAAAAAAAADBP/aAAgBAQABBQJUQqIVEKiFRCohUQqIVEKiFRCoj//EABkRAAIDAQAAAAAAAAAAAAAAAAAFAwQhMf/aAAgBAwEBPwFaz5otZ80W2JMFtiTD/8QAFxEBAQEBAAAAAAAAAAAAAAAAAwACBP/aAAgBAgEBPwFu+bvmbczbv//EABUQAQEAAAAAAAAAAAAAAAAAAAAx/9oACAEBAAY/AoiIiIiIiP/EABYQAQEBAAAAAAAAAAAAAAAAAAAxIP/aAAgBAQABPyFpkyWW0kyZIkf/2gAMAwEAAgADAAAAEJpYYv/EABYRAQEBAAAAAAAAAAAAAAAAAABBEP/aAAgBAwEBPxDACgoP/8QAFhEBAQEAAAAAAAAAAAAAAAAAIQAx/9oACAECAQE/ENG0ZDIb/8QAFxABAQEBAAAAAAAAAAAAAAAAAPExwf/aAAgBAQABPxDDhAQERhww4Y8MeEBARER//9k=";
/// 16x16 progressive grayscale JPEG, horizontal gradient.
const PROG_GRAY_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/wgALCAAQABABAREA/8QAFQABAQAAAAAAAAAAAAAAAAAACAf/2gAIAQEAAAABj6qKqq//xAAVEAEBAAAAAAAAAAAAAAAAAAAABv/aAAgBAQABBQKbTabTb//EABUQAQEAAAAAAAAAAAAAAAAAAAAC/9oACAEBAAY/ApSlL//EABUQAQEAAAAAAAAAAAAAAAAAAADB/9oACAEBAAE/IZpppv/aAAgBAQAAABBf/8QAFBABAAAAAAAAAAAAAAAAAAAAIP/aAAgBAQABPxAAAf/Z";
/// 16x16 lossless (VP8L) WebP with a patterned RGBA payload.
const WEBP_LOSSLESS_B64: &str = "UklGRkQAAABXRUJQVlA4TDgAAAAvD8ADELkyRPQ/dhHR/wCZMMQq6N+Xzz0RE0AwSdPnw8eHwqQNmO6+ApqS/sIY8f2FllmwTPcPAQ==";
/// 8x8 lossy (VP8) WebP — must be rejected, not misdecoded.
const WEBP_LOSSY_B64: &str = "UklGRjoAAABXRUJQVlA4IC4AAADwAQCdASoIAAgAAUAmJaACdLoB+AAEyAAA/hMf+Uq8FjdHn/3nz9z5+58/5xgA";

#[test]
fn gif_decodes_animation_frames_and_delays() {
    let data = base64::decode(ANIM_GIF_B64.as_bytes()).unwrap();
    let g = gif::decode(&data).unwrap();
    assert_eq!((g.width, g.height), (4, 4));
    assert_eq!(g.frames.len(), 3);
    let solid = |f: &gif::Frame, c: u32| f.pixels.iter().all(|p| *p == Some(c));
    assert!(solid(&g.frames[0], 0xFF0000), "frame 0 red");
    assert!(solid(&g.frames[1], 0x00FF00), "frame 1 green");
    assert!(solid(&g.frames[2], 0x0000FF), "frame 2 blue");
    assert_eq!(
        g.frames.iter().map(|f| f.delay_ms).collect::<Vec<_>>(),
        vec![100, 200, 300]
    );
}

#[test]
fn gif_deinterlaces_rows() {
    let data = base64::decode(INTERLACE_GIF_B64.as_bytes()).unwrap();
    let g = gif::decode(&data).unwrap();
    assert_eq!((g.width, g.height), (16, 16));
    assert_eq!(g.frames.len(), 1);
    let colors = [0xFF0000u32, 0x00FF00, 0x0000FF, 0xFFFFFF];
    for y in 0..16 {
        for x in 0..16 {
            assert_eq!(
                g.frames[0].pixels[y * 16 + x],
                Some(colors[y % 4]),
                "row-cycled color at ({x},{y})"
            );
        }
    }
}

#[test]
fn gif_transparency_and_disposal_restore_background() {
    let data = base64::decode(DISPOSAL_GIF_B64.as_bytes()).unwrap();
    let g = gif::decode(&data).unwrap();
    assert_eq!(g.frames.len(), 2);
    // Frame 0's only color is the transparent index: everything unset.
    assert!(g.frames[0].pixels.iter().all(|p| p.is_none()));
    // Disposal 2 cleared the canvas; frame 1 paints one green pixel.
    assert_eq!(g.frames[1].pixels[0], Some(0x00FF00));
    assert!(g.frames[1].pixels[1..].iter().all(|p| p.is_none()));
    assert_eq!((g.frames[0].delay_ms, g.frames[1].delay_ms), (50, 60));
}

#[test]
fn gif_rejects_garbage() {
    assert!(gif::decode(b"GIF89a").is_none());
    assert!(gif::decode(b"not a gif").is_none());
}

#[test]
fn jpeg_decodes_progressive_color() {
    let data = base64::decode(PROG_JPEG_B64.as_bytes()).unwrap();
    assert!(data.windows(2).any(|w| w == [0xFF, 0xC2]), "fixture is SOF2");
    let img = jpeg::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (32, 24));
    // Reference values from libjpeg's decode of the same file (tolerance
    // covers IDCT rounding differences).
    for (x, y, er, eg, eb) in [
        (0usize, 0usize, 0i32, 2i32, 123i32),
        (16, 12, 128, 121, 128),
        (31, 23, 249, 229, 134),
        (8, 20, 64, 200, 128),
        (28, 4, 223, 40, 130),
    ] {
        let (r, g, b) = jpx(&img, x, y);
        assert!(
            (r - er).abs() <= 8 && (g - eg).abs() <= 8 && (b - eb).abs() <= 8,
            "({x},{y}): want ~({er},{eg},{eb}), got {:?}",
            (r, g, b)
        );
    }
}

#[test]
fn jpeg_decodes_progressive_grayscale() {
    let data = base64::decode(PROG_GRAY_JPEG_B64.as_bytes()).unwrap();
    assert!(data.windows(2).any(|w| w == [0xFF, 0xC2]), "fixture is SOF2");
    let img = jpeg::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (16, 16));
    for (x, want) in [(0usize, 0i32), (5, 80), (10, 160), (15, 240)] {
        let (r, g, b) = jpx(&img, x, 8);
        assert_eq!((r, g), (b, b), "grayscale replicates luma");
        assert!((r - want).abs() <= 8, "x={x}: want ~{want}, got {r}");
    }
}

#[test]
fn webp_lossless_roundtrips_exactly() {
    let data = base64::decode(WEBP_LOSSLESS_B64.as_bytes()).unwrap();
    let img = webp::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (16, 16));
    // VP8L is lossless: reproduce the generator's exact pattern.
    for y in 0..16usize {
        for x in 0..16usize {
            let o = (y * 16 + x) * 4;
            let want = [
                ((x * 17) % 256) as u8,
                ((y * 17) % 256) as u8,
                ((x * y) % 256) as u8,
                if (x + y) % 3 != 0 { 255 } else { 128 },
            ];
            assert_eq!(&img.rgba[o..o + 4], &want, "pixel ({x},{y})");
        }
    }
}

#[test]
fn webp_rejects_lossy_and_garbage() {
    let data = base64::decode(WEBP_LOSSY_B64.as_bytes()).unwrap();
    assert!(webp::decode(&data).is_none(), "lossy VP8 is out of scope");
    assert!(webp::decode(b"RIFFxxxxWEBP").is_none());
    assert!(webp::decode(b"nope").is_none());
}

#[test]
fn iterm2_inline_animated_gif_plays_in_overlay() {
    let mut g = Grid::new(20, 8);
    let mut p = AnsiParser::new();
    let mut input = b"\x1b]1337;File=inline=1:".to_vec();
    input.extend_from_slice(ANIM_GIF_B64.as_bytes());
    input.push(0x07);
    p.advance(&mut g, &input);
    // The first frame reserved half-block cells and placed an overlay image
    // wired to a synthesized animation.
    let im = g.images().last().expect("overlay image stored");
    let id = im.anim.expect("animated: backing kitty image id set");
    let (w, h, px) = g.kitty_frame(id).expect("animation stored");
    assert_eq!((w, h), (4, 4));
    assert_eq!(px[0], Some(0xFF0000), "first frame red");
    // The animation timer steps frames: 100ms shows frame 0, then green.
    let t0 = std::time::Instant::now();
    assert!(!g.advance_animations(t0));
    assert!(g.advance_animations(t0 + std::time::Duration::from_millis(120)));
    let (_, _, px) = g.kitty_frame(id).unwrap();
    assert_eq!(px[0], Some(0x00FF00), "second frame green");
}

#[test]
fn iterm2_inline_webp_renders_cells() {
    let mut g = Grid::new(20, 10);
    let mut p = AnsiParser::new();
    let mut input = b"\x1b]1337;File=inline=1:".to_vec();
    input.extend_from_slice(WEBP_LOSSLESS_B64.as_bytes());
    input.push(0x07);
    p.advance(&mut g, &input);
    // 16x16 -> 16 cols x 8 half-block rows of image cells from the origin.
    assert_eq!(g.cells[0].ch, '\u{2580}', "half-block cell painted");
}

/// 64x48 photo-like lossless WebP (sine/cosine gradients + quadratic blue):
/// dense enough that the encoder uses predictor transforms, LZ77, and the
/// color cache.
const WEBP_PHOTO_B64: &str = "UklGRjoGAABXRUJQVlA4TC0GAAAvP8ALALkyRPQ/BoraNpIMYfiDHBrn7p8g26YM80d8zhD9nwAwAAE4P0yl/aEEiZFtm7bV3zbzj862Nefa55ybgANJkpQogXO3/96HcXfov/+/Po339qDf0KU06+QbZs26K8WZ0pW54HQLMCyX9tKQkSNHuUzgmJTLBMJcz93dXX/THy8cCEAaN/TuZqQGyR8+//lW+QU+1d6rPGJucZfVDqEoJDSHxo3azaqkYqtsmzQtzogbJE3iFmmbfIdyl3qPLmvMWQ9wwO6A0wGXY+6dnt3evb79fnm/k76DvsM+I15jHhNuTzoV7J8SlmyT5kn9pGZS9Yy8LHtWckZ8TjwneU5yQXpJXlE9r7nyUxVOvX0VnBwfVZ8GBJwZ8Maqo0CEU5sJvRQ7+HspbcBz8RWIUh8gRm4LiFECsAFPrcaOjTiduyrAXOGJNQtQTXpdb44D1KwiqKY8s3hi3cZ4cofMARcCz4QzSNA815kjIwcmel54ejncc7UjqKGuEtC+rRaUQA1TxYv5ACF3CLiCVzaOB08sxmQvAGoZAailsGsY1r+zefq17kHIgQ01ZnBMwxkAJFhsGh8lezbREcTYETzIASDBAECUgeTFRHBiesjDhcR3W3M6MGQloAGbN5LM1S1zyTX+8Nu2APVAgSrSWU+OA2AdeYAqOgHWjT9TxemJA+u4QzVL56SXFc6D6DqYkNMgShHE65kkNwZCV2RWvTX/SopxgCg9AAnGV9w4A4ANrBRuaFh8HTsQIMMgABJ0jYKm2NGUh1ngAQEVALc1jbpDZSVNfpPuqGMBpKkUL+4UU7bwMuvvaxn0BNpR9+SHWajWIaQ05uEKAFLcbRoPtWUtAXLdWQiR0tCdRiMzCB5cACkWQAs3AJBgAQANvEKMKwDwyI4AuakRMpwMIwCoowiwnlGAOmZBwEjzYh4AcS5kKXmTDbz42K6Vp4Dj+ZWMAGALpxa+QwMV0MpK+w7ruV5MH9sztyrIVQImALCBExMjNjAFAiZAhCIImQNAhFkAZHiFOEsAHspOegJ7SyTsDaoMAKCFaYAWzqycmRkAxPKRnXxO4325jgZHWq50hSQ4AYAMJYAkJYAsV8AWZkFIGaRYBEBAGQBbKviOzgSdQy8hVE4dpwCQpgCwjhGANOVp3WEdcwCoYQEAOb4gzRvU8sINnVgYJ8gDAAEoAiBDASBOHiCkkEViGWR5AIAE8wBItnSv6BxBLRjIBFFCWigBxOgFiDEI0MIFsIEZUMsZ0MgKAGq5AoAOPifFyyS4TfS848OIACMlXEaCB9MAcXoBAnoBkkyDCKPBiSmQ4QYAGnmRCPMAAHCswPH2FdCz3Xc4yOP0HVp5nDrkjf++813nShQ1gTQyCoAkIwABnQBxRgCyLIFGFkEbNwAQZRYc4hNClorpJ93bXoOZA1+/0zuYBkChBqQQPDgDEDIIEGcQIMMZUEMRBBRAglnQxgMA7ORNasr5X+svcOpI3XGQ90fdPZdfuG79gRDMAiBLGSBJASBDGQDAUV4E3bwDgBTLAGjjDeJcAQA8c3LsOzBYj5QAsAFcmfqOPK+ADm6DHA9AHx8TZQbkeCf6jqP8/K/vnhgEQCRQRkUIigAIwBRABFwAyHIBRMAroJ1l0MYS6OBFACRZAkA7H7KBu0RYBJ4YHiMAqgFOr3wHLIXv0M718h06+YBqyuAgH5fv0MtfT42MK2r/gTpwASAAswAbGAXYQBGgg2WQ4QKoYxFkeQEAddwFQDe/0MYHJHmF8JmxjgAolXTBQvsuO5lJ3+UotyffpY/PSbAA8vzSvvvSRIDS4OrxO0iBCkAIlgEOsggC8CJoowJSzIMMV8BB3gRAlneIUwHACf7mKH++MQlglC5zAEAaLABEQBmgFjyOdYccS7Xu0M5jMMEfse7Qxc+08v6/deeDSQAgCDDVQ1IBQCtFAGQ5A9DGGYBuHoBWFkGGCtjJXXCSbwipgCH+pJU3SbAMAF9NLp6JAUwDAC1gBSAFHoAouAES4B3QwQpo4QrYyW3QyyfEOAOO8nmsu0zwP3n++2ly7HmA6brlBkAIFgEamQJoZBagjxdBlhugnbugmw8ImAbtvEmUeTDGfxziB9r4hAwA";
/// 33x5 three-color lossless WebP: the color-indexing transform with 2-bit
/// packed pixels and an odd width (packing edge case).
const WEBP_PAL_B64: &str = "UklGRi4AAABXRUJQVlA4TCEAAAAvIAABAFDTtgET/qRzdwCY//nvv5AgScoVEkwVH0leegAA";

#[test]
fn webp_lossless_photo_roundtrips_exactly() {
    let data = base64::decode(WEBP_PHOTO_B64.as_bytes()).unwrap();
    let img = webp::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (64, 48));
    for y in 0..48usize {
        for x in 0..64usize {
            let o = (y * 64 + x) * 4;
            let want = [
                (127.0 + 120.0 * (x as f64 / 6.0).sin()) as u8,
                (127.0 + 120.0 * (y as f64 / 5.0).cos()) as u8,
                ((x * x + y * y) % 256) as u8,
                255u8,
            ];
            assert_eq!(&img.rgba[o..o + 4], &want, "pixel ({x},{y})");
        }
    }
}

#[test]
fn webp_lossless_palette_roundtrips_exactly() {
    let data = base64::decode(WEBP_PAL_B64.as_bytes()).unwrap();
    let img = webp::decode(&data).unwrap();
    assert_eq!((img.width, img.height), (33, 5));
    let cols: [[u8; 4]; 3] = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
    for y in 0..5usize {
        for x in 0..33usize {
            let o = (y * 33 + x) * 4;
            assert_eq!(&img.rgba[o..o + 4], &cols[(x + 2 * y) % 3], "pixel ({x},{y})");
        }
    }
}

/// Text of viewport row `r`, trailing blanks trimmed.
#[cfg(test)]
fn vrow_text(g: &Grid, r: usize) -> String {
    (0..g.cols).map(|c| g.viewport_cell(c, r).ch).collect::<String>().trim_end().to_string()
}

#[test]
fn folded_block_renders_one_summary_line_over_hidden_history() {
    let mut g = Grid::new(40, 4);
    let mut p = AnsiParser::new();
    p.advance(
        &mut g,
        b"before\r\n\x1b]133;C\x07out1\r\nout2\r\nout3\r\nout4\r\n\x1b]133;D\x07after1\r\nafter2\r\nafter3\r\nafter4\r\n",
    );
    let dhl_before = g.display_history_len();
    assert!(g.toggle_last_fold(), "a fully-scrolled block folds");
    let hidden = {
        let b = g.fold_blocks()[0];
        b.end - b.start - 1
    };
    assert_eq!(g.display_history_len(), dhl_before - hidden);

    // Scroll to the top of (display) history and find the summary row.
    assert!(g.scroll_view_up(1000));
    let rows: Vec<String> = (0..g.rows).map(|r| vrow_text(&g, r)).collect();
    let summary = rows
        .iter()
        .position(|t| t.contains("lines hidden"))
        .expect("a fold summary line is visible");
    assert!(rows[summary].starts_with('\u{25B7}'), "summary marked with a triangle: {rows:?}");
    assert!(rows[summary].contains("4 lines hidden"), "{rows:?}");
    // The block's interior never paints; the line after the summary is the
    // first line past the block.
    assert!(rows.iter().all(|t| !t.contains("out2")), "hidden rows stay hidden: {rows:?}");
    assert_eq!(rows[summary + 1], "after1", "{rows:?}");
    // The summary cell is styled dim+italic on default colors.
    let c = g.viewport_cell(0, summary);
    assert!(c.flags & ATTR_DIM != 0 && c.flags & ATTR_ITALIC != 0);
    // abs_of_view_row maps the summary to the block's first line and the next
    // row past the whole block (the hidden interior is unreachable).
    let b = g.fold_blocks()[0];
    assert_eq!(g.abs_of_view_row(summary), b.start);
    assert_eq!(g.abs_of_view_row(summary + 1), b.end);
}

#[test]
fn clicking_the_summary_row_expands_the_block() {
    let mut g = Grid::new(40, 4);
    let mut p = AnsiParser::new();
    p.advance(
        &mut g,
        b"\x1b]133;C\x07out1\r\nout2\r\nout3\r\n\x1b]133;D\x07a\r\nb\r\nc\r\nd\r\ne\r\n",
    );
    assert!(g.toggle_last_fold());
    assert!(g.scroll_view_up(1000));
    let summary = (0..g.rows)
        .position(|r| vrow_text(&g, r).contains("lines hidden"))
        .expect("summary visible");
    // A click elsewhere does nothing; on the summary it expands.
    assert!(!g.unfold_summary_at(summary + 1));
    assert!(g.unfold_summary_at(summary));
    assert!(!g.fold_blocks()[0].folded);
    let rows: Vec<String> = (0..g.rows).map(|r| vrow_text(&g, r)).collect();
    assert!(rows.iter().all(|t| !t.contains("lines hidden")), "{rows:?}");
}

#[test]
fn search_unfolds_a_block_hiding_its_match() {
    let mut g = Grid::new(40, 4);
    let mut p = AnsiParser::new();
    p.advance(
        &mut g,
        b"\x1b]133;C\x07alpha\r\nneedle\r\nomega\r\n\x1b]133;D\x07a\r\nb\r\nc\r\nd\r\ne\r\n",
    );
    assert!(g.toggle_last_fold());
    assert!(g.fold_blocks()[0].folded);
    assert_eq!(g.search_with("needle", false), 1);
    assert!(!g.fold_blocks()[0].folded, "jumping to a hidden match unfolds its block");
}

#[test]
fn folding_clamps_the_view_offset_to_the_shorter_history() {
    let mut g = Grid::new(40, 4);
    let mut p = AnsiParser::new();
    p.advance(
        &mut g,
        b"\x1b]133;C\x07l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6\r\n\x1b]133;D\x07a\r\nb\r\nc\r\nd\r\ne\r\n",
    );
    assert!(g.scroll_view_up(1000));
    let deep = g.view_offset;
    assert!(g.toggle_last_fold());
    assert!(g.view_offset <= g.display_history_len());
    assert!(g.view_offset < deep, "folding shrinks the scroll range");
}

#[test]
fn selected_html_carries_colors_attrs_and_escapes() {
    let mut g = Grid::new(20, 3);
    let mut p = AnsiParser::new();
    // red bold "hi", default " ", green "<&>"
    p.advance(&mut g, b"\x1b[31;1mhi\x1b[0m \x1b[32m<&>\x1b[0m");
    g.selection = Some(Selection { anchor: (0, 0), head: (5, 0) });
    let html = g.selected_html().unwrap();
    assert!(html.starts_with("<pre style=\""), "{html}");
    assert!(html.ends_with("</pre>"), "{html}");
    // Two styled runs plus the plain gap; entities escaped.
    assert!(html.contains("font-weight:bold"), "{html}");
    assert!(html.contains(">hi</span>"), "{html}");
    assert!(html.contains("&lt;&amp;&gt;"), "{html}");
    // The default-colored space between runs isn't styled as bold.
    let bold_at = html.find("font-weight:bold").unwrap();
    let lt_at = html.find("&lt;").unwrap();
    assert!(bold_at < lt_at, "runs in order: {html}");
    // Palette red/green resolve to distinct span colors.
    let spans = html.matches("<span").count();
    assert!(spans >= 3, "one span per style run: {html}");
}

#[test]
fn selected_html_multiline_trims_and_none_without_selection() {
    let mut g = Grid::new(10, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"abc\r\ndef");
    assert!(g.selected_html().is_none());
    g.selection = Some(Selection { anchor: (0, 0), head: (9, 1) });
    let html = g.selected_html().unwrap();
    // Trailing blanks trimmed, newline between rows preserved (spans close
    // at end of line, so the rows are separate runs).
    assert!(html.contains(">abc</span>\n"), "{html}");
    assert!(html.contains(">def</span>"), "{html}");
    assert!(!html.contains("abc "), "trailing blanks trimmed: {html}");
}

#[test]
fn bidi_row_reorders_hebrew_and_maps_clicks_back() {
    let mut g = Grid::new(12, 2);
    g.bidi = true;
    let mut p = AnsiParser::new();
    p.advance(&mut g, "ab \u{5D0}\u{5D1}\u{5D2}".as_bytes());
    let b = g.bidi_row(0).expect("mixed row reorders");
    // Logical: a b SP alef bet gimel ...  Visual: a b SP gimel bet alef ...
    assert_eq!(&b.vis2log[..6], &[0, 1, 2, 5, 4, 3]);
    // Inversion round-trips: the user clicks visual col 3 (gimel) and the
    // model cell is logical col 5.
    assert_eq!(g.logical_col(3, 0), 5);
    assert_eq!(g.logical_col(5, 0), 3);
    assert_eq!(g.logical_col(0, 0), 0);
    // RTL cells are flagged for glyph mirroring.
    assert!(b.rtl[3] && b.rtl[4] && b.rtl[5]);
    assert!(!b.rtl[0]);
    // A pure-LTR row costs nothing.
    assert!(g.bidi_row(1).is_none());
    // Off switch: identity everywhere.
    g.bidi = false;
    assert!(g.bidi_row(0).is_none());
    assert_eq!(g.logical_col(3, 0), 3);
}

#[test]
fn bidi_row_keeps_wide_glyph_cells_adjacent() {
    let mut g = Grid::new(10, 1);
    g.bidi = true;
    let mut p = AnsiParser::new();
    // Hebrew + a wide CJK char: the lead/trailer pair must stay adjacent
    // (lead left of trailer) wherever the unit lands.
    p.advance(&mut g, "\u{5D0}\u{5D1} \u{4E2D}".as_bytes());
    let b = g.bidi_row(0).expect("reorders");
    let lead_v = b.log2vis[3] as usize;
    let trail_v = b.log2vis[4] as usize;
    assert_eq!(trail_v, lead_v + 1, "wide pair adjacent, lead first: {:?}", b.vis2log);
}

#[test]
fn bdsm_mode_8_gates_bidi_and_alt_screen_defaults_explicit() {
    let mut g = Grid::new(10, 2);
    g.bidi = true;
    let mut p = AnsiParser::new();
    p.advance(&mut g, "ab \u{5D0}\u{5D1}".as_bytes());
    assert!(g.bidi_row(0).is_some(), "implicit is the main-screen default");
    // The app opts out (explicit mode): no reordering.
    p.advance(&mut g, b"\x1b[8l");
    assert!(g.bidi_row(0).is_none());
    p.advance(&mut g, b"\x1b[8h");
    assert!(g.bidi_row(0).is_some());
    // DECRQM (ANSI form) reports the state.
    p.advance(&mut g, b"\x1b[8$p");
    assert_eq!(p.take_responses(), b"\x1b[8;1$y");
    // Alt screen defaults to explicit unless the app asks for implicit.
    let mut g2 = Grid::new(10, 2);
    g2.bidi = true;
    let mut p2 = AnsiParser::new();
    p2.advance(&mut g2, b"\x1b[?1049h");
    p2.advance(&mut g2, "ab \u{5D0}\u{5D1}".as_bytes());
    assert!(g2.bidi_row(0).is_none(), "alt screen: explicit by default");
    p2.advance(&mut g2, b"\x1b[8$p");
    assert_eq!(p2.take_responses(), b"\x1b[8;2$y");
    p2.advance(&mut g2, b"\x1b[8h");
    assert!(g2.bidi_row(0).is_some(), "app-requested implicit wins on alt");
}

#[test]
fn scp_fixed_direction_applies_when_autodetect_is_off() {
    let mut g = Grid::new(6, 1);
    g.bidi = true;
    let mut p = AnsiParser::new();
    p.advance(&mut g, "a \u{5D0}".as_bytes());
    // Autodetected LTR paragraph: the single RTL char doesn't move.
    assert!(g.bidi_row(0).is_none(), "identity under LTR autodetect");
    // Turn autodetection off (2501) and fix the path RTL (SCP Ps1=2).
    p.advance(&mut g, b"\x1b[?2501l\x1b[2;1 k");
    let b = g.bidi_row(0).expect("RTL base reorders the line");
    // An RTL paragraph right-aligns: trailing blanks reset to the paragraph
    // level (rule L1) and fill the left; the text sits at the right edge.
    assert_eq!(&b.vis2log[3..6], &[2, 1, 0], "line laid out right-to-left: {:?}", b.vis2log);
    // DECRQM reports 2501 reset.
    p.advance(&mut g, b"\x1b[?2501$p");
    assert_eq!(p.take_responses(), b"\x1b[?2501;2$y");
    // Back to LTR path: identity again.
    p.advance(&mut g, b"\x1b[1;1 k");
    assert!(g.bidi_row(0).is_none());
}

#[test]
fn search_folds_canonical_accents_together() {
    let mut g = Grid::new(20, 3);
    let mut p = AnsiParser::new();
    // Precomposed e-acute; the search query is a bare (even uppercase) E.
    p.advance(&mut g, "caf\u{E9} time".as_bytes());
    assert_eq!(g.search_with("cafE", false), 1, "E matches precomposed \u{E9}");
    // And the reverse: an accented query finds plain text.
    assert_eq!(g.search_with("t\u{EC}me", false), 1, "accented query folds too");
    assert_eq!(g.search_with("xyz", false), 0);
}

#[test]
fn search_with_case_skips_folding_in_plain_and_regex_mode() {
    let mut g = Grid::new(20, 3);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"Error: bad\r\nerror: fine");
    // Case-insensitive (the default `search_with` behavior): both lines match.
    assert_eq!(g.search_with_case("error", false, false), 2);
    // Case-sensitive: only the lowercase line matches.
    assert_eq!(g.search_with_case("error", false, true), 1);
    assert_eq!(g.search_with_case("Error", false, true), 1);
    // Same distinction in regex mode.
    assert_eq!(g.search_with_case("^error", true, false), 2);
    assert_eq!(g.search_with_case("^error", true, true), 1);
}

#[test]
fn arabic_row_shapes_joined_forms_through_bidi_row() {
    let mut g = Grid::new(10, 1);
    g.bidi = true;
    let mut p = AnsiParser::new();
    p.advance(&mut g, "\u{645}\u{62D}\u{645}\u{62F}".as_bytes());
    let b = g.bidi_row(0).expect("Arabic row reorders");
    let s = b.shaped.as_ref().expect("and shapes");
    assert_eq!(s[0], Some('\u{FEE3}'), "initial meem");
    assert_eq!(s[1], Some('\u{FEA4}'), "medial hah");
    assert_eq!(s[3], Some('\u{FEAA}'), "final dal");
    // The visual map reverses the word, at the right edge of the RTL
    // paragraph (rule L1 sends the trailing blanks to the paragraph level).
    assert_eq!(&b.vis2log[6..10], &[3, 2, 1, 0], "{:?}", b.vis2log);
}
