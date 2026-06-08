# Feature Backlog

15 enhancements were identified for rusty_term via review. **The statuses below
were re-audited against the source tree on 2026-06-07** — the earlier bulk
"completed (2026-06-07)" stamps did not match the code. **All 15 items** are now
implemented. Each entry
records what exists and what is missing, grounded in the symbols/files that
back it.

## 1. In-window scrollback browsing
Add mouse wheel + Shift+PageUp/Down scrollback browsing to the `gui` backend (grid already retains history).

- **Status:** done (2026-06-06)
- **Notes:** Mouse wheel, Shift+PageUp/Down, and Ctrl+Shift+PageUp/Down (prompt-to-prompt, OSC 133) browse the active tab's history; typing snaps back to the live bottom. `Grid::{view_offset, scroll_view_up, scroll_view_down, scroll_to_prev_prompt, reset_view}` + `viewport_cell`; `src/gui/window.rs::scroll_key`; composited by `src/gui/{cpu,gpu}.rs`. Tests: `viewport_composites_history_above_live_grid`, `scroll_view_clamps_and_resets`, `no_history_browsing_on_alt_screen`.

## 2. Native SGR/1006 mouse reporting
Emit SGR/1006 mouse reporting from the window backend so child apps receive clicks/scrolls.

- **Status:** done (2026-06-07)
- **Notes:** When the child enables mouse tracking (`?1000`/`?1002`/`?1003` with SGR `?1006`), the window backend reports **left click/release and the wheel** as SGR/1006 events, gated on the tracking state; the wheel falls back to local scrollback browsing when the child isn't tracking. The parser now records modes into `Grid::mouse_modes` (mirroring `bracketed_paste`); `src/gui/mouse.rs::SgrEncoder` encodes; `src/gui/window.rs::report_mouse` wires it. Tests: `core::tests::{mouse_modes_tracked_for_window_backend, sgr_extended_mouse_flag_tracked_and_reset_by_ris}` and `gui::mouse::tests::*`.
- **Not yet:** drag/motion reporting (`?1002`/`?1003` any-event on `CursorMoved`) and right/middle buttons — only left + wheel are emitted.
- **History:** previously marked complete but did **not compile** (it called non-existent `Grid::encode_mouse_*` and never recorded mode state, so reporting was permanently inert). Fixed and wired 2026-06-07.

## 3. Scrollback search/find overlay
Add incremental search with highlight + next/prev using existing reflowed logical lines.

- **Status:** done (2026-06-07)
- **Notes:** `Grid::search` (`src/core/grid.rs`) scans the scrollback + live screen, joining soft-wrapped rows into logical lines so a query matches across a wrap, case-insensitively (ASCII); it stores per-row highlight spans + per-match anchors (bounded by `SEARCH_MAX`) and scrolls the first match into view. `search_highlight`/`search_jump`/`search_status`/`clear_search` drive rendering and next/prev (cycling, viewport snapped via `scroll_to_abs`). The window backend (`src/gui/window.rs`) adds a `Search` keymap action (Ctrl+Shift+F) that enters an incremental search mode: typing edits the query (re-searching live), Enter / Shift+Enter step matches, Esc exits; a find bar (`Find: <q>   n/m`) shows in the chrome row. Both renderers highlight matches amber / the active one orange (`search_highlight`, like `is_selected` — no signature change). Tests: `core::tests::{search_finds_matches_across_scrollback_and_screen, search_matches_across_a_soft_wrap, search_jump_cycles_and_clear_resets}`, `gui::cpu::tests::search_match_cell_is_highlighted`.

## 4. Split panes within a tab
Horizontal/vertical splits using existing Tab/PTY plumbing and cell-based chrome layout.

- **Status:** done (2026-06-07)
- **Notes:** New `src/gui/layout.rs` is a toolkit-free binary split tree (`Layout` over pane ids) that tiles a tab's cell area into rects with one-cell dividers (unit-tested). A `Tab` now holds `Vec<Pane>` + a `Layout` + a focused pane id; `Pane` is the former per-shell state (grid/parser/PTY). The renderer trait paints `&[PaneFrame]` (each grid at a cell offset, cursor/IME only on the focused pane); `cpu::draw_grid` and `gpu::append_grid` were extracted to draw one grid at `(col0, row0)`, the window clears gaps to a divider color. Keys (rebindable): `split_right` (Ctrl+Shift+D), `split_down` (Ctrl+Shift+E), `focus_next` (Ctrl+Shift+J); `close_tab` (Ctrl+Shift+W) now closes the focused pane (last pane closes the tab). Each pane runs its own shell, is resized to its rect (grid + PTY) on window resize/split, and click-to-focus + pane-local selection/links/mouse use `pane_under`/`cell_in_focused`. Tests: `gui::layout::tests::*` (7), `gui::cpu::tests::draw_grid_honors_cell_offset`.

