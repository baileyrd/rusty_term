//! The windowed front-end: a `winit` event loop driving a real OS window, with
//! `softbuffer` CPU presentation.
//!
//! The window is borderless (`decorations(false)`) and draws its own chrome: a
//! one-cell-row bar across the top holding the session tabs, a `+` new-tab
//! button, and the minimize/maximize/close caption buttons, all laid out as
//! ordinary cells so both renderers composite it for free. Dragging the bar
//! moves the window, double-click toggles maximize, and a thin band at the
//! window edges drag-resizes (the native frame is gone).
//!
//! Each tab owns a PTY session: its own grid, parser, writer, and a reader
//! thread pumping PTY output into the grid and waking the loop to repaint.
//! Key events are encoded natively (see [`super::input`]) and written to the
//! *active* tab's PTY; window resizes re-grid every tab + `TIOCSWINSZ`.
//!
//! Note: this drives a live window and so cannot be exercised in a headless
//! environment — the render, input-encoding, and font layers it composes are
//! unit-tested independently. Wired here: mouse drag-selection, clipboard
//! copy/paste (Ctrl+Shift+C / Ctrl+Shift+V) plus OSC 52 get/set, SGR/1006 mouse
//! reporting, Ctrl+click to open OSC 8 hyperlinks, DECSCUSR cursor styles +
//! blink, and a Windows child-exit watcher.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowId};

use crate::backend::{Backend, BackendHandle};
use crate::config::Config;
use crate::core::{AnsiParser, Cell, Grid, Selection, Theme, WIDE_TRAILER, char_width};
use crate::keymap::{Action, Chord};
use crate::gui::mouse::{MouseEvent, SgrEncoder};
use super::font::{self, FontCache, GlyphSource};
use super::layout::{Dir, Layout, Rect};
use super::render::{CpuRenderer, PaneFrame, Renderer};
use super::settings::{Field, Settings};

/// Built-in defaults, overridable via the config file (`[window]` section).
const FONT_PX: f32 = 18.0;
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;
/// Pixel band at the window edges acting as a resize handle (the native frame
/// is gone with decorations off).
const RESIZE_BORDER: f64 = 6.0;
/// Total cell budget for one tab in the chrome bar (label plus its × button).
const TAB_CELLS: usize = 26;
/// Grid row where overlay (menu / settings) list rows begin (header sits above).
const OVERLAY_ITEMS_TOP: usize = 2;
/// Two clicks on the drag strip within this window toggle maximize.
const DOUBLE_CLICK_MS: u128 = 400;
/// Scrollback lines moved per mouse-wheel notch.
const WHEEL_LINES: isize = 3;

/// Wakeups sent from per-tab PTY reader threads into the winit loop, tagged
/// with the tab id they concern.
enum UserEvent {
    /// New output was parsed into the tab's grid; repaint if it's the active one.
    Redraw(u64),
    /// The tab's child exited; close that tab (the last one closes the window).
    Exit(u64),
    /// The config file changed on disk; reload and apply what can change live.
    ConfigChanged,
}

/// What a click on a given chrome-bar cell does.
#[derive(Clone, Copy, PartialEq)]
enum Hit {
    /// Activate tab `i`.
    Tab(usize),
    /// Close tab `i` (its × button).
    CloseTab(usize),
    /// Spawn a new tab (the `+` button).
    NewTab,
    /// Open the shell-launcher / settings dropdown (the `▾` button).
    ShellMenu,
    Minimize,
    Maximize,
    Close,
    /// Empty bar: drag moves the window, double-click toggles maximize.
    Drag,
}

/// A transient full-window page drawn over the active tab while open: the shell
/// launcher / settings dropdown menu, or the settings page. Keys and
/// below-chrome clicks route to it; clicking a tab or a chrome button dismisses
/// it.
enum Overlay {
    /// The `▾` dropdown: a list of menu rows to choose from.
    Menu { items: Vec<MenuItem>, sel: usize },
    /// The settings page.
    Settings(Settings),
}

/// One row of the dropdown menu and the action choosing it triggers.
struct MenuItem {
    label: String,
    kind: MenuKind,
}

#[derive(Clone, Copy)]
enum MenuKind {
    /// Launch a new tab running detected shell `[index]`.
    LaunchShell(usize),
    /// Open the in-app settings page.
    Settings,
    /// Open the config file in the user's editor.
    EditConfig,
}

/// One terminal session inside a tab (a split pane): its screen state and PTY
/// plumbing. The owning PTY handle lives here, so dropping a `Pane` tears the
/// session down (its reader thread then EOFs and its exit event becomes a no-op).
struct Pane {
    id: u64,
    grid: Arc<Mutex<Grid>>,
    /// Shared with the pane's reader thread; the loop takes it only on config reload.
    parser: Arc<Mutex<AnsiParser>>,
    writer: Box<dyn BackendHandle>,
}

/// A tab: one or more [`Pane`]s tiled by a [`Layout`], one of them focused.
struct Tab {
    panes: Vec<Pane>,
    layout: Layout,
    /// Id of the focused pane — it receives input and shows the cursor.
    focus: u64,
}

impl Tab {
    fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }
    fn focused(&self) -> Option<&Pane> {
        self.pane(self.focus)
    }
    fn focused_mut(&mut self) -> Option<&mut Pane> {
        let f = self.focus;
        self.panes.iter_mut().find(|p| p.id == f)
    }
}

/// Launch the windowed terminal. Blocks until the window closes or the last
/// tab's child exits. Returns an error if the window/PTY/font can't be set up.
pub fn run(backend: &dyn Backend, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    // The child renders through us now, so advertise a real terminal identity.
    unsafe {
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
    }

    let font_px = config.font_size.unwrap_or(FONT_PX);
    let font_set = font::load_set(
        config.font.as_deref(),
        config.font_bold.as_deref(),
        config.font_italic.as_deref(),
        config.font_bold_italic.as_deref(),
        config.font_fallback.as_deref(),
    )
    .ok_or("no monospace font found")?;
    let ligatures = config.ligatures.unwrap_or(true);
    let font = FontCache::new(font_set, font_px, ligatures).ok_or("font failed to parse")?;
    let (cell_w, cell_h) = font.cell_size();

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Config live reload: watch the file and wake the loop on changes.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = crate::config::Config::file_path(&args);
    if let Some(path) = config_path.clone() {
        let watch_proxy = proxy.clone();
        crate::config::watch(path, move || {
            let _ = watch_proxy.send_event(UserEvent::ConfigChanged);
        });
    }

    let mut app = App {
        backend,
        config: config.clone(),
        config_path,
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        proxy,
        font,
        cell_w: cell_w.max(1),
        cell_h: cell_h.max(1),
        window: None,
        renderer: None,
        mods: ModifiersState::empty(),
        cols: config.cols.unwrap_or(INIT_COLS),
        rows: config.rows.unwrap_or(INIT_ROWS),
        theme: config.theme,
        clipboard: arboard::Clipboard::new().ok(),
        mouse_pos: (0.0, 0.0),
        selecting: false,
        sel_anchor: None,
        hits: Vec::new(),
        last_strip_click: None,
        cursor_blink_on: true,
        last_blink: Instant::now(),
        searching: None,
        shells: crate::shells::detect_all(),
        overlay: None,
        font_px,
    };
    app.spawn_tab()?; // the first shell; more come from Ctrl+Shift+T / the + button
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// PTY reader loop (own thread, one per tab): parse output into the tab's
/// grid, send replies (DA/DSR/structured-channel) back to the child, and wake
/// the window with the tab's id.
fn reader_loop(
    id: u64,
    mut reader: Box<dyn BackendHandle>,
    mut replies: Box<dyn BackendHandle>,
    grid: Arc<Mutex<Grid>>,
    proxy: EventLoopProxy<UserEvent>,
    parser: Arc<Mutex<AnsiParser>>,
) {
    loop {
        match reader.read() {
            Ok(data) if data.is_empty() => break, // EOF: child exited
            Ok(data) => {
                let response = {
                    let mut g = grid.lock();
                    let mut parser = parser.lock();
                    parser.advance(&mut g, &data);
                    // host_out (clipboard/title relay) has no host here; dropped.
                    let _ = g.take_host_out();
                    parser.take_responses()
                };
                if !response.is_empty() {
                    let _ = replies.write(&response);
                }
                if proxy.send_event(UserEvent::Redraw(id)).is_err() {
                    break; // loop gone
                }
            }
            Err(_) => break,
        }
    }
    let _ = proxy.send_event(UserEvent::Exit(id));
}

struct App<'a> {
    /// Spawns the shell behind each new tab.
    backend: &'a dyn Backend,
    /// The effective config; refreshed on live reload so new tabs follow it.
    config: Config,
    /// The config file in effect, for the open shortcut + reload re-reads.
    config_path: Option<std::path::PathBuf>,
    tabs: Vec<Tab>,
    /// Index into `tabs` of the session being shown and fed input.
    active: usize,
    /// Monotonic id source for tabs (ids outlive indices across closes).
    next_id: u64,
    proxy: EventLoopProxy<UserEvent>,
    font: FontCache,
    cell_w: usize,
    cell_h: usize,
    window: Option<Arc<Window>>,
    renderer: Option<Box<dyn Renderer>>,
    mods: ModifiersState,
    /// Grid size in cells (the chrome bar adds one extra screen row on top).
    cols: u16,
    rows: u16,
    /// The theme in effect, painting the chrome bar and any new tab's grid.
    theme: Theme,
    /// System clipboard for copy/paste; `None` if unavailable (e.g. headless).
    clipboard: Option<arboard::Clipboard>,
    /// Current pointer position in physical pixels, used for hit-testing.
    mouse_pos: (f64, f64),
    /// Whether the left button is held (a drag-selection is in progress).
    selecting: bool,
    /// Cell where the current drag-selection began.
    sel_anchor: Option<(usize, usize)>,
    /// Per-cell click actions for the chrome bar, rebuilt with each layout.
    hits: Vec<Hit>,
    /// Time of the last single click on the drag strip (double-click detect).
    last_strip_click: Option<Instant>,
    /// Whether the blinking cursor is in its visible phase this frame.
    cursor_blink_on: bool,
    /// When the blink phase last toggled, paced by the event loop.
    last_blink: Instant,
    /// Active scrollback-search query (windowed front-end); `Some` means search
    /// mode is on, intercepting keys and showing the find bar in the chrome.
    searching: Option<String>,
    /// Shells detected on this machine, for the launcher menu + settings page.
    shells: Vec<crate::shells::DetectedShell>,
    /// The open settings page / shell menu, if any (drawn over the active tab).
    overlay: Option<Overlay>,
    /// Current font size in px, tracked so the settings page can rebuild it.
    font_px: f32,
}

