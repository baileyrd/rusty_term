# Feature Backlog

15 enhancements identified for rusty_term via review.

## 1. In-window scrollback browsing
Add mouse wheel + Shift+PageUp/Down scrollback browsing to the `gui` backend (grid already retains history).

- **Status:** completed (2026-06-06)
- **Notes:** `src/gui/window.rs` now drives `view_offset` on the active tab's grid: `MouseWheel` browses history (`WHEEL_LINES` per notch), Shift+PageUp/Down page through it, and Ctrl+Shift+PageUp/Down jump prompt-to-prompt (OSC 133) — all intercepted before native key encoding so they never reach the child. Typing snaps back to the live bottom (`reset_view`). The CPU and GPU renderers compose scrollback above the live grid via the new `Grid::viewport_cell` when `view_offset > 0` (they previously ignored the offset); selection highlight is limited to the live view. TUI path unchanged.

## 2. Native SGR/1006 mouse reporting
Emit SGR/1006 mouse reporting from the window backend so child apps receive clicks/scrolls.

- **Status:** pending

## 3. Scrollback search/find overlay
Add incremental search with highlight + next/prev using existing reflowed logical lines.

- **Status:** pending

## 4. Split panes within a tab
Horizontal/vertical splits using existing Tab/PTY plumbing and cell-based chrome layout.

- **Status:** pending

## 5. Clickable OSC 8 hyperlinks
Support Ctrl+click to open OSC 8 hyperlinks via system opener.

- **Status:** pending

## 6. Configurable DECSCUSR cursor styles
Support block/underline/bar + steady/blinking, configurable via settings.

- **Status:** pending

## 7. User-configurable keybindings
Move new-tab, copy, prompt-nav, etc out of compile-time constants into config file.

- **Status:** pending

## 8. IME/composition events in winit backend
Wire IME/composition events for CJK and dead-key input.

- **Status:** pending

## 9. Desktop notifications via OSC 9/777
Implement OSC 9 and OSC 777 notification support.

- **Status:** pending

## 10. Windows host resize propagation in TUI
Fix host console size change detection and reflow in TUI/conhost mode.

- **Status:** pending

## 11. Font fallback + variants + ligatures
Add font fallback chains, bold/italic variants, and optional ligature shaping.

- **Status:** pending

## 12. Full OSC 52 clipboard handling
Handle OSC 52 query path; window backend services programmatic clipboard get/set.

- **Status:** pending

## 13. Accurate Sixel/Kitty image rendering
Pixel-perfect image rendering via framebuffer overlay instead of half-block path.

- **Status:** pending

## 14. iTerm2 inline images + JPEG decoder
Implement iTerm2 inline image protocol with JPEG decoding.

- **Status:** pending

## 15. XTGETTCAP responses
Implement `DCS +q` capability-probing responses consistent with terminfo.

- **Status:** pending