## 5. Clickable OSC 8 hyperlinks
Support Ctrl+click to open OSC 8 hyperlinks via system opener.

- **Status:** done (2026-06-07)
- **Notes:** The parser interns OSC 8 link URIs onto covered cells (`src/core/osc.rs` code "8"; tests `osc_8_*`); `Grid::link_at` resolves the URI under a viewport `(col, row)`, compositing scrollback so links in history resolve too. In the window backend, **Ctrl+left-click** hit-tests the cell under the pointer (`App::open_link_under_pointer` in `src/gui/window.rs`) and hands the URI to the OS opener (`open_url`: `cmd /C start` / `open` / `xdg-open`), suppressing selection and mouse reporting for that click. `is_openable_url` restricts schemes to http/https/ftp/file/mailto so arbitrary terminal output can't drive the shell's URL handler. Tests: `core::tests::link_at_resolves_covered_cells`, `gui::window::tests::only_known_url_schemes_are_openable`.

## 6. Configurable DECSCUSR cursor styles
Support block/underline/bar + steady/blinking, configurable via settings.

- **Status:** done (2026-06-07)
- **Notes:** DECSCUSR (`CSI Ps SP q`) sets `Grid::{cursor_shape, cursor_blink}` (`src/core/parser.rs`; 0/1 blinking block, 2 steady block, 3/4 underline, 5/6 bar) and is relayed to the host for TUI mode; RIS/DECSTR restore the configured default. Both renderers draw block (cell invert), underline (bottom stripe), and bar (left stripe): `src/gui/cpu.rs` overlays pixels, `src/gui/gpu.rs` overlays in the fragment shader (new per-instance `curs`/`ccol`). The window event loop animates blink (`about_to_wait`, 530ms). Config keys `cursor_style` (block/underline/bar + aliases) and `cursor_blink` (bool) set the default. Tests: `core::tests::{decscusr_sets_cursor_shape_and_blink, decscusr_is_relayed_to_host_not_printed, ris_restores_configured_default_cursor}`, `config::tests::cursor_style_*`, `gui::cpu::tests::{underline_cursor_*, bar_cursor_*, blinking_cursor_*}`.

## 7. User-configurable keybindings
Move new-tab, copy, prompt-nav, etc out of compile-time constants into config file.

- **Status:** done (2026-06-07)
- **Notes:** A new toolkit-free `src/keymap.rs` defines the terminal-owned `Action`s (copy/paste/new-tab/close-tab/next-tab/prev-tab/open-config/scroll page+prompt up/down), a `Chord` (ctrl/shift/alt + `Key`), and a `Keymap` whose `Default` holds the built-in bindings. The `[keys]` config section rebinds any action (`copy = "Ctrl+Alt+C"`, `next_tab = "Ctrl+Tab"`, …) via `parse_action`/`parse_chord`, validated with warnings on unknown action or malformed chord. `src/gui/window.rs` maps winit keys to `keymap::Key` (`chord_key`) and dispatches the resolved action through `run_action` instead of hard-coded `KeyCode` matches. Tests: `keymap::tests::*` (defaults, rebind, chord/action parsing) and `config::tests::{keys_section_rebinds_actions, keys_section_warns_on_bad_action_or_chord}`.

## 8. IME/composition events in winit backend
Wire IME/composition events for CJK and dead-key input.

- **Status:** done (2026-06-07)
- **Notes:** `resumed` calls `set_ime_allowed(true)`; `src/gui/window.rs` handles `WindowEvent::Ime` — `Preedit` stores the composition into `Grid::ime_preedit` and repositions the candidate popup at the cursor (`update_ime_area` → `set_ime_cursor_area`), `Commit` clears it and writes the committed text (CJK + dead keys) to the child, `Disabled` clears it. A composing guard skips native key encoding while a preedit is active so input isn't doubled. Both renderers draw the preedit reverse-video over the cells at the cursor (`src/gui/cpu.rs`, `src/gui/gpu.rs`, reading `Grid::ime_preedit` — no renderer signature change). Test: `gui::cpu::tests::ime_preedit_overlays_reverse_video_at_cursor`.

## 9. Desktop notifications via OSC 9/777
Implement OSC 9 and OSC 777 notification support.

