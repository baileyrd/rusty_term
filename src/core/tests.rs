use super::cell::*;
use super::color::*;
use super::grid::*;
use super::parser::*;

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
fn resize_preserves_content_and_clamps_cursor() {
    let mut g = parse(b"hello", 80, 24);
    g.cursor = (40, 20);
    g.resize(10, 5);
    assert_eq!(g.cols, 10);
    assert_eq!(g.rows, 5);
    assert_eq!(&row_text(&g, 0)[..5], "hello"); // top-left content kept
    assert_eq!(g.cursor, (9, 4)); // clamped into new bounds
    assert!(g.dirty.iter().all(|&d| d)); // full repaint queued
    // Growing back keeps the surviving content and blanks new area.
    g.resize(80, 24);
    assert_eq!(&row_text(&g, 0)[..5], "hello");
    assert_eq!(g.cells[79].ch, ' ');
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
    assert_eq!(row_text(&g, 3).trim_end(), "");      // region bottom blanked
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
    assert_eq!(g.cells[0].combining[0], '\u{0301}'); // mark composed onto 'a'
    assert_eq!(g.cells[1].ch, 'b'); // mark consumed no cell
    assert_eq!(g.cursor, (2, 0));
}

#[test]
fn multiple_combining_marks_and_overflow() {
    let mut g = Grid::new(80, 24);
    g.put_char('e', Pen::default());
    // Two marks fill both slots; a third is dropped (bounded).
    g.put_char('\u{0301}', Pen::default());
    g.put_char('\u{0323}', Pen::default());
    g.put_char('\u{0308}', Pen::default());
    assert_eq!(g.cells[0].combining, ['\u{0301}', '\u{0323}']);
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
    assert_eq!(g.cells[0].combining[0], '\u{0301}'); // on the head, not the trailer
    assert_ne!(g.cells[1].flags & WIDE_TRAILER, 0);
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
    // Both the bare and explicit-0 forms are queries.
    p.advance(&mut g, b"\x1b[c");
    assert_eq!(p.take_responses(), b"\x1b[?1;2c");
    p.advance(&mut g, b"\x1b[0c");
    assert_eq!(p.take_responses(), b"\x1b[?1;2c");
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
fn no_query_means_no_response() {
    // A normal print run owes the host nothing.
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"hello\x1b[31mworld");
    assert!(p.take_responses().is_empty());
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
    let line: String = g.scrollback[0].iter().map(|c| c.ch).collect();
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
fn osc_52_query_is_not_forwarded() {
    let mut g = Grid::new(80, 24);
    let mut p = AnsiParser::new();
    p.advance(&mut g, b"\x1b]52;c;?\x07");
    assert!(g.take_host_out().is_empty());
}

#[test]
fn osc_8_stamps_link_on_covered_cells() {
    let g = parse(b"\x1b]8;;http://example.com\x1b\\AB\x1b]8;;\x1b\\C", 80, 24);
    assert_ne!(g.cells[0].link, 0);
    assert_eq!(g.cells[0].link, g.cells[1].link); // A and B share the link
    assert_eq!(g.links[(g.cells[0].link - 1) as usize], "http://example.com");
    assert_eq!(g.cells[2].link, 0); // C is after the close
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
    let line: String = g.scrollback[0].iter().map(|c| c.ch).collect();
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
    // A tab also clears the memory (the spaces it emits aren't a "last char").
    let g = parse(b"A\t\x1b[2b", 80, 24);
    // 'A' at col 0, tab to col 8, REP repeats nothing.
    assert_eq!(g.cells[0].ch, 'A');
    assert_eq!(g.cursor, (8, 0));
}