impl App<'_> {
    /// Spawn one shell sized `cols × rows`, wire its reader + exit-watcher
    /// threads (which signal by pane id), and return the pane.
    fn new_pane(&mut self, cols: u16, rows: u16, shell: Option<&str>) -> Result<Pane, std::io::Error> {
        let handle = self.backend.spawn_shell(cols, rows, shell)?;
        let id = self.next_id;
        self.next_id += 1;

        let mut g = Grid::new(cols as usize, rows as usize);
        if let Some(max) = self.config.scrollback {
            g.set_scrollback_max(max);
        }
        g.apply_theme(&self.theme);
        g.set_default_cursor(
            self.config.cursor_style.unwrap_or_default(),
            self.config.cursor_blink.unwrap_or(false),
        );
        let grid = Arc::new(Mutex::new(g));
        let parser = Arc::new(Mutex::new(AnsiParser::with_theme(self.theme)));

        // Reader thread: PTY -> parser -> grid, writing replies back and waking
        // the loop. Independent handle clones so it shares no lock with us.
        let read_handle = handle.try_clone()?;
        let reply_handle = handle.try_clone()?;
        let reader_grid = Arc::clone(&grid);
        let reader_parser = Arc::clone(&parser);
        let reader_proxy = self.proxy.clone();
        std::thread::spawn(move || {
            reader_loop(id, read_handle, reply_handle, reader_grid, reader_proxy, reader_parser)
        });

        // Child-exit watcher (Windows ConPTY: the output pipe only EOFs at
        // teardown, so block on the child handle; `None` on Unix where read-EOF
        // already signals exit).
        if let Some(wait) = handle.exit_token() {
            let exit_proxy = self.proxy.clone();
            std::thread::spawn(move || {
                wait();
                let _ = exit_proxy.send_event(UserEvent::Exit(id));
            });
        }
        Ok(Pane { id, grid, parser, writer: handle })
    }

    /// Open a new tab (one full-area pane) and make it active.
    fn spawn_tab(&mut self) -> Result<(), std::io::Error> {
        self.spawn_tab_with(None)
    }

    /// Open a new tab running `shell` (or the configured default when `None`)
    /// and make it active. Backs the `+` button, `Ctrl+Shift+T`, and the
    /// shell-launcher menu (which passes a detected shell's path).
    fn spawn_tab_with(&mut self, shell: Option<String>) -> Result<(), std::io::Error> {
        let shell = shell.or_else(|| self.config.shell.clone());
        let pane = self.new_pane(self.cols, self.rows, shell.as_deref())?;
        let focus = pane.id;
        self.tabs.push(Tab { panes: vec![pane], layout: Layout::single(focus), focus });
        self.active = self.tabs.len() - 1;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        Ok(())
    }

    /// Split the active tab's focused pane, spawning a new shell beside it and
    /// focusing it. `dir` is the divider orientation.
    fn split_pane(&mut self, dir: Dir) {
        let shell = self.config.shell.clone();
        let Ok(pane) = self.new_pane(self.cols.max(1), self.rows.max(1), shell.as_deref()) else {
            return;
        };
        let new_id = pane.id;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let target = tab.focus;
        tab.panes.push(pane);
        tab.layout.split(target, new_id, dir);
        tab.focus = new_id;
        self.layout_panes(self.active);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Move focus to the next (`forward`) or previous pane of the active tab.
    fn focus_pane(&mut self, forward: bool) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.focus = tab.layout.cycle(tab.focus, forward);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Close pane `id`: collapse its split into the sibling, or close the whole
    /// tab when it was the last pane. Idempotent for stale exit events.
    fn close_pane(&mut self, id: u64, event_loop: &ActiveEventLoop) {
        let Some(ti) = self.tabs.iter().position(|t| t.panes.iter().any(|p| p.id == id)) else {
            return;
        };
        let tab = &mut self.tabs[ti];
        match tab.layout.close(id) {
            None => {
                self.close_tab_at(ti, event_loop);
                return;
            }
            Some(next) => {
                tab.panes.retain(|p| p.id != id); // drops the PTY handle
                if tab.focus == id {
                    tab.focus = next;
                }
            }
        }
        self.layout_panes(ti);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Remove the tab at `ti` (dropping all its panes). The last tab closes the
    /// window.
    fn close_tab_at(&mut self, ti: usize, event_loop: &ActiveEventLoop) {
        self.tabs.remove(ti);
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        if ti < self.active {
            self.active -= 1;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Resize tab `ti`'s panes (grids + PTYs) to their layout rectangles.
    fn layout_panes(&mut self, ti: usize) {
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        let Some(tab) = self.tabs.get_mut(ti) else {
            return;
        };
        for (id, r) in tab.layout.rects(area) {
            if let Some(p) = tab.panes.iter_mut().find(|p| p.id == id) {
                let (c, rw) = (r.cols.max(1) as u16, r.rows.max(1) as u16);
                {
                    let mut g = p.grid.lock();
                    g.resize(c as usize, rw as usize);
                    g.selection = None; // viewport changed; old coords are stale
                }
                let _ = p.writer.set_winsize(c, rw);
            }
        }
    }

    /// Recompute the cell grid from the window's pixel size (minus the chrome
    /// row) and relay it to every pane of every tab.
    fn apply_size(&mut self, px_w: u32, px_h: u32) {
        let cols = ((px_w as usize / self.cell_w).max(1)) as u16;
        let rows = (((px_h as usize / self.cell_h).saturating_sub(1)).max(1)) as u16;
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            for ti in 0..self.tabs.len() {
                self.layout_panes(ti);
            }
        }
    }

    /// Paint the chrome bar + the active tab's panes through the renderer.
    fn redraw(&mut self) {
        let chrome = self.chrome_row();
        let divider = mix(self.theme.bg, self.theme.fg, 60);
        if self.overlay.is_some() {
            let page = self.build_overlay_grid();
            let (Some(renderer), Some(window)) = (self.renderer.as_mut(), self.window.as_ref()) else {
                return;
            };
            let size = window.inner_size();
            let frame =
                PaneFrame { grid: &page, col0: 0, row0: 1, focused: false, cursor_on: false };
            renderer.render(
                std::slice::from_ref(&frame),
                &chrome,
                &mut self.font,
                size.width,
                size.height,
                divider,
            );
            return;
        }
        let (Some(renderer), Some(window)) = (self.renderer.as_mut(), self.window.as_ref()) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let size = window.inner_size();
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        if let Some(p) = tab.focused() {
            let g = p.grid.lock();
            window.set_title(if g.title.is_empty() { "rusty_term" } else { &g.title });
        }
        // Lock each pane's grid for the frame, then hand the renderer offset views
        // (the chrome bar occupies screen row 0, so panes start at row 1).
        let blink = self.cursor_blink_on;
        let focus = tab.focus;
        let mut held = Vec::new();
        for (id, r) in tab.layout.rects(area) {
            if let Some(p) = tab.pane(id) {
                held.push((p.grid.lock(), r, id == focus));
            }
        }
        let frames: Vec<PaneFrame> = held
            .iter()
            .map(|(g, r, foc)| PaneFrame {
                grid: g,
                col0: r.col,
                row0: r.row + 1,
                focused: *foc,
                cursor_on: blink,
            })
            .collect();
        renderer.render(&frames, &chrome, &mut self.font, size.width, size.height, divider);
    }

    /// The active tab's focused pane — the input, cursor, and search target.
    fn pane(&self) -> Option<&Pane> {
        self.tabs.get(self.active).and_then(|t| t.focused())
    }
    fn pane_mut(&mut self) -> Option<&mut Pane> {
        self.tabs.get_mut(self.active).and_then(|t| t.focused_mut())
    }
    /// Any pane by id across all tabs (reader/exit threads signal by pane id).
    fn pane_by_id(&self, id: u64) -> Option<&Pane> {
        self.tabs.iter().find_map(|t| t.pane(id))
    }
    fn pane_by_id_mut(&mut self, id: u64) -> Option<&mut Pane> {
        self.tabs.iter_mut().find_map(|t| t.panes.iter_mut().find(|p| p.id == id))
    }

    /// Lay out the chrome bar for the current tabs/size: tab labels and the
    /// `+` on the left, the caption buttons (─ □ ×) right-aligned, drag strip
    /// between. Rebuilds the per-cell hit map as it goes.
    fn chrome_row(&mut self) -> Vec<Cell> {
        let cols = self.cols as usize;
        if let Some(query) = self.searching.clone() {
            let mut row = vec![Cell::blank(); cols];
            let bar_bg = mix(self.theme.bg, self.theme.fg, 45);
            for c in &mut row {
                c.fg = self.theme.fg;
                c.bg = bar_bg;
            }
            let status = self.pane().and_then(|p| p.grid.lock().search_status());
            let count = match status {
                Some((cur, total)) => format!(" {cur}/{total} "),
                None if query.is_empty() => String::new(),
                None => " no matches ".to_string(),
            };
            let limit = cols.saturating_sub(count.chars().count());
            let mut hits = vec![Hit::Drag; cols];
            let mut col = 0;
            put_text(&mut row, &mut hits, &mut col, limit, &format!(" Find: {query}"), self.theme.fg, bar_bg, Hit::Drag);
            let mut ccol = limit;
            put_text(&mut row, &mut hits, &mut ccol, cols, &count, self.theme.fg, bar_bg, Hit::Drag);
            self.hits = hits;
            return row;
        }
        let bar_bg = mix(self.theme.bg, self.theme.fg, 30);
        let dim_fg = mix(self.theme.fg, self.theme.bg, 110);
        let mut row: Vec<Cell> = vec![Cell::blank(); cols];
        for c in &mut row {
            c.fg = self.theme.fg;
            c.bg = bar_bg;
        }
        let mut hits = vec![Hit::Drag; cols];

        // Caption buttons get the last 12 cells (4 each); the `+` / `▾` buttons
        // sit just left of them, and the tabs fill the rest without overrunning.
        let btn0 = cols.saturating_sub(12);
        let tab_limit = btn0.saturating_sub(8);

        let close_w = 3; // " × " on each tab
        let mut col = 0usize;
        for (i, tab) in self.tabs.iter().enumerate() {
            if col >= tab_limit {
                break; // out of room; the rest stay reachable by keyboard
            }
            let is_active = i == self.active;
            // The active tab adopts the terminal background, visually merging
            // with the grid below; inactive ones sit dimmed on the bar.
            let (fg, bg) = if is_active { (self.theme.fg, self.theme.bg) } else { (dim_fg, bar_bg) };
            let title = {
                let label = tab.focused().map(|p| p.grid.lock().title.clone()).unwrap_or_default();
                if label.is_empty() { format!("shell {}", i + 1) } else { label }
            };
            let tab_end = (col + TAB_CELLS).min(tab_limit);
            // Paint the whole tab span in its color and make it activate on click.
            for c in col..tab_end {
                row[c] = Cell::blank();
                row[c].fg = fg;
                row[c].bg = bg;
                hits[c] = Hit::Tab(i);
            }
            // Title text, leaving room for the per-tab close button when wide enough.
            let has_close = tab_end - col > close_w + 1;
            let label_end = if has_close { tab_end - close_w } else { tab_end };
            let mut tcol = col;
            let label: String = format!(" {title}").chars().take(label_end - col).collect();
            put_text(&mut row, &mut hits, &mut tcol, label_end, &label, fg, bg, Hit::Tab(i));
            if has_close {
                let mut ccol = tab_end - close_w;
                put_text(&mut row, &mut hits, &mut ccol, tab_end, " × ", fg, bg, Hit::CloseTab(i));
            }
            col = tab_end;
            if col < tab_limit {
                col += 1; // one bar-colored cell as a tab separator
            }
        }
        put_text(&mut row, &mut hits, &mut col, btn0, " + ", self.theme.fg, bar_bg, Hit::NewTab);
        put_text(&mut row, &mut hits, &mut col, btn0, " ▾ ", self.theme.fg, bar_bg, Hit::ShellMenu);

        let mut bcol = btn0;
        put_text(&mut row, &mut hits, &mut bcol, btn0 + 4, "  ─ ", self.theme.fg, bar_bg, Hit::Minimize);
        put_text(&mut row, &mut hits, &mut bcol, btn0 + 8, "  □ ", self.theme.fg, bar_bg, Hit::Maximize);
        put_text(&mut row, &mut hits, &mut bcol, cols, "  × ", self.theme.fg, bar_bg, Hit::Close);

        self.hits = hits;
        row
    }

    /// Build the configured renderer for `window`: the GPU one when `--gpu` is
    /// passed and `gui-gpu` is built (falling back to CPU on failure), else CPU.
    fn make_renderer(&mut self, window: Arc<Window>) -> Option<Box<dyn Renderer>> {
        #[cfg(feature = "gui-gpu")]
        if std::env::args().any(|a| a == "--gpu") {
            match super::gpu::GpuRenderer::new(window.clone(), &mut self.font) {
                Ok(r) => return Some(Box::new(r)),
                Err(e) => eprintln!("GPU renderer unavailable ({e}); falling back to CPU"),
            }
        }
        match CpuRenderer::new(window) {
            Ok(r) => Some(Box::new(r)),
            Err(e) => {
                eprintln!("CPU renderer failed: {e}");
                None
            }
        }
    }

    /// Map a physical pixel position to a clamped `(col, row)` *grid* cell
    /// (the chrome bar occupies the screen row above grid row 0).
    fn cell_at(&self, px: f64, py: f64) -> (usize, usize) {
        let col = (px.max(0.0) as usize / self.cell_w).min((self.cols as usize).saturating_sub(1));
        let row = (py.max(0.0) as usize / self.cell_h)
            .saturating_sub(1)
            .min((self.rows as usize).saturating_sub(1));
        (col, row)
    }

    /// The resize direction for a pointer near the window edge, `None` away
    /// from the edges or while maximized (a maximized window doesn't resize).
    fn resize_zone(&self, x: f64, y: f64) -> Option<ResizeDirection> {
        let window = self.window.as_ref()?;
        if window.is_maximized() {
            return None;
        }
        let size = window.inner_size();
        let (w, h) = (size.width as f64, size.height as f64);
        let (l, r) = (x < RESIZE_BORDER, x > w - RESIZE_BORDER);
        let (t, b) = (y < RESIZE_BORDER, y > h - RESIZE_BORDER);
        Some(match (l, r, t, b) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            (_, true, _, true) => ResizeDirection::SouthEast,
            (true, ..) => ResizeDirection::West,
            (_, true, ..) => ResizeDirection::East,
            (_, _, true, _) => ResizeDirection::North,
            (_, _, _, true) => ResizeDirection::South,
            _ => return None,
        })
    }

    /// Left button pressed: edge band starts a drag-resize, the chrome bar
    /// dispatches its hit action, anywhere else anchors a drag-selection.
    fn on_left_press(&mut self, event_loop: &ActiveEventLoop) {
        let (x, y) = self.mouse_pos;
        if let Some(dir) = self.resize_zone(x, y) {
            if let Some(window) = &self.window {
                let _ = window.drag_resize_window(dir);
            }
            return;
        }
        if (y.max(0.0) as usize) < self.cell_h {
            return self.on_bar_click(x, event_loop);
        }
        if self.overlay.is_some() {
            return self.overlay_click(y);
        }
        if let Some(id) = self.pane_under(x, y)
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.focus = id; // click focuses the pane under the pointer
        }
        self.sel_anchor = Some(self.cell_in_focused(x, y));
        self.selecting = true;
        if let Some(p) = self.pane() {
            p.grid.lock().selection = None; // cleared until the drag moves
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// The pane under pixel `(px, py)` in the active tab's grid area, if any.
    fn pane_under(&self, px: f64, py: f64) -> Option<u64> {
        let (col, row) = self.cell_at(px, py);
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        self.tabs.get(self.active)?.layout.pane_at(area, col, row)
    }

    /// Map pixel `(px, py)` to a cell within the *focused* pane, clamped to it.
    fn cell_in_focused(&self, px: f64, py: f64) -> (usize, usize) {
        let (col, row) = self.cell_at(px, py);
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        if let Some(tab) = self.tabs.get(self.active)
            && let Some((_, r)) =
                tab.layout.rects(area).into_iter().find(|(id, _)| *id == tab.focus)
        {
            return (
                col.saturating_sub(r.col).min(r.cols.saturating_sub(1)),
                row.saturating_sub(r.row).min(r.rows.saturating_sub(1)),
            );
        }
        (col, row)
    }

    /// Dispatch a click on the chrome bar through the hit map.
    fn on_bar_click(&mut self, x: f64, event_loop: &ActiveEventLoop) {
        if self.hits.is_empty() {
            return; // no frame laid out yet
        }
        let col = (x.max(0.0) as usize / self.cell_w).min(self.hits.len() - 1);
        let Some(window) = self.window.clone() else {
            return;
        };
        match self.hits[col] {
            Hit::Tab(i) => {
                self.close_overlay();
                if i < self.tabs.len() && i != self.active {
                    self.active = i;
                }
                window.request_redraw();
            }
            Hit::CloseTab(i) => {
                self.close_overlay();
                if i < self.tabs.len() {
                    self.close_tab_at(i, event_loop);
                }
            }
            Hit::NewTab => {
                self.close_overlay();
                if let Err(e) = self.spawn_tab() {
                    eprintln!("rusty_term: new tab: {e}");
                }
            }
            Hit::ShellMenu => {
                let was_menu = matches!(self.overlay, Some(Overlay::Menu { .. }));
                self.close_overlay(); // persists a dirty settings page first
                if !was_menu {
                    self.open_menu();
                }
                window.request_redraw();
            }
            Hit::Minimize => window.set_minimized(true),
            Hit::Maximize => window.set_maximized(!window.is_maximized()),
            Hit::Close => event_loop.exit(),
            Hit::Drag => {
                let now = Instant::now();
                if self
                    .last_strip_click
                    .take()
                    .is_some_and(|last| now.duration_since(last).as_millis() <= DOUBLE_CLICK_MS)
                {
                    window.set_maximized(!window.is_maximized());
                } else {
                    self.last_strip_click = Some(now);
                    let _ = window.drag_window();
                }
            }
        }
    }

    /// Copy the active tab's selection to the system clipboard (Ctrl+Shift+C).
    fn copy_selection(&mut self) {
        let Some(p) = self.pane() else { return };
        let text = p.grid.lock().selected_text();
        if let (Some(text), Some(cb)) = (text, self.clipboard.as_mut()) {
            let _ = cb.set_text(text);
        }
    }

    /// Service a tab's pending OSC 52 clipboard request recorded by the parser.
    /// A set copies the child's text to the system clipboard; a query replies to
    /// the child from the system clipboard. Called on a tab's output, so
    /// background tabs are serviced too.
    fn service_clipboard(&mut self, id: u64) {
        let (set, query) = {
            let Some(p) = self.pane_by_id(id) else { return };
            let mut g = p.grid.lock();
            if g.clipboard_set.is_none() && !g.clipboard_query {
                return;
            }
            (g.clipboard_set.take(), std::mem::take(&mut g.clipboard_query))
        };
        if let Some(text) = set
            && let Some(cb) = self.clipboard.as_mut()
        {
            let _ = cb.set_text(text);
        }
        if query
            && let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok())
        {
            let reply = osc52_reply(&text);
            if let Some(p) = self.pane_by_id_mut(id) {
                let _ = p.writer.write(&reply);
            }
        }
    }

    /// Raise any desktop notifications (OSC 9/777) the tab's child queued. Run on
    /// a tab's output, so background tabs notify too.
    fn service_notifications(&mut self, id: u64) {
        let notes = {
            let Some(p) = self.pane_by_id(id) else { return };
            let mut g = p.grid.lock();
            if g.notifications.is_empty() {
                return;
            }
            std::mem::take(&mut g.notifications)
        };
        for (title, body) in notes {
            let title = if title.is_empty() { "rusty_term" } else { title.as_str() };
            notify(title, &body);
        }
    }

    /// Dispatch a terminal-owned [`Action`] resolved from the keymap.
    fn run_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::Copy => self.copy_selection(),
            Action::Paste => self.paste(),
            Action::NewTab => {
                if let Err(e) = self.spawn_tab() {
                    eprintln!("rusty_term: new tab: {e}");
                }
            }
            Action::CloseTab => {
                if let Some(id) = self.tabs.get(self.active).map(|t| t.focus) {
                    self.close_pane(id, event_loop);
                }
            }
            Action::NextTab => self.cycle_tab(true),
            Action::PrevTab => self.cycle_tab(false),
            Action::OpenConfig => self.open_config(),
            Action::OpenSettings => self.open_settings(),
            Action::Search => self.start_search(),
            Action::SplitRight => self.split_pane(Dir::Vertical),
            Action::SplitDown => self.split_pane(Dir::Horizontal),
            Action::FocusNext => self.focus_pane(true),
            Action::ScrollPageUp => self.scroll_key(false, true),
            Action::ScrollPageDown => self.scroll_key(false, false),
            Action::ScrollPromptUp => self.scroll_key(true, true),
            Action::ScrollPromptDown => self.scroll_key(true, false),
        }
    }

    /// Enter incremental scrollback-search mode with an empty query.
    fn start_search(&mut self) {
        self.searching = Some(String::new());
        self.grid_clear_search();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Handle a key while in search mode: edit the query, step matches (Enter /
    /// Shift+Enter), or exit (Esc). Returns whether it was consumed — `false`
    /// when not searching, so normal handling continues.
    fn search_key(&mut self, event: &KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        if self.searching.is_none() {
            return false;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.searching = None;
                self.grid_clear_search();
            }
            Key::Named(NamedKey::Enter) => {
                let forward = !self.mods.shift_key();
                if let Some(p) = self.pane() {
                    p.grid.lock().search_jump(forward);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(q) = self.searching.as_mut() {
                    q.pop();
                }
                self.run_search();
            }
            Key::Character(s) if !self.mods.control_key() && !self.mods.alt_key() => {
                if let Some(q) = self.searching.as_mut() {
                    q.push_str(s);
                }
                self.run_search();
            }
            _ => {}
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        true
    }

    /// Re-run the active tab's search for the current query.
    fn run_search(&mut self) {
        let q = self.searching.clone().unwrap_or_default();
        if let Some(p) = self.pane() {
            p.grid.lock().search(&q);
        }
    }

    /// Clear the active tab's search highlights.
    fn grid_clear_search(&mut self) {
        if let Some(p) = self.pane() {
            p.grid.lock().clear_search();
        }
    }

    /// Switch to the next (`forward`) or previous tab, wrapping; a no-op with one
    /// tab. Repaints since the active grid changed.
    fn cycle_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n > 1 {
            self.active = if forward {
                (self.active + 1) % n
            } else {
                (self.active + n - 1) % n
            };
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Tell the IME where the text cursor is, so its candidate/composition popup
    /// appears at the terminal cursor rather than the window origin.
    fn update_ime_area(&self) {
        let (Some(window), Some(p)) = (&self.window, self.pane()) else {
            return;
        };
        let (col, row) = p.grid.lock().cursor;
        let x = (col * self.cell_w) as f64;
        // +1 cell row for the chrome bar above the grid.
        let y = ((row + 1) * self.cell_h) as f64;
        window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(x, y),
            winit::dpi::PhysicalSize::new(self.cell_w as u32, self.cell_h as u32),
        );
    }

    /// Paste the system clipboard into the active tab's child (Ctrl+Shift+V).
    fn paste(&mut self) {
        let Some(cb) = self.clipboard.as_mut() else { return };
        let Ok(text) = cb.get_text() else { return };
        if text.is_empty() {
            return;
        }
        let Some(p) = self.pane_mut() else { return };
        let bracketed = p.grid.lock().bracketed_paste;
        let _ = p.writer.write(&encode_paste(&text, bracketed));
    }

    /// Encode a native mouse event as SGR/1006 and send it to the active tab's
    /// child — but only when the child enabled mouse reporting (`?1000`/`?1002`/
    /// `?1003`). `build` turns the cell under the pointer into the event. Returns
    /// whether bytes were sent, so the wheel path can fall back to local
    /// scrollback browsing when the child isn't tracking the mouse.
    fn report_mouse(&mut self, build: impl FnOnce(usize, usize) -> MouseEvent) -> bool {
        let Some(p) = self.pane() else { return false };
        let modes = p.grid.lock().mouse_modes;
        if !modes.active() {
            return false;
        }
        let (col, row) = self.cell_in_focused(self.mouse_pos.0, self.mouse_pos.1);
        let mut out = Vec::new();
        SgrEncoder::new(modes).write(build(col, row), &mut out);
        if out.is_empty() {
            return false;
        }
        let Some(p) = self.pane_mut() else { return false };
        let _ = p.writer.write(&out);
        true
    }

    /// If an OSC 8 hyperlink covers the cell under the pointer, open it with the
    /// OS handler. Returns whether a link was opened, so a Ctrl+click can
    /// suppress the normal selection / mouse-report path.
    fn open_link_under_pointer(&self) -> bool {
        let Some(p) = self.pane() else { return false };
        let (col, row) = self.cell_in_focused(self.mouse_pos.0, self.mouse_pos.1);
        let url = p.grid.lock().link_at(col, row).map(str::to_owned);
        url.is_some_and(|u| open_url(&u))
    }

    /// Browse the active tab's scrollback: move the viewport by `lines`
    /// (positive = up into history, negative = back toward the live bottom),
    /// clamped to the available history. Repaints if the view actually moved.
    fn scroll_active(&mut self, lines: isize) {
        let Some(p) = self.pane() else { return };
        let moved = {
            let mut g = p.grid.lock();
            if lines >= 0 {
                g.scroll_view_up(lines as usize)
            } else {
                g.scroll_view_down((-lines) as usize)
            }
        };
        if moved && let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Apply a scrollback-browse key to the active tab: Shift+PageUp/Down page
    /// through history, the same with Ctrl jumps prompt-to-prompt (OSC 133
    /// marks). Repaints if the view moved. Mirrors the TUI's scroll keys.
    fn scroll_key(&mut self, ctrl: bool, up: bool) {
        let page = (self.rows.saturating_sub(1)).max(1) as usize;
        let Some(p) = self.pane() else { return };
        let moved = {
            let mut g = p.grid.lock();
            match (ctrl, up) {
                (false, true) => g.scroll_view_up(page),
                (false, false) => g.scroll_view_down(page),
                (true, true) => g.scroll_to_prev_prompt(),
                (true, false) => g.scroll_to_next_prompt(),
            }
        };
        if moved && let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Snap the active tab's viewport back to the live bottom (e.g. after the
    /// user types), repainting if it had been scrolled into history.
    fn snap_to_bottom(&mut self) {
        let Some(p) = self.pane() else { return };
        if p.grid.lock().reset_view()
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }

    /// Open the config file in the user's editor (Ctrl+Shift+,), creating it
    /// from the commented template first if needed. The live-reload watcher
    /// then applies any save the user makes.
    fn open_config(&self) {
        let Some(path) = &self.config_path else { return };
        if let Err(e) = crate::config::open_in_editor(path) {
            eprintln!("rusty_term: open config: {e}");
        }
    }

    /// Re-read the config file and apply what can change live: theme (every
    /// tab's parser palette + grid recolor, the chrome bar, the window border)
    /// and scrollback cap. Shell changes apply to tabs opened afterwards;
    /// font and window size are launch-time choices. Parse warnings go to
    /// stderr, same as at startup.
    fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        let args = vec!["--config".to_string(), path.to_string_lossy().into_owned()];
        let (new, warnings) = crate::config::Config::load(&args);
        for w in &warnings {
            eprintln!("rusty_term: {w}");
        }
        for tab in &self.tabs {
            for p in &tab.panes {
                let mut g = p.grid.lock();
                let old = p.parser.lock().retheme(new.theme);
                if old != new.theme {
                    g.retheme(&old, &new.theme);
                }
                g.set_scrollback_max(new.scrollback.unwrap_or(crate::core::SCROLLBACK_MAX));
            }
        }
        self.theme = new.theme;
        self.config = new;
        if let Some(window) = &self.window {
            apply_chrome(window, &self.theme);
            window.request_redraw();
        }
    }
}

impl App<'_> {
    /// Open the in-app settings page over the active tab, seeded from the live
    /// configuration. `Ctrl+,` and the dropdown's *Settings* entry route here.
    fn open_settings(&mut self) {
        let s = Settings::new(
            &self.theme,
            self.font_px,
            self.config.cursor_style.unwrap_or_default(),
            self.config.cursor_blink.unwrap_or(false),
            self.config.ligatures.unwrap_or(true),
            self.config.scrollback.unwrap_or(crate::core::SCROLLBACK_MAX),
            self.config.shell.as_deref(),
            &self.shells,
        );
        self.overlay = Some(Overlay::Settings(s));
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Open the shell-launcher dropdown (the `▾` button): the detected shells
    /// plus *Settings* and *Open config file* entries.
    fn open_menu(&mut self) {
        let items = shell_menu_items(&self.shells);
        self.overlay = Some(Overlay::Menu { items, sel: 0 });
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Close any open overlay, persisting settings changes on the way out.
    fn close_overlay(&mut self) {
        if let Some(Overlay::Settings(s)) = self.overlay.take()
            && s.dirty
        {
            self.persist_settings(&s);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Write the settings page's values to the config file. The live-reload
    /// watcher re-reads the save; live application already happened per change.
    fn persist_settings(&self, s: &Settings) {
        let Some(path) = &self.config_path else { return };
        if let Err(e) = crate::config::save_settings(path, &s.edits()) {
            eprintln!("rusty_term: save settings: {e}");
        }
    }

    /// Route a key to the open overlay (it owns all input while up): arrows
    /// navigate / change, Enter activates, digits pick a menu row, Esc closes.
    fn overlay_key(&mut self, event: &KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_overlay(),
            Key::Named(NamedKey::ArrowUp) => self.overlay_move(false),
            Key::Named(NamedKey::ArrowDown) => self.overlay_move(true),
            Key::Named(NamedKey::ArrowLeft) => self.overlay_change(false),
            Key::Named(NamedKey::ArrowRight) => self.overlay_change(true),
            Key::Named(NamedKey::Enter) => self.overlay_activate(),
            Key::Character(s) => {
                if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10)) {
                    self.menu_pick_index(d as usize);
                }
            }
            _ => {}
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Move the highlight within the overlay.
    fn overlay_move(&mut self, forward: bool) {
        match &mut self.overlay {
            Some(Overlay::Menu { items, sel }) => {
                let n = items.len();
                if n > 0 {
                    *sel = if forward { (*sel + 1) % n } else { (*sel + n - 1) % n };
                }
            }
            Some(Overlay::Settings(s)) => s.move_sel(forward),
            None => {}
        }
    }

    /// Change the highlighted setting (no effect on the menu) and apply it live.
    fn overlay_change(&mut self, forward: bool) {
        let field = match &mut self.overlay {
            Some(Overlay::Settings(s)) => s.change(forward),
            _ => return,
        };
        self.apply_setting(field);
    }

    /// Activate the highlighted overlay row: pick a menu item, or cycle the
    /// highlighted setting (Enter mirrors →).
    fn overlay_activate(&mut self) {
        match &self.overlay {
            Some(Overlay::Menu { items, sel }) => {
                if let Some(kind) = items.get(*sel).map(|i| i.kind) {
                    self.menu_pick(kind);
                }
            }
            Some(Overlay::Settings(_)) => self.overlay_change(true),
            None => {}
        }
    }

    /// Pick menu row `n` (1-based, from a digit key); ignored off the menu.
    fn menu_pick_index(&mut self, n: usize) {
        let kind = match &self.overlay {
            Some(Overlay::Menu { items, .. }) if n >= 1 => items.get(n - 1).map(|i| i.kind),
            _ => None,
        };
        if let Some(kind) = kind {
            self.menu_pick(kind);
        }
    }

    /// Carry out a chosen menu item.
    fn menu_pick(&mut self, kind: MenuKind) {
        match kind {
            MenuKind::LaunchShell(i) => {
                let shell = self.shells.get(i).map(|s| s.path.to_string_lossy().into_owned());
                self.overlay = None;
                if let Err(e) = self.spawn_tab_with(shell) {
                    eprintln!("rusty_term: new tab: {e}");
                }
            }
            MenuKind::Settings => self.open_settings(),
            MenuKind::EditConfig => {
                self.overlay = None;
                self.open_config();
            }
        }
    }

    /// Handle a click in the overlay body (below the chrome bar): select the row
    /// under the pointer, activating it for a menu.
    fn overlay_click(&mut self, y: f64) {
        let screen_row = (y.max(0.0) as usize) / self.cell_h;
        let Some(grid_row) = screen_row.checked_sub(1) else { return };
        let Some(i) = grid_row.checked_sub(OVERLAY_ITEMS_TOP) else { return };
        let activate = match &mut self.overlay {
            Some(Overlay::Menu { items, sel }) if i < items.len() => {
                *sel = i;
                true
            }
            Some(Overlay::Settings(s)) if i < s.len() => {
                s.select(i);
                false
            }
            _ => false,
        };
        if activate {
            self.overlay_activate();
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Apply a just-changed setting to the running terminal. Values are
    /// snapshotted first so the overlay borrow ends before we mutate `self`.
    fn apply_setting(&mut self, field: Field) {
        let Some(Overlay::Settings(s)) = self.overlay.as_ref() else { return };
        let theme = crate::config::preset(s.theme_name());
        let font_size = s.font_size();
        let cursor = s.cursor();
        let blink = s.blink();
        let ligatures = s.ligatures();
        let scrollback = s.scrollback();
        let shell = s.shell_path();
        match field {
            Field::Theme => {
                if let Some(t) = theme {
                    self.apply_theme_live(t);
                }
            }
            Field::FontSize => self.rebuild_font(font_size, self.config.ligatures.unwrap_or(true)),
            Field::Cursor => {
                self.config.cursor_style = Some(cursor);
                let blink = self.config.cursor_blink.unwrap_or(false);
                self.set_cursor_all(cursor, blink);
            }
            Field::Blink => {
                self.config.cursor_blink = Some(blink);
                let shape = self.config.cursor_style.unwrap_or_default();
                self.set_cursor_all(shape, blink);
            }
            Field::Ligatures => self.rebuild_font(self.font_px, ligatures),
            Field::Scrollback => {
                self.config.scrollback = Some(scrollback);
                for tab in &self.tabs {
                    for p in &tab.panes {
                        p.grid.lock().set_scrollback_max(scrollback);
                    }
                }
            }
            Field::Shell => self.config.shell = shell,
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Set the default (and live) cursor on every pane of every tab.
    fn set_cursor_all(&self, shape: crate::core::CursorShape, blink: bool) {
        for tab in &self.tabs {
            for p in &tab.panes {
                p.grid.lock().set_default_cursor(shape, blink);
            }
        }
    }

    /// Recolor every tab to `new` (mirrors the config live-reload path) and
    /// repaint the chrome / window border.
    fn apply_theme_live(&mut self, new: Theme) {
        for tab in &self.tabs {
            for p in &tab.panes {
                let mut g = p.grid.lock();
                let old = p.parser.lock().retheme(new);
                if old != new {
                    g.retheme(&old, &new);
                }
            }
        }
        self.theme = new;
        self.config.theme = new;
        if let Some(window) = &self.window {
            apply_chrome(window, &self.theme);
        }
    }

    /// Rebuild the glyph cache at `px` / `ligatures`, re-fit the grid to the new
    /// cell size, and rebuild the renderer (the GPU atlas is font-bound).
    fn rebuild_font(&mut self, px: f32, ligatures: bool) {
        let Some(font_set) = font::load_set(
            self.config.font.as_deref(),
            self.config.font_bold.as_deref(),
            self.config.font_italic.as_deref(),
            self.config.font_bold_italic.as_deref(),
            self.config.font_fallback.as_deref(),
        ) else {
            return;
        };
        let Some(font) = FontCache::new(font_set, px, ligatures) else { return };
        let (cw, ch) = font.cell_size();
        self.font = font;
        self.cell_w = cw.max(1);
        self.cell_h = ch.max(1);
        self.font_px = px;
        self.config.font_size = Some(px);
        self.config.ligatures = Some(ligatures);
        if let Some(window) = self.window.clone() {
            if let Some(r) = self.make_renderer(window.clone()) {
                self.renderer = Some(r);
            }
            let size = window.inner_size();
            self.apply_size(size.width, size.height);
        }
    }

    /// Render the open overlay into a fresh full-area grid (chrome stays on top).
    fn build_overlay_grid(&self) -> Grid {
        let (cols, rows) = (self.cols as usize, self.rows as usize);
        let mut g = Grid::new(cols, rows);
        let (fg, bg) = (self.theme.fg, self.theme.bg);
        let dim = mix(self.theme.fg, self.theme.bg, 110);
        for cell in &mut g.cells {
            *cell = Cell::blank();
            cell.fg = fg;
            cell.bg = bg;
        }
        let bar = cols.min(56);
        let footer = rows.saturating_sub(2);
        match self.overlay.as_ref() {
            Some(Overlay::Menu { items, sel }) => {
                write_row(&mut g, 0, 2, "Open a new shell or page", dim, bg);
                for (i, item) in items.iter().enumerate() {
                    let r = OVERLAY_ITEMS_TOP + i;
                    if r >= footer {
                        break;
                    }
                    let on = i == *sel;
                    let (rfg, rbg) = if on { (bg, self.theme.fg) } else { (fg, bg) };
                    if on {
                        fill_row(&mut g, r, 0, bar, rfg, rbg);
                    }
                    write_row(&mut g, r, 2, &format!("{}. {}", i + 1, item.label), rfg, rbg);
                }
                write_row(&mut g, footer, 2, "Up/Down select   Enter/1-9 open   Esc close", dim, bg);
            }
            Some(Overlay::Settings(s)) => {
                write_row(&mut g, 0, 2, "Settings", fg, bg);
                for (i, (label, value)) in s.display().into_iter().enumerate() {
                    let r = OVERLAY_ITEMS_TOP + i;
                    if r >= footer {
                        break;
                    }
                    let on = i == s.sel;
                    let (rfg, rbg) = if on { (bg, self.theme.fg) } else { (fg, bg) };
                    if on {
                        fill_row(&mut g, r, 0, bar, rfg, rbg);
                    }
                    write_row(&mut g, r, 2, label, rfg, rbg);
                    let value = if on { format!("< {value} >") } else { format!("  {value}") };
                    write_row(&mut g, r, 22, &value, rfg, rbg);
                }
                write_row(&mut g, footer, 2, "Up/Down select   Left/Right change   Esc close & save", dim, bg);
            }
            None => {}
        }
        g
    }
}

/// Build the shell-launcher dropdown: each detected shell (launching a new tab)
/// then the *Settings* and *Open config file* entries.
fn shell_menu_items(shells: &[crate::shells::DetectedShell]) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = shells
        .iter()
        .enumerate()
        .map(|(i, s)| MenuItem { label: s.name.to_string(), kind: MenuKind::LaunchShell(i) })
        .collect();
    items.push(MenuItem { label: "Settings".to_string(), kind: MenuKind::Settings });
    items.push(MenuItem { label: "Open config file".to_string(), kind: MenuKind::EditConfig });
    items
}

/// Write ASCII `text` into grid `row` from `col` (width-1 cells) in the given
/// colors, clipping at the grid edge. Overlay-page labels are all ASCII.
fn write_row(grid: &mut Grid, row: usize, col: usize, text: &str, fg: u32, bg: u32) {
    for (i, ch) in text.chars().enumerate() {
        let c = col + i;
        if c >= grid.cols || row >= grid.rows {
            break;
        }
        let mut cell = Cell::blank();
        cell.ch = ch;
        cell.fg = fg;
        cell.bg = bg;
        grid.set_cell(c, row, cell);
    }
}

/// Fill grid `row` cells `[col0, col1)` with a blank cell in the given colors
/// (the selection bar behind a highlighted overlay row).
fn fill_row(grid: &mut Grid, row: usize, col0: usize, col1: usize, fg: u32, bg: u32) {
    for c in col0..col1.min(grid.cols) {
        let mut cell = Cell::blank();
        cell.fg = fg;
        cell.bg = bg;
        grid.set_cell(c, row, cell);
    }
}

/// Write `text` into the chrome `row` from `*col` up to (not including)
/// `limit`, advancing wide glyphs by two cells with a flagged trailer and
/// stopping at the boundary. Every written cell adopts `fg`/`bg` and `hit`.
#[allow(clippy::too_many_arguments)]
fn put_text(
    row: &mut [Cell],
    hits: &mut [Hit],
    col: &mut usize,
    limit: usize,
    text: &str,
    fg: u32,
    bg: u32,
    hit: Hit,
) {
    let limit = limit.min(row.len());
    for ch in text.chars() {
        if *col >= limit {
            break;
        }
        let w = char_width(ch);
        if w == 0 {
            continue; // zero-width scalars don't occupy a cell
        }
        if w == 2 && *col + 1 >= limit {
            break; // a wide glyph's trailer wouldn't fit
        }
        row[*col].ch = ch;
        row[*col].fg = fg;
        row[*col].bg = bg;
        hits[*col] = hit;
        *col += 1;
        if w == 2 {
            row[*col].ch = ' ';
            row[*col].fg = fg;
            row[*col].bg = bg;
            row[*col].flags = WIDE_TRAILER;
            hits[*col] = hit;
            *col += 1;
        }
    }
}

/// Per-channel mix of `t/255` of `b` into `a` (`0xRRGGBB`) — used to derive
/// the chrome bar's bg and dimmed text from the theme without new config keys.
fn mix(a: u32, b: u32, t: u32) -> u32 {
    let chan = |s: u32| {
        let av = (a >> s) & 0xff;
        let bv = (b >> s) & 0xff;
        ((av * (255 - t) + bv * t) / 255) << s
    };
    chan(16) | chan(8) | chan(0)
}

/// Paint the window border with the theme background so the frame reads as
/// part of the terminal (the title bar itself is ours now). Windows 11 only
/// (DWM ignores the attribute on 10); a no-op on other platforms.
fn apply_chrome(window: &Window, theme: &Theme) {
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::{Color, WindowExtWindows};
        let c = |rgb: u32| Color::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        window.set_border_color(Some(c(theme.bg)));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, theme);
    }
}

impl ApplicationHandler<UserEvent> for App<'_> {
    /// Drive cursor blink: when the active tab's cursor blinks and is visible,
    /// wake on a fixed interval to toggle its phase and repaint; otherwise wait
    /// for the next real event (no idle wakeups).
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        const BLINK: Duration = Duration::from_millis(530);
        let blinking = self.pane().is_some_and(|p| {
            let g = p.grid.lock();
            g.cursor_blink && g.cursor_visible && g.view_offset == 0
        });
        if !blinking {
            self.cursor_blink_on = true;
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_blink) >= BLINK {
            self.cursor_blink_on = !self.cursor_blink_on;
            self.last_blink = now;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.last_blink + BLINK));
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let width = (self.cols as usize * self.cell_w) as u32;
        // One extra cell row on top for the chrome bar.
        let height = ((self.rows as usize + 1) * self.cell_h) as u32;
        let attrs = Window::default_attributes()
            .with_title("rusty_term")
            .with_decorations(false)
            .with_inner_size(winit::dpi::PhysicalSize::new(width, height));
        // Keep the DWM drop shadow so the borderless window still reads as
        // raised above the desktop.
        #[cfg(target_os = "windows")]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_undecorated_shadow(true)
        };
        let Ok(window) = event_loop.create_window(attrs) else {
            event_loop.exit();
            return;
        };
        let window = Arc::new(window);
        apply_chrome(&window, &self.theme);
        self.window = Some(window.clone());
        // Let the OS deliver IME composition events (CJK, dead keys).
        window.set_ime_allowed(true);
        match self.make_renderer(window.clone()) {
            Some(r) => self.renderer = Some(r),
            None => {
                event_loop.exit();
                return;
            }
        }
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(size) => {
                self.apply_size(size.width, size.height);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // A settings page / shell menu, if open, owns all key input.
                if self.overlay.is_some() {
                    self.overlay_key(&event);
                    return;
                }
                // In search mode, keys edit the query / step matches; nothing
                // reaches the keymap or the child until Esc exits.
                if self.search_key(&event) {
                    return;
                }
                // Terminal-owned shortcuts (configurable via the `[keys]` config
                // section) are looked up before native encoding, so a bound chord
                // never reaches the child.
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(key) = chord_key(code)
                {
                    let chord = Chord::new(
                        self.mods.control_key(),
                        self.mods.shift_key(),
                        self.mods.alt_key(),
                        key,
                    );
                    if let Some(action) = self.config.keys.action(chord) {
                        self.run_action(action, event_loop);
                        return;
                    }
                }
                // While the IME is composing, it owns key input; don't also
                // encode it (the committed text arrives via `WindowEvent::Ime`).
                if self.pane().is_some_and(|p| !p.grid.lock().ime_preedit.is_empty()) {
                    return;
                }
                if let Some(bytes) = super::input::encode(&event.logical_key, self.mods, false) {
                    if let Some(p) = self.pane_mut() {
                        let _ = p.writer.write(&bytes);
                    }
                    // Typing returns the view to the live bottom, as most
                    // terminals do, so the echoed input is visible.
                    self.snap_to_bottom();
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, _) => {
                    if let Some(p) = self.pane() {
                        p.grid.lock().ime_preedit = text;
                    }
                    self.update_ime_area();
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                Ime::Commit(text) => {
                    if let Some(p) = self.pane_mut() {
                        p.grid.lock().ime_preedit.clear();
                        let _ = p.writer.write(text.as_bytes());
                    }
                    self.snap_to_bottom();
                }
                Ime::Enabled => {}
                Ime::Disabled => {
                    if let Some(p) = self.pane() {
                        p.grid.lock().ime_preedit.clear();
                    }
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                // The edge band shows a resize cursor; everywhere else default.
                let icon = match self.resize_zone(position.x, position.y) {
                    Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => {
                        CursorIcon::NwseResize
                    }
                    Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => {
                        CursorIcon::NeswResize
                    }
                    Some(ResizeDirection::West | ResizeDirection::East) => CursorIcon::EwResize,
                    Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
                    None => CursorIcon::Default,
                };
                if let Some(window) = &self.window {
                    window.set_cursor(icon);
                }
                if self.selecting
                    && let Some(anchor) = self.sel_anchor
                    && let Some(p) = self.pane()
                {
                    let head = self.cell_in_focused(position.x, position.y);
                    p.grid.lock().selection = Some(Selection { anchor, head });
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    // Ctrl+click follows an OSC 8 hyperlink under the pointer,
                    // suppressing selection and mouse reporting for that click.
                    if self.mods.control_key() && self.open_link_under_pointer() {
                        return;
                    }
                    self.on_left_press(event_loop);
                    let (sh, al, ct) =
                        (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                    self.report_mouse(|c, r| {
                        MouseEvent::new_point(c, r).with_button(true).with_modifiers(sh, al, ct)
                    });
                }
                ElementState::Released => {
                    self.selecting = false;
                    let (sh, al, ct) =
                        (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                    self.report_mouse(|c, r| {
                        MouseEvent::new_point(c, r).with_button(false).with_modifiers(sh, al, ct)
                    });
                }
            },
            // Wheel up browses into scrollback history, wheel down back toward
            // the live bottom. A notch is `WHEEL_LINES`; trackpads report pixels.
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as isize * WHEEL_LINES,
                    MouseScrollDelta::PixelDelta(p) => (p.y / self.cell_h as f64).round() as isize,
                };
                if lines == 0 {
                    return;
                }
                let (sh, al, ct) =
                    (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                if self.report_mouse(|c, r| {
                    MouseEvent::new_point(c, r).with_scroll(lines).with_modifiers(sh, al, ct)
                }) {
                    return;
                }
                self.scroll_active(lines);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw(id) => {
                self.service_clipboard(id);
                self.service_notifications(id);
                // Output on a background tab doesn't repaint; its bar label
                // refreshes with the next frame the active tab causes.
                if self.tabs.get(self.active).is_some_and(|t| t.panes.iter().any(|p| p.id == id))
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
            UserEvent::Exit(id) => self.close_pane(id, event_loop),
            UserEvent::ConfigChanged => self.reload_config(),
        }
    }
}

/// Encode clipboard `text` for the child: normalize line endings to CR, and
/// when `bracketed` wrap it in `ESC[200~`/`ESC[201~`, stripping any embedded
/// end marker first so the payload can't close the bracket early (a
/// paste-injection guard).
fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.into_bytes()
    }
}

/// Open `url` with the OS default handler when its scheme is one we allow
/// (see [`is_openable_url`]). Ctrl+click on an OSC 8 hyperlink routes here.
/// Returns whether a handler was launched.
fn open_url(url: &str) -> bool {
    if !is_openable_url(url) {
        return false;
    }
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
    spawned.is_ok()
}

/// Whether `url`'s scheme is one we're willing to hand to the OS opener.
/// Restricting it keeps arbitrary or custom-scheme URIs from terminal output
/// from reaching the shell's URL handler. The scheme match is case-insensitive.
fn is_openable_url(url: &str) -> bool {
    const ALLOWED: [&str; 5] = ["http://", "https://", "ftp://", "file://", "mailto:"];
    let b = url.as_bytes();
    ALLOWED
        .iter()
        .any(|p| b.len() > p.len() && b[..p.len()].eq_ignore_ascii_case(p.as_bytes()))
}

/// Raise an OS desktop notification (OSC 9/777), per-platform with no extra
/// crates (mirroring [`open_url`]). The untrusted title/body are passed as
/// environment variables so they can't inject into the spawned PowerShell /
/// AppleScript / `notify-send` command.
fn notify(title: &str, body: &str) {
    use std::process::Command;
    #[cfg(target_os = "windows")]
    let mut cmd = {
        const PS: &str = "Add-Type -AssemblyName System.Windows.Forms; $n = New-Object System.Windows.Forms.NotifyIcon; $n.Icon = [System.Drawing.SystemIcons]::Information; $n.BalloonTipTitle = $env:RT_TITLE; $n.BalloonTipText = $env:RT_BODY; $n.Visible = $true; $n.ShowBalloonTip(6000); Start-Sleep -Seconds 7; $n.Dispose()";
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", PS]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("osascript");
        c.args(["-e", "display notification (system attribute \"RT_BODY\") with title (system attribute \"RT_TITLE\")"]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("notify-send");
        c.arg(title).arg(body);
        c
    };
    let _ = cmd.env("RT_TITLE", title).env("RT_BODY", body).spawn();
}

/// Map a winit physical key to a toolkit-free [`crate::keymap::Key`] for binding
/// lookup, or `None` for keys that can't be bound (they fall through to native
/// encoding). Letters and digits are position-based, matching most terminals.
fn chord_key(code: KeyCode) -> Option<crate::keymap::Key> {
    use crate::keymap::Key;
    Some(match code {
        KeyCode::Tab => Key::Tab,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Comma => Key::Char(','),
        KeyCode::Period => Key::Char('.'),
        KeyCode::KeyA => Key::Char('a'),
        KeyCode::KeyB => Key::Char('b'),
        KeyCode::KeyC => Key::Char('c'),
        KeyCode::KeyD => Key::Char('d'),
        KeyCode::KeyE => Key::Char('e'),
        KeyCode::KeyF => Key::Char('f'),
        KeyCode::KeyG => Key::Char('g'),
        KeyCode::KeyH => Key::Char('h'),
        KeyCode::KeyI => Key::Char('i'),
        KeyCode::KeyJ => Key::Char('j'),
        KeyCode::KeyK => Key::Char('k'),
        KeyCode::KeyL => Key::Char('l'),
        KeyCode::KeyM => Key::Char('m'),
        KeyCode::KeyN => Key::Char('n'),
        KeyCode::KeyO => Key::Char('o'),
        KeyCode::KeyP => Key::Char('p'),
        KeyCode::KeyQ => Key::Char('q'),
        KeyCode::KeyR => Key::Char('r'),
        KeyCode::KeyS => Key::Char('s'),
        KeyCode::KeyT => Key::Char('t'),
        KeyCode::KeyU => Key::Char('u'),
        KeyCode::KeyV => Key::Char('v'),
        KeyCode::KeyW => Key::Char('w'),
        KeyCode::KeyX => Key::Char('x'),
        KeyCode::KeyY => Key::Char('y'),
        KeyCode::KeyZ => Key::Char('z'),
        KeyCode::Digit0 => Key::Char('0'),
        KeyCode::Digit1 => Key::Char('1'),
        KeyCode::Digit2 => Key::Char('2'),
        KeyCode::Digit3 => Key::Char('3'),
        KeyCode::Digit4 => Key::Char('4'),
        KeyCode::Digit5 => Key::Char('5'),
        KeyCode::Digit6 => Key::Char('6'),
        KeyCode::Digit7 => Key::Char('7'),
        KeyCode::Digit8 => Key::Char('8'),
        KeyCode::Digit9 => Key::Char('9'),
        _ => return None,
    })
}

/// Build the OSC 52 clipboard query reply for the child: `OSC 52 ; c ; <b64>`
/// (BEL-terminated), answering a `OSC 52 ; … ; ?` query from the system
/// clipboard.
fn osc52_reply(text: &str) -> Vec<u8> {
    let mut out = Vec::from(&b"\x1b]52;c;"[..]);
    out.extend_from_slice(crate::core::base64_encode(text.as_bytes()).as_bytes());
    out.push(0x07);
    out
}

#[cfg(test)]
mod tests {
    use super::{Hit, MenuKind, encode_paste, is_openable_url, mix, osc52_reply, put_text, shell_menu_items};
    use crate::core::Cell;

    #[test]
    fn paste_normalizes_newlines_to_cr() {
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc");
    }

    #[test]
    fn unbracketed_paste_is_raw_after_newline_fix() {
        assert_eq!(encode_paste("hello", false), b"hello");
    }

    #[test]
    fn bracketed_paste_wraps_and_strips_end_marker() {
        // An embedded end marker must not close the bracket early.
        assert_eq!(encode_paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }

    #[test]
    fn bracketed_paste_wraps_plain_text() {
        assert_eq!(encode_paste("ls", true), b"\x1b[200~ls\x1b[201~");
    }

    #[test]
    fn only_known_url_schemes_are_openable() {
        assert!(is_openable_url("https://example.com"));
        assert!(is_openable_url("HTTP://Example.COM")); // scheme is case-insensitive
        assert!(is_openable_url("mailto:a@b.com"));
        assert!(is_openable_url("file:///etc/hosts"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("data:text/html,x"));
        assert!(!is_openable_url("https://")); // nothing after the scheme
        assert!(!is_openable_url("notaurl"));
    }

    #[test]
    fn osc52_reply_wraps_base64() {
        // "hi" -> base64 "aGk=", framed as an OSC 52 clipboard reply (BEL).
        assert_eq!(osc52_reply("hi"), b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn put_text_writes_cells_and_hits_up_to_limit() {
        let mut row = vec![Cell::blank(); 8];
        let mut hits = vec![Hit::Drag; 8];
        let mut col = 1;
        put_text(&mut row, &mut hits, &mut col, 4, "abcdef", 0x111111, 0x222222, Hit::NewTab);
        assert_eq!(col, 4, "stops at the limit");
        assert_eq!(row[1].ch, 'a');
        assert_eq!(row[3].ch, 'c');
        assert_eq!(row[4].ch, ' ', "cell past the limit untouched");
        assert_eq!(row[1].fg, 0x111111);
        assert!(hits[1] == Hit::NewTab && hits[3] == Hit::NewTab);
        assert!(hits[4] == Hit::Drag);
    }

    #[test]
    fn put_text_gives_wide_glyphs_a_trailer() {
        use crate::core::WIDE_TRAILER;
        let mut row = vec![Cell::blank(); 8];
        let mut hits = vec![Hit::Drag; 8];
        let mut col = 0;
        put_text(&mut row, &mut hits, &mut col, 8, "你x", 0xAAAAAA, 0x0, Hit::Close);
        assert_eq!(col, 3, "wide glyph advances two cells");
        assert_eq!(row[0].ch, '你');
        assert_ne!(row[1].flags & WIDE_TRAILER, 0, "trailer flagged");
        assert_eq!(row[2].ch, 'x');
    }

    #[test]
    fn put_text_breaks_when_wide_trailer_does_not_fit() {
        let mut row = vec![Cell::blank(); 4];
        let mut hits = vec![Hit::Drag; 4];
        let mut col = 0;
        put_text(&mut row, &mut hits, &mut col, 2, "a你", 0x0, 0x0, Hit::Close);
        assert_eq!(col, 1, "wide head would straddle the limit; stops");
        assert_eq!(row[1].ch, ' ');
    }

    #[test]
    fn mix_blends_toward_second_color() {
        assert_eq!(mix(0x000000, 0xFFFFFF, 0), 0x000000);
        assert_eq!(mix(0x000000, 0xFFFFFF, 255), 0xFFFFFF);
        let mid = mix(0x000000, 0xFFFFFF, 128);
        assert!((0x7F..=0x81).contains(&(mid & 0xff)), "roughly half: {mid:#x}");
    }

    #[test]
    fn shell_menu_lists_shells_then_settings_and_config() {
        use crate::shells::DetectedShell;
        use std::path::PathBuf;
        let shells = vec![
            DetectedShell { name: "pwsh", path: PathBuf::from("/x/pwsh") },
            DetectedShell { name: "bash", path: PathBuf::from("/bin/bash") },
        ];
        let items = shell_menu_items(&shells);
        assert_eq!(items.len(), 4, "2 shells + Settings + config file");
        assert!(matches!(items[0].kind, MenuKind::LaunchShell(0)));
        assert!(matches!(items[1].kind, MenuKind::LaunchShell(1)));
        assert!(matches!(items[2].kind, MenuKind::Settings));
        assert!(matches!(items[3].kind, MenuKind::EditConfig));
    }
}