- **Status:** done (2026-06-07)
- **Notes:** `src/core/osc.rs` records OSC 9 (iTerm2, free-text message — ConEmu's numeric `9;N;…` progress subcommands are excluded) and OSC 777 (`777;notify;<title>;<body>`) into `Grid::notifications` (bounded by `push_notification`); both are also relayed to the host for TUI mode. The window backend drains them per tab on output (`App::service_notifications`) and raises an OS notification via `notify` — per-platform with no new crates (PowerShell `NotifyIcon` / `osascript` / `notify-send`), passing the untrusted title/body as environment variables to avoid command injection. Tests: `core::tests::{osc_9_records_notification_and_relays, osc_9_conemu_progress_is_not_a_notification, osc_777_parses_title_and_body, osc_777_non_notify_is_ignored}`.

## 10. Windows host resize propagation in TUI
Fix host console size change detection and reflow in TUI/conhost mode.

- **Status:** done (2026-06-07)
- **Notes:** The Windows runtime polls the console size every 150ms (there is no SIGWINCH) and applies changes: `src/runtime/tokio_rt.rs` `resize_poll` -> `backend.terminal_size()` (`GetConsoleScreenBufferInfo`) -> `Grid::resize` + `set_winsize` (`ResizePseudoConsole`). The README's "known gap" caution is stale.

## 11. Font fallback + variants + ligatures
Add font fallback chains, bold/italic variants, and optional ligature shaping.

- **Status:** done (2026-06-08) — fallback + variants + ligatures (CPU renderer)
- **Notes:** `src/gui/font.rs` holds a `Style` (Regular/Bold/Italic/BoldItalic) and a `FontCache` with four faces + a fallback chain, caching glyphs by `(char, Style)`; `glyph(ch, style)` picks the styled face (or regular if absent) and, when it lacks `ch`, the first fallback font that covers it (`face_for`). `load_set` loads the regular face plus explicit `[window]` `font_bold`/`font_italic`/`font_bold_italic`/`font_fallback` paths, with filename-derived siblings of `font` and built-in system CJK/symbol fonts as fallbacks. Renderers pass `Style::new(cell bold, italic)` per cell (the gpu atlas is keyed by `(char, Style)`). **Ligatures:** `src/gui/shape.rs` is a hand-rolled GSUB shaper over `ttf-parser` (already in the tree via `ab_glyph` -> `owned_ttf_parser`, so no new compiled crate) that applies the `liga`/`calt` features — single (type 1), ligature (type 4), and context / chained-context (types 5/6, all three formats, with nested lookup records applied recursively); unsupported substitutions pass through. `FontCache` builds a per-face `Shaper`, maps a run's chars to glyph ids via the face cmap, shapes them, and rasterizes shaped glyph ids (`glyph_by_gid`, cached by `(gid, Style)`). The CPU renderer (`src/gui/cpu.rs::draw_grid`) shapes each row's maximal runs of same-style, same-fg, single-width, non-highlighted cells into a glyph plan; a ligature glyph is drawn once (its wide bitmap overdraws the cells it spans) and the consumed columns draw nothing — cursor/selection/search cells stay out of runs so ligatures never split across them. Toggled by the `[window]` `ligatures` key (default on; ignored when the font has no GSUB). Tests: `gui::shape::tests::{maps_chars_to_gids, liga_merges_fi_into_one_wide_glyph, calt_chained_context_substitutes_in_context_only, runs_without_rules_pass_through}` (against a generated fixture font, `extra/gen_ligtest_font.py` / `src/gui/testdata/ligtest.ttf`), `gui::cpu::tests::ligatures_collapse_a_run_into_one_glyph`, `config::tests::ligatures_key_parses`.
- **Not yet:** the GPU renderer keeps per-cell glyphs (no ligatures) — its atlas tiles are one cell wide, so wide ligature glyphs need a multi-cell-quad pipeline (a documented follow-up, like #13/#14's GPU image path).

## 12. Full OSC 52 clipboard handling
Handle OSC 52 query path; window backend services programmatic clipboard get/set.

- **Status:** done (2026-06-07)
- **Notes:** The parser records OSC 52 on the grid for the window backend (`src/core/osc.rs` code "52"): a *set* decodes the base64 into `Grid::clipboard_set` (and is still relayed to the host for TUI mode); a *query* (`?`) sets `Grid::clipboard_query`. The window backend drains these on each tab's output (`App::service_clipboard` in `src/gui/window.rs`): a set copies into the system clipboard (`arboard`), a query replies to the child from the clipboard via `osc52_reply` (`OSC 52 ; c ; <base64> BEL`, encoded by the new `core::base64::encode`). Background tabs are serviced too. TUI mode still relays *set* to the host and leaves *query* to it. Tests: `core::tests::{osc_52_set_records_decoded_text_for_window_backend, osc_52_query_flags_window_backend, base64_encodes_with_padding_and_round_trips}`, `gui::window::tests::osc52_reply_wraps_base64`.

## 13. Accurate Sixel/Kitty image rendering
Pixel-perfect image rendering via framebuffer overlay instead of half-block path.

- **Status:** done (2026-06-07) — CPU framebuffer overlay; GPU/TUI keep half-block
- **Notes:** `Grid::render_image` (the shared Sixel/Kitty/PNG sink in `src/core/grid.rs`) now also retains the full-resolution source as a serial-anchored `GridImage` (`store_image`, gated `#[cfg(any(test, feature = "gui"))]` like `search`): `serial = total_scrolled + cursor row`, so it scrolls with the text and is evicted once it falls out of scrollback (`evict_scrolled_images`, hooked into `scroll_up`); `clear_all`/`resize`/`enter_alt_screen`/`reset` drop placed images. The half-block cells are still written, so the **GPU and TUI renderers fall back to them**. The CPU renderer (`src/gui/cpu.rs::draw_grid`) composites each image after glyphs, nearest-neighbor scaled to its `cols x rows` cell footprint and clipped to the pane (`images()`/`image_top_row`), painting transparent pixels through to the cell behind. Capped at 8 images (oldest dropped). Tests: `core::tests::render_image_stores_pixel_image_for_overlay`, `gui::cpu::tests::image_pixels_overlay_the_cells`.
- **Not yet:** GPU textured-quad image pipeline (the GPU path keeps the half-block fallback); ligatures/scaling filters beyond nearest-neighbor.

## 14. iTerm2 inline images + JPEG decoder
Implement iTerm2 inline image protocol with JPEG decoding.

- **Status:** done (2026-06-08)
- **Notes:** `src/core/iterm.rs` handles `OSC 1337 ; File=<args>:<base64>`: it honors `inline=1` (a non-inline transfer is a download, which a terminal has no surface for — ignored), base64-decodes the payload, format-detects PNG (`src/core/png.rs`) vs baseline JPEG by magic bytes, and feeds the pixels to the shared `Grid::render_image` sink (half-block cells in every build, plus the full-res overlay under the `gui` CPU renderer from #13). Dispatched from `src/core/osc.rs` (`1337` arm); other `1337;` subcommands are ignored. Because an OSC image far exceeds a title, `src/core/parser.rs` raises the OSC buffer cap to `OSC_IMAGE_MAX` (8 MiB) only for the `1337;File=` prefix, leaving ordinary OSC strings at the tight `OSC_MAX` (4096). `src/core/jpeg.rs` is a from-scratch baseline (SOF0/SOF1) decoder: Huffman (Annex F), dequant + zig-zag, separable float IDCT, restart intervals, 1-component grayscale and 3-component YCbCr at 4:4:4 / 4:2:2 / 4:2:0 (nearest-neighbor chroma upsample); progressive/arithmetic/12-bit/CMYK decode to `None` (caller skips display), mirroring the PNG decoder. Tests: `jpeg_decodes_grayscale`, `jpeg_decodes_solid_rgb_with_420_subsampling`, `jpeg_decodes_two_colors_444`, `jpeg_decodes_multi_mcu_image`, `jpeg_rejects_unsupported`, `iterm2_inline_jpeg_renders_image`, `iterm2_non_inline_transfer_is_ignored`, `iterm2_non_file_subcommand_is_ignored`, `iterm2_large_image_payload_is_not_truncated`.
- **Not yet:** the optional `width`/`height`/`preserveAspectRatio` geometry hints are not honored (images auto-fit to the available columns like Sixel/Kitty); GIF/WebP payloads and progressive JPEG are not decoded.

## 15. XTGETTCAP responses
Implement `DCS +q` capability-probing responses consistent with terminfo.

- **Status:** done (2026-06-07)
- **Notes:** `src/core/parser.rs::answer_xtgettcap` answers `DCS + q <hex>;... ST` queries: for each `;`-separated hex name it replies `DCS 1 + r <name>=<hexvalue> ST` (string/number cap), `DCS 1 + r <name> ST` (boolean), or `DCS 0 + r <name> ST` (unknown/malformed), echoing the requested name. The `+q` intermediate distinguishes it from Sixel. Advertised caps mirror `extra/rusty_term.terminfo`: `Co`/`colors` = 256, the `Tc` truecolor flag, and `TN`/`name` = `rusty_term`. Tests: `xtgettcap_answers_known_caps_and_truecolor`, `xtgettcap_reports_terminal_name`, `xtgettcap_unknown_and_malformed_fail`.

## Next up (optional follow-ups)

All 15 backlog items are implemented. Remaining optional enhancements:

- **GPU text/image pipeline** — multi-cell-quad rendering to give the GPU renderer ligatures (#11) and pixel-perfect images (#13/#14) instead of the per-cell / half-block fallbacks.
- **iTerm2 geometry hints** — honor `width`/`height`/`preserveAspectRatio` sizing for #14.
