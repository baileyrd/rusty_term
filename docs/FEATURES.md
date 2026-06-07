# Feature Backlog

15 enhancements were identified for rusty_term via review. **The statuses below
were re-audited against the source tree on 2026-06-07** — the earlier bulk
"completed (2026-06-07)" stamps did not match the code. Items **1, 2, 5, 6, 7, 8,
9, 10, 12, and 15** are implemented; the rest have no implementing code yet. Each entry
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

- **Status:** not implemented
- **Notes:** No search state, input mode, or highlight overlay exists in `src/gui/window.rs`; the reflowed-line search was never built.

## 4. Split panes within a tab
Horizontal/vertical splits using existing Tab/PTY plumbing and cell-based chrome layout.

- **Status:** not implemented
- **Notes:** No split/pane/region/layout code; `src/gui/window.rs` models tabs only (one PTY per tab).

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

- **Status:** not implemented
- **Notes:** `src/gui/font.rs` loads a single face and caches glyph coverage; no fallback chain, no bold/italic faces, no ligature shaping. ("fallback" appears only as the wgpu adapter flag.)

## 12. Full OSC 52 clipboard handling
Handle OSC 52 query path; window backend services programmatic clipboard get/set.

- **Status:** done (2026-06-07)
- **Notes:** The parser records OSC 52 on the grid for the window backend (`src/core/osc.rs` code "52"): a *set* decodes the base64 into `Grid::clipboard_set` (and is still relayed to the host for TUI mode); a *query* (`?`) sets `Grid::clipboard_query`. The window backend drains these on each tab's output (`App::service_clipboard` in `src/gui/window.rs`): a set copies into the system clipboard (`arboard`), a query replies to the child from the clipboard via `osc52_reply` (`OSC 52 ; c ; <base64> BEL`, encoded by the new `core::base64::encode`). Background tabs are serviced too. TUI mode still relays *set* to the host and leaves *query* to it. Tests: `core::tests::{osc_52_set_records_decoded_text_for_window_backend, osc_52_query_flags_window_backend, base64_encodes_with_padding_and_round_trips}`, `gui::window::tests::osc52_reply_wraps_base64`.

## 13. Accurate Sixel/Kitty image rendering
Pixel-perfect image rendering via framebuffer overlay instead of half-block path.

- **Status:** not implemented
- **Notes:** Images still take the half-block-cell path (`Grid::render_sixel` writes cells; test `render_sixel_writes_halfblock_cells`); the renderers blit only glyph coverage tiles — there is no pixel framebuffer/image overlay in `src/gui/{cpu,gpu}.rs`.

## 14. iTerm2 inline images + JPEG decoder
Implement iTerm2 inline image protocol with JPEG decoding.

- **Status:** not implemented
- **Notes:** No iTerm2 `OSC 1337` handler and no JPEG decoder (no `jpeg`/`jpg`/`1337` anywhere). Image decoders are PNG/Sixel/Kitty only.

## 15. XTGETTCAP responses
Implement `DCS +q` capability-probing responses consistent with terminfo.

- **Status:** done (2026-06-07)
- **Notes:** `src/core/parser.rs::answer_xtgettcap` answers `DCS + q <hex>;... ST` queries: for each `;`-separated hex name it replies `DCS 1 + r <name>=<hexvalue> ST` (string/number cap), `DCS 1 + r <name> ST` (boolean), or `DCS 0 + r <name> ST` (unknown/malformed), echoing the requested name. The `+q` intermediate distinguishes it from Sixel. Advertised caps mirror `extra/rusty_term.terminfo`: `Co`/`colors` = 256, the `Tc` truecolor flag, and `TN`/`name` = `rusty_term`. Tests: `xtgettcap_answers_known_caps_and_truecolor`, `xtgettcap_reports_terminal_name`, `xtgettcap_unknown_and_malformed_fail`.

## Next up (recommended order)

Larger / multi-file (each its own project):

- **#3** search overlay, **#4** split panes, **#11** font
  fallback/variants/ligatures, **#13** image framebuffer overlay,
  **#14** iTerm2 + JPEG.
