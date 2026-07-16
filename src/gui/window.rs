//! The windowed front-end: a `winit` event loop driving one or more real OS
//! windows (C13), with `softbuffer` CPU presentation.
//!
//! [`App`] is a thin router owning a [`WindowState`] per open window: window
//! events dispatch by `WindowId`, PTY wakeups by pane id (ids come from a
//! shared counter so they're unique across windows). New windows open with
//! Ctrl+Shift+N or `rusty_term ctl new-window`; `rusty_term ctl quake`
//! toggles a dropdown "quake" window docked to the top of the monitor (G30).
//! The loop exits when the last window closes.
//!
//! Each window is borderless (`decorations(false)`) and draws its own chrome: a
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
use winit::keyboard::{KeyCode, KeyLocation, ModifiersState, PhysicalKey};
use std::path::PathBuf;
use winit::window::{CursorIcon, Fullscreen, ResizeDirection, UserAttentionType, Window, WindowId};

use crate::backend::{Backend, BackendHandle};
use crate::config::{ClipboardPolicy, Config, LaunchMode};
use crate::core::{
    ATTR_BOLD, ATTR_UNDERLINE, ATTR_UNDERLINE_COLOR, AnsiParser, Cell, Grid, Selection, Theme,
    WIDE_TRAILER, char_width,
};
use crate::keymap::{Action, Chord};
use crate::gui::mouse::{MouseButtonKind, MouseEvent, MousePoint, SgrEncoder};
use super::font::{self, FontCache, GlyphSource};
use super::layout::{Dir, Layout, Rect};
use super::render::{CpuRenderer, PaneFrame, Renderer};
use super::settings::{CATEGORIES, Field, Settings, Widget};

/// Built-in defaults, overridable via the config file (`[window]` section).
const FONT_PX: f32 = 18.0;
/// Step for the Ctrl+=/Ctrl+- runtime font zoom (`Action::FontSizeUp/Down`).
const FONT_ZOOM_STEP: f32 = 1.0;
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;
/// Pixel band at the window edges acting as a resize handle (the native frame
/// is gone with decorations off).
const RESIZE_BORDER: f64 = 6.0;
/// Widest a chrome-bar tab grows (label plus its × button); tabs size to
/// their titles between [`TAB_MIN`] and this.
const TAB_CELLS: usize = 26;
/// Narrowest a tab shrinks when the strip crowds before falling back to the
/// scrolled uniform-width strip: room for the ×, padding, and a few label
/// characters.
const TAB_MIN: usize = 12;
/// Grid row where overlay (menu / settings) list rows begin (header sits above).
const OVERLAY_ITEMS_TOP: usize = 2;
/// Settings-page layout (cells): the category sidebar's width, the rows one
/// setting card occupies (title+widget, description, spacing), and the rows
/// reserved below the list (a blank spacer plus the key-hint footer).
const SETTINGS_SIDEBAR_W: usize = 16;
const SETTINGS_ROW_H: usize = 3;
const SETTINGS_FOOTER_H: usize = 2;
/// First content row of the settings page: one blank row of breathing space
/// under the chrome bar, so the sidebar band and page title don't sit flush
/// against the tab strip.
const SETTINGS_TOP: usize = 1;
/// Width of the command dock in cells (its divider column adds one more).
const DOCK_CELLS: usize = 30;
/// Narrowest pane area the dock may leave behind; below this it auto-hides.
const DOCK_MIN_PANE: usize = 40;
/// First grid row of the setting cards: the page title + its rule sit on
/// the two rows above (the strip tab names the page, so no header row).
const SETTINGS_CARDS_TOP: usize = SETTINGS_TOP + 2;
/// Width of the clickable `/ search` affordance at the title row's right.
const SETTINGS_SEARCH_W: usize = 14;
/// First column of a setting card's text (right of the sidebar + separator).
const SETTINGS_CARD_X: usize = SETTINGS_SIDEBAR_W + 3;
/// Cells left free at the cards' right edge (the value widget ends here).
const SETTINGS_RIGHT_PAD: usize = 3;
/// Cap on the card column's width. On a wide window an uncapped card puts
/// the value ~200 cells from its label — the eye loses the pairing — so the
/// content column stays readable-width like a settings page, not a table.
const SETTINGS_CARD_W_MAX: usize = 80;
/// Two clicks on the drag strip within this window toggle maximize.
const DOUBLE_CLICK_MS: u128 = 400;
/// Default inner margin around the pane area (`[window] padding` overrides).
const DEFAULT_PAD: u32 = 8;

/// One pane-resize keypress moves the split boundary by this ratio fraction.
const RESIZE_STEP: f32 = 0.05;
/// Scrollback lines moved per mouse-wheel notch.
const WHEEL_LINES: isize = 3;
/// Dragging a selection past a pane's top/bottom edge by more than this many
/// pixels auto-scrolls the viewport, so text beyond what's currently visible
/// can still be reached — matches most editors/terminals with a scrollback.
const DRAG_SCROLL_MARGIN: f64 = 24.0;
/// Auto-scroll step cadence while the pointer is held past the edge.
const DRAG_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// Wakeups sent from per-tab PTY reader threads into the winit loop, tagged
/// with the tab id they concern.
pub(crate) enum UserEvent {
    /// New output was parsed into the tab's grid; repaint if it's the active one.
    Redraw(u64),
    /// The tab's child exited; close that tab (the last one closes the window).
    Exit(u64),
    /// The config file changed on disk; reload and apply what can change live.
    ConfigChanged,
    /// A control-socket request (`--single-instance` / `rusty_term ctl`),
    /// with the channel its connection thread waits on for the reply.
    #[cfg_attr(windows, allow(dead_code))] // constructed once the Windows named-pipe transport lands (G31)
    Control(super::control::CtlCommand, std::sync::mpsc::Sender<String>),
}

/// An in-flight cursor-trail hop (G36): from where, to where, started when.
#[derive(Clone, Copy)]
struct Trail {
    from: (usize, usize),
    to: (usize, usize),
    since: Instant,
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
    /// Overflow indicator: more tabs exist than fit in the strip. Clicking
    /// cycles to the next tab, scrolling the strip to reveal it.
    MoreTabs,
    /// The settings page's own strip tab, shown while the page is open
    /// (already active; clicking it is a no-op).
    SettingsTab,
    /// Close the settings page (the settings tab's × button).
    CloseSettings,
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

#[derive(Clone)]
enum MenuKind {
    /// Launch a new tab running detected shell `[index]`.
    LaunchShell(usize),
    /// Launch a new tab from configured profile `[index]`.
    LaunchProfile(usize),
    /// Open this URL with the OS handler (the visible-links menu).
    OpenUrl(String),
    /// Open the in-app settings page.
    Settings,
    /// Open the config file in the user's editor.
    EditConfig,
}

/// Keyboard copy-mode state: the moving cursor in viewport cells, and the
/// selection anchor (absolute coords) once `v`/Space pins one.
struct CopyMode {
    cur: (usize, usize),
    anchor: Option<(usize, usize)>,
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
    /// Unseen-alert badge (bell or finished command while the tab was in the
    /// background / the window unfocused); cleared when the tab is active in
    /// a focused window.
    attention: bool,
    /// Pane zoom: the focused pane temporarily takes the whole tab area
    /// (toggled by `zoom_pane`); cleared by any split/close/focus change.
    zoomed: bool,
}

impl Tab {
    fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }
    /// The tab's pane rectangles within `area`, honoring pane zoom (a
    /// zoomed tab shows only its focused pane, full-area).
    fn rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        if self.zoomed {
            return vec![(self.focus, area)];
        }
        self.layout.rects(area)
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

    let mut event_loop_builder = EventLoop::<UserEvent>::with_user_event();
    // Global quake hotkey (G30 stretch, Windows only): `with_msg_hook` sees
    // WM_HOTKEY before winit does, since RegisterHotKey posts it to this
    // thread's queue rather than a window's. The parsed spec can't be
    // registered until the loop below runs on this thread, so just remember
    // it here; a bad spec warns and the app runs on without a hotkey.
    #[cfg(windows)]
    let quake_hotkey = config.quake_hotkey.as_ref().and_then(|spec| match super::hotkey::parse(spec) {
        Ok(hk) => Some(hk),
        Err(e) => {
            eprintln!("rusty_term: quake_hotkey `{spec}`: {e}");
            None
        }
    });
    #[cfg(windows)]
    let hotkey_pressed = quake_hotkey.map(|_| std::rc::Rc::new(std::cell::Cell::new(false)));
    #[cfg(windows)]
    if let Some(pressed) = &hotkey_pressed {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        event_loop_builder.with_msg_hook(super::hotkey::make_msg_hook(pressed.clone()));
    }
    #[cfg(not(windows))]
    let hotkey_pressed: Option<std::rc::Rc<std::cell::Cell<bool>>> = None;
    let event_loop = event_loop_builder.build()?;
    // Now that the loop's thread is established, actually register.
    #[cfg(windows)]
    let hotkey_registered = quake_hotkey.is_some_and(super::hotkey::register);
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

    // Control socket/pipe (`single_instance` config / `--single-instance`):
    // serve `rusty_term ctl` requests and let a second launch reuse this
    // instance.
    if config.single_instance.unwrap_or(false) || args.iter().any(|a| a == "--single-instance") {
        match super::control::serve(proxy.clone()) {
            Ok(path) => eprintln!("rusty_term: control socket at {}", path.display()),
            Err(e) => eprintln!("rusty_term: control socket unavailable: {e}"),
        }
    }

    let profile_override = crate::config::flag_value(&args, "--profile").map(str::to_string);
    let mut app = App {
        backend,
        config: config.clone(),
        config_path,
        profile_override,
        proxy,
        next_id: std::rc::Rc::new(std::cell::Cell::new(0)),
        shells: crate::shells::detect_all(),
        windows: Vec::new(),
        focused_window: None,
        quake_hotkey_pressed: hotkey_pressed,
    };
    // The first window (its OS window comes with the loop's `resumed`).
    // Building it up front surfaces font/shell errors before the loop runs.
    let mut first = app.new_window_state(false)?;
    // A session file builds the initial tab set; otherwise one default shell
    // (more come from Ctrl+Shift+T / the + button either way).
    let session = first.config.session.clone();
    let mut opened = 0usize;
    if let Some(path) = &session {
        let (tabs, warns) = crate::config::load_session(path);
        for w in &warns {
            eprintln!("rusty_term: session: {w}");
        }
        for t in tabs {
            if let Err(e) = first.spawn_session_tab(&t) {
                eprintln!("rusty_term: session tab: {e}");
            } else {
                opened += 1;
            }
        }
        if opened > 0 {
            first.active = 0; // land on the session's first tab
        }
    }
    if opened == 0 {
        first.spawn_tab()?;
    }
    app.windows.push(first);
    event_loop.run_app(&mut app)?;
    #[cfg(windows)]
    if hotkey_registered {
        super::hotkey::unregister();
    }
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
                // Always signal, even mid synchronized-output window: the
                // `Redraw` handler itself checks `sync_output_active` and
                // skips painting, so the proxy's liveness (this send failing
                // signals the window closed) is still checked every chunk.
                if proxy.send_event(UserEvent::Redraw(id)).is_err() {
                    break; // loop gone
                }
            }
            Err(_) => break,
        }
    }
    let _ = proxy.send_event(UserEvent::Exit(id));
}

/// An in-progress drag-to-reorder of chrome-bar tabs: pressing a tab arms
/// it, and once the pointer travels past a small slop (so a plain click
/// never reorders) the tab follows the pointer, trading places as it
/// crosses its neighbors' midpoints. `idx` tracks the dragged tab's current
/// position across those moves; releasing the button clears the state.
struct TabDrag {
    idx: usize,
    press_x: f64,
    moving: bool,
}

/// One top-level OS window: its tabs/panes, renderer, chrome, and every bit
/// of per-window UI state (selection, overlays, search, copy mode, …). The
/// [`App`] router owns one of these per open window and dispatches winit
/// events to the right one by `WindowId`.
struct WindowState<'a> {
    /// Spawns the shell behind each new tab.
    backend: &'a dyn Backend,
    /// The effective config; refreshed on live reload so new tabs follow it.
    config: Config,
    /// The config file in effect, for the open shortcut + reload re-reads.
    config_path: Option<std::path::PathBuf>,
    /// The `--profile <name>` this instance was launched with, if any —
    /// reapplied after every `reload_config()` re-read (see its doc comment)
    /// so the launch-time profile override survives a config-file save.
    profile_override: Option<String>,
    tabs: Vec<Tab>,
    /// Index into `tabs` of the session being shown and fed input.
    active: usize,
    /// Monotonic id source for panes, shared by every window so reader-thread
    /// wakeups (`Redraw(id)`/`Exit(id)`) route unambiguously across windows.
    next_id: std::rc::Rc<std::cell::Cell<u64>>,
    proxy: EventLoopProxy<UserEvent>,
    font: FontCache,
    cell_w: usize,
    cell_h: usize,
    /// Inner margin in pixels around the pane area (`[window] padding`). The
    /// chrome bar stays flush; the grid sits inset by this on the other three
    /// sides (and below the bar), painted in the theme background.
    pad: usize,
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
    /// Fractional remainder of the last `MouseWheel` delta-to-lines
    /// conversion. High-resolution wheels and slow trackpad flicks emit many
    /// events with less than one line's worth of delta each; rounding each
    /// event independently (the old behavior) dropped all of them on the
    /// floor and slow scrolling didn't move anything. Accumulating instead
    /// means every bit of delta eventually turns into a scrolled line.
    scroll_accum: f64,
    /// Whether the left button is held (a drag-selection is in progress).
    selecting: bool,
    /// Physical button currently held, if any — drives `?1002` drag-motion
    /// reporting (which button to report) and whether `?1002` reports motion
    /// at all (only while a button is down; `?1003` reports regardless).
    mouse_button_down: Option<MouseButtonKind>,
    /// Cell where the current drag-selection began.
    /// Cell where the current drag-selection began, in **absolute**
    /// coordinates (`(col, abs_row)`) so the anchor survives scrolling
    /// mid-drag.
    sel_anchor: Option<(usize, usize)>,
    /// Per-cell click actions for the chrome bar, rebuilt with each layout.
    hits: Vec<Hit>,
    /// An in-progress tab drag (press on a tab arms it; releasing clears).
    tab_drag: Option<TabDrag>,
    /// Chrome-bar element under the pointer, for hover feedback. `None` when
    /// the pointer is below the bar or over an inert (drag-strip) cell.
    hover: Option<Hit>,
    /// A Ctrl-hovered hyperlink under the pointer in the focused pane's grid,
    /// `(row, start_col, end_col)` — drives the pointer cursor and hover
    /// underline (G22). Recomputed on every `CursorMoved`/`ModifiersChanged`;
    /// `None` when Ctrl isn't held, the pointer is off the grid, or there's
    /// no link there.
    hover_link: Option<(usize, usize, usize)>,
    /// Time of the last single click on the drag strip (double-click detect).
    last_strip_click: Option<Instant>,
    /// Whether the window currently has OS focus. Gates the `?1004` focus
    /// reports (wave 1), bell attention requests, and command-finished
    /// notifications.
    focused: bool,
    /// Whether the find bar matches as a regex (`rusty_regx`, POSIX ERE)
    /// instead of plain text. Toggled with Ctrl+R inside search mode;
    /// remembered across searches.
    search_regex: bool,
    /// Whether the find bar's match is case-sensitive (skips Unicode case
    /// folding). Toggled with Alt+C inside search mode; remembered across
    /// searches, mirroring `search_regex`.
    search_case_sensitive: bool,
    /// Broadcast input (G28): while set, keystrokes and pastes go to every
    /// pane in the active tab (multi-host workflows), not just the focused
    /// one. Toggled per-window; the active tab shows a `⇉` marker.
    broadcast: bool,
    /// Keyboard copy mode (G18): a viewport-cell cursor plus an optional
    /// anchor (absolute coords). While `Some`, keys move the cursor /
    /// extend the selection instead of reaching the keymap or the child.
    copy_mode: Option<CopyMode>,
    /// Last left-click on pane content `(when, cell)` plus the current
    /// consecutive-click count — double-click selects a word, triple a
    /// logical line (`click_streak` cycles 1 → 2 → 3 → 1).
    last_grid_click: Option<(Instant, (usize, usize))>,
    click_streak: u8,
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
    /// The window's current monitor DPI scale factor (1.0 = 100%), tracked so
    /// `WindowEvent::ScaleFactorChanged` can rescale `font_px` proportionally
    /// instead of leaving text rendered at the wrong physical size after a
    /// cross-monitor move. Winit reports this at window creation and on every
    /// change; there is no ambient way to query it in between.
    scale_factor: f64,
    /// Set when this window should close (last tab closed, × button, OS close
    /// request); the [`App`] router drops it and exits the loop when none are
    /// left.
    closed: bool,
    /// Set by `Action::NewWindow` (Ctrl+Shift+N); the router picks it up after
    /// the event dispatch and opens a sibling window.
    wants_new_window: bool,
    /// Cursor-trail state (G36): where the focused pane's cursor was last
    /// frame, and the in-flight trail if it just jumped. Tracked per window
    /// for its focused pane only.
    cursor_prev: Option<(u64, (usize, usize))>,
    trail: Option<Trail>,
    /// Whether this is the quake (dropdown) window: borderless strip docked to
    /// the top of the monitor, kept above other windows, toggled with
    /// `rusty_term ctl quake`.
    quake: bool,
    /// Cached git branch for the status ribbon: `(cwd it was resolved for,
    /// branch or None, when)`. `.git/HEAD` is re-read at most every
    /// [`GIT_BRANCH_TTL`] instead of on every frame.
    git_branch: Option<(std::path::PathBuf, Option<String>, Instant)>,
    /// Whether the right-hand command dock is open (`toggle_dock`,
    /// Ctrl+Shift+K). It auto-hides — without flipping this — when the
    /// window is too narrow to fit it beside a usable pane area.
    dock_open: bool,
    /// The dock's click map, rebuilt with each redraw: for each dock grid
    /// row, the absolute scrollback line a click jumps the focused pane to
    /// (`None` for headers/blank rows).
    dock_items: Vec<Option<usize>>,
}

impl WindowState<'_> {
    /// Spawn one shell sized `cols × rows`, wire its reader + exit-watcher
    /// threads (which signal by pane id), and return the pane.
    fn new_pane(
        &mut self,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
        args: &[String],
        cwd: Option<&std::path::Path>,
        theme: Option<Theme>,
    ) -> Result<Pane, std::io::Error> {
        let handle = self.backend.spawn_shell(cols, rows, shell, args, cwd)?;
        let id = self.next_id.get();
        self.next_id.set(id + 1);

        let theme = theme.unwrap_or(self.theme);
        let mut g = Grid::new(cols as usize, rows as usize);
        if let Some(max) = self.config.scrollback {
            g.set_scrollback_max(max);
        }
        g.cell_px = Some((self.cell_w as u16, self.cell_h as u16));
        g.apply_theme(&theme);
        g.set_default_cursor(
            self.config.cursor_style.unwrap_or_default(),
            self.config.cursor_blink.unwrap_or(false),
        );
        g.min_contrast = self.config.minimum_contrast.unwrap_or(1.0);
        g.bidi = self.config.bidi.unwrap_or(false);
        let grid = Arc::new(Mutex::new(g));
        let parser = Arc::new(Mutex::new(AnsiParser::with_theme(theme)));

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

    /// The active tab's focused pane's cwd, as last reported via OSC 7 —
    /// `None` if it hasn't reported one yet (or there's no active tab). A new
    /// tab/pane spawned from here starts in that directory instead of
    /// wherever the launch `--cwd` pointed, so "open a new tab" follows where
    /// the user actually navigated to.
    fn focused_pane_cwd(&self) -> Option<std::path::PathBuf> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.focused()?;
        let uri = pane.grid.lock().cwd.clone();
        path_from_file_uri(&uri)
    }

    /// Open a new tab (one full-area pane) and make it active.
    fn spawn_tab(&mut self) -> Result<(), std::io::Error> {
        self.spawn_tab_with(None)
    }

    /// Open a new tab running `shell` (or the configured default when `None`)
    /// and make it active. Backs the `+` button, `Ctrl+Shift+T`, and the
    /// shell-launcher menu (which passes a detected shell's path).
    fn spawn_tab_with(&mut self, shell: Option<String>) -> Result<(), std::io::Error> {
        self.spawn_tab_opts(shell, &[], None, None)
    }

    /// Spawn a tab from a profile: its shell/cwd/theme, top-level config for
    /// anything the profile leaves unset.
    fn spawn_tab_profile(&mut self, i: usize) -> Result<(), std::io::Error> {
        let Some(p) = self.config.profiles.get(i).cloned() else { return Ok(()) };
        self.spawn_tab_opts(p.shell, &[], p.cwd, p.theme)
    }

    /// Execute one control-socket command and produce its reply text
    /// (data lines + a trailing `ok` / `err …` line).
    fn handle_control(&mut self, cmd: super::control::CtlCommand) -> String {
        use super::control::CtlCommand;
        match cmd {
            CtlCommand::Ping => "ok\n".to_string(),
            // Window-level commands are handled by the App router before any
            // per-window dispatch; reaching here is a routing bug, not user
            // error, so answer with a stable diagnostic rather than panicking.
            CtlCommand::NewWindow { .. } | CtlCommand::Quake => {
                "err window-level command not routed\n".to_string()
            }
            CtlCommand::NewTab { cwd, profile, shell } => {
                let p = profile.as_deref().and_then(|n| self.config.profile(n)).cloned();
                if profile.is_some() && p.is_none() {
                    return format!("err no profile named `{}`\n", profile.unwrap_or_default());
                }
                let p = p.unwrap_or_default();
                let shell = shell.or(p.shell);
                let cwd = cwd.or(p.cwd);
                match self.spawn_tab_opts(shell, &[], cwd, p.theme) {
                    Ok(()) => {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                            window.focus_window();
                        }
                        "ok\n".to_string()
                    }
                    Err(e) => format!("err {e}\n"),
                }
            }
            CtlCommand::SendText(text) => match self.pane_mut() {
                Some(p) => {
                    let _ = p.writer.write(text.as_bytes());
                    "ok\n".to_string()
                }
                None => "err no pane\n".to_string(),
            },
            CtlCommand::ListTabs => {
                let mut out = String::new();
                for (i, tab) in self.tabs.iter().enumerate() {
                    let title =
                        tab.focused().map(|p| p.grid.lock().title.clone()).unwrap_or_default();
                    let marker = if i == self.active { "*" } else { " " };
                    out.push_str(&format!("{i}\t{marker}\t{title}\n"));
                }
                out.push_str("ok\n");
                out
            }
            CtlCommand::FocusTab(n) => {
                if n >= self.tabs.len() {
                    return format!("err no tab {n}\n");
                }
                self.active = n;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                "ok\n".to_string()
            }
        }
    }

    /// Spawn one session-file tab: profile defaults, then the tab's own
    /// cwd/command overrides, then its splits (each split pane runs the same
    /// shell and inherits the tab's cwd).
    fn spawn_session_tab(
        &mut self,
        t: &crate::config::SessionTab,
    ) -> Result<(), std::io::Error> {
        let profile = t.profile.as_deref().and_then(|n| self.config.profile(n)).cloned();
        if t.profile.is_some() && profile.is_none() {
            eprintln!(
                "rusty_term: session: no profile named `{}`",
                t.profile.as_deref().unwrap_or_default()
            );
        }
        let profile = profile.unwrap_or_default();
        // A `command` runs directly (whitespace-split argv); else the
        // profile's shell; else the configured default.
        let (shell, args): (Option<String>, Vec<String>) = match &t.command {
            Some(cmd) => {
                let mut it = cmd.split_whitespace().map(str::to_string);
                (it.next(), it.collect())
            }
            None => (profile.shell.clone(), Vec::new()),
        };
        let cwd = t.cwd.clone().or_else(|| profile.cwd.clone());
        self.spawn_tab_opts(shell, &args, cwd.clone(), profile.theme)?;
        for split in &t.splits {
            let dir = if split == "right" { Dir::Vertical } else { Dir::Horizontal };
            self.split_pane_with(dir, profile.shell.clone(), cwd.clone(), profile.theme);
        }
        Ok(())
    }

    /// The general tab spawner: explicit shell/args/cwd/theme, each falling
    /// back to the config (and, for cwd, the focused pane) when `None`.
    fn spawn_tab_opts(
        &mut self,
        shell: Option<String>,
        args: &[String],
        cwd: Option<PathBuf>,
        theme: Option<Theme>,
    ) -> Result<(), std::io::Error> {
        let shell = shell.or_else(|| self.config.shell.clone());
        // The launch-time `-- prog arg...` argv only applies when spawning
        // that same configured shell; a shell picked fresh from the menu
        // starts bare rather than replaying args meant for a different program.
        let args: Vec<String> = if !args.is_empty() {
            args.to_vec()
        } else if shell == self.config.shell {
            self.config.command_args.clone()
        } else {
            Vec::new()
        };
        // An explicit cwd (profile/session) wins; else the focused pane's
        // cwd (so a new tab follows where the user navigated to); the launch
        // `--cwd` is the final fallback.
        let cwd =
            cwd.or_else(|| self.focused_pane_cwd()).or_else(|| self.config.cwd.clone());
        let pane =
            self.new_pane(self.cols, self.rows, shell.as_deref(), &args, cwd.as_deref(), theme)?;
        let focus = pane.id;
        self.tabs.push(Tab {
            panes: vec![pane],
            layout: Layout::single(focus),
            focus,
            attention: false,
            zoomed: false,
        });
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
        let cwd = self.focused_pane_cwd().or_else(|| self.config.cwd.clone());
        self.split_pane_with(dir, shell, cwd, None);
    }

    /// [`Self::split_pane`] with explicit shell/cwd/theme (session tabs pass
    /// their profile's, so a split doesn't fall back to the global config).
    fn split_pane_with(
        &mut self,
        dir: Dir,
        shell: Option<String>,
        cwd: Option<PathBuf>,
        theme: Option<Theme>,
    ) {
        let shell = shell.or_else(|| self.config.shell.clone());
        let Ok(pane) =
            self.new_pane(self.cols.max(1), self.rows.max(1), shell.as_deref(), &[], cwd.as_deref(), theme)
        else {
            return;
        };
        let new_id = pane.id;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        tab.zoomed = false; // the pane set changed; zoom no longer meaningful
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

    /// Enter keyboard copy mode: a movable cursor (hjkl/arrows) over the
    /// viewport, `v`/Space to anchor a selection, `y`/Enter to copy and
    /// exit, Esc/q to leave. The cursor renders through the ordinary
    /// selection highlight (a one-cell selection until anchored).
    fn enter_copy_mode(&mut self) {
        let Some(p) = self.pane() else { return };
        let cur = {
            let mut g = p.grid.lock();
            let cur = (g.cursor.0.min(g.cols.saturating_sub(1)), g.cursor.1);
            let abs = g.abs_of_view_row(cur.1);
            g.selection = Some(Selection { anchor: (cur.0, abs), head: (cur.0, abs) });
            cur
        };
        self.copy_mode = Some(CopyMode { cur, anchor: None });
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Handle a key press while copy mode is active. Returns whether the key
    /// was consumed (always, while active — copy mode owns the keyboard).
    fn copy_mode_key(&mut self, event: &KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        let Some(mut cm) = self.copy_mode.take() else { return false };
        let Some(p) = self.pane() else { return false };
        let mut exit = false;
        let mut yank = false;
        {
            let mut g = p.grid.lock();
            let (cols, rows) = (g.cols, g.rows);
            let mv = |cm: &mut CopyMode, dc: isize, dr: isize, g: &mut Grid| {
                cm.cur.0 = cm.cur.0.saturating_add_signed(dc).min(cols.saturating_sub(1));
                if dr < 0 && cm.cur.1 == 0 {
                    g.scroll_view_up(dr.unsigned_abs()); // keep moving into history
                } else if dr > 0 && cm.cur.1 + 1 >= rows {
                    g.scroll_view_down(dr as usize);
                } else {
                    cm.cur.1 = cm.cur.1.saturating_add_signed(dr).min(rows.saturating_sub(1));
                }
            };
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => exit = true,
                Key::Named(NamedKey::Enter) => {
                    yank = true;
                    exit = true;
                }
                Key::Named(NamedKey::ArrowLeft) => mv(&mut cm, -1, 0, &mut g),
                Key::Named(NamedKey::ArrowRight) => mv(&mut cm, 1, 0, &mut g),
                Key::Named(NamedKey::ArrowUp) => mv(&mut cm, 0, -1, &mut g),
                Key::Named(NamedKey::ArrowDown) => mv(&mut cm, 0, 1, &mut g),
                Key::Named(NamedKey::PageUp) => mv(&mut cm, 0, -(rows as isize), &mut g),
                Key::Named(NamedKey::PageDown) => mv(&mut cm, 0, rows as isize, &mut g),
                Key::Named(NamedKey::Home) => cm.cur.0 = 0,
                Key::Named(NamedKey::End) => cm.cur.0 = cols.saturating_sub(1),
                Key::Named(NamedKey::Space) => {
                    cm.anchor = Some((cm.cur.0, g.abs_of_view_row(cm.cur.1)));
                }
                Key::Character(s) => match s.as_str() {
                    "q" => exit = true,
                    "h" => mv(&mut cm, -1, 0, &mut g),
                    "l" => mv(&mut cm, 1, 0, &mut g),
                    "k" => mv(&mut cm, 0, -1, &mut g),
                    "j" => mv(&mut cm, 0, 1, &mut g),
                    "0" => cm.cur.0 = 0,
                    "$" => cm.cur.0 = cols.saturating_sub(1),
                    "g" => {
                        // Top of scrollback.
                        g.scroll_view_up(usize::MAX / 2);
                        cm.cur = (0, 0);
                    }
                    "G" => {
                        g.reset_view();
                        cm.cur = (0, rows.saturating_sub(1));
                    }
                    "v" => {
                        // Toggle the anchor (vi visual mode).
                        cm.anchor = match cm.anchor {
                            Some(_) => None,
                            None => Some((cm.cur.0, g.abs_of_view_row(cm.cur.1))),
                        };
                    }
                    "y" => {
                        yank = true;
                        exit = true;
                    }
                    _ => {}
                },
                _ => return true, // modifiers etc.: consumed, no effect
            }
            // Publish the selection: anchor..cursor, or the bare cursor cell.
            let head = (cm.cur.0, g.abs_of_view_row(cm.cur.1));
            let anchor = cm.anchor.unwrap_or(head);
            g.selection = if exit && !yank { None } else { Some(Selection { anchor, head }) };
        }
        if yank {
            self.copy_selection();
            self.copy_selection_primary();
        }
        if exit {
            if let Some(p) = self.pane()
                && yank
            {
                p.grid.lock().selection = None;
            }
        } else {
            self.copy_mode = Some(cm);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        true
    }

    /// Move focus to the nearest pane in direction `(dx, dy)` (one axis at a
    /// time): the closest pane whose rect lies strictly beyond the focused
    /// pane's edge, by center distance. No wrap.
    fn focus_dir(&mut self, dx: isize, dy: isize) {
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let rects = tab.rects(area);
        let Some(&(_, f)) = rects.iter().find(|(id, _)| *id == tab.focus) else { return };
        let center = |r: &Rect| (r.col as isize * 2 + r.cols as isize, r.row as isize * 2 + r.rows as isize);
        let (fcx, fcy) = center(&f);
        let best = rects
            .iter()
            .filter(|(id, _)| *id != tab.focus)
            .filter(|(_, r)| match (dx, dy) {
                (1, _) => r.col >= f.col + f.cols,
                (-1, _) => r.col + r.cols <= f.col,
                (_, 1) => r.row >= f.row + f.rows,
                _ => r.row + r.rows <= f.row,
            })
            .min_by_key(|(_, r)| {
                let (cx, cy) = center(r);
                (cx - fcx).abs() + (cy - fcy).abs()
            })
            .map(|(id, _)| *id);
        if let Some(id) = best {
            tab.focus = id;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Grow (`delta > 0`) or shrink the focused pane along `dir`, then
    /// re-lay panes out to the moved boundary. Leaves a zoomed tab alone —
    /// there is no visible boundary to move.
    fn resize_pane(&mut self, dir: Dir, delta: f32) {
        let ti = self.active;
        let Some(tab) = self.tabs.get_mut(ti) else { return };
        if tab.zoomed || !tab.layout.resize(tab.focus, dir, delta) {
            return;
        }
        self.layout_panes(ti);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Toggle pane zoom: the focused pane takes the whole tab area until
    /// toggled back (or the pane set changes).
    fn toggle_zoom(&mut self) {
        let ti = self.active;
        let Some(tab) = self.tabs.get_mut(ti) else { return };
        if tab.panes.len() < 2 {
            return; // a single pane already has the whole area
        }
        tab.zoomed = !tab.zoomed;
        self.layout_panes(ti);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Close pane `id`: collapse its split into the sibling, or close the whole
    /// tab when it was the last pane. Idempotent for stale exit events.
    fn close_pane(&mut self, id: u64) {
        let Some(ti) = self.tabs.iter().position(|t| t.panes.iter().any(|p| p.id == id)) else {
            return;
        };
        let tab = &mut self.tabs[ti];
        match tab.layout.close(id) {
            None => {
                self.close_tab_at(ti);
                return;
            }
            Some(next) => {
                tab.panes.retain(|p| p.id != id); // drops the PTY handle
                tab.zoomed = false; // the pane set changed
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
    fn close_tab_at(&mut self, ti: usize) {
        self.tabs.remove(ti);
        if self.tabs.is_empty() {
            self.closed = true;
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
    /// Cells the command dock currently occupies (its width plus the divider
    /// column), `0` when closed or when opening it would squeeze the pane
    /// area below [`DOCK_MIN_PANE`] columns.
    fn dock_cols(&self) -> usize {
        if self.dock_open && self.cols as usize >= DOCK_CELLS + 1 + DOCK_MIN_PANE {
            DOCK_CELLS + 1
        } else {
            0
        }
    }

    /// The cell area the tab's panes tile: the full grid minus the command
    /// dock (which sits at the right edge, behind a one-cell divider).
    fn pane_area(&self) -> Rect {
        Rect::new(0, 0, (self.cols as usize).saturating_sub(self.dock_cols()), self.rows as usize)
    }

    fn layout_panes(&mut self, ti: usize) {
        let area = self.pane_area();
        let Some(tab) = self.tabs.get_mut(ti) else {
            return;
        };
        for (id, r) in tab.rects(area) {
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
        // The padding band surrounds the pane area on three sides plus the
        // gap under the (flush, full-width) chrome bar.
        let inner_w = (px_w as usize).saturating_sub(2 * self.pad);
        let inner_h = (px_h as usize).saturating_sub(2 * self.pad);
        let cols = ((inner_w / self.cell_w).max(1)) as u16;
        // One screen row goes to the chrome bar; one more to the status
        // ribbon when it's on.
        let bars = 1 + usize::from(self.status_enabled());
        let rows = (((inner_h / self.cell_h).saturating_sub(bars)).max(1)) as u16;
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
        let status = self.status_row();
        let divider = mix(self.theme.bg, self.theme.fg, 60);
        if self.overlay.is_some() {
            let page = self.build_overlay_grid();
            let (Some(renderer), Some(window)) = (self.renderer.as_mut(), self.window.as_ref()) else {
                return;
            };
            let size = window.inner_size();
            let frame = PaneFrame {
                grid: &page,
                col0: 0,
                row0: 1,
                focused: false,
                cursor_on: false,
                trail: Vec::new(),
                hover_link: None,
                marks: Vec::new(),
            };
            renderer.render(
                std::slice::from_ref(&frame),
                &chrome,
                &status,
                &mut self.font,
                size.width,
                size.height,
                divider,
                self.theme.bg,
                mix(self.theme.bg, self.theme.fg, 48),
                self.pad,
            );
            return;
        }
        let area = self.pane_area();
        // The command dock renders as one more (synthetic, unfocused) pane
        // right of the pane area. Built before the renderer is borrowed and
        // before the panes lock their grids — it locks the focused grid
        // itself, and the lock isn't reentrant.
        let dock_grid = if self.dock_cols() > 0 {
            let built = self.pane().map(|p| {
                let g = p.grid.lock();
                build_dock_grid(&g, &self.theme, DOCK_CELLS, self.rows as usize)
            });
            let (grid, items) = built.unwrap_or_else(|| {
                (Grid::new(DOCK_CELLS, self.rows as usize), Vec::new())
            });
            self.dock_items = items;
            Some(grid)
        } else {
            self.dock_items = Vec::new();
            None
        };
        let (Some(renderer), Some(window)) = (self.renderer.as_mut(), self.window.as_ref()) else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let size = window.inner_size();
        // Taskbar progress (G01 stretch, Windows only): resolved once per
        // frame like the title below, from the same winit `Window`.
        #[cfg(windows)]
        let hwnd = {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            window.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(h) => {
                    Some(h.hwnd.get() as windows_sys::Win32::Foundation::HWND)
                }
                _ => None,
            })
        };
        if let Some(p) = tab.focused() {
            let g = p.grid.lock();
            let fallback = self.config.title.as_deref().unwrap_or("rusty_term");
            window.set_title(if g.title.is_empty() { fallback } else { &g.title });
            #[cfg(windows)]
            if let Some(hwnd) = hwnd {
                super::taskbar::sync(hwnd, g.progress);
            }
        }
        // Lock each pane's grid for the frame, then hand the renderer offset views
        // (the chrome bar occupies screen row 0, so panes start at row 1).
        let blink = self.cursor_blink_on;
        let focus = tab.focus;
        let mut held = Vec::new();
        for (id, r) in tab.rects(area) {
            if let Some(p) = tab.pane(id) {
                held.push((p.grid.lock(), r, id == focus));
            }
        }
        // Cursor trail (G36): when the focused pane's cursor jumped since the
        // last frame, remember the hop and paint fading ghosts along it for
        // TRAIL_MS (the tick loop keeps frames coming while one is live).
        const TRAIL_MS: u64 = 150;
        let mut ghosts: Vec<(usize, usize, f32)> = Vec::new();
        if self.config.cursor_trail.unwrap_or(false) {
            let now = Instant::now();
            if let Some((g, _, _)) = held.iter().find(|(_, _, foc)| *foc) {
                let cur = (g.cursor_visible && g.view_offset == 0).then_some(g.cursor);
                match (self.cursor_prev, cur) {
                    (Some((pid, prev)), Some(cur)) if pid == focus && prev != cur => {
                        self.trail = Some(Trail { from: prev, to: cur, since: now });
                    }
                    _ => {}
                }
                self.cursor_prev = cur.map(|c| (focus, c));
            }
            if let Some(tr) = self.trail {
                let t = tr.since.elapsed().as_millis() as f32 / TRAIL_MS as f32;
                ghosts = super::cpu::trail_ghosts(tr.from, tr.to, t);
                if ghosts.is_empty() {
                    self.trail = None;
                }
            }
        } else {
            self.trail = None;
        }
        // Command gutter marks (per pane): resolve each row's BlockMark to a
        // theme color — success green, failure red, running in the accent.
        let marks_on = self.config.command_marks.unwrap_or(true);
        let mark_color = |m: crate::core::BlockMark| match m {
            crate::core::BlockMark::Success => self.theme.palette16[2],
            crate::core::BlockMark::Error => self.theme.palette16[1],
            crate::core::BlockMark::Running => self.theme.cursor,
        };
        let mut frames: Vec<PaneFrame> = held
            .iter()
            .map(|(g, r, foc)| PaneFrame {
                grid: g,
                col0: r.col,
                row0: r.row + 1,
                focused: *foc,
                cursor_on: blink,
                trail: if *foc { std::mem::take(&mut ghosts) } else { Vec::new() },
                hover_link: if *foc { self.hover_link } else { None },
                marks: if marks_on {
                    g.viewport_block_marks()
                        .into_iter()
                        .enumerate()
                        .filter_map(|(row, m)| m.map(|m| (row, mark_color(m))))
                        .collect()
                } else {
                    Vec::new()
                },
            })
            .collect();
        if let Some(dg) = &dock_grid {
            frames.push(PaneFrame {
                grid: dg,
                col0: area.cols + 1, // right of the pane area's divider column
                row0: 1,
                focused: false,
                cursor_on: false,
                trail: Vec::new(),
                hover_link: None,
                marks: Vec::new(),
            });
        }
        renderer.render(
            &frames,
            &chrome,
            &status,
            &mut self.font,
            size.width,
            size.height,
            divider,
            self.theme.bg,
            mix(self.theme.bg, self.theme.fg, 48),
            self.pad,
        );
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
    /// Cells the chrome bar spans. The bar is flush with the window (unlike
    /// the pad-inset grid), so its width comes from the window's pixels, not
    /// `self.cols`: `paint` cells cover the full width (the last may clip at
    /// the edge), while controls lay out within `layout` whole cells so the
    /// caption buttons reach the right edge without being cut.
    fn bar_cells(&self) -> (usize, usize) {
        let px = self.window.as_ref().map(|w| w.inner_size().width).unwrap_or(0) as usize;
        if px == 0 || self.cell_w == 0 {
            let c = self.cols as usize;
            return (c, c);
        }
        (px.div_ceil(self.cell_w), px / self.cell_w)
    }

    /// Whether the bottom status ribbon is on (`[window] status_bar`,
    /// default on).
    fn status_enabled(&self) -> bool {
        self.config.status_bar.unwrap_or(true)
    }

    /// Lay out the bottom status ribbon: the focused pane's cwd and git
    /// branch on the left; the last command's exit pill, the scrollback
    /// position, and the grid size on the right. Empty when disabled — the
    /// renderers skip an empty row and the grid keeps the space.
    fn status_row(&mut self) -> Vec<Cell> {
        if !self.status_enabled() {
            return Vec::new();
        }
        let (paint_cols, cols) = self.bar_cells();
        let bar_bg = mix(self.theme.bg, self.theme.fg, 48);
        let dim_fg = mix(self.theme.fg, self.theme.bg, 110);
        let mut row = vec![Cell::blank(); paint_cols];
        for c in &mut row {
            c.fg = dim_fg;
            c.bg = bar_bg;
        }
        // Everything below reads the focused pane's grid; a window with no
        // panes (mid-teardown) shows a bare bar.
        let Some(pane) = self.pane() else { return row };
        let (cwd, last_exit, view_offset, gcols, grows) = {
            let g = pane.grid.lock();
            (g.cwd.clone(), g.last_command_exit(), g.view_offset, g.cols, g.rows)
        };
        let cwd = path_from_file_uri(&cwd);
        let branch = cwd.as_deref().and_then(|d| self.git_branch_for(d));

        // Right side, innermost first: grid size, scrollback position, exit
        // pill. Each segment carries its own color.
        let ok = self.theme.palette16[2];
        let err = self.theme.palette16[1];
        let warn = self.theme.palette16[3];
        let mut right: Vec<(String, u32)> = Vec::new();
        right.push((format!("{gcols}\u{00d7}{grows} "), dim_fg));
        if view_offset > 0 {
            right.push((format!("\u{2191}{view_offset}  "), warn));
        }
        match last_exit {
            Some(0) => right.push(("\u{2713} 0  ".into(), ok)),
            Some(code) => right.push((format!("\u{2717} {code}  "), err)),
            None => {}
        }
        let right_len: usize = right.iter().map(|(s, _)| s.chars().count()).sum();

        // Left side: cwd (home shortened to ~, long paths trimmed from the
        // left so the leaf stays visible), then the git branch in the accent.
        let mut hits = vec![Hit::Drag; paint_cols]; // discarded: the ribbon is inert
        let limit = cols.saturating_sub(right_len);
        let mut col = 0;
        if let Some(dir) = &cwd {
            let mut text = display_path(dir);
            let branch_len = branch.as_ref().map(|b| b.chars().count() + 3).unwrap_or(0);
            let avail = limit.saturating_sub(2 + branch_len);
            let count = text.chars().count();
            if count > avail && avail > 1 {
                let tail: String = text.chars().skip(count - (avail - 1)).collect();
                text = format!("\u{2026}{tail}");
            }
            put_text(&mut row, &mut hits, &mut col, limit, &format!(" {text}"), self.theme.fg, bar_bg, Hit::Drag);
            if let Some(b) = &branch {
                let accent = self.theme.cursor;
                put_text(&mut row, &mut hits, &mut col, limit, &format!("  \u{2387} {b}"), accent, bar_bg, Hit::Drag);
            }
        }
        let mut rcol = cols.saturating_sub(right_len);
        for (text, color) in &right {
            put_text(&mut row, &mut hits, &mut rcol, cols, text, *color, bar_bg, Hit::Drag);
        }
        row
    }

    /// The git branch for `cwd` (walking up to the repository root), cached
    /// for [`GIT_BRANCH_TTL`] so the per-frame status ribbon doesn't hit the
    /// filesystem on every redraw. `None` outside a repository.
    fn git_branch_for(&mut self, cwd: &std::path::Path) -> Option<String> {
        if let Some((dir, branch, at)) = &self.git_branch
            && dir == cwd
            && at.elapsed() < GIT_BRANCH_TTL
        {
            return branch.clone();
        }
        let branch = read_git_branch(cwd);
        self.git_branch = Some((cwd.to_path_buf(), branch.clone(), Instant::now()));
        branch
    }

    fn chrome_row(&mut self) -> Vec<Cell> {
        let (paint_cols, cols) = self.bar_cells();
        if let Some(query) = self.searching.clone() {
            let mut row = vec![Cell::blank(); paint_cols];
            let bar_bg = mix(self.theme.bg, self.theme.fg, 48);
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
            let mut hits = vec![Hit::Drag; paint_cols];
            let mut col = 0;
            let mode = if self.search_regex { " Find(re): " } else { " Find: " };
            put_text(&mut row, &mut hits, &mut col, limit, &format!("{mode}{query}"), self.theme.fg, bar_bg, Hit::Drag);
            let mut ccol = limit;
            put_text(&mut row, &mut hits, &mut ccol, cols, &count, self.theme.fg, bar_bg, Hit::Drag);
            self.hits = hits;
            return row;
        }
        if self.copy_mode.is_some() {
            let mut row = vec![Cell::blank(); paint_cols];
            let bar_bg = mix(self.theme.bg, self.theme.fg, 48);
            for c in &mut row {
                c.fg = self.theme.fg;
                c.bg = bar_bg;
            }
            let mut hits = vec![Hit::Drag; paint_cols];
            let mut col = 0;
            put_text(
                &mut row,
                &mut hits,
                &mut col,
                cols,
                " COPY   move: hjkl/arrows   v: select   y: copy   Esc: exit",
                self.theme.fg,
                bar_bg,
                Hit::Drag,
            );
            self.hits = hits;
            return row;
        }
        // Three contrast tiers give the strip depth: the empty band tints
        // hardest, inactive tabs sit on it as lighter chips, hover lightens
        // further, and the active tab adopts the terminal background outright
        // — so a tab is always distinguishable from the strip it sits on, in
        // light and dark themes alike. The accent line follows the cursor
        // color rather than adding config keys.
        let bar_bg = mix(self.theme.bg, self.theme.fg, 48);
        let tab_bg = mix(self.theme.bg, self.theme.fg, 20);
        let dim_fg = mix(self.theme.fg, self.theme.bg, 110);
        let hover = self.hover;
        let hover_bg = mix(self.theme.bg, self.theme.fg, 10);
        let btn_hover_bg = mix(bar_bg, self.theme.fg, 35);
        let accent = self.theme.cursor;
        let mut row: Vec<Cell> = vec![Cell::blank(); paint_cols];
        for c in &mut row {
            c.fg = self.theme.fg;
            c.bg = bar_bg;
        }
        let mut hits = vec![Hit::Drag; paint_cols];

        // Caption buttons get the last 12 cells (4 each); the `+` / `▾` buttons
        // sit just left of them, and the tabs fill the rest without overrunning.
        let btn0 = cols.saturating_sub(12);
        // The open settings page is a peer of the shells, not a veil over
        // one: it gets its own (active) strip tab, and the shell tabs render
        // inactive behind it. The chip's room comes off the tab budget.
        let settings_open = matches!(self.overlay, Some(Overlay::Settings(_)));
        const SETTINGS_TAB_W: usize = 12; // " Settings" + " × "
        let tab_limit = btn0
            .saturating_sub(8)
            .saturating_sub(if settings_open { SETTINGS_TAB_W + 1 } else { 0 });

        // The active tab of a focused window is being watched; its badge is
        // stale by definition. (Cleared before labels are gathered below.)
        if self.focused
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.attention = false;
        }

        // First pass: every tab's label, so widths can adapt to the titles.
        // OSC 9;4 progress rides the label: ` 42%`, `!42%` on error/paused,
        // `…` while indeterminate; an attention badge prepends `•` in the
        // alert color (painted separately, but its width counts here).
        let close_w = 3; // " × " on a tab that shows its close button
        let infos: Vec<(String, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let state = tab.focused().map(|p| {
                    let g = p.grid.lock();
                    (g.title.clone(), g.progress)
                });
                let (label, progress) = state.unwrap_or_default();
                let title = if label.is_empty() { format!("shell {}", i + 1) } else { label };
                let suffix = match progress {
                    Some((2 | 4, pct)) => format!(" !{pct}%"),
                    Some((3, _)) => " …".to_string(),
                    Some((_, pct)) => format!(" {pct}%"),
                    None => String::new(),
                };
                let cast = if i == self.active && self.broadcast { "⇉ " } else { "" };
                (format!(" {cast}{title}{suffix}"), tab.attention)
            })
            .collect();
        let desired: Vec<usize> = infos
            .iter()
            .map(|(label, attention)| {
                label.chars().count() + if *attention { 2 } else { 0 } + close_w
            })
            .collect();

        // Tabs size to their titles (clamped, shrinking together as the
        // strip crowds); once even minimum-width tabs can't fit, fall back
        // to the uniform-width strip scrolled to keep the active tab
        // visible, with a `»N` overflow badge for the hidden rest.
        const OVERFLOW_INDICATOR_W: usize = 4; // " »N"-ish
        let (tab_start, widths, hidden) = match tab_widths(&desired, tab_limit) {
            Some(widths) => (0, widths, 0),
            None => {
                let strip = tab_limit.saturating_sub(OVERFLOW_INDICATOR_W);
                let (start, end) = visible_tab_range(self.active, self.tabs.len(), strip);
                (start, vec![TAB_CELLS; end - start], self.tabs.len() - (end - start))
            }
        };
        let strip_limit =
            if hidden > 0 { tab_limit.saturating_sub(OVERFLOW_INDICATOR_W) } else { tab_limit };

        let mut col = 0usize;
        for (vi, &w) in widths.iter().enumerate() {
            let i = tab_start + vi;
            if col >= strip_limit {
                break; // out of room; the rest stay reachable by keyboard
            }
            // While the settings page is open its own chip is the active
            // one; every shell tab renders inactive behind it.
            let is_active = i == self.active && !settings_open;
            let tab_hovered = hover == Some(Hit::Tab(i)) || hover == Some(Hit::CloseTab(i));
            // The active tab adopts the terminal background, visually merging
            // with the grid below; inactive ones sit dimmed on the bar and
            // light up (full fg on a lighter bg) under the pointer.
            let (fg, bg) = if is_active {
                (self.theme.fg, self.theme.bg)
            } else if tab_hovered {
                (self.theme.fg, hover_bg)
            } else {
                (dim_fg, tab_bg)
            };
            let (label, attention) = (&infos[i].0, infos[i].1);
            let tab_end = (col + w).min(strip_limit);
            // Paint the whole tab span in its color and make it activate on click.
            for c in col..tab_end {
                row[c] = Cell::blank();
                row[c].fg = fg;
                row[c].bg = bg;
                hits[c] = Hit::Tab(i);
            }
            // The close button shows on the active and hovered tabs only — a
            // full strip of × is a row of misclicks right where tabs are
            // clicked to switch. Width still reserves its cells, so the label
            // doesn't reflow when the × appears.
            let has_close = (is_active || tab_hovered) && tab_end - col > close_w + 1;
            let label_end = if has_close { tab_end - close_w } else { tab_end - close_w.min(tab_end - col) };
            // A title that doesn't fit ends in an ellipsis, not a hard cut.
            let badge_w = if attention { 2 } else { 0 };
            let space = label_end.saturating_sub(col + badge_w);
            let shown: String = if label.chars().count() > space {
                let mut t: String = label.chars().take(space.saturating_sub(1)).collect();
                t.push('…');
                t
            } else {
                label.clone()
            };
            // Center the label in the chip: the reserved (hidden) close cells
            // then read as padding rather than a hole after the title. The
            // cap keeps the label out of the close slot, and the lead doesn't
            // depend on `has_close`, so hovering (which reveals the ×) never
            // reflows the title.
            let content = badge_w + shown.chars().count();
            let lead = (tab_end.saturating_sub(col).saturating_sub(content) / 2)
                .min(label_end.saturating_sub(col).saturating_sub(content));
            let mut tcol = col + lead;
            if attention {
                let alert = self.theme.palette16[1]; // ANSI red
                put_text(&mut row, &mut hits, &mut tcol, label_end, " •", alert, bg, Hit::Tab(i));
            }
            put_text(&mut row, &mut hits, &mut tcol, label_end, &shown, fg, bg, Hit::Tab(i));
            if has_close {
                let mut ccol = tab_end - close_w;
                let (cfg, cbg) = if hover == Some(Hit::CloseTab(i)) {
                    (self.theme.fg, mix(bg, self.theme.fg, 55))
                } else {
                    (fg, bg)
                };
                put_text(&mut row, &mut hits, &mut ccol, tab_end, " × ", cfg, cbg, Hit::CloseTab(i));
            }
            // Active tab: bold label plus an accent underline under the label
            // area — stopping before the ×, so it reads as the tab's title
            // marker rather than underlining the close control.
            if is_active {
                for c in &mut row[col..label_end.max(col)] {
                    c.flags |= ATTR_BOLD | ATTR_UNDERLINE | ATTR_UNDERLINE_COLOR;
                    c.underline_color = accent;
                }
            }
            col = tab_end;
            if col < strip_limit {
                col += 1; // strip-colored gap; the darker band edges each chip
            }
        }
        if hidden > 0 {
            let (ifg, ibg) = if hover == Some(Hit::MoreTabs) {
                (self.theme.fg, btn_hover_bg)
            } else {
                (dim_fg, bar_bg)
            };
            let indicator_end = (col + OVERFLOW_INDICATOR_W).min(btn0);
            for c in col..indicator_end {
                row[c] = Cell::blank();
                row[c].fg = ifg;
                row[c].bg = ibg;
                hits[c] = Hit::MoreTabs;
            }
            put_text(&mut row, &mut hits, &mut col, indicator_end, &format!(" »{hidden}"), ifg, ibg, Hit::MoreTabs);
            col = indicator_end;
        }
        // The settings page's own chip: rendered exactly like an active tab
        // (terminal bg, bold, accent underline) with its × closing the page.
        if settings_open {
            let start = col;
            let end = (col + SETTINGS_TAB_W).min(btn0);
            let bg = self.theme.bg;
            for c in start..end {
                row[c] = Cell::blank();
                row[c].fg = self.theme.fg;
                row[c].bg = bg;
                hits[c] = Hit::SettingsTab;
            }
            let label_end = end.saturating_sub(close_w);
            let mut tcol = start;
            put_text(&mut row, &mut hits, &mut tcol, label_end, " Settings", self.theme.fg, bg, Hit::SettingsTab);
            let (cfg, cbg) = if hover == Some(Hit::CloseSettings) {
                (self.theme.fg, mix(bg, self.theme.fg, 55))
            } else {
                (self.theme.fg, bg)
            };
            let mut ccol = label_end;
            put_text(&mut row, &mut hits, &mut ccol, end, " × ", cfg, cbg, Hit::CloseSettings);
            for c in &mut row[start..label_end.max(start)] {
                c.flags |= ATTR_BOLD | ATTR_UNDERLINE | ATTR_UNDERLINE_COLOR;
                c.underline_color = accent;
            }
            col = end;
            if col < btn0 {
                col += 1;
            }
        }
        let btn_bg = |hit: Hit| if hover == Some(hit) { btn_hover_bg } else { bar_bg };
        put_text(&mut row, &mut hits, &mut col, btn0, " + ", self.theme.fg, btn_bg(Hit::NewTab), Hit::NewTab);
        put_text(&mut row, &mut hits, &mut col, btn0, " ▾ ", self.theme.fg, btn_bg(Hit::ShellMenu), Hit::ShellMenu);

        let mut bcol = btn0;
        put_text(&mut row, &mut hits, &mut bcol, btn0 + 4, "  ─ ", self.theme.fg, btn_bg(Hit::Minimize), Hit::Minimize);
        put_text(&mut row, &mut hits, &mut bcol, btn0 + 8, "  □ ", self.theme.fg, btn_bg(Hit::Maximize), Hit::Maximize);
        // The caption close button hovers native-red, matching Windows 11.
        let (close_fg, close_bg) =
            if hover == Some(Hit::Close) { (0xFFFFFF, 0xC42B1C) } else { (self.theme.fg, bar_bg) };
        put_text(&mut row, &mut hits, &mut bcol, cols, "  × ", close_fg, close_bg, Hit::Close);
        // Any partial cell past the last whole one still belongs to the ×:
        // the window's top-right corner should always close (Fitts's law),
        // never fall through to the drag strip.
        for c in bcol..paint_cols {
            row[c].bg = close_bg;
            hits[c] = Hit::Close;
        }

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

    /// The grid content's vertical pixel offset: the padding band plus the
    /// chrome bar's own downward inset (the strip band above the tabs pushes
    /// the bar — and everything under it — down; see `render::bar_inset`).
    fn grid_oy(&self) -> usize {
        self.pad + super::render::bar_inset(self.pad, self.cell_h)
    }

    /// Map a physical pixel position to a clamped `(col, row)` *grid* cell
    /// (the chrome bar occupies the screen row above grid row 0).
    fn cell_at(&self, px: f64, py: f64) -> (usize, usize) {
        let pad = self.pad as f64;
        let col = ((px - pad).max(0.0) as usize / self.cell_w).min((self.cols as usize).saturating_sub(1));
        let row = ((py - self.grid_oy() as f64).max(0.0) as usize / self.cell_h)
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
        // `x`/`y` are physical pixels (as is `inner_size()`); scale the grab
        // zone by the monitor's DPI factor so it stays a consistent *logical*
        // size instead of shrinking to a barely-hittable sliver on HiDPI
        // displays (6 physical px is ~3 logical px at 200%).
        let border = RESIZE_BORDER * self.scale_factor;
        let (l, r) = (x < border, x > w - border);
        let (t, b) = (y < border, y > h - border);
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
    fn on_left_press(&mut self) {
        let (x, y) = self.mouse_pos;
        if let Some(dir) = self.resize_zone(x, y) {
            if let Some(window) = &self.window {
                let _ = window.drag_resize_window(dir);
            }
            return;
        }
        if (y.max(0.0) as usize) < self.cell_h {
            return self.on_bar_click(x);
        }
        if self.overlay.is_some() {
            return self.overlay_click(x, y);
        }
        // A click on the command dock jumps the focused pane's scrollback to
        // the clicked command block; dock clicks never reach the terminal.
        if self.dock_cols() > 0 {
            let (col, row) = self.cell_at(x, y);
            if col >= self.pane_area().cols {
                if let Some(abs) = self.dock_items.get(row).copied().flatten()
                    && let Some(p) = self.pane()
                {
                    p.grid.lock().scroll_to_abs(abs);
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                return;
            }
        }
        if let Some(id) = self.pane_under(x, y)
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.focus = id; // click focuses the pane under the pointer
        }
        let cell = self.cell_in_focused(x, y);
        // Bidi: selection anchors, word/line select, click-to-move, and the
        // summary check below are all logical-cell consumers.
        let cell = match self.pane() {
            Some(p) => (p.grid.lock().logical_col(cell.0, cell.1), cell.1),
            None => cell,
        };
        // A click on a fold-summary line expands the folded command block
        // (C17') instead of starting a selection.
        if let Some(p) = self.pane()
            && p.grid.lock().unfold_summary_at(cell.1)
        {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return;
        }
        // Click-to-move-cursor (G21): a plain first click at the shell
        // prompt sends the arrow presses that walk the readline cursor to
        // the clicked cell. Mouse-tracking apps own their clicks, so this
        // only fires when reporting is off — or Shift bypasses it, same as
        // `report_mouse`'s escape hatch below; drag-selection still arms
        // below either way (the arrows only ever move within the prompt's
        // own line).
        if self.config.click_to_move.unwrap_or(true)
            && let Some(p) = self.pane()
        {
            let moves = {
                let g = p.grid.lock();
                if g.mouse_modes.active() && !self.mods.shift_key() {
                    None
                } else {
                    g.prompt_cursor_moves(cell.0, cell.1).map(|m| (m, g.app_cursor_keys))
                }
            };
            if let Some(((dx, dy), app_cursor)) = moves {
                let bytes = arrow_presses(dx, dy, app_cursor);
                if let Some(p) = self.pane_mut() {
                    let _ = p.writer.write(&bytes);
                }
            }
        }
        // Consecutive clicks on the same cell escalate the selection: one
        // arms a drag, two select the word under the pointer, three the
        // whole (soft-wrap-joined) line; a fourth starts over.
        let now = Instant::now();
        self.click_streak = match self.last_grid_click {
            Some((t, c)) if c == cell && now.duration_since(t).as_millis() <= DOUBLE_CLICK_MS => {
                self.click_streak % 3 + 1
            }
            _ => 1,
        };
        self.last_grid_click = Some((now, cell));
        self.sel_anchor =
            self.pane().map(|p| (cell.0, p.grid.lock().abs_of_view_row(cell.1)));
        // Every streak arms a drag now, not just a plain click: dragging
        // after a double/triple click extends the selection by whole
        // word/line (`CursorMoved` below reads `click_streak` to pick the
        // granularity), instead of the drag being inert past the initial
        // word/line as it was before.
        self.selecting = true;
        if let Some(p) = self.pane() {
            let mut g = p.grid.lock();
            match self.click_streak {
                2 => g.select_word_at(cell.0, cell.1),
                3 => g.select_line_at(cell.1),
                _ => g.selection = None, // cleared until the drag moves
            }
        }
        if self.click_streak > 1 {
            self.copy_selection_primary(); // copy-on-select
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// The pane under pixel `(px, py)` in the active tab's grid area, if any.
    fn pane_under(&self, px: f64, py: f64) -> Option<u64> {
        let (col, row) = self.cell_at(px, py);
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        self.tabs.get(self.active).and_then(|t| {
            t.rects(area).into_iter().find(|(_, r)| r.contains(col, row)).map(|(id, _)| id)
        })
    }

    /// Map pixel `(px, py)` to a cell within the *focused* pane, clamped to it.
    fn cell_in_focused(&self, px: f64, py: f64) -> (usize, usize) {
        match self.tabs.get(self.active) {
            Some(tab) => self.cell_in_pane(tab.focus, px, py),
            None => self.cell_at(px, py),
        }
    }

    /// Map pixel `(px, py)` to a cell within pane `id`, clamped to it. Used
    /// both for the focused pane (`cell_in_focused`) and for routing wheel
    /// events to whichever pane is under the pointer, regardless of focus.
    fn cell_in_pane(&self, id: u64, px: f64, py: f64) -> (usize, usize) {
        let (col, row) = self.cell_at(px, py);
        if let Some(r) = self.pane_rect(id) {
            return (
                col.saturating_sub(r.col).min(r.cols.saturating_sub(1)),
                row.saturating_sub(r.row).min(r.rows.saturating_sub(1)),
            );
        }
        (col, row)
    }

    /// The focused pane's cell rect within the window's grid area, `None`
    /// before any tab exists.
    fn focused_pane_rect(&self) -> Option<Rect> {
        let tab = self.tabs.get(self.active)?;
        self.pane_rect(tab.focus)
    }

    /// Pane `id`'s cell rect within the window's grid area, `None` if `id`
    /// isn't a pane of the active tab.
    fn pane_rect(&self, id: u64) -> Option<Rect> {
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        let tab = self.tabs.get(self.active)?;
        tab.rects(area).into_iter().find(|(pid, _)| *pid == id).map(|(_, r)| r)
    }

    /// The focused pane's id, `None` before any tab exists.
    fn focused_pane_id(&self) -> Option<u64> {
        self.tabs.get(self.active).map(|t| t.focus)
    }

    /// Update the in-progress drag-selection's head to the cell at pixel
    /// `(hx, hy)` (clamped to the focused pane), applying word/line
    /// extension per `click_streak`. Returns whether a selection was
    /// updated. Shared by `CursorMoved`'s live drag and the edge-auto-scroll
    /// tick below, which re-applies it each scroll step at the pointer's
    /// last known (off-pane) position.
    fn update_drag_selection_head(&mut self, hx: f64, hy: f64) -> bool {
        let (Some(anchor), Some(p)) = (self.sel_anchor, self.pane()) else {
            return false;
        };
        let (hc, hr) = self.cell_in_focused(hx, hy);
        let mut g = p.grid.lock();
        let hc = g.logical_col(hc, hr);
        let head = (hc, g.abs_of_view_row(hr));
        match self.click_streak {
            2 => g.extend_word_selection(anchor, head),
            3 => g.extend_line_selection(anchor.1, head.1),
            _ => g.selection = Some(Selection { anchor, head }),
        }
        true
    }

    /// While dragging a selection with the pointer past the focused pane's
    /// top/bottom edge, scroll the viewport one line into/out of history so
    /// text beyond what's currently visible can still be reached, extending
    /// the selection to the new edge row. Returns whether it scrolled, so
    /// `tick` knows to keep waking up at `DRAG_SCROLL_INTERVAL` — held past
    /// the edge with the pointer stationary should keep scrolling, not just
    /// scroll once per `CursorMoved`.
    fn drag_edge_autoscroll(&mut self) -> bool {
        if !self.selecting {
            return false;
        }
        let Some(r) = self.focused_pane_rect() else { return false };
        let top_px = (self.grid_oy() + (r.row + 1) * self.cell_h) as f64;
        let bottom_px = (self.grid_oy() + (r.row + r.rows + 1) * self.cell_h) as f64;
        let Some(scroll_up) =
            drag_scroll_direction(self.mouse_pos.1, top_px, bottom_px, DRAG_SCROLL_MARGIN)
        else {
            return false;
        };
        let Some(p) = self.pane() else { return false };
        let moved = {
            let mut g = p.grid.lock();
            if scroll_up { g.scroll_view_up(1) } else { g.scroll_view_down(1) }
        };
        if !moved {
            return false;
        }
        self.update_drag_selection_head(self.mouse_pos.0, self.mouse_pos.1);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        true
    }

    /// Dispatch a click on the chrome bar through the hit map.
    fn on_bar_click(&mut self, x: f64) {
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
                // Arm drag-to-reorder; it only engages past the slop, so a
                // plain click stays a plain click.
                self.tab_drag = Some(TabDrag { idx: i, press_x: x, moving: false });
                window.request_redraw();
            }
            Hit::CloseTab(i) => {
                self.close_overlay();
                if i < self.tabs.len() {
                    self.close_tab_at(i);
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
            Hit::Close => self.closed = true,
            // The settings chip is already the active "tab"; only its ×
            // does anything (closing the page saves it).
            Hit::SettingsTab => {}
            Hit::CloseSettings => self.close_overlay(),
            Hit::MoreTabs => {
                self.close_overlay();
                self.cycle_tab(true);
            }
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

    /// Middle-click on the chrome bar closes the tab under the pointer
    /// (browser tab-strip convention), or does nothing over a non-tab
    /// element (new-tab/menu/caption buttons/drag strip) — those don't have
    /// a meaningful middle-click action.
    fn on_bar_middle_click(&mut self, x: f64) {
        if self.hits.is_empty() {
            return;
        }
        let col = (x.max(0.0) as usize / self.cell_w).min(self.hits.len() - 1);
        if let Hit::Tab(i) | Hit::CloseTab(i) = self.hits[col]
            && i < self.tabs.len()
        {
            self.close_overlay();
            self.close_tab_at(i);
        }
    }

    /// Copy the active tab's selection to the system clipboard (Ctrl+Shift+C).
    fn copy_selection(&mut self) {
        let Some(p) = self.pane() else { return };
        let (text, html) = {
            let g = p.grid.lock();
            let html = if self.config.copy_html.unwrap_or(true) { g.selected_html() } else { None };
            (g.selected_text(), html)
        };
        if let (Some(text), Some(cb)) = (text, self.clipboard.as_mut()) {
            // The HTML flavor carries the selection's colors/attributes for
            // rich-paste targets (G29); plain editors read the text flavor.
            let _ = match html {
                Some(html) => cb.set_html(html, Some(text)),
                None => cb.set_text(text),
            };
        }
    }

    /// Service a tab's pending OSC 52 clipboard request recorded by the parser.
    /// A set copies the child's text to the system clipboard; a query replies to
    /// the child from the system clipboard. Called on a tab's output, so
    /// background tabs are serviced too.
    ///
    /// Gated by the `clipboard` config policy (default write-only): with no
    /// gate, any program that can write to the pty could read `52;c;?` and
    /// have the terminal hand back whatever the user last copied — passwords,
    /// tokens, anything on the clipboard — with zero user interaction. Sets
    /// are lower-risk (the common case is a program copying its own output,
    /// e.g. over SSH) so they're allowed by default; queries require opting
    /// into `clipboard = "read-write"`.
    fn service_clipboard(&mut self, id: u64) {
        let policy = self.config.clipboard.unwrap_or_default();
        let (set, set_primary, query, query_primary) = {
            let Some(p) = self.pane_by_id(id) else { return };
            let mut g = p.grid.lock();
            if g.clipboard_set.is_none()
                && g.clipboard_set_primary.is_none()
                && !g.clipboard_query
                && !g.clipboard_query_primary
            {
                return;
            }
            (
                g.clipboard_set.take(),
                g.clipboard_set_primary.take(),
                std::mem::take(&mut g.clipboard_query),
                std::mem::take(&mut g.clipboard_query_primary),
            )
        };
        // Drain the pending request regardless of policy (above) so a denied
        // request doesn't re-fire on every subsequent poll; only actually
        // touch the clipboard when the policy allows it.
        let allow_write = policy != ClipboardPolicy::Off;
        let allow_read = policy == ClipboardPolicy::ReadWrite;
        if allow_write
            && let Some(text) = set
            && let Some(cb) = self.clipboard.as_mut()
        {
            let _ = cb.set_text(text);
        }
        if allow_write
            && let Some(text) = set_primary
            && let Some(cb) = self.clipboard.as_mut()
        {
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                use arboard::SetExtLinux as _;
                let _ = cb.set().clipboard(arboard::LinuxClipboardKind::Primary).text(text);
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            let _ = cb.set_text(text); // no primary selection: best effort
        }
        if allow_read
            && query
            && let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok())
        {
            let reply = osc52_reply('c', &text);
            if let Some(p) = self.pane_by_id_mut(id) {
                let _ = p.writer.write(&reply);
            }
        }
        if allow_read
            && query_primary
            && let Some(text) = self.primary_text()
        {
            let reply = osc52_reply('p', &text);
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

    /// Drain a pane's bell ring and finished-command records and raise the
    /// configured alerts: a bell requests window attention when the window is
    /// unfocused and badges a background tab; a command that ran at least
    /// `command_notify_secs` and finished while the window was unfocused (or
    /// its tab in the background) raises a desktop notification. Alerts for
    /// the active tab of a focused window are dropped — the user watched it
    /// happen.
    fn service_alerts(&mut self, id: u64) {
        let (bell, finished) = {
            let Some(p) = self.pane_by_id(id) else { return };
            let mut g = p.grid.lock();
            (std::mem::take(&mut g.bell), std::mem::take(&mut g.finished_commands))
        };
        if !bell && finished.is_empty() {
            return;
        }
        let tab_idx = self.tabs.iter().position(|t| t.panes.iter().any(|p| p.id == id));
        let background = tab_idx != Some(self.active);
        let unseen = background || !self.focused;
        let mut badge = false;
        if bell && self.config.bell.unwrap_or(true) && unseen {
            if !self.focused
                && let Some(window) = &self.window
            {
                window.request_user_attention(Some(UserAttentionType::Informational));
            }
            badge = true;
        }
        let threshold = self.config.command_notify_secs.unwrap_or(10);
        for (exit, runtime) in finished {
            if threshold != 0 && runtime.as_secs() >= threshold && unseen {
                let status = match exit {
                    Some(0) => "succeeded".to_string(),
                    Some(code) => format!("failed (exit {code})"),
                    None => "finished".to_string(),
                };
                notify(
                    "Command finished",
                    &format!("A command {status} after {}s", runtime.as_secs()),
                );
                badge = true;
            }
        }
        if badge
            && let Some(i) = tab_idx
            && let Some(tab) = self.tabs.get_mut(i)
        {
            tab.attention = true;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Dispatch a terminal-owned [`Action`] resolved from the keymap.
    fn run_action(&mut self, action: Action) {
        match action {
            Action::Copy => self.copy_selection(),
            Action::Paste => self.paste(),
            Action::NewTab => {
                if let Err(e) = self.spawn_tab() {
                    eprintln!("rusty_term: new tab: {e}");
                }
            }
            // Window creation needs the event loop, which lives above this
            // per-window layer — flag it for the router to act on.
            Action::NewWindow => self.wants_new_window = true,
            // Collapse/expand the last finished command's output (C17'):
            // its rows fold to a one-line "N lines hidden" summary.
            Action::FoldOutput => {
                if let Some(p) = self.pane()
                    && p.grid.lock().toggle_last_fold()
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
            // Toggle the command dock: the pane area shrinks/grows, so every
            // tab relayouts (grids + PTY winsize) to the new width.
            Action::ToggleDock => {
                self.dock_open = !self.dock_open;
                for ti in 0..self.tabs.len() {
                    self.layout_panes(ti);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            Action::CloseTab => {
                if let Some(id) = self.tabs.get(self.active).map(|t| t.focus) {
                    self.close_pane(id);
                }
            }
            Action::NextTab => self.cycle_tab(true),
            Action::PrevTab => self.cycle_tab(false),
            Action::OpenConfig => self.open_config(),
            Action::OpenSettings => self.open_settings(),
            Action::Search => self.start_search(),
            Action::OpenLinks => self.open_links_menu(),
            Action::SplitRight => self.split_pane(Dir::Vertical),
            Action::SplitDown => self.split_pane(Dir::Horizontal),
            Action::FocusNext => self.focus_pane(true),
            Action::FocusLeft => self.focus_dir(-1, 0),
            Action::FocusRight => self.focus_dir(1, 0),
            Action::FocusUp => self.focus_dir(0, -1),
            Action::FocusDown => self.focus_dir(0, 1),
            Action::ResizeLeft => self.resize_pane(Dir::Vertical, -RESIZE_STEP),
            Action::ResizeRight => self.resize_pane(Dir::Vertical, RESIZE_STEP),
            Action::ResizeUp => self.resize_pane(Dir::Horizontal, -RESIZE_STEP),
            Action::ResizeDown => self.resize_pane(Dir::Horizontal, RESIZE_STEP),
            Action::ZoomPane => self.toggle_zoom(),
            Action::CopyMode => self.enter_copy_mode(),
            Action::Broadcast => {
                self.broadcast = !self.broadcast;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            Action::ScrollPageUp => self.scroll_key(false, true),
            Action::ScrollPageDown => self.scroll_key(false, false),
            Action::ScrollPromptUp => self.scroll_key(true, true),
            Action::ScrollPromptDown => self.scroll_key(true, false),
            Action::ToggleFullscreen => self.toggle_fullscreen(),
            Action::FontSizeUp => self.zoom_font(FONT_ZOOM_STEP),
            Action::FontSizeDown => self.zoom_font(-FONT_ZOOM_STEP),
            Action::FontSizeReset => {
                let px = self.config.font_size.unwrap_or(FONT_PX);
                let ligatures = self.config.ligatures.unwrap_or(true);
                self.rebuild_font(px, ligatures);
            }
        }
    }

    /// Flip the window in/out of fullscreen (F11 / Alt+Enter conventions —
    /// previously only reachable via `launch_mode` at startup). A quake
    /// window docks to the monitor edge instead and never toggles.
    fn toggle_fullscreen(&mut self) {
        if self.quake {
            return;
        }
        let Some(window) = &self.window else { return };
        let now_fullscreen = window.fullscreen().is_some();
        window.set_fullscreen(
            (!now_fullscreen).then_some(Fullscreen::Borderless(None)),
        );
    }

    /// Grow/shrink the font by `delta` px (Ctrl+=/Ctrl+-, the Chrome/VS Code/
    /// iTerm2/Windows Terminal convention), clamped to the same range the
    /// settings overlay enforces.
    fn zoom_font(&mut self, delta: f32) {
        let px = (self.font_px + delta).clamp(super::settings::FONT_MIN, super::settings::FONT_MAX);
        let ligatures = self.config.ligatures.unwrap_or(true);
        self.rebuild_font(px, ligatures);
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
            Key::Character(s) if self.mods.control_key() && s.as_str() == "r" => {
                self.search_regex = !self.search_regex;
                self.run_search();
            }
            Key::Character(s) if self.mods.alt_key() && !self.mods.control_key() && s.as_str() == "c" => {
                self.search_case_sensitive = !self.search_case_sensitive;
                self.run_search();
            }
            Key::Character(s) if self.mods.control_key() && s.as_str() == "v" => {
                let text = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok());
                self.paste_into_search(text);
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
        let (regex, case_sensitive) = (self.search_regex, self.search_case_sensitive);
        if let Some(p) = self.pane() {
            p.grid.lock().search_with_case(&q, regex, case_sensitive);
        }
    }

    /// Append clipboard text into the find bar's query — Ctrl+V (regular
    /// clipboard) or a middle-click while it's open (PRIMARY selection,
    /// mirroring `paste_primary`'s convention). Only the first line
    /// contributes: a query is one line, and a pasted multi-line clipboard
    /// would otherwise produce a query no match could ever satisfy.
    fn paste_into_search(&mut self, text: Option<String>) {
        let Some(text) = text.filter(|t| !t.is_empty()) else { return };
        if let Some(q) = self.searching.as_mut() {
            q.push_str(text.lines().next().unwrap_or(""));
        }
        self.run_search();
        if let Some(window) = &self.window {
            window.request_redraw();
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

    /// Jump straight to tab `i` (Ctrl+Alt+digit); out-of-range is ignored.
    fn select_tab(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.active {
            self.active = i;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Move tab `from` to position `to` (one drag-reorder step). The dragged
    /// tab is the active one (pressing it activated it), so activity follows.
    fn reorder_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = to;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Tell the IME where the text cursor is, so its candidate/composition popup
    /// appears at the terminal cursor rather than the window origin.
    fn update_ime_area(&self) {
        let (Some(window), Some(p)) = (&self.window, self.pane()) else {
            return;
        };
        let (col, row) = p.grid.lock().cursor;
        // The cursor is pane-local; add the focused pane's own rect offset
        // within the split layout, or the popup lands at the cursor's
        // position within an unsplit *first* pane regardless of which split
        // pane is actually focused (e.g. a right/bottom split composing IME
        // would show the candidate window over the wrong pane entirely).
        let (base_col, base_row) =
            self.focused_pane_rect().map_or((0, 0), |r| (r.col, r.row));
        let (x, y) = ime_cursor_area_origin(
            self.pad,
            self.grid_oy(),
            self.cell_w,
            self.cell_h,
            (base_col, base_row),
            (col, row),
        );
        window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(x, y),
            winit::dpi::PhysicalSize::new(self.cell_w as u32, self.cell_h as u32),
        );
    }

    /// Write child input to the focused pane — or, while broadcast is on,
    /// to every pane in the active tab.
    fn write_child(&mut self, bytes: &[u8]) {
        if !self.broadcast {
            if let Some(p) = self.pane_mut() {
                let _ = p.writer.write(bytes);
            }
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            for p in &mut tab.panes {
                let _ = p.writer.write(bytes);
            }
        }
    }

    /// Copy the focused pane's selection to the PRIMARY selection
    /// (X11/Wayland copy-on-select; a no-op elsewhere — macOS/Windows have
    /// no primary selection concept).
    fn copy_selection_primary(&mut self) {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let Some(p) = self.pane() else { return };
            let text = p.grid.lock().selected_text();
            if let (Some(text), Some(cb)) = (text.filter(|t| !t.is_empty()), self.clipboard.as_mut()) {
                use arboard::SetExtLinux as _;
                let _ = cb.set().clipboard(arboard::LinuxClipboardKind::Primary).text(text);
            }
        }
    }

    /// The PRIMARY selection's text (X11/Wayland), or the regular clipboard
    /// where no primary selection exists.
    fn primary_text(&mut self) -> Option<String> {
        let cb = self.clipboard.as_mut()?;
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use arboard::GetExtLinux as _;
            cb.get().clipboard(arboard::LinuxClipboardKind::Primary).text().ok()
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            cb.get_text().ok()
        }
    }

    /// Middle-click pastes the PRIMARY selection (X11 muscle memory), unless
    /// the child is tracking the mouse — then the click is the child's.
    fn paste_primary(&mut self) {
        let Some(text) = self.primary_text().filter(|t| !t.is_empty()) else { return };
        if let Some(p) = self.pane_mut() {
            let bracketed = p.grid.lock().bracketed_paste;
            let _ = p.writer.write(&encode_paste(&text, bracketed));
        }
        self.snap_to_bottom();
    }

    /// Paste the system clipboard into the active tab's child (Ctrl+Shift+V).
    fn paste(&mut self) {
        let Some(cb) = self.clipboard.as_mut() else { return };
        let Ok(text) = cb.get_text() else { return };
        if text.is_empty() {
            return;
        }
        let Some(p) = self.pane() else { return };
        let bracketed = p.grid.lock().bracketed_paste;
        let bytes = encode_paste(&text, bracketed);
        self.write_child(&bytes);
        // Middle-click paste (`paste_primary`) already does this; pasting
        // while scrolled into history left the viewport there, so the user
        // couldn't see what they'd just pasted appear at the live prompt.
        self.snap_to_bottom();
    }

    /// Encode a native mouse event as SGR/1006 and send it to the active tab's
    /// child — but only when the child enabled mouse reporting (`?1000`/`?1002`/
    /// `?1003`). `build` turns the cell under the pointer into the event. Returns
    /// whether bytes were sent, so the wheel path can fall back to local
    /// scrollback browsing when the child isn't tracking the mouse.
    ///
    /// Holding Shift bypasses app mouse tracking entirely, the same escape
    /// hatch xterm/iTerm2/gnome-terminal have used for decades: a mouse-
    /// tracking full-screen app (vim, tmux, htop) grabs every click, so
    /// without this there would be no way to select text or scroll the
    /// scrollback at all while one is running.
    ///
    /// `id` is the pane the event is addressed to — the focused pane for
    /// press/release/drag-motion (a click already refocuses to the pane
    /// under the pointer in `on_left_press`, so "focused" and "under
    /// pointer" agree by the time this runs), or whichever pane is under
    /// the pointer for wheel events, which don't go through `on_left_press`
    /// and so would otherwise always report to the wrong pane in a split.
    fn report_mouse(&mut self, id: Option<u64>, build: impl FnOnce(usize, usize) -> MouseEvent) -> bool {
        if self.mods.shift_key() {
            return false;
        }
        let Some(id) = id else { return false };
        let Some(p) = self.pane_by_id(id) else { return false };
        let modes = p.grid.lock().mouse_modes;
        if !modes.active() {
            return false;
        }
        let (col, row) = self.cell_in_pane(id, self.mouse_pos.0, self.mouse_pos.1);
        // Bidi: apps address cells in logical order; a click on a reordered
        // row reports the logical cell shown at the pointer's visual slot.
        let col = p.grid.lock().logical_col(col, row);
        let mut e = build(col, row);
        // SGR-pixel mode (`?1016`): the same SGR encoding, but the position
        // is the pointer's pixel offset within the target pane's text area
        // (this pane is its own terminal, so pane-relative is the analogue of
        // xterm's text-area-relative pixels). The chrome bar above the grid
        // is one cell row tall, hence the +1 on the pane's row origin.
        if modes.extended & 8 != 0
            && let Some(r) = self.pane_rect(id)
        {
            e.point = pane_pixel_point(
                self.mouse_pos,
                (self.pad + r.col * self.cell_w, self.pad + (r.row + 1) * self.cell_h),
                (r.cols * self.cell_w, r.rows * self.cell_h),
            );
        }
        let mut out = Vec::new();
        SgrEncoder::new(modes).write(e, &mut out);
        if out.is_empty() {
            return false;
        }
        let Some(p) = self.pane_by_id_mut(id) else { return false };
        let _ = p.writer.write(&out);
        true
    }

    /// If an OSC 8 hyperlink covers the cell under the pointer, open it with the
    /// OS handler. Returns whether a link was opened, so a Ctrl+click can
    /// suppress the normal selection / mouse-report path.
    fn open_link_under_pointer(&self) -> bool {
        let Some(p) = self.pane() else { return false };
        let (col, row) = self.cell_in_focused(self.mouse_pos.0, self.mouse_pos.1);
        let url = {
            let g = p.grid.lock();
            let col = g.logical_col(col, row);
            // An explicit OSC 8 link wins; otherwise scan the cell's logical
            // line for a plain-text URL (G16) — most programs never emit OSC 8.
            g.link_at(col, row).map(str::to_owned).or_else(|| g.url_at(col, row))
        };
        url.is_some_and(|u| open_url(&u))
    }

    /// Recompute `hover_link` for the pointer's current position: only set
    /// while Ctrl is held and the pointer is over grid content (not the
    /// chrome bar), and only when a hyperlink is actually there — mirrors
    /// `open_link_under_pointer`'s OSC-8-then-plain-text lookup, but via
    /// `hover_link_at` for the column span instead of a single cell.
    /// Returns whether the value changed, so callers know whether to
    /// repaint.
    fn update_hover_link(&mut self) -> bool {
        let (x, y) = self.mouse_pos;
        let new = if self.mods.control_key() && (y.max(0.0) as usize) >= self.cell_h {
            self.pane().and_then(|p| {
                let (col, row) = self.cell_in_focused(x, y);
                let g = p.grid.lock();
                let col = g.logical_col(col, row);
                g.hover_link_at(col, row).map(|(start, end, _)| (row, start, end))
            })
        } else {
            None
        };
        if new != self.hover_link {
            self.hover_link = new;
            true
        } else {
            false
        }
    }

    /// Open a dropdown listing every link visible in the focused pane —
    /// explicit OSC 8 hyperlinks and detected plain-text URLs — so links are
    /// reachable without the mouse (Ctrl+Shift+O; the keyboard side of G16).
    fn open_links_menu(&mut self) {
        let links = match self.pane() {
            Some(p) => p.grid.lock().visible_links(),
            None => return,
        };
        if links.is_empty() {
            return;
        }
        let items = links
            .into_iter()
            .map(|u| {
                // Menu rows are narrow; elide the middle of long URLs.
                let label = if u.chars().count() > 46 {
                    let head: String = u.chars().take(30).collect();
                    let tail: String = u.chars().rev().take(13).collect::<Vec<_>>().into_iter().rev().collect();
                    format!("{head}…{tail}")
                } else {
                    u.clone()
                };
                MenuItem { label, kind: MenuKind::OpenUrl(u) }
            })
            .collect();
        self.overlay = Some(Overlay::Menu { items, sel: 0 });
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Whether alternate scroll mode (`?1007`) should intercept wheel input
    /// for pane `id` right now: the mode is on *and* its alternate screen is
    /// active (mode 1007 only ever applies there — the primary screen keeps
    /// browsing rusty_term's own scrollback, same as xterm).
    fn alt_scroll_active(&self, id: u64) -> bool {
        self.pane_by_id(id).is_some_and(|p| {
            let g = p.grid.lock();
            g.alt_scroll && g.in_alt_screen()
        })
    }

    /// Translate a wheel scroll into repeated Up/Down (DECCKM-aware) key
    /// presses for alternate scroll mode (`?1007`) on pane `id`, so the wheel
    /// drives a pager (`less`, `man`, …) that never registered native mouse
    /// support.
    fn send_alt_scroll_keys(&mut self, id: u64, lines: isize) {
        let app_cursor = self.pane_by_id(id).is_some_and(|p| p.grid.lock().app_cursor_keys);
        let seq: &[u8] = match (lines >= 0, app_cursor) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        if let Some(p) = self.pane_by_id_mut(id) {
            for _ in 0..lines.unsigned_abs() {
                let _ = p.writer.write(seq);
            }
        }
    }

    /// Browse pane `id`'s scrollback: move the viewport by `lines` (positive
    /// = up into history, negative = back toward the live bottom), clamped to
    /// the available history. Repaints if the view actually moved. Takes a
    /// pane id (rather than always the focused pane) so wheel-scroll can
    /// target whichever pane is under the pointer, tmux/iTerm2-style, without
    /// stealing focus.
    fn scroll_active(&mut self, id: u64, lines: isize) {
        let Some(p) = self.pane_by_id(id) else { return };
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
    ///
    /// `Config::load` only sees this reload's own `--config <path>` args, so
    /// a `--profile` given at launch is reapplied via `profile_override`
    /// below — otherwise this window's profile-selected theme would silently
    /// revert to the file's top-level default on every save (mirrors
    /// `runtime::tokio_rt::watch_config`'s fix for the TUI front-end).
    fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        let args = vec!["--config".to_string(), path.to_string_lossy().into_owned()];
        let (mut new, mut warnings) = crate::config::Config::load(&args);
        if let Some(name) = &self.profile_override
            && let Some(w) = new.apply_profile(name)
        {
            warnings.push(w);
        }
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
        let status_changed =
            new.status_bar.unwrap_or(true) != self.config.status_bar.unwrap_or(true);
        self.config = new;
        let new_pad = self.config.padding.unwrap_or(DEFAULT_PAD) as usize;
        let pad_changed = new_pad != self.pad;
        self.pad = new_pad;
        if (pad_changed || status_changed)
            && let Some(size) = self.window.as_ref().map(|w| w.inner_size())
        {
            // The band / status ribbon grew or shrank; refit the grid to the
            // same window.
            self.apply_size(size.width, size.height);
        }
        if let Some(window) = &self.window {
            apply_chrome(window, &self.theme);
            window.request_redraw();
        }
    }
}

impl WindowState<'_> {
    /// Open the in-app settings page over the active tab, seeded from the live
    /// configuration. `Ctrl+,` and the dropdown's *Settings* entry route here.
    fn open_settings(&mut self) {
        // Ctrl+, toggles: with the page already open, close (and save) it
        // instead of silently rebuilding it and losing the user's position.
        if matches!(self.overlay, Some(Overlay::Settings(_))) {
            self.close_overlay();
            return;
        }
        let opacity_supported =
            self.renderer.as_ref().is_some_and(|r| r.supports_opacity());
        let s = Settings::new(
            &self.config,
            &self.theme,
            self.font_px,
            &self.shells,
            opacity_supported,
        );
        self.overlay = Some(Overlay::Settings(s));
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Open the shell-launcher dropdown (the `▾` button): the detected shells
    /// plus *Settings* and *Open config file* entries.
    fn open_menu(&mut self) {
        let items = shell_menu_items(&self.config.profiles, &self.shells);
        self.overlay = Some(Overlay::Menu { items, sel: 0 });
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Close any open overlay, persisting settings changes on the way out.
    fn close_overlay(&mut self) {
        if let Some(Overlay::Settings(mut s)) = self.overlay.take() {
            // A still-pending number edit commits like any blur; its live
            // apply comes from the reload watcher re-reading the save.
            s.commit_edit();
            if s.dirty {
                self.persist_settings(&s);
            }
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
    /// The settings page has richer input (search typing, number editing) and
    /// routes through [`Self::settings_key`].
    fn overlay_key(&mut self, event: &KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        if matches!(self.overlay, Some(Overlay::Settings(_))) {
            self.settings_key(event);
        } else {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => self.close_overlay(),
                Key::Named(NamedKey::ArrowUp) => self.overlay_move(false),
                Key::Named(NamedKey::ArrowDown) => self.overlay_move(true),
                Key::Named(NamedKey::Enter) => self.overlay_activate(),
                Key::Character(s) => {
                    if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10)) {
                        self.menu_pick_index(d as usize);
                    }
                }
                _ => {}
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Keys on the settings page. An in-progress number edit owns the
    /// keyboard first (digits, Backspace, Enter commit, Esc cancel); then
    /// Esc peels one layer at a time (edit → filter → page); typing filters
    /// (or, digits on a number row, starts an edit); the rest navigates.
    fn settings_key(&mut self, event: &KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        let Some(Overlay::Settings(s)) = &mut self.overlay else { return };
        let editing = s.editing.is_some();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                if editing {
                    s.cancel_edit();
                } else if s.filter.is_some() {
                    s.clear_filter();
                } else {
                    self.close_overlay();
                }
            }
            Key::Named(NamedKey::Enter) => {
                if editing {
                    let field = s.commit_edit();
                    if let Some(field) = field {
                        self.apply_setting(field);
                    }
                } else if !s.begin_edit(None) {
                    // Not an editable number: Enter cycles, mirroring →.
                    self.overlay_change(true);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if editing {
                    s.edit_backspace();
                } else {
                    s.filter_backspace();
                }
            }
            Key::Named(NamedKey::ArrowUp) => self.overlay_move(false),
            Key::Named(NamedKey::ArrowDown) => self.overlay_move(true),
            Key::Named(NamedKey::ArrowLeft) if !editing => self.overlay_change(false),
            Key::Named(NamedKey::ArrowRight) if !editing => self.overlay_change(true),
            // Sidebar: Tab / Shift+Tab steps the category (dropping any
            // filter — the user is navigating away from the results).
            Key::Named(NamedKey::Tab) => {
                let forward = !self.mods.shift_key();
                if let Some(Overlay::Settings(s)) = &mut self.overlay {
                    s.clear_filter();
                    s.cycle_category(forward);
                }
            }
            // Numbers: Home/End jump to the bound.
            Key::Named(NamedKey::Home) if !editing => self.overlay_jump(false),
            Key::Named(NamedKey::End) if !editing => self.overlay_jump(true),
            // Space extends an active search query ("click to move"); it
            // arrives as a named key, not a Character.
            Key::Named(NamedKey::Space) if !editing && s.filter.is_some() => {
                s.filter_input(' ');
            }
            Key::Character(text) => {
                let Some(c) = text.chars().next() else { return };
                if self.mods.control_key() {
                    // Ctrl+F opens the search, matching the find bar.
                    if text.as_str() == "f"
                        && let Some(Overlay::Settings(s)) = &mut self.overlay
                    {
                        s.start_filter();
                    }
                    return;
                }
                if editing {
                    s.edit_input(c);
                } else if c.is_ascii_digit() && s.filter.is_none() {
                    // A digit on a number row starts an edit with that digit;
                    // on anything else it falls through to the filter.
                    if !s.begin_edit(Some(c)) {
                        s.filter_input(c);
                    }
                } else if c == '/' && s.filter.is_none() {
                    // `/` opens the search (vim/less convention) rather than
                    // becoming the query's first character.
                    s.start_filter();
                } else if !c.is_control() {
                    s.filter_input(c);
                }
            }
            _ => {}
        }
    }

    /// Move the highlight within the overlay. Moving off a number row with
    /// an edit in progress commits it (blur), like any form.
    fn overlay_move(&mut self, forward: bool) {
        let mut committed = None;
        match &mut self.overlay {
            Some(Overlay::Menu { items, sel }) => {
                let n = items.len();
                if n > 0 {
                    *sel = if forward { (*sel + 1) % n } else { (*sel + n - 1) % n };
                }
            }
            Some(Overlay::Settings(s)) => {
                committed = s.commit_edit();
                s.move_sel(forward);
                let visible = settings_visible(self.rows as usize);
                s.ensure_visible(visible);
            }
            None => {}
        }
        if let Some(field) = committed {
            self.apply_setting(field);
        }
    }

    /// Change the highlighted setting (no effect on the menu) and apply it
    /// live. Shift multiplies a stepped number's step by 10.
    fn overlay_change(&mut self, forward: bool) {
        let big = self.mods.shift_key();
        let field = match &mut self.overlay {
            Some(Overlay::Settings(s)) => s.change_by(forward, big),
            _ => return,
        };
        if let Some(field) = field {
            self.apply_setting(field);
        }
    }

    /// Jump the highlighted stepped number to its bound (Home/End) and apply
    /// it live; no effect on choices, toggles, or the menu.
    fn overlay_jump(&mut self, to_max: bool) {
        let field = match &mut self.overlay {
            Some(Overlay::Settings(s)) => s.jump(to_max),
            _ => return,
        };
        if let Some(field) = field {
            self.apply_setting(field);
        }
    }

    /// Activate the highlighted overlay row: pick a menu item, or cycle the
    /// highlighted setting (Enter mirrors →).
    fn overlay_activate(&mut self) {
        match &self.overlay {
            Some(Overlay::Menu { items, sel }) => {
                if let Some(kind) = items.get(*sel).map(|i| i.kind.clone()) {
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
            Some(Overlay::Menu { items, .. }) if n >= 1 => items.get(n - 1).map(|i| i.kind.clone()),
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
            MenuKind::LaunchProfile(i) => {
                if let Err(e) = self.spawn_tab_profile(i) {
                    eprintln!("rusty_term: profile tab: {e}");
                }
            }
            MenuKind::OpenUrl(url) => {
                open_url(&url);
            }
            MenuKind::Settings => self.open_settings(),
            MenuKind::EditConfig => {
                self.overlay = None;
                self.open_config();
            }
        }
    }

    /// The overlay-grid cell under window pixel `(x, y)`, `None` above the
    /// chrome bar. Mirrors the overlay's own layout: the bar takes the first
    /// cell row flush, the grid sits below it inset by `pad`.
    fn overlay_cell(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        let col = ((x - self.pad as f64).max(0.0) as usize) / self.cell_w;
        let screen_row = ((y - self.grid_oy() as f64).max(0.0) as usize) / self.cell_h;
        Some((col, screen_row.checked_sub(1)?))
    }

    /// Handle a click in the overlay body (below the chrome bar): select the
    /// row under the pointer (activating it for a menu); on the settings page
    /// also switch sidebar categories and cycle a clicked value widget.
    fn overlay_click(&mut self, x: f64, y: f64) {
        let Some((col, grid_row)) = self.overlay_cell(x, y) else { return };
        let mut apply: Vec<Field> = Vec::new();
        let mut activate = false;
        match &mut self.overlay {
            Some(Overlay::Menu { items, sel }) => {
                if let Some(i) = grid_row.checked_sub(OVERLAY_ITEMS_TOP)
                    && i < items.len()
                {
                    *sel = i;
                    activate = true;
                }
            }
            Some(Overlay::Settings(s)) => {
                // A click anywhere blurs an in-progress number edit,
                // committing it first (like any form).
                apply.extend(s.commit_edit());
                let grid_rows = self.rows as usize;
                let grid_cols = self.cols as usize;
                match settings_hit(col, grid_row, grid_cols, grid_rows, s.scroll, s.len()) {
                    SettingsHit::Category(i) => s.set_category(i),
                    SettingsHit::Search => s.start_filter(),
                    SettingsHit::Row(i) => {
                        s.select(i);
                        // Inside the value widget: the left arrow cell cycles
                        // back, anywhere else forward (toggles just flip).
                        let row = &s.rows()[i];
                        let wtext = widget_text(&row.widget, true);
                        let value_end = settings_value_end(grid_cols);
                        let wcol = value_end.saturating_sub(wtext.chars().count());
                        if col >= wcol && col < value_end {
                            apply.extend(s.change(col != wcol));
                        }
                    }
                    SettingsHit::None => {}
                }
            }
            None => {}
        }
        for field in apply {
            self.apply_setting(field);
        }
        if activate {
            self.overlay_activate();
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Apply a just-changed setting to the running terminal. Values are
    /// snapshotted first so the overlay borrow ends before we mutate `self`.
    /// Launch-time fields (launch mode, cols, rows) only update the config —
    /// their rows say "applies at next launch" — but still count as applied
    /// so persistence picks them up on close.
    fn apply_setting(&mut self, field: Field) {
        let Some(Overlay::Settings(s)) = self.overlay.as_ref() else { return };
        let theme = crate::config::preset(s.theme_name());
        let font_size = s.font_size();
        let cursor = s.cursor();
        let blink = s.blink();
        let ligatures = s.ligatures();
        let cursor_trail = s.cursor_trail();
        let min_contrast = s.min_contrast();
        let scrollback = s.scrollback();
        let shell = s.shell_path();
        let copy_html = s.copy_html();
        let clipboard = s.clipboard();
        let click_to_move = s.click_to_move();
        let bell = s.bell();
        let padding = s.padding();
        let status_bar = s.status_bar();
        let command_marks = s.command_marks();
        let opacity = s.opacity();
        let launch_mode = s.launch_mode_value();
        let cols = s.cols_value();
        let rows = s.rows_value();
        match field {
            Field::Theme => {
                if let Some(t) = theme {
                    self.apply_theme_live(t);
                }
            }
            Field::FontSize => self.rebuild_font(font_size, self.config.ligatures.unwrap_or(true)),
            Field::Ligatures => self.rebuild_font(self.font_px, ligatures),
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
            // Read live at draw time; updating the config is the whole apply.
            Field::CursorTrail => self.config.cursor_trail = Some(cursor_trail),
            Field::MinContrast => {
                self.config.minimum_contrast = min_contrast;
                for tab in &self.tabs {
                    for p in &tab.panes {
                        p.grid.lock().min_contrast = min_contrast.unwrap_or(1.0);
                    }
                }
            }
            Field::Scrollback => {
                self.config.scrollback = Some(scrollback);
                for tab in &self.tabs {
                    for p in &tab.panes {
                        p.grid.lock().set_scrollback_max(scrollback);
                    }
                }
            }
            Field::Shell => self.config.shell = shell,
            // Read live at copy/paste/alert time; the config *is* the state.
            Field::CopyHtml => self.config.copy_html = Some(copy_html),
            Field::Clipboard => self.config.clipboard = Some(clipboard),
            Field::ClickToMove => self.config.click_to_move = Some(click_to_move),
            Field::Bell => self.config.bell = Some(bell),
            Field::Padding => {
                self.config.padding = Some(padding);
                self.pad = padding as usize;
                if let Some(size) = self.window.as_ref().map(|w| w.inner_size()) {
                    // The band grew or shrank; refit the grid to the same window.
                    self.apply_size(size.width, size.height);
                }
            }
            // Read live at draw time; updating the config is the whole apply.
            Field::CommandMarks => self.config.command_marks = Some(command_marks),
            Field::StatusBar => {
                self.config.status_bar = Some(status_bar);
                if let Some(size) = self.window.as_ref().map(|w| w.inner_size()) {
                    // The ribbon claimed or released a row; refit the grid.
                    self.apply_size(size.width, size.height);
                }
            }
            Field::Opacity => {
                self.config.opacity = Some(opacity);
                self.apply_opacity();
            }
            // Launch-time only: persisted on close, applied by the next launch.
            Field::LaunchMode => self.config.launch_mode = launch_mode,
            Field::Cols => self.config.cols = Some(cols),
            Field::Rows => self.config.rows = Some(rows),
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
        for tab in &mut self.tabs {
            for p in &mut tab.panes {
                let (report, changed) = {
                    let mut g = p.grid.lock();
                    let was_dark = g.appearance_is_dark();
                    let old = p.parser.lock().retheme(new);
                    if old != new {
                        g.retheme(&old, &new);
                    }
                    let report = g.report_color_scheme && g.appearance_is_dark() != was_dark;
                    (g.color_scheme_report(), report)
                };
                // Mode 2031: tell a subscribed child its world flipped
                // light/dark (Neovim flips `background` off this).
                if changed {
                    let _ = p.writer.write(&report);
                }
            }
        }
        self.theme = new;
        self.config.theme = new;
        if let Some(window) = &self.window {
            apply_chrome(window, &self.theme);
        }
    }

    /// Resolve `theme = "auto"` against the OS appearance: `hint` is a live
    /// `ThemeChanged` value, else the window's current report (dark when
    /// unknown). Presets come from `theme_dark` / `theme_light`, defaulting
    /// to the built-in default (dark) and Solarized Light.
    fn auto_theme(&self, hint: Option<winit::window::Theme>) -> Theme {
        let dark = !matches!(
            hint.or_else(|| self.window.as_ref().and_then(|w| w.theme())),
            Some(winit::window::Theme::Light)
        );
        if dark {
            self.config.theme_dark.unwrap_or_default()
        } else {
            self.config
                .theme_light
                .or_else(|| crate::config::preset("solarized-light"))
                .unwrap_or_default()
        }
    }

    /// Push the configured window opacity (`[window] opacity` / `--opacity`)
    /// to the current renderer. Only the GPU renderer honors it (see
    /// `Renderer::set_opacity`'s default no-op); called whenever the renderer
    /// is (re)built, since a fresh one starts back at fully opaque.
    fn apply_opacity(&mut self) {
        let opacity = self.config.opacity.unwrap_or(1.0).clamp(0.0, 1.0);
        if let Some(r) = &mut self.renderer {
            r.set_opacity(opacity);
        }
    }

    /// Rebuild the glyph cache at `px` / `ligatures`, re-fit the grid to the new
    /// cell size, and rebuild the renderer (the GPU atlas is font-bound). This
    /// is the user-driven entry point (settings overlay font-size/ligatures
    /// fields): it persists `px` as the configured font size. A monitor-DPI
    /// change should *not* overwrite that preference — see
    /// [`Self::on_scale_factor_changed`], which calls [`Self::rebuild_font_at`]
    /// directly instead.
    fn rebuild_font(&mut self, px: f32, ligatures: bool) {
        if self.rebuild_font_at(px, ligatures) {
            self.config.font_size = Some(px);
            self.config.ligatures = Some(ligatures);
        }
    }

    /// Core of [`Self::rebuild_font`], minus persisting to config: rebuild the
    /// glyph cache at `px` / `ligatures` (at the current `scale_factor`),
    /// re-fit the grid to the new cell size, and rebuild the renderer (the
    /// GPU atlas is font-bound). Returns whether the font actually rebuilt (a
    /// missing/unparseable font leaves state untouched).
    fn rebuild_font_at(&mut self, px: f32, ligatures: bool) -> bool {
        if !self.rescale_font_cache(px, ligatures, self.scale_factor) {
            return false;
        }
        self.after_font_rescale();
        true
    }

    /// Rebuild just the glyph cache / cell size / tracked scale factor — no
    /// renderer rebuild or grid re-fit, so this is safe to call before an OS
    /// window exists (see the HiDPI sizing in `ensure_window`). Returns
    /// whether it succeeded (a missing/unparseable font leaves state
    /// untouched).
    fn rescale_font_cache(&mut self, px: f32, ligatures: bool, scale_factor: f64) -> bool {
        let Some(font_set) = font::load_set(
            self.config.font.as_deref(),
            self.config.font_bold.as_deref(),
            self.config.font_italic.as_deref(),
            self.config.font_bold_italic.as_deref(),
            self.config.font_fallback.as_deref(),
        ) else {
            return false;
        };
        let Some(font) = FontCache::new(font_set, px, ligatures) else { return false };
        let (cw, ch) = font.cell_size();
        self.font = font;
        self.cell_w = cw.max(1);
        self.cell_h = ch.max(1);
        self.font_px = px;
        self.scale_factor = scale_factor;
        true
    }

    /// The renderer-facing tail of a font rescale: keep every pane's XTWINOPS
    /// pixel-size answer (14t/16t) current, and — once an OS window exists —
    /// rebuild the renderer and re-fit the grid to the new cell size.
    fn after_font_rescale(&mut self) {
        let cell_px = Some((self.cell_w as u16, self.cell_h as u16));
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.grid.lock().cell_px = cell_px;
            }
        }
        if let Some(window) = self.window.clone() {
            if let Some(r) = self.make_renderer(window.clone()) {
                self.renderer = Some(r);
            }
            self.apply_opacity();
            let size = window.inner_size();
            self.apply_size(size.width, size.height);
        }
    }

    /// Handle `WindowEvent::ScaleFactorChanged` (the window moved to a monitor
    /// with a different DPI scale, or the OS scale setting changed). Winit
    /// reports `inner_size()` and pointer coordinates in physical pixels
    /// throughout this file, but font rasterization was never scaled by the
    /// monitor's factor — dragging the window from a 100% to a 200% display
    /// used to render text at half the intended visual size. Rescale the
    /// *current* physical font size proportionally to the factor change
    /// rather than recomputing from the configured (logical, monitor-
    /// independent) `font_size`, so the user's chosen size is preserved
    /// exactly when they move back to the original monitor. This
    /// deliberately does not touch `self.config.font_size` — a DPI move is
    /// not a user preference change.
    fn on_scale_factor_changed(&mut self, new_scale: f64) {
        if !(new_scale > 0.0) || (new_scale - self.scale_factor).abs() < 1e-9 {
            return;
        }
        let ratio = new_scale / self.scale_factor;
        let ligatures = self.config.ligatures.unwrap_or(true);
        let px = ((self.font_px as f64) * ratio) as f32;
        if self.rescale_font_cache(px, ligatures, new_scale) {
            self.after_font_rescale();
        }
        if let Some(window) = &self.window {
            window.request_redraw();
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
                // No header row: the strip's own "Settings" tab names the
                // page, so the content starts at the top and the save state
                // lives in the footer.
                let footer_top = rows.saturating_sub(SETTINGS_FOOTER_H);
                // Selection tints hard, hover barely — the two must never
                // read as the same state when both are on screen.
                let sel_bg = mix(bg, fg, 56);
                let hover_bg = mix(bg, fg, 14);
                // Selected-card description: brighter than dim (it sits on
                // the tinted band, where dim text loses its contrast).
                let mid = mix(fg, bg, 60);
                let accent = self.theme.cursor;

                // Category sidebar: active gets an accent bar + tint (flush
                // to the `│` rule separating it from the cards), the rest
                // render dim; a category holding modified settings carries an
                // accent dot. While a search is on, results span categories,
                // so no sidebar entry is "the" active one.
                let filtering = s.filter.is_some();
                for (i, (cat, label)) in CATEGORIES.iter().enumerate() {
                    let r = SETTINGS_TOP + i * 2;
                    if r >= footer_top {
                        break;
                    }
                    let active = i == s.cat && !filtering;
                    let (cfg, cbg) = if active {
                        fill_row(&mut g, r, 0, SETTINGS_SIDEBAR_W + 1, fg, sel_bg);
                        write_row(&mut g, r, 0, "\u{258C}", fg, sel_bg);
                        (fg, sel_bg)
                    } else {
                        (dim, bg)
                    };
                    write_row(&mut g, r, 2, label, cfg, cbg);
                    if s.category_modified(*cat) {
                        let dcol = 2 + label.chars().count() + 1;
                        write_row(&mut g, r, dcol, "\u{2022}", accent, cbg);
                    }
                }
                for r in SETTINGS_TOP..footer_top {
                    write_row(&mut g, r, SETTINGS_SIDEBAR_W + 1, "\u{2502}", dim, bg);
                }
                // The sidebar's idle bottom answers "what file does Esc save
                // into, and which build is this" without opening the docs.
                if footer_top >= 2 {
                    if let Some(name) =
                        self.config_path.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str())
                    {
                        let shown: String = name.chars().take(SETTINGS_SIDEBAR_W - 2).collect();
                        write_row(&mut g, footer_top - 2, 2, &shown, dim, bg);
                    }
                    write_row(
                        &mut g,
                        footer_top - 1,
                        2,
                        concat!("v", env!("CARGO_PKG_VERSION")),
                        dim,
                        bg,
                    );
                }

                // Title row: the active category's name — or the search
                // result count — left, and the search affordance right (the
                // live query while one is active, a dim `/ search` invite
                // otherwise). The rule underneath anchors the column width.
                let value_end = settings_value_end(cols);
                let title = if filtering {
                    match s.len() {
                        0 => "No settings match".to_string(),
                        1 => "1 result".to_string(),
                        n => format!("{n} results"),
                    }
                } else {
                    CATEGORIES[s.cat].1.to_string()
                };
                write_row(&mut g, SETTINGS_TOP, SETTINGS_CARD_X, &title, fg, bg);
                match &s.filter {
                    Some(q) => {
                        let qcol = value_end.saturating_sub(q.chars().count() + 2);
                        write_row(&mut g, SETTINGS_TOP, qcol, &format!("/{q}"), fg, bg);
                        write_row(&mut g, SETTINGS_TOP, value_end.saturating_sub(1), "_", dim, bg);
                    }
                    None => {
                        let t = "/ search";
                        write_row(&mut g, SETTINGS_TOP, value_end.saturating_sub(t.len()), t, dim, bg);
                    }
                }
                let rule_w = value_end.saturating_sub(SETTINGS_CARD_X);
                write_row(&mut g, SETTINGS_TOP + 1, SETTINGS_CARD_X, &"\u{2500}".repeat(rule_w), dim, bg);

                // Setting cards: title + right-aligned value widget, then a
                // dim one-line description. Highlight bands stop at the card
                // column's edge, not the window's. The scroll clamp guards a
                // scroll left past the end by a resize.
                let visible = settings_visible(rows);
                let list = s.rows();
                let scroll = s.scroll.min(list.len().saturating_sub(1));
                for (vi, row) in list.iter().enumerate().skip(scroll).take(visible) {
                    let top = SETTINGS_CARDS_TOP + (vi - scroll) * SETTINGS_ROW_H;
                    let selected = vi == s.sel;
                    let hovered = s.hover == Some(vi);
                    let rbg = if selected {
                        sel_bg
                    } else if hovered {
                        hover_bg
                    } else {
                        bg
                    };
                    if rbg != bg {
                        fill_row(&mut g, top, SETTINGS_SIDEBAR_W + 2, value_end + 1, fg, rbg);
                        fill_row(&mut g, top + 1, SETTINGS_SIDEBAR_W + 2, value_end + 1, fg, rbg);
                    }
                    if selected {
                        write_row(&mut g, top, SETTINGS_SIDEBAR_W + 2, "\u{258C}", fg, rbg);
                    }
                    let label_fg = if row.disabled { dim } else { fg };
                    write_row(&mut g, top, SETTINGS_CARD_X, row.label, label_fg, rbg);
                    // An accent dot marks a value changed from its default —
                    // the same gate that decides what lands in the config
                    // file, so customizations are visible at a glance.
                    let mut after = SETTINGS_CARD_X + row.label.chars().count();
                    if row.modified {
                        write_row(&mut g, top, after + 1, "\u{2022}", accent, rbg);
                        after += 2;
                    }
                    // Search results span categories; tag each row with its
                    // home so "Scrollback" isn't ambiguous out of context.
                    if let Some(cat) = row.category {
                        write_row(&mut g, top, after + 2, &format!("\u{B7} {cat}"), dim, rbg);
                    }
                    let desc_fg = if selected { mid } else { dim };
                    write_row(&mut g, top + 1, SETTINGS_CARD_X, row.description, desc_fg, rbg);
                    // The theme row previews its palette: the 16 ANSI colors
                    // as swatches on the description row, so cycling themes
                    // is a visual choice rather than a name lottery. Skipped
                    // when a narrow column would run them into the
                    // description text — a broken overlap is worse than no
                    // preview.
                    let swatch0 = value_end.saturating_sub(16);
                    if row.field == Field::Theme
                        && swatch0 >= SETTINGS_CARD_X + row.description.chars().count() + 2
                        && let Some(t) = crate::config::preset(s.theme_name())
                    {
                        for (k, color) in t.palette16.iter().enumerate() {
                            let mut cell = Cell::blank();
                            cell.ch = '\u{2588}';
                            cell.fg = *color;
                            cell.bg = rbg;
                            g.set_cell(swatch0 + k, top + 1, cell);
                        }
                    }
                    // An in-progress edit shows the typed buffer + a cursor
                    // cell in place of the value.
                    let editing_this = selected && s.editing.is_some();
                    // Hover gets the `< >` affordance too — but never on a
                    // disabled row, which renders inert.
                    let active = (selected || hovered) && !row.disabled && !editing_this;
                    let wtext = match (&s.editing, editing_this) {
                        (Some(buf), true) => format!("{buf}_"),
                        _ => widget_text(&row.widget, active),
                    };
                    let wlen = wtext.chars().count();
                    let wcol = value_end.saturating_sub(wlen);
                    let wfg = if row.disabled {
                        dim
                    } else {
                        match row.widget {
                            Widget::Toggle(false) if !editing_this => dim,
                            _ => fg,
                        }
                    };
                    write_row(&mut g, top, wcol, &wtext, wfg, rbg);
                    if active && matches!(row.widget, Widget::Choice(_) | Widget::Number(_)) {
                        // The cycle arrows read as controls, not value text.
                        write_row(&mut g, top, wcol, "<", dim, rbg);
                        write_row(&mut g, top, value_end.saturating_sub(1), ">", dim, rbg);
                    }
                    // "next launch" tag: scannable, out of the description.
                    if row.next_launch && !editing_this {
                        let tag = "next launch";
                        let tcol = wcol.saturating_sub(tag.len() + 2);
                        write_row(&mut g, top, tcol, tag, dim, rbg);
                    }
                }
                // Proportional scrollbar when the list continues past the
                // window: position and extent at a glance, not just "more".
                if list.len() > visible && visible > 0 {
                    let track_top = SETTINGS_CARDS_TOP;
                    let track_h = (visible * SETTINGS_ROW_H).saturating_sub(1).max(1);
                    let thumb_h = (track_h * visible / list.len()).max(1);
                    let denom = (list.len() - visible).max(1);
                    let thumb_top = track_top + (track_h - thumb_h) * scroll / denom;
                    for r in track_top..track_top + track_h {
                        let (ch, color) = if (thumb_top..thumb_top + thumb_h).contains(&r) {
                            ('\u{2590}', dim) // ▐ thumb
                        } else {
                            ('\u{2502}', mix(bg, fg, 40)) // │ track
                        };
                        let mut cell = Cell::blank();
                        cell.ch = ch;
                        cell.fg = color;
                        cell.bg = bg;
                        g.set_cell(value_end + 2, r, cell);
                    }
                }
                // Footer: keycap-styled contextual hints (keys bright, verbs
                // dim, so the eye can find the key it needs) with the save
                // state at the right — live changes persist on close, so
                // "modified" is a promise, not a warning.
                let hints: &[(&str, &str)] = if s.editing.is_some() {
                    &[("Enter", "apply"), ("Esc", "keep old value")]
                } else if filtering {
                    &[("type", "to refine"), ("Up/Down", "result"), ("Esc", "back")]
                } else {
                    &[
                        ("Tab", "category"),
                        ("Up/Down", "row"),
                        ("Left/Right", "change"),
                        ("/", "search"),
                        ("Enter", "edit"),
                        ("Esc", "close & save"),
                    ]
                };
                let status = if s.dirty { "modified - saves on close" } else { "saved" };
                let hrow = rows.saturating_sub(1);
                let limit = cols.saturating_sub(status.chars().count() + 4);
                let mut hcol = 2;
                for (key, verb) in hints {
                    if hcol + key.chars().count() + verb.chars().count() + 4 > limit {
                        break; // drop trailing hints rather than clip mid-word
                    }
                    write_row(&mut g, hrow, hcol, key, fg, bg);
                    hcol += key.chars().count() + 1;
                    write_row(&mut g, hrow, hcol, verb, dim, bg);
                    hcol += verb.chars().count() + 3;
                }
                let scol = cols.saturating_sub(status.chars().count() + 2);
                write_row(&mut g, hrow, scol, status, if s.dirty { fg } else { dim }, bg);
            }
            None => {}
        }
        g
    }
}

/// Build the shell-launcher dropdown: configured profiles first, then each
/// detected shell (launching a new tab), then the *Settings* and *Open
/// config file* entries.
fn shell_menu_items(
    profiles: &[crate::config::Profile],
    shells: &[crate::shells::DetectedShell],
) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = profiles
        .iter()
        .enumerate()
        .map(|(i, p)| MenuItem { label: format!("Profile: {}", p.name), kind: MenuKind::LaunchProfile(i) })
        .collect();
    items.extend(
        shells
            .iter()
            .enumerate()
            .map(|(i, s)| MenuItem { label: s.name.to_string(), kind: MenuKind::LaunchShell(i) }),
    );
    items.push(MenuItem { label: "Settings".to_string(), kind: MenuKind::Settings });
    items.push(MenuItem { label: "Open config file".to_string(), kind: MenuKind::EditConfig });
    items
}

/// How many setting cards fit in a `grid_rows`-tall overlay grid.
fn settings_visible(grid_rows: usize) -> usize {
    grid_rows.saturating_sub(SETTINGS_CARDS_TOP + SETTINGS_FOOTER_H) / SETTINGS_ROW_H
}

/// The column a card's value widget ends at (exclusive): the window's right
/// pad, or the card-width cap on a wide window — whichever is nearer.
fn settings_value_end(cols: usize) -> usize {
    (cols.saturating_sub(SETTINGS_RIGHT_PAD)).min(SETTINGS_SIDEBAR_W + 2 + SETTINGS_CARD_W_MAX)
}

/// What a settings-page cell `(col, row)` lands on, for clicks and hover.
/// `scroll`/`len` describe the active category's list; `Row` carries the
/// *list* index (scroll already added). `cols` sizes the search affordance's
/// zone at the title row's right edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsHit {
    Category(usize),
    Row(usize),
    /// The `/ search` affordance on the title row.
    Search,
    None,
}

fn settings_hit(
    col: usize,
    row: usize,
    cols: usize,
    grid_rows: usize,
    scroll: usize,
    len: usize,
) -> SettingsHit {
    let footer = grid_rows.saturating_sub(SETTINGS_FOOTER_H);
    if row < SETTINGS_TOP || row >= footer {
        return SettingsHit::None;
    }
    if col < SETTINGS_SIDEBAR_W {
        // Sidebar entries sit on every other row, and the whole strip is
        // clickable: the gap row belongs to the entry above it.
        let i = (row - SETTINGS_TOP) / 2;
        if i < CATEGORIES.len() {
            return SettingsHit::Category(i);
        }
        return SettingsHit::None;
    }
    // Title row: its right end is the search affordance.
    let value_end = settings_value_end(cols);
    if row == SETTINGS_TOP {
        if col >= value_end.saturating_sub(SETTINGS_SEARCH_W) && col < value_end {
            return SettingsHit::Search;
        }
        return SettingsHit::None;
    }
    // Card area (below the page title + rule): a card's title and
    // description rows both count; the spacing row between cards is dead.
    let Some(r) = row.checked_sub(SETTINGS_CARDS_TOP) else { return SettingsHit::None };
    let card = r / SETTINGS_ROW_H;
    if r % SETTINGS_ROW_H < 2 && card < settings_visible(grid_rows) {
        let i = scroll + card;
        if i < len {
            return SettingsHit::Row(i);
        }
    }
    SettingsHit::None
}

/// A value widget's cell text. Toggles render a block-glyph switch (boxdraw
/// synthesizes U+2588/2591, so no font dependency); a selected *or hovered*
/// choice/number grows `< value >` cycle arrows — hover gets the affordance
/// too, so values don't read as static text until clicked.
fn widget_text(w: &Widget, active: bool) -> String {
    match w {
        Widget::Toggle(true) => "[\u{2588}\u{2588}] on".to_string(),
        Widget::Toggle(false) => "[\u{2591}\u{2591}] off".to_string(),
        Widget::Choice(v) | Widget::Number(v) => {
            if active {
                format!("< {v} >")
            } else {
                v.clone()
            }
        }
    }
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

/// Fold a wheel/trackpad delta (already converted to a "lines" unit) plus a
/// carried fractional remainder into a whole line count and the remainder to
/// carry into the next event. Pulled out as a pure function (rather than
/// inlined in the `MouseWheel` handler) so the accumulation math — the fix
/// for slow scrolling silently doing nothing — is unit-testable without a
/// live window.
fn accumulate_scroll_lines(accum: f64, raw_lines: f64) -> (isize, f64) {
    let total = accum + raw_lines;
    let lines = total.trunc() as isize;
    (lines, total - lines as f64)
}

/// Per-channel mix of `t/255` of `b` into `a` (`0xRRGGBB`) — used to derive
/// the chrome bar's bg and dimmed text from the theme without new config keys.
/// Build the command dock's grid (`cols × rows`) from the focused pane's
/// state: a header, then the running command and the finished command
/// blocks newest-first — exit glyph (✓ green / ✗ red / · when the shell
/// reported no code), the command line (the prompt line just above the
/// block's output), and the runtime right-aligned. Returns the grid plus
/// the per-row click map (the absolute line a click jumps to).
fn build_dock_grid(
    g: &Grid,
    theme: &crate::core::Theme,
    cols: usize,
    rows: usize,
) -> (Grid, Vec<Option<usize>>) {
    let mut d = Grid::new(cols, rows);
    let (fg, bg) = (theme.fg, theme.bg);
    let dim = mix(fg, bg, 110);
    for r in 0..rows {
        fill_row(&mut d, r, 0, cols, fg, bg);
    }
    let mut items: Vec<Option<usize>> = vec![None; rows];
    write_row(&mut d, 0, 1, "Commands", dim, bg);

    // Entries newest-first: the running command (if any), then finished
    // blocks in reverse stream order.
    let mut entries: Vec<(char, u32, String, usize)> = Vec::new();
    if let Some(start) = g.running_command() {
        entries.push(('\u{25b6}', theme.cursor, "\u{2026}".to_string(), start));
    }
    for b in g.fold_blocks().iter().rev() {
        let (glyph, color) = match b.exit {
            Some(0) => ('\u{2713}', theme.palette16[2]),
            Some(_) => ('\u{2717}', theme.palette16[1]),
            None => ('\u{00b7}', dim),
        };
        let dur = b.runtime.map(fmt_runtime).unwrap_or_default();
        entries.push((glyph, color, dur, b.start));
    }

    if entries.is_empty() {
        write_row(&mut d, 2, 1, "No commands yet.", dim, bg);
        write_row(&mut d, 4, 1, "Needs OSC 133 shell", dim, bg);
        write_row(&mut d, 5, 1, "integration.", dim, bg);
        return (d, items);
    }
    let mut row = 2;
    for (glyph, color, dur, start) in entries {
        if row >= rows {
            break;
        }
        // The command's text: the prompt line just above its output (where
        // it was typed); the first output line when there is none.
        let mut label = g.abs_line_text(start.saturating_sub(1));
        if label.is_empty() {
            label = g.abs_line_text(start);
        }
        write_row(&mut d, row, 1, &glyph.to_string(), color, bg);
        let label_max = cols.saturating_sub(4 + dur.chars().count() + 1);
        let label: String = label.chars().take(label_max).collect();
        write_row(&mut d, row, 3, &label, fg, bg);
        let dcol = cols.saturating_sub(dur.chars().count() + 1);
        write_row(&mut d, row, dcol, &dur, dim, bg);
        items[row] = Some(start);
        row += 1;
    }
    (d, items)
}

/// A command runtime for the dock: `<1s`, `42s`, `3m07s`, `2h05m`.
fn fmt_runtime(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => "<1s".to_string(),
        1..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// How long a resolved git branch is trusted before `.git/HEAD` is re-read
/// (the status ribbon repaints far more often than branches change).
const GIT_BRANCH_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// A path for the status ribbon: the home directory shortened to `~`.
fn display_path(p: &std::path::Path) -> String {
    let text = p.display().to_string();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    if let Some(home) = home
        && let Ok(rest) = p.strip_prefix(&home)
    {
        let rest = rest.display().to_string();
        return if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~{}{rest}", std::path::MAIN_SEPARATOR)
        };
    }
    text
}

/// The current git branch for `dir`: walk up to the nearest `.git`, follow a
/// worktree/submodule `gitdir:` file if that's what it is, and parse `HEAD` —
/// `ref: refs/heads/<name>` yields the name, a detached head yields the
/// short hash. Pure file reads (no `git` subprocess), `None` outside a repo
/// or on any parse surprise.
fn read_git_branch(dir: &std::path::Path) -> Option<String> {
    let mut cur = Some(dir);
    let git = loop {
        let d = cur?;
        let candidate = d.join(".git");
        if candidate.exists() {
            break candidate;
        }
        cur = d.parent();
    };
    let head_path = if git.is_file() {
        // A worktree/submodule: `.git` is a one-line `gitdir: <path>` file.
        let text = std::fs::read_to_string(&git).ok()?;
        let target = text.strip_prefix("gitdir:")?.trim();
        let target = std::path::Path::new(target);
        let base =
            if target.is_absolute() { target.to_path_buf() } else { git.parent()?.join(target) };
        base.join("HEAD")
    } else {
        git.join("HEAD")
    };
    let head = std::fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(r) => Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string()),
        None if head.len() >= 8 => Some(head[..8].to_string()),
        None => None,
    }
}

fn mix(a: u32, b: u32, t: u32) -> u32 {
    let chan = |s: u32| {
        let av = (a >> s) & 0xff;
        let bv = (b >> s) & 0xff;
        ((av * (255 - t) + bv * t) / 255) << s
    };
    chan(16) | chan(8) | chan(0)
}

/// Paint the window border with the theme background so the frame reads as
/// part of the terminal (the title bar itself is ours now), and ask DWM to
/// round the window corners so the borderless frame matches native Windows 11
/// windows. Both are Windows 11 only (DWM ignores the attributes on 10);
/// a no-op on other platforms.
fn apply_chrome(window: &Window, theme: &Theme) {
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::{Color, WindowExtWindows};
        let c = |rgb: u32| Color::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        window.set_border_color(Some(c(theme.bg)));
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw()
        {
            use windows_sys::Win32::Graphics::Dwm::{
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
            };
            let pref: i32 = DWMWCP_ROUND;
            // SAFETY: a live HWND from winit; DWM copies the 4-byte value.
            unsafe {
                DwmSetWindowAttribute(
                    h.hwnd.get() as windows_sys::Win32::Foundation::HWND,
                    DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                    &pref as *const i32 as *const core::ffi::c_void,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, theme);
    }
}

impl WindowState<'_> {
    /// Drive cursor blink + Kitty animations for this window: when the active
    /// tab's cursor blinks and is visible, toggle its phase on a fixed
    /// interval; while an animation plays, tick at the frame floor. Returns
    /// the next wake deadline, or `None` when the window can sleep until a
    /// real event (the router picks the earliest deadline across windows).
    fn tick(&mut self, now: Instant) -> Option<Instant> {
        const BLINK: Duration = Duration::from_millis(530);
        /// Kitty animation tick — the floor gap is 40ms, so ticking at it
        /// hits every frame boundary within a frame's tolerance.
        const ANIM: Duration = Duration::from_millis(40);
        // Kitty graphics animations: advance every visible pane's playing
        // images; a frame change repaints.
        let mut animating = false;
        if let Some(tab) = self.tabs.get(self.active) {
            for p in &tab.panes {
                let mut g = p.grid.lock();
                if g.advance_animations(now)
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
                animating |= g.kitty_images.iter().any(|i| i.playing);
            }
        }
        let blinking = self.pane().is_some_and(|p| {
            let g = p.grid.lock();
            g.cursor_blink && g.cursor_visible && g.view_offset == 0
        });
        // A live cursor trail needs frames until it fades (~150ms).
        if self.trail.is_some() {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return Some(now + ANIM);
        }
        // Dragging a selection past the pane's edge keeps scrolling on its
        // own timer as long as the pointer stays there, even with no new
        // `CursorMoved` events (the pointer held stationary past the edge).
        if self.drag_edge_autoscroll() {
            return Some(now + DRAG_SCROLL_INTERVAL);
        }
        if !blinking && !animating {
            self.cursor_blink_on = true;
            return None;
        }
        if blinking && now.duration_since(self.last_blink) >= BLINK {
            self.cursor_blink_on = !self.cursor_blink_on;
            self.last_blink = now;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        Some(if animating { now + ANIM } else { self.last_blink + BLINK })
    }

    /// Create this state's OS window + renderer if it doesn't have one yet.
    /// Returns whether the window is usable; `false` marks the state closed
    /// (the router drops it).
    fn ensure_window(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if self.window.is_some() {
            return true;
        }
        // `theme = "auto"`: seed from the OS appearance if winit can tell us
        // (falling back to dark); `ThemeChanged` keeps following it live.
        if self.config.theme_auto {
            self.theme = self.auto_theme(None);
            self.config.theme = self.theme;
        }
        // Size the font for the monitor this window is about to land on
        // *before* computing its pixel size below. `font_px`/`cell_w`/`cell_h`
        // were computed assuming scale factor 1.0 (`new_window_state` runs
        // before any OS window — and thus any monitor — exists); launching
        // directly on a HiDPI display without this would start with text at
        // half the intended visual size, and no `ScaleFactorChanged` event
        // ever fires to correct it if the window never changes monitors.
        let target_scale = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .map(|m| m.scale_factor())
            .unwrap_or(1.0);
        if (target_scale - self.scale_factor).abs() > 1e-9 {
            let ratio = target_scale / self.scale_factor;
            let px = ((self.font_px as f64) * ratio) as f32;
            let ligatures = self.config.ligatures.unwrap_or(true);
            self.rescale_font_cache(px, ligatures, target_scale);
        }
        let width = (self.cols as usize * self.cell_w + 2 * self.pad) as u32;
        // One extra cell row on top for the chrome bar (plus one at the bottom
        // for the status ribbon when it's on), plus the padding band.
        let bars = 1 + usize::from(self.status_enabled());
        let height = ((self.rows as usize + bars) * self.cell_h + 2 * self.pad) as u32;
        // `--title` (or a config `title` key) seeds the initial title; the
        // child's own OSC 0/2 still wins once it emits one (see the per-frame
        // `set_title` above).
        // Only request a transparent (alpha-capable) surface when opacity is
        // actually configured below 1.0 — an unnecessarily transparent
        // window can cost a compositor extra work for no visible benefit.
        let transparent = self.config.opacity.is_some_and(|o| o < 1.0);
        let attrs = Window::default_attributes()
            .with_title(self.config.title.as_deref().unwrap_or("rusty_term"))
            .with_decorations(false)
            .with_transparent(transparent)
            .with_inner_size(winit::dpi::PhysicalSize::new(width, height));
        // Keep the DWM drop shadow so the borderless window still reads as
        // raised above the desktop.
        #[cfg(target_os = "windows")]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_undecorated_shadow(true)
        };
        // `--maximized` / `--fullscreen` (or a `[window] launch_mode` config
        // key); unset leaves the normal windowed default. The quake window
        // instead docks to the top of the monitor: full monitor width, a
        // configured fraction of its height, and kept above other windows so
        // it drops over whatever has focus.
        let quake_geom = self.quake.then(|| {
            let mon = event_loop.primary_monitor().or_else(|| event_loop.available_monitors().next());
            let (mw, mh, pos) = mon
                .map(|m| (m.size().width, m.size().height, m.position()))
                .unwrap_or((1920, 1080, winit::dpi::PhysicalPosition::new(0, 0)));
            let frac = self.config.quake_height.unwrap_or(0.4).clamp(0.1, 1.0);
            (mw.max(1), ((mh as f32 * frac) as u32).max(self.cell_h as u32 * 2), pos)
        });
        let attrs = if let Some((w, h, pos)) = quake_geom {
            attrs
                .with_inner_size(winit::dpi::PhysicalSize::new(w, h))
                .with_position(pos)
                .with_window_level(winit::window::WindowLevel::AlwaysOnTop)
        } else {
            match self.config.launch_mode {
                Some(LaunchMode::Maximized) => attrs.with_maximized(true),
                Some(LaunchMode::Fullscreen) => {
                    attrs.with_fullscreen(Some(Fullscreen::Borderless(None)))
                }
                None => attrs,
            }
        };
        let Ok(window) = event_loop.create_window(attrs) else {
            self.closed = true;
            return false;
        };
        let window = Arc::new(window);
        apply_chrome(&window, &self.theme);
        self.window = Some(window.clone());
        // Let the OS deliver IME composition events (CJK, dead keys).
        window.set_ime_allowed(true);
        match self.make_renderer(window.clone()) {
            Some(r) => self.renderer = Some(r),
            None => {
                self.window = None;
                self.closed = true;
                return false;
            }
        }
        self.apply_opacity();
        window.request_redraw();
        true
    }

    /// This state's OS window id, once the window exists.
    fn window_id(&self) -> Option<WindowId> {
        self.window.as_ref().map(|w| w.id())
    }

    /// Whether pane `id` (a reader-thread wakeup tag) belongs to this window.
    fn has_pane(&self, id: u64) -> bool {
        self.tabs.iter().any(|t| t.panes.iter().any(|p| p.id == id))
    }

    /// Handle one winit event addressed to this window.
    fn on_window_event(&mut self, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.closed = true,
            WindowEvent::ThemeChanged(t) => {
                if self.config.theme_auto {
                    let new = self.auto_theme(Some(t));
                    self.apply_theme_live(new);
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                // Focus reporting (`?1004`): tell the focused pane's child it
                // gained/lost focus, if it asked. Redraw either way — the
                // cursor renders only while focused, and regaining focus
                // clears the active tab's attention badge (in `chrome_row`).
                self.focused = focused;
                if let Some(p) = self.pane_mut()
                    && p.grid.lock().focus_reporting
                {
                    let _ = p.writer.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => {
                // Paste the dropped file's shell-quoted path (plus a trailing
                // space, so several drops land as separate arguments), same
                // bracketed-paste handling as a clipboard paste.
                let quoted = format!("{} ", shell_quote(&path.to_string_lossy()));
                if let Some(p) = self.pane_mut() {
                    let bracketed = p.grid.lock().bracketed_paste;
                    let _ = p.writer.write(&encode_paste(&quoted, bracketed));
                }
                self.snap_to_bottom();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(size) => {
                self.apply_size(size.width, size.height);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.on_scale_factor_changed(scale_factor);
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.mods = mods.state();
                // Ctrl pressed/released with the pointer stationary should
                // show/hide the link affordance immediately, not wait for
                // the next `CursorMoved`.
                if self.update_hover_link()
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    // Key releases are terminal input only under win32-input-
                    // mode (?9001) or Kitty keyboard flag 2 (report event
                    // types); the UI layers (chords, search, copy mode) never
                    // see them.
                    let (app_cursor, kitty_flags, win32_input) = self
                        .pane()
                        .map(|p| {
                            let g = p.grid.lock();
                            (g.app_cursor_keys, g.kitty_keyboard_flags(), g.win32_input)
                        })
                        .unwrap_or_default();
                    if self.overlay.is_some() || self.searching.is_some() || self.copy_mode.is_some()
                    {
                        return;
                    }
                    if win32_input {
                        if let Some(bytes) = super::input::encode_win32(
                            event.physical_key,
                            &event.logical_key,
                            self.mods,
                            false,
                        ) {
                            self.write_child(&bytes);
                        }
                        return;
                    }
                    if kitty_flags & 2 != 0
                        && let Some(bytes) = super::input::encode_full(
                            &event.logical_key,
                            self.mods,
                            app_cursor,
                            kitty_flags,
                            super::input::KeyPhase::Release,
                            kitty_alternate(&event.logical_key, self.mods),
                            None,
                        )
                    {
                        self.write_child(&bytes);
                    }
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
                // Copy mode owns the keyboard while active.
                if self.copy_mode_key(&event) {
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
                        self.run_action(action);
                        return;
                    }
                }
                // Ctrl+Alt+1..9 jump straight to that tab (the Windows
                // Terminal convention; 9 = last tab, browser-style). After
                // the keymap lookup so a user chord still wins, and on the
                // Ctrl+Alt layer so plain Ctrl+digit stays encodable to
                // kitty-protocol apps.
                if self.mods.control_key() && self.mods.alt_key() && !self.mods.shift_key()
                    && let PhysicalKey::Code(code) = event.physical_key
                {
                    let digit = match code {
                        KeyCode::Digit1 => Some(0),
                        KeyCode::Digit2 => Some(1),
                        KeyCode::Digit3 => Some(2),
                        KeyCode::Digit4 => Some(3),
                        KeyCode::Digit5 => Some(4),
                        KeyCode::Digit6 => Some(5),
                        KeyCode::Digit7 => Some(6),
                        KeyCode::Digit8 => Some(7),
                        KeyCode::Digit9 => Some(usize::MAX), // last tab
                        _ => None,
                    };
                    if let Some(d) = digit {
                        let i = if d == usize::MAX { self.tabs.len().saturating_sub(1) } else { d };
                        self.select_tab(i);
                        return;
                    }
                }
                // While the IME is composing, it owns key input; don't also
                // encode it (the committed text arrives via `WindowEvent::Ime`).
                if self.pane().is_some_and(|p| !p.grid.lock().ime_preedit.is_empty()) {
                    return;
                }
                let (app_cursor, app_keypad, kitty_flags, win32_input) = self
                    .pane()
                    .map(|p| {
                        let g = p.grid.lock();
                        (g.app_cursor_keys, g.app_keypad, g.kitty_keyboard_flags(), g.win32_input)
                    })
                    .unwrap_or_default();
                // win32-input-mode (?9001) supersedes every VT encoding: the
                // child asked for raw key records, presses and releases alike.
                if win32_input {
                    if let Some(bytes) = super::input::encode_win32(
                        event.physical_key,
                        &event.logical_key,
                        self.mods,
                        true,
                    ) {
                        self.write_child(&bytes);
                        self.snap_to_bottom();
                    }
                    return;
                }
                // Application keypad mode (DECKPAM/DECNKM): a key physically
                // on the numpad encodes as its SS3 sequence; everything else
                // (mode off, modifiers held) falls through to normal encoding.
                let numpad = (event.location == KeyLocation::Numpad)
                    .then(|| super::input::encode_numpad(&event.logical_key, self.mods, app_keypad))
                    .flatten();
                let phase = if event.repeat {
                    super::input::KeyPhase::Repeat
                } else {
                    super::input::KeyPhase::Press
                };
                if let Some(bytes) = numpad.or_else(|| {
                    super::input::encode_full(
                        &event.logical_key,
                        self.mods,
                        app_cursor,
                        kitty_flags,
                        phase,
                        kitty_alternate(&event.logical_key, self.mods),
                        event.text.as_deref(),
                    )
                }) {
                    self.write_child(&bytes);
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
                    if let Some(p) = self.pane() {
                        p.grid.lock().ime_preedit.clear();
                    }
                    // The find bar owns composed text while it's open — mirror
                    // `search_key`'s `Key::Character` arm above instead of
                    // leaking the committed IME text into the background
                    // shell underneath it. An open overlay (settings/shell
                    // menu) or copy mode has no free-text field of its own,
                    // so committed text is simply dropped rather than
                    // reaching the child either.
                    match ime_commit_target(
                        self.searching.is_some(),
                        self.overlay.is_some(),
                        self.copy_mode.is_some(),
                    ) {
                        ImeCommitTarget::SearchBar => {
                            if let Some(q) = self.searching.as_mut() {
                                q.push_str(&text);
                            }
                            self.run_search();
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                        ImeCommitTarget::Pane => {
                            if let Some(p) = self.pane_mut() {
                                let _ = p.writer.write(text.as_bytes());
                            }
                            self.snap_to_bottom();
                        }
                        ImeCommitTarget::Dropped => {}
                    }
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
            WindowEvent::CursorLeft { .. } => {
                // Drop any chrome hover highlight when the pointer leaves.
                if self.hover.take().is_some()
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
                if self.hover_link.take().is_some()
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                // Tab drag-to-reorder: past the slop, the pressed tab trades
                // places as the pointer crosses its neighbors' midpoints. The
                // spans come from the painted hit map, so the drop math can't
                // disagree with what's on screen.
                if self.mouse_button_down == Some(MouseButtonKind::Left)
                    && self.tab_drag.is_some()
                    && !self.hits.is_empty()
                {
                    let (mut moving, press_x, from) = {
                        let d = self.tab_drag.as_ref().unwrap();
                        (d.moving, d.press_x, d.idx)
                    };
                    if !moving && (position.x - press_x).abs() > self.cell_w as f64 {
                        moving = true;
                    }
                    let mut to = from;
                    if moving {
                        let col =
                            (position.x.max(0.0) as usize / self.cell_w).min(self.hits.len() - 1);
                        to = tab_drag_target(col, &tab_spans(&self.hits), from);
                    }
                    if let Some(d) = &mut self.tab_drag {
                        d.moving = moving;
                        d.idx = to;
                    }
                    if to != from {
                        self.reorder_tab(from, to);
                    }
                }
                // Chrome-bar hover feedback: track the element under the
                // pointer and repaint when it changes (drag-strip cells and
                // anything below the bar count as no hover).
                let hover = if (position.y.max(0.0) as usize) < self.cell_h && !self.hits.is_empty() {
                    let col = (position.x.max(0.0) as usize / self.cell_w).min(self.hits.len() - 1);
                    match self.hits[col] {
                        Hit::Drag => None,
                        h => Some(h),
                    }
                } else {
                    None
                };
                if hover != self.hover {
                    self.hover = hover;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                // Settings-page hover: highlight the card under the pointer.
                if let Some((col, grid_row)) = self.overlay_cell(position.x, position.y) {
                    let grid_rows = self.rows as usize;
                    let grid_cols = self.cols as usize;
                    if let Some(Overlay::Settings(s)) = &mut self.overlay {
                        let hovered =
                            match settings_hit(col, grid_row, grid_cols, grid_rows, s.scroll, s.len()) {
                                SettingsHit::Row(i) => Some(i),
                                _ => None,
                            };
                        if hovered != s.hover {
                            s.hover = hovered;
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                    }
                }
                if self.update_hover_link()
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
                // The edge band shows a resize cursor; over pane content (below
                // the chrome bar) a Ctrl-hovered hyperlink shows a pointer, the
                // child's OSC 22 request wins if it made one, and plain grid
                // text otherwise gets the usual I-beam (any other text area);
                // everywhere else (the chrome bar itself) default.
                let icon = match self.resize_zone(position.x, position.y) {
                    Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => {
                        CursorIcon::NwseResize
                    }
                    Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => {
                        CursorIcon::NeswResize
                    }
                    Some(ResizeDirection::West | ResizeDirection::East) => CursorIcon::EwResize,
                    Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
                    // An open overlay (menu / settings) is a page of controls,
                    // not selectable text — the I-beam below would mislead.
                    None if self.overlay.is_some() => CursorIcon::Default,
                    None if (position.y.max(0.0) as usize) >= self.cell_h => {
                        if self.hover_link.is_some() {
                            CursorIcon::Pointer
                        } else {
                            self.pane()
                                .and_then(|p| {
                                    p.grid.lock().cursor_icon.as_deref().and_then(parse_cursor_icon)
                                })
                                .unwrap_or(CursorIcon::Text)
                        }
                    }
                    None => CursorIcon::Default,
                };
                if let Some(window) = &self.window {
                    window.set_cursor(icon);
                }
                if self.selecting
                    && self.update_drag_selection_head(position.x, position.y)
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
                // Motion reporting (`?1002` while a button is held, `?1003`
                // regardless) is independent of the local drag-selection
                // above — both can be active at once. `report_mouse` bypasses
                // itself when Shift is held, so a Shift-drag is pure local
                // selection with nothing forwarded to the app.
                let (sh, al, ct) =
                    (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                let dragging = self.mouse_button_down.is_some();
                let button = self.mouse_button_down.unwrap_or_default();
                self.report_mouse(self.focused_pane_id(), |c, r| {
                    MouseEvent::new_point(c, r)
                        .with_move(dragging)
                        .with_button_kind(button)
                        .with_modifiers(sh, al, ct)
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(kind) = (match button {
                    MouseButton::Left => Some(MouseButtonKind::Left),
                    MouseButton::Middle => Some(MouseButtonKind::Middle),
                    MouseButton::Right => Some(MouseButtonKind::Right),
                    _ => None, // no SGR encoding for side/other buttons
                }) else {
                    return;
                };
                match state {
                    ElementState::Pressed => {
                        // Ctrl+click follows an OSC 8 hyperlink under the
                        // pointer, suppressing selection and mouse reporting
                        // for that click. Left-button only: the resize/chrome/
                        // selection semantics in `on_left_press` are as well.
                        if kind == MouseButtonKind::Left {
                            if self.mods.control_key() && self.open_link_under_pointer() {
                                return;
                            }
                            self.on_left_press();
                        }
                        if kind == MouseButtonKind::Middle {
                            // Middle-click on the chrome bar closes the tab
                            // under the pointer (browser convention) instead
                            // of falling through to PRIMARY-paste, which
                            // previously happened regardless of y-position —
                            // the chrome bar was never even consulted.
                            if (self.mouse_pos.1.max(0.0) as usize) < self.cell_h {
                                self.on_bar_middle_click(self.mouse_pos.0);
                                return;
                            }
                            if self.searching.is_some() {
                                let text = self.primary_text();
                                self.paste_into_search(text);
                                return;
                            }
                            if self.mods.shift_key()
                                || !self.pane().is_some_and(|p| p.grid.lock().mouse_modes.active())
                            {
                                self.paste_primary();
                                return;
                            }
                        }
                        self.mouse_button_down = Some(kind);
                        let (sh, al, ct) =
                            (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                        self.report_mouse(self.focused_pane_id(), |c, r| {
                            MouseEvent::new_point(c, r)
                                .with_button(true)
                                .with_button_kind(kind)
                                .with_modifiers(sh, al, ct)
                        });
                    }
                    ElementState::Released => {
                        if kind == MouseButtonKind::Left {
                            if self.selecting {
                                self.copy_selection_primary(); // copy-on-select
                            }
                            self.selecting = false;
                            self.tab_drag = None; // a reorder drag ends with the button
                        }
                        if self.mouse_button_down == Some(kind) {
                            self.mouse_button_down = None;
                        }
                        let (sh, al, ct) =
                            (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                        self.report_mouse(self.focused_pane_id(), |c, r| {
                            MouseEvent::new_point(c, r)
                                .with_button(false)
                                .with_button_kind(kind)
                                .with_modifiers(sh, al, ct)
                        });
                    }
                }
            }
            // Wheel up browses into scrollback history, wheel down back toward
            // the live bottom. A notch is `WHEEL_LINES`; trackpads report pixels.
            WindowEvent::MouseWheel { delta, .. } => {
                // Accumulate fractional delta across events instead of
                // rounding each one independently — a high-resolution wheel
                // or a slow trackpad flick emits many sub-line events, and
                // rounding per-event dropped all of them (scrolling did
                // nothing until a large enough single delta arrived).
                let raw_lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * WHEEL_LINES as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / self.cell_h as f64,
                };
                let (lines, remainder) = accumulate_scroll_lines(self.scroll_accum, raw_lines);
                self.scroll_accum = remainder;
                if lines == 0 {
                    return;
                }
                // The settings overlay owns the wheel while it's up: scroll
                // its list (wheel-up = toward the top, like the scrollback).
                if let Some(Overlay::Settings(s)) = &mut self.overlay {
                    let visible = settings_visible(self.rows as usize);
                    s.scroll_by(-lines, visible);
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return;
                }
                if self.overlay.is_some() {
                    return; // the menu has no scroll; don't scroll the pane under it
                }
                // Unlike click/drag (which already refocus to the pane under
                // the pointer via `on_left_press`), the wheel never changes
                // focus — it targets whichever pane the pointer happens to
                // be over, tmux/iTerm2-style, without stealing focus from
                // whatever the user was typing into.
                let Some(id) = self.pane_under(self.mouse_pos.0, self.mouse_pos.1) else {
                    return;
                };
                let (sh, al, ct) =
                    (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                if self.report_mouse(Some(id), |c, r| {
                    MouseEvent::new_point(c, r).with_scroll(lines).with_modifiers(sh, al, ct)
                }) {
                    return;
                }
                if self.alt_scroll_active(id) {
                    self.send_alt_scroll_keys(id, lines);
                    return;
                }
                self.scroll_active(id, lines);
            }
            _ => {}
        }
    }

    /// New PTY output arrived on pane `id`: service its host-side requests
    /// (clipboard, notifications, alerts) and repaint if it's visible.
    fn on_pane_output(&mut self, id: u64) {
        self.service_clipboard(id);
        self.service_notifications(id);
        self.service_alerts(id);
        // A synchronized-output window on this pane suppresses the
        // repaint until it closes (or times out), so a multi-write
        // frame update never paints half-drawn.
        let syncing = self
            .pane_by_id_mut(id)
            .is_some_and(|p| p.grid.lock().sync_output_active());
        // Output on a background tab doesn't repaint; its bar label
        // refreshes with the next frame the active tab causes.
        if !syncing
            && self.tabs.get(self.active).is_some_and(|t| t.panes.iter().any(|p| p.id == id))
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}

/// The winit application: a set of top-level windows (C13), each a full
/// [`WindowState`], plus what's needed to open new ones at runtime. Events
/// route to the owning window by `WindowId` (window events) or pane id
/// (reader-thread wakeups); the loop exits when the last window closes.
struct App<'a> {
    backend: &'a dyn Backend,
    /// The launch config, template for every new window.
    config: Config,
    config_path: Option<std::path::PathBuf>,
    /// The `--profile <name>` this instance was launched with, if any; copied
    /// into every [`WindowState`] (see its field doc).
    profile_override: Option<String>,
    proxy: EventLoopProxy<UserEvent>,
    /// Shared pane-id source (see [`WindowState::next_id`]).
    next_id: std::rc::Rc<std::cell::Cell<u64>>,
    shells: Vec<crate::shells::DetectedShell>,
    windows: Vec<WindowState<'a>>,
    /// The window that last had OS focus — control-socket commands without a
    /// window of their own (`new-tab`, `send-text`, …) act on it.
    focused_window: Option<WindowId>,
    /// Set by the Windows global-hotkey message hook (G30 stretch) on every
    /// press; checked and cleared in [`ApplicationHandler::about_to_wait`].
    /// Always `None` off Windows or when no `quake_hotkey` is configured.
    quake_hotkey_pressed: Option<std::rc::Rc<std::cell::Cell<bool>>>,
}

impl<'a> App<'a> {
    /// Build a [`WindowState`] from the launch config (its OS window comes
    /// later via `ensure_window`). Each window owns its font, clipboard, and
    /// config copy so per-window settings changes stay per-window.
    fn new_window_state(&self, quake: bool) -> Result<WindowState<'a>, String> {
        let font_px = self.config.font_size.unwrap_or(FONT_PX);
        let font_set = font::load_set(
            self.config.font.as_deref(),
            self.config.font_bold.as_deref(),
            self.config.font_italic.as_deref(),
            self.config.font_bold_italic.as_deref(),
            self.config.font_fallback.as_deref(),
        )
        .ok_or("no monospace font found")?;
        let ligatures = self.config.ligatures.unwrap_or(true);
        let font = FontCache::new(font_set, font_px, ligatures).ok_or("font failed to parse")?;
        let (cell_w, cell_h) = font.cell_size();
        Ok(WindowState {
            backend: self.backend,
            config: self.config.clone(),
            config_path: self.config_path.clone(),
            profile_override: self.profile_override.clone(),
            tabs: Vec::new(),
            active: 0,
            next_id: self.next_id.clone(),
            proxy: self.proxy.clone(),
            font,
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            pad: self.config.padding.unwrap_or(DEFAULT_PAD) as usize,
            window: None,
            renderer: None,
            mods: ModifiersState::empty(),
            cols: self.config.cols.unwrap_or(INIT_COLS),
            rows: self.config.rows.unwrap_or(INIT_ROWS),
            theme: self.config.theme,
            clipboard: arboard::Clipboard::new().ok(),
            mouse_pos: (0.0, 0.0),
            scroll_accum: 0.0,
            selecting: false,
            hover: None,
            hover_link: None,
            focused: true,
            search_regex: false,
            search_case_sensitive: false,
            broadcast: false,
            copy_mode: None,
            last_grid_click: None,
            click_streak: 0,
            mouse_button_down: None,
            sel_anchor: None,
            hits: Vec::new(),
            tab_drag: None,
            last_strip_click: None,
            cursor_blink_on: true,
            last_blink: Instant::now(),
            searching: None,
            shells: self.shells.clone(),
            overlay: None,
            font_px,
            // Updated to the real value once the OS window exists (see
            // `ensure_window`); 1.0 is the correct assumption until then.
            scale_factor: 1.0,
            closed: false,
            wants_new_window: false,
            cursor_prev: None,
            trail: None,
            quake,
            git_branch: None,
            dock_open: false,
            dock_items: Vec::new(),
        })
    }

    /// Open a new top-level window at runtime: state, first tab, OS window.
    /// `spawn` seeds the first tab (shell/cwd/theme); errors are reported,
    /// not fatal — the existing windows keep running.
    fn spawn_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        quake: bool,
        shell: Option<String>,
        cwd: Option<std::path::PathBuf>,
        theme: Option<Theme>,
    ) -> Result<(), String> {
        let mut ws = self.new_window_state(quake)?;
        ws.spawn_tab_opts(shell, &[], cwd, theme).map_err(|e| format!("spawn shell: {e}"))?;
        if !ws.ensure_window(event_loop) {
            return Err("could not create a window".into());
        }
        self.windows.push(ws);
        Ok(())
    }

    /// The window that should act on window-less control commands: the
    /// focused one, else the first non-quake one, else any.
    fn control_target(&mut self) -> Option<&mut WindowState<'a>> {
        let focused = self.focused_window;
        if let Some(i) = self.windows.iter().position(|w| w.window_id() == focused && focused.is_some()) {
            return self.windows.get_mut(i);
        }
        if let Some(i) = self.windows.iter().position(|w| !w.quake) {
            return self.windows.get_mut(i);
        }
        self.windows.first_mut()
    }

    /// Toggle the quake window (G30): create it on first use, otherwise flip
    /// its visibility, focusing it when shown.
    fn toggle_quake(&mut self, event_loop: &ActiveEventLoop) -> String {
        if let Some(ws) = self.windows.iter_mut().find(|w| w.quake) {
            if let Some(window) = &ws.window {
                let visible = window.is_visible().unwrap_or(true);
                window.set_visible(!visible);
                if !visible {
                    window.focus_window();
                    window.request_redraw();
                }
            }
            return "ok\n".to_string();
        }
        match self.spawn_window(event_loop, true, None, None, None) {
            Ok(()) => "ok\n".to_string(),
            Err(e) => format!("err {e}\n"),
        }
    }

    /// Drop windows marked closed and open ones requested via
    /// `Action::NewWindow`; exits the loop when no window remains.
    fn reap_and_spawn(&mut self, event_loop: &ActiveEventLoop) {
        let mut open = 0usize;
        for i in 0..self.windows.len() {
            if std::mem::take(&mut self.windows[i].wants_new_window) {
                open += 1;
            }
        }
        for _ in 0..open {
            let cwd = self.control_target().and_then(|w| w.focused_pane_cwd());
            if let Err(e) = self.spawn_window(event_loop, false, None, cwd, None) {
                eprintln!("rusty_term: new window: {e}");
            }
        }
        self.windows.retain(|w| !w.closed);
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for App<'_> {
    /// Drive cursor blink and Kitty animations across every window, waking at
    /// the earliest deadline any of them needs (no idle wakeups otherwise).
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(pressed) = &self.quake_hotkey_pressed
            && pressed.replace(false)
        {
            self.toggle_quake(event_loop);
        }
        let now = Instant::now();
        let next = self.windows.iter_mut().filter_map(|w| w.tick(now)).min();
        event_loop.set_control_flow(match next {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        for w in &mut self.windows {
            w.ensure_window(event_loop);
        }
        self.reap_and_spawn(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let WindowEvent::Focused(gained) = event {
            if gained {
                self.focused_window = Some(id);
            } else if self.focused_window == Some(id) {
                self.focused_window = None;
            }
        }
        if let Some(w) = self.windows.iter_mut().find(|w| w.window_id() == Some(id)) {
            w.on_window_event(event);
        }
        self.reap_and_spawn(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw(id) => {
                if let Some(w) = self.windows.iter_mut().find(|w| w.has_pane(id)) {
                    w.on_pane_output(id);
                }
            }
            UserEvent::Exit(id) => {
                if let Some(w) = self.windows.iter_mut().find(|w| w.has_pane(id)) {
                    w.close_pane(id);
                }
            }
            UserEvent::ConfigChanged => {
                for w in &mut self.windows {
                    w.reload_config();
                }
            }
            UserEvent::Control(cmd, reply) => {
                use super::control::CtlCommand;
                // Window-level commands are the router's; everything else
                // acts on the focused window.
                let text = match cmd {
                    CtlCommand::NewWindow { cwd, profile, shell } => {
                        let p = profile
                            .as_deref()
                            .and_then(|n| self.config.profile(n))
                            .cloned();
                        if profile.is_some() && p.is_none() {
                            format!("err no profile named `{}`\n", profile.unwrap_or_default())
                        } else {
                            let p = p.unwrap_or_default();
                            match self.spawn_window(
                                event_loop,
                                false,
                                shell.or(p.shell),
                                cwd.or(p.cwd),
                                p.theme,
                            ) {
                                Ok(()) => "ok\n".to_string(),
                                Err(e) => format!("err {e}\n"),
                            }
                        }
                    }
                    CtlCommand::Quake => self.toggle_quake(event_loop),
                    cmd => match self.control_target() {
                        Some(w) => w.handle_control(cmd),
                        None => "err no window\n".to_string(),
                    },
                };
                let _ = reply.send(text);
            }
        }
        self.reap_and_spawn(event_loop);
    }
}

/// Encode clipboard `text` for the child: normalize line endings to CR, and
/// when `bracketed` wrap it in `ESC[200~`/`ESC[201~`, stripping any embedded
/// end marker first so the payload can't close the bracket early (a
/// paste-injection guard). Without bracketed-paste support to tell the child
/// "this is literal data, don't interpret it", C0/DEL control bytes are
/// stripped instead — otherwise a paste containing a hidden `ESC` (trivial to
/// embed in text copied from a web page, and invisible in the source) could
/// smuggle an arbitrary escape sequence into the child's input, or a hidden
/// bell/backspace could do something surprising at the prompt. `\t` and the
/// `\r` newlines were just normalized to are kept — running each pasted line
/// as its own command is the expected, visible behavior for an app with no
/// bracketed-paste support, not something this needs to guard against.
fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // Strip until no marker remains: a single pass is bypassable, since
        // removing one occurrence can splice a new one together (e.g.
        // `ESC[2` + `ESC[201~` + `01~` re-forms the end marker).
        let mut text = text;
        while text.contains("\x1b[201~") {
            text = text.replace("\x1b[201~", "");
        }
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.bytes().filter(|&b| b == b'\t' || b == b'\r' || !b.is_ascii_control()).collect()
    }
}

/// The Kitty flag-4 "alternate key": the shifted form of a text key while
/// Shift is held (winit's logical key already has Shift applied), used as
/// the `code:shifted` sub-parameter. `None` for named keys or unshifted
/// presses.
fn kitty_alternate(key: &winit::keyboard::Key, mods: ModifiersState) -> Option<char> {
    if !mods.shift_key() {
        return None;
    }
    match key {
        winit::keyboard::Key::Character(s) => s.chars().next(),
        _ => None,
    }
}

/// The DECCKM-aware arrow-key byte sequence that moves a readline cursor by
/// `(dx, dy)` cells: vertical first (multiline editing), then horizontal,
/// capped so a pathological distance can't flood the child.
fn arrow_presses(dx: isize, dy: isize, app_cursor: bool) -> Vec<u8> {
    let prefix: &[u8] = if app_cursor { b"\x1bO" } else { b"\x1b[" };
    let mut out = Vec::new();
    let mut push = |n: isize, pos: u8, neg: u8| {
        let final_byte = if n >= 0 { pos } else { neg };
        for _ in 0..n.unsigned_abs().min(400) {
            out.extend_from_slice(prefix);
            out.push(final_byte);
        }
    };
    push(dy, b'B', b'A'); // Down / Up
    push(dx, b'C', b'D'); // Right / Left
    out
}

/// The pointer's position in pixels relative to a pane's pixel origin,
/// clamped into the pane (`0..size-1` per axis) so a pointer over the chrome
/// or a divider never reports an out-of-pane position. 0-based; the SGR
/// encoder adds the protocol's 1-basing.
/// Where committed IME text should go, given which UI layer currently owns
/// keyboard input. Pulled out of the `Ime::Commit` handler as a pure
/// decision so the routing (find bar > pane, overlay/copy-mode swallow it)
/// is unit-testable without a live `WindowState`.
#[derive(Debug, PartialEq, Eq)]
enum ImeCommitTarget {
    /// The find bar is open: append to its query string.
    SearchBar,
    /// Nothing else owns the keyboard: forward to the focused pane's child.
    Pane,
    /// An overlay (settings/shell menu) or copy mode owns the keyboard and
    /// has no free-text field of its own — the committed text is dropped
    /// rather than leaking into the background shell underneath it.
    Dropped,
}

fn ime_commit_target(searching: bool, overlay: bool, copy_mode: bool) -> ImeCommitTarget {
    if searching {
        ImeCommitTarget::SearchBar
    } else if overlay || copy_mode {
        ImeCommitTarget::Dropped
    } else {
        ImeCommitTarget::Pane
    }
}

/// The IME candidate-popup origin, in physical pixels, for a cursor at
/// pane-local `(col, row)` within the pane whose rect starts at
/// `pane_origin` (in cells) — pulled out of `update_ime_area` so the split
/// offset math is unit-testable without a live `winit::Window`.
fn ime_cursor_area_origin(
    pad: usize,
    oy: usize,
    cell_w: usize,
    cell_h: usize,
    pane_origin: (usize, usize),
    cursor: (usize, usize),
) -> (f64, f64) {
    let (base_col, base_row) = pane_origin;
    let (col, row) = cursor;
    let x = (pad + (base_col + col) * cell_w) as f64;
    // +1 cell row for the chrome bar above the grid, plus the grid's own
    // vertical offset (padding band + the bar's inset).
    let y = (oy + (base_row + row + 1) * cell_h) as f64;
    (x, y)
}

/// Which of `total` tabs are shown in the chrome bar right now: `[start,
/// end)` indices, sized to fit within `available_cells` (the tab strip's
/// pixel-cell budget, before the overflow indicator / `+`/`▾`/caption
/// buttons) and always including `active` — recomputed fresh each frame from
/// the current active tab (no persisted scroll state needed), so clicking
/// or cycling to an off-screen tab scrolls the minimal amount to reveal it,
/// instead of the strip always starting at tab 0 and silently truncating
/// the rest with no way to reach or even see the truncated ones.
/// The visible tabs' strip spans, recovered from a laid-out chrome hit map:
/// each entry is `(tab index, start col, end col)` (end exclusive), a tab's
/// label and close-button cells merged. Drag-to-reorder consumes these, so
/// the drop math always agrees with what was actually painted.
fn tab_spans(hits: &[Hit]) -> Vec<(usize, usize, usize)> {
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (c, h) in hits.iter().enumerate() {
        let i = match h {
            Hit::Tab(i) | Hit::CloseTab(i) => *i,
            _ => continue,
        };
        match spans.last_mut() {
            Some((li, _, end)) if *li == i && *end == c => *end = c + 1,
            _ => spans.push((i, c, c + 1)),
        }
    }
    spans
}

/// Where a dragged tab should sit for pointer column `col`: the position of
/// the nearest span (moving out from the dragged tab's own) whose *midpoint*
/// the pointer has crossed, returned as the tab index currently holding that
/// position. Midpoint-based so unequal adaptive widths can't oscillate — a
/// swap only fires once the pointer is past the point where swapping back
/// would immediately retrigger. `from` when nothing was crossed.
fn tab_drag_target(col: usize, spans: &[(usize, usize, usize)], from: usize) -> usize {
    let Some(pos) = spans.iter().position(|&(i, _, _)| i == from) else {
        return from;
    };
    let mid = |s: &(usize, usize, usize)| (s.1 + s.2) / 2;
    // Leftmost span left of the drag whose midpoint the pointer sits left of…
    for s in spans.iter().take(pos) {
        if col < mid(s) {
            return s.0;
        }
    }
    // …else the rightmost span right of it whose midpoint the pointer passed.
    for s in spans.iter().skip(pos + 1).rev() {
        if col > mid(s) {
            return s.0;
        }
    }
    from
}

/// Adaptive tab widths: each tab wants `desired` cells (its label plus the
/// close button), clamped to `[TAB_MIN, TAB_CELLS]`. When the clamped sum
/// (plus the 1-cell gaps) overflows `strip`, every tab shares the largest
/// uniform cap that fits, down to `TAB_MIN`; wanting less than the cap keeps
/// costing less (short titles don't pad out to the cap). Returns `None` when
/// even `TAB_MIN`-wide tabs overflow — the caller falls back to the scrolled
/// uniform strip.
fn tab_widths(desired: &[usize], strip: usize) -> Option<Vec<usize>> {
    if desired.is_empty() {
        return Some(Vec::new());
    }
    let gaps = desired.len() - 1;
    let fits = |cap: usize| {
        desired.iter().map(|&d| d.clamp(TAB_MIN, cap)).sum::<usize>() + gaps <= strip
    };
    if !fits(TAB_MIN) {
        return None;
    }
    // Largest cap that still fits; the range is small, so a scan reads
    // clearer than a binary search.
    let cap = (TAB_MIN..=TAB_CELLS).rev().find(|&c| fits(c)).unwrap_or(TAB_MIN);
    Some(desired.iter().map(|&d| d.clamp(TAB_MIN, cap)).collect())
}

fn visible_tab_range(active: usize, total: usize, available_cells: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let per_tab = TAB_CELLS + 1; // the tab's own width plus its separator
    let capacity = (available_cells / per_tab).max(1).min(total);
    if total <= capacity {
        return (0, total);
    }
    let max_start = total - capacity;
    let start = active.saturating_sub(capacity - 1).min(active).min(max_start);
    (start, start + capacity)
}

/// Whether a drag-selection past a pane's `[top_px, bottom_px]` edge should
/// auto-scroll: `Some(true)` above the top margin, `Some(false)` below the
/// bottom margin, `None` within the pane (plus its margin) so the caller
/// does nothing. Pulled out of `drag_edge_autoscroll` for unit testing
/// without a live `WindowState`.
fn drag_scroll_direction(y: f64, top_px: f64, bottom_px: f64, margin: f64) -> Option<bool> {
    if y < top_px - margin {
        Some(true)
    } else if y > bottom_px + margin {
        Some(false)
    } else {
        None
    }
}

fn pane_pixel_point(pos: (f64, f64), origin: (usize, usize), size: (usize, usize)) -> MousePoint {
    let clamp = |v: f64, o: usize, s: usize| {
        ((v.max(0.0) as usize).saturating_sub(o)).min(s.saturating_sub(1))
    };
    MousePoint { col: clamp(pos.0, origin.0, size.0), row: clamp(pos.1, origin.1, size.1) }
}

/// Quote `path` for pasting into a shell command line (a drag-and-dropped
/// file). Unix: single quotes, with embedded `'` rewritten to `'\''` — safe
/// for every byte a path can contain. Windows: double quotes when the path
/// needs them (paths can't contain `"` there, so no escaping is required).
fn shell_quote(path: &str) -> String {
    #[cfg(windows)]
    {
        if path.is_empty() || path.contains(' ') {
            return format!("\"{path}\"");
        }
        path.to_string()
    }
    #[cfg(not(windows))]
    {
        format!("'{}'", path.replace('\'', "'\\''"))
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

/// The filesystem path out of an OSC 7 `file://[host]/path` URI, percent-
/// decoded to raw bytes (so non-UTF8 path components round-trip on Unix).
/// The host component (if any) is ignored — OSC 7 only ever reports the
/// *local* shell's cwd, so a remote hostname there is not actionable. `None`
/// for anything that isn't a `file://` URI with a path.
fn path_from_file_uri(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut decoded: Vec<u8> = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    if decoded.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&decoded)))
    }
    #[cfg(not(unix))]
    {
        Some(std::path::PathBuf::from(String::from_utf8_lossy(&decoded).into_owned()))
    }
}

/// Map an OSC 22-requested pointer shape — a CSS `cursor` keyword, the
/// convention Kitty and others use — to a winit cursor icon. `None` for a
/// name we don't recognize (the child's own concern; we just fall back to
/// the default arrow rather than guessing).
fn parse_cursor_icon(name: &str) -> Option<CursorIcon> {
    Some(match name {
        "default" => CursorIcon::Default,
        "pointer" => CursorIcon::Pointer,
        "text" => CursorIcon::Text,
        "wait" => CursorIcon::Wait,
        "progress" => CursorIcon::Progress,
        "crosshair" => CursorIcon::Crosshair,
        "move" => CursorIcon::Move,
        "not-allowed" => CursorIcon::NotAllowed,
        "grab" => CursorIcon::Grab,
        "grabbing" => CursorIcon::Grabbing,
        "help" => CursorIcon::Help,
        "cell" => CursorIcon::Cell,
        "copy" => CursorIcon::Copy,
        "alias" => CursorIcon::Alias,
        "context-menu" => CursorIcon::ContextMenu,
        "vertical-text" => CursorIcon::VerticalText,
        "no-drop" => CursorIcon::NoDrop,
        "all-scroll" => CursorIcon::AllScroll,
        "zoom-in" => CursorIcon::ZoomIn,
        "zoom-out" => CursorIcon::ZoomOut,
        "col-resize" => CursorIcon::ColResize,
        "row-resize" => CursorIcon::RowResize,
        _ => return None,
    })
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
        KeyCode::ArrowLeft => Key::Left,
        KeyCode::ArrowRight => Key::Right,
        KeyCode::ArrowUp => Key::Up,
        KeyCode::ArrowDown => Key::Down,
        KeyCode::Space => Key::Space,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Enter => Key::Enter,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::Escape => Key::Escape,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::F1 => Key::F1,
        KeyCode::F2 => Key::F2,
        KeyCode::F3 => Key::F3,
        KeyCode::F4 => Key::F4,
        KeyCode::F5 => Key::F5,
        KeyCode::F6 => Key::F6,
        KeyCode::F7 => Key::F7,
        KeyCode::F8 => Key::F8,
        KeyCode::F9 => Key::F9,
        KeyCode::F10 => Key::F10,
        KeyCode::F11 => Key::F11,
        KeyCode::F12 => Key::F12,
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
fn osc52_reply(sel: char, text: &str) -> Vec<u8> {
    let mut out = Vec::from(&b"\x1b]52;"[..]);
    out.push(sel as u8);
    out.push(b';');
    out.extend_from_slice(crate::core::base64_encode(text.as_bytes()).as_bytes());
    out.push(0x07);
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Hit, ImeCommitTarget, MenuKind, OVERLAY_ITEMS_TOP, SETTINGS_CARDS_TOP, SETTINGS_FOOTER_H,
        SETTINGS_ROW_H, SETTINGS_SIDEBAR_W, SettingsHit, Widget, accumulate_scroll_lines,
        SETTINGS_SEARCH_W, arrow_presses, drag_scroll_direction, encode_paste,
        ime_commit_target, ime_cursor_area_origin, is_openable_url, mix, osc52_reply,
        display_path, fmt_runtime, pane_pixel_point, path_from_file_uri, put_text,
        read_git_branch, settings_hit, settings_value_end,
        settings_visible, shell_menu_items, tab_drag_target, tab_spans, tab_widths,
        visible_tab_range, widget_text,
    };
    #[cfg(not(windows))]
    use super::shell_quote;
    use crate::core::Cell;

    #[test]
    fn settings_visible_scales_with_the_window_and_never_underflows() {
        // 24 grid rows: 1 top pad + 2 title/rule + 2 footer leaves 19, at
        // 3 rows per card = 6.
        assert_eq!(settings_visible(24), 6);
        // A taller window fits another card.
        assert_eq!(settings_visible(26), 7);
        // A tiny window still returns 0 rather than wrapping.
        assert_eq!(settings_visible(0), 0);
        assert_eq!(settings_visible(SETTINGS_CARDS_TOP + SETTINGS_FOOTER_H), 0);
    }

    #[test]
    fn settings_value_end_caps_the_card_column_on_a_wide_window() {
        // Narrow window: the right pad governs.
        assert_eq!(settings_value_end(80), 77);
        // Wide window: the card-width cap governs, so the value stays near
        // its label instead of drifting to the window edge.
        assert_eq!(settings_value_end(240), SETTINGS_SIDEBAR_W + 2 + 80);
    }

    #[test]
    fn settings_hit_maps_sidebar_cards_search_and_dead_space() {
        let (cols, rows) = (100, 24); // 6 visible cards (see above)
        // The top-pad row is dead everywhere.
        assert_eq!(settings_hit(2, 0, cols, rows, 0, 7), SettingsHit::None);
        // Sidebar: categories every other row, with the gap row belonging to
        // the entry above it — the whole strip is clickable.
        let t = super::SETTINGS_TOP;
        assert_eq!(settings_hit(2, t, cols, rows, 0, 7), SettingsHit::Category(0));
        assert_eq!(settings_hit(2, t + 1, cols, rows, 0, 7), SettingsHit::Category(0));
        assert_eq!(settings_hit(0, t + 2, cols, rows, 0, 7), SettingsHit::Category(1));
        assert_eq!(settings_hit(2, t + 4, cols, rows, 0, 7), SettingsHit::Category(2));
        // Below the last category the strip is dead.
        assert_eq!(settings_hit(2, t + 6, cols, rows, 0, 7), SettingsHit::None);
        // The title row's right end is the search affordance; its left is dead.
        let value_end = settings_value_end(cols);
        assert_eq!(settings_hit(value_end - 1, t, cols, rows, 0, 7), SettingsHit::Search);
        assert_eq!(
            settings_hit(value_end - SETTINGS_SEARCH_W, t, cols, rows, 0, 7),
            SettingsHit::Search,
        );
        assert_eq!(settings_hit(SETTINGS_SIDEBAR_W + 4, t, cols, rows, 0, 7), SettingsHit::None);
        // Card area starts below the title + rule; the rule row and the
        // spacer between cards are dead.
        let x = SETTINGS_SIDEBAR_W + 4;
        assert_eq!(settings_hit(x, t + 1, cols, rows, 0, 7), SettingsHit::None);
        assert_eq!(settings_hit(x, SETTINGS_CARDS_TOP, cols, rows, 0, 7), SettingsHit::Row(0));
        assert_eq!(settings_hit(x, SETTINGS_CARDS_TOP + 1, cols, rows, 0, 7), SettingsHit::Row(0));
        assert_eq!(settings_hit(x, SETTINGS_CARDS_TOP + 2, cols, rows, 0, 7), SettingsHit::None);
        assert_eq!(
            settings_hit(x, SETTINGS_CARDS_TOP + SETTINGS_ROW_H, cols, rows, 0, 7),
            SettingsHit::Row(1),
        );
        // Scroll offsets the list index the same card resolves to.
        assert_eq!(settings_hit(x, SETTINGS_CARDS_TOP, cols, rows, 3, 7), SettingsHit::Row(3));
        // Past the end of a short list: dead.
        assert_eq!(
            settings_hit(x, SETTINGS_CARDS_TOP + SETTINGS_ROW_H, cols, rows, 0, 1),
            SettingsHit::None,
        );
        // Footer rows never hit.
        assert_eq!(settings_hit(x, rows - 1, cols, rows, 0, 7), SettingsHit::None);
    }

    #[test]
    fn widget_text_grows_cycle_arrows_only_when_selected() {
        let choice = Widget::Choice("dracula".into());
        assert_eq!(widget_text(&choice, false), "dracula");
        assert_eq!(widget_text(&choice, true), "< dracula >");
        // Toggles render the same switch either way — selection flips, it
        // doesn't cycle.
        let on = Widget::Toggle(true);
        assert_eq!(widget_text(&on, false), widget_text(&on, true));
        assert!(widget_text(&on, true).ends_with("on"));
        assert!(widget_text(&Widget::Toggle(false), true).ends_with("off"));
    }

    #[test]
    fn scroll_accumulator_carries_sub_line_deltas_instead_of_dropping_them() {
        // Five events at 0.3 lines each used to round to 0 individually and
        // scroll nothing; accumulated, they must eventually emit whole lines
        // and the running remainder must never lose or invent a line.
        let mut accum = 0.0;
        let mut total_lines = 0isize;
        for _ in 0..5 {
            let (lines, next) = accumulate_scroll_lines(accum, 0.3);
            accum = next;
            total_lines += lines;
        }
        assert_eq!(total_lines, 1, "5 * 0.3 == 1.5, so exactly one whole line has fired so far");
        assert!((accum - 0.5).abs() < 1e-9, "the remaining 0.5 must still be carried: {accum}");
    }

    #[test]
    fn scroll_accumulator_handles_a_single_large_delta_and_negative_direction() {
        assert_eq!(accumulate_scroll_lines(0.0, 3.0), (3, 0.0));
        let (lines, remainder) = accumulate_scroll_lines(0.0, -0.7);
        assert_eq!(lines, 0);
        assert!((remainder + 0.7).abs() < 1e-9);
        let (lines, remainder) = accumulate_scroll_lines(remainder, -0.7);
        assert_eq!(lines, -1);
        assert!((remainder + 0.4).abs() < 1e-9);
    }

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
    fn bracketed_paste_strips_spliced_end_marker() {
        // Removing one marker can splice a new one together; the sanitizer
        // must iterate until none remain or the bracket closes early.
        let out = encode_paste("\x1b[2\x1b[201~01~rm -rf ~", true);
        let body = &out[6..out.len() - 6];
        assert!(
            !body.windows(6).any(|w| w == b"\x1b[201~"),
            "end marker survived sanitization: {:?}",
            String::from_utf8_lossy(body)
        );
    }

    #[test]
    fn bracketed_paste_wraps_plain_text() {
        assert_eq!(encode_paste("ls", true), b"\x1b[200~ls\x1b[201~");
    }

    #[test]
    fn unbracketed_paste_strips_hidden_control_bytes() {
        // With no bracketed-paste support, the child interprets pasted text
        // as if it were typed — a hidden ESC (trivially embedded in text
        // copied from a web page, and invisible in the source) could smuggle
        // an arbitrary escape sequence, and a hidden BEL/backspace could do
        // something surprising at the prompt. Both must be stripped.
        assert_eq!(encode_paste("a\x1bb", false), b"ab");
        assert_eq!(encode_paste("rm\x07 -rf ~", false), b"rm -rf ~");
        assert_eq!(encode_paste("a\x08b", false), b"ab"); // backspace
        assert_eq!(encode_paste("a\x7fb", false), b"ab"); // DEL
    }

    #[test]
    fn unbracketed_paste_keeps_tabs_and_newlines() {
        // Newlines (normalized to CR) running each pasted line as its own
        // command is the expected, visible behavior without bracketed-paste
        // support — not something the control-byte filter should touch.
        assert_eq!(encode_paste("a\tb\nc\r\nd", false), b"a\tb\rc\rd");
    }

    #[test]
    fn unbracketed_paste_preserves_multibyte_utf8() {
        // UTF-8 continuation/lead bytes (>= 0x80) are never mistaken for the
        // ASCII control bytes the filter strips.
        assert_eq!(encode_paste("héllo→wörld", false), "héllo→wörld".as_bytes());
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
    fn path_from_file_uri_strips_scheme_and_host() {
        assert_eq!(
            path_from_file_uri("file:///home/user/project"),
            Some(std::path::PathBuf::from("/home/user/project"))
        );
        assert_eq!(
            path_from_file_uri("file://myhost/home/user/project"),
            Some(std::path::PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn path_from_file_uri_percent_decodes() {
        assert_eq!(
            path_from_file_uri("file:///home/user/my%20project"),
            Some(std::path::PathBuf::from("/home/user/my project"))
        );
    }

    #[test]
    fn path_from_file_uri_rejects_non_file_or_empty() {
        assert_eq!(path_from_file_uri("https://example.com/path"), None);
        assert_eq!(path_from_file_uri(""), None);
        assert_eq!(path_from_file_uri("file://"), None);
    }

    #[test]
    fn osc52_reply_wraps_base64() {
        // "hi" -> base64 "aGk=", framed as an OSC 52 clipboard reply (BEL).
        assert_eq!(osc52_reply('c', "hi"), b"\x1b]52;c;aGk=\x07");
        assert_eq!(osc52_reply('p', "hi"), b"\x1b]52;p;aGk=\x07");
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
        let profiles = vec![crate::config::Profile {
            name: "dev".into(),
            ..Default::default()
        }];
        let with_profiles = shell_menu_items(&profiles, &shells);
        assert_eq!(with_profiles[0].label, "Profile: dev");
        assert!(matches!(with_profiles[0].kind, MenuKind::LaunchProfile(0)));
        let items = shell_menu_items(&[], &shells);
        assert_eq!(items.len(), 4, "2 shells + Settings + config file");
        assert!(matches!(items[0].kind, MenuKind::LaunchShell(0)));
        assert!(matches!(items[1].kind, MenuKind::LaunchShell(1)));
        assert!(matches!(items[2].kind, MenuKind::Settings));
        assert!(matches!(items[3].kind, MenuKind::EditConfig));
    }

    #[test]
    fn arrow_presses_encode_decckm_aware_sequences() {
        assert_eq!(arrow_presses(2, 0, false), b"\x1b[C\x1b[C");
        assert_eq!(arrow_presses(-1, 1, false), b"\x1b[B\x1b[D");
        assert_eq!(arrow_presses(0, -2, true), b"\x1bOA\x1bOA");
        // Capped so a pathological delta can't flood the child.
        assert_eq!(arrow_presses(10_000, 0, false).len(), 400 * 3);
    }

    #[test]
    fn pane_pixel_point_offsets_and_clamps() {
        // Pointer at window (25, 40), pane origin (10, 16), pane 100x80 px.
        let p = pane_pixel_point((25.0, 40.0), (10, 16), (100, 80));
        assert_eq!((p.col, p.row), (15, 24));
        // Above/left of the pane clamps to 0; beyond it clamps to size-1.
        let p = pane_pixel_point((3.0, 3.0), (10, 16), (100, 80));
        assert_eq!((p.col, p.row), (0, 0));
        let p = pane_pixel_point((500.0, 500.0), (10, 16), (100, 80));
        assert_eq!((p.col, p.row), (99, 79));
    }

    #[test]
    fn ime_cursor_area_origin_is_window_relative_for_an_unsplit_pane() {
        // Unsplit pane origin (0, 0): matches the pre-fix single-pane math
        // exactly, including the chrome-bar +1 row and the padding band.
        let (x, y) = ime_cursor_area_origin(4, 4, 10, 20, (0, 0), (3, 2));
        assert_eq!((x, y), ((4 + 3 * 10) as f64, (4 + (2 + 1) * 20) as f64));
    }

    #[test]
    fn ime_cursor_area_origin_adds_the_split_panes_own_offset() {
        // A pane starting at cell (40, 0) in a right-hand split: the popup
        // must land at the *pane's* cursor position, not the cursor's
        // position within an imaginary pane at the window origin — this is
        // the bug: before the fix, a right/bottom split's IME popup always
        // appeared over the leftmost/topmost pane instead.
        let unsplit = ime_cursor_area_origin(4, 4, 10, 20, (0, 0), (3, 2));
        let split = ime_cursor_area_origin(4, 4, 10, 20, (40, 0), (3, 2));
        assert_eq!(split.0 - unsplit.0, 40.0 * 10.0);
        assert_eq!(split.1, unsplit.1); // same row offset, no vertical split here
    }

    #[test]
    fn ime_commit_routes_to_the_find_bar_when_searching() {
        // Regardless of overlay/copy-mode state, an open find bar wins — it's
        // the innermost, most specific input owner.
        assert_eq!(ime_commit_target(true, false, false), ImeCommitTarget::SearchBar);
        assert_eq!(ime_commit_target(true, true, true), ImeCommitTarget::SearchBar);
    }

    #[test]
    fn ime_commit_is_dropped_under_an_overlay_or_copy_mode() {
        assert_eq!(ime_commit_target(false, true, false), ImeCommitTarget::Dropped);
        assert_eq!(ime_commit_target(false, false, true), ImeCommitTarget::Dropped);
    }

    #[test]
    fn ime_commit_reaches_the_pane_only_when_nothing_else_owns_the_keyboard() {
        assert_eq!(ime_commit_target(false, false, false), ImeCommitTarget::Pane);
    }

    #[test]
    fn tab_spans_merge_label_and_close_cells_and_split_on_gaps() {
        // [Tab0 Tab0 Close0 Close0] gap [Tab1 Tab1] then chrome buttons.
        let hits = vec![
            Hit::Tab(0),
            Hit::Tab(0),
            Hit::CloseTab(0),
            Hit::CloseTab(0),
            Hit::Drag,
            Hit::Tab(1),
            Hit::Tab(1),
            Hit::NewTab,
        ];
        assert_eq!(tab_spans(&hits), vec![(0, 0, 4), (1, 5, 7)]);
    }

    #[test]
    fn tab_drag_target_fires_on_midpoint_crossings_only() {
        // Three 10-cell tabs with 1-cell gaps: spans at 0..10, 11..21, 22..32
        // (midpoints 5, 16, 27).
        let spans = vec![(0, 0, 10), (1, 11, 21), (2, 22, 32)];
        // Dragging tab 0 right: nothing until past tab 1's midpoint…
        assert_eq!(tab_drag_target(15, &spans, 0), 0);
        assert_eq!(tab_drag_target(17, &spans, 0), 1);
        // …and crossing tab 2's midpoint targets the far slot directly.
        assert_eq!(tab_drag_target(28, &spans, 0), 2);
        // Dragging tab 1 left: fires only left of tab 0's midpoint.
        assert_eq!(tab_drag_target(5, &spans, 1), 1);
        assert_eq!(tab_drag_target(4, &spans, 1), 0);
        // Pointer inside the dragged tab's own span: stay put.
        assert_eq!(tab_drag_target(13, &spans, 1), 1);
        // A dragged tab that isn't in the spans (scrolled out): no move.
        assert_eq!(tab_drag_target(4, &spans, 7), 7);
    }

    #[test]
    fn tab_widths_size_to_titles_within_the_clamp() {
        // Plenty of room: a short title floors at TAB_MIN, a long one caps
        // at TAB_CELLS, one in between keeps its exact desired width.
        let w = tab_widths(&[5, 18, 60], 200).unwrap();
        assert_eq!(w, vec![super::TAB_MIN, 18, super::TAB_CELLS]);
    }

    #[test]
    fn tab_widths_shrink_together_when_the_strip_crowds() {
        // Four tabs wanting the max: 4*26 + 3 gaps = 107 > 80, so all share
        // the largest uniform cap that fits (4*cap + 3 <= 80 -> cap = 19).
        let w = tab_widths(&[60, 60, 60, 60], 80).unwrap();
        assert_eq!(w, vec![19, 19, 19, 19]);
        // A short tab keeps costing less than the cap.
        let w = tab_widths(&[60, 5, 60, 60], 80).unwrap();
        assert_eq!(w[1], super::TAB_MIN);
        assert!(w[0] > 19, "the freed cells go back to the wide tabs: {w:?}");
        // The chosen widths actually fit.
        assert!(w.iter().sum::<usize>() + 3 <= 80);
    }

    #[test]
    fn tab_widths_give_up_when_even_minimum_tabs_overflow() {
        // 10 tabs at TAB_MIN=12 need 129 cells; 100 can't hold them.
        assert!(tab_widths(&[20; 10], 100).is_none());
        // The empty strip trivially fits.
        assert_eq!(tab_widths(&[], 10), Some(vec![]));
    }

    #[test]
    fn visible_tab_range_shows_everything_when_it_all_fits() {
        // TAB_CELLS=26, so +1 separator = 27 cells/tab; 4 tabs need 4*27-1=107.
        assert_eq!(visible_tab_range(0, 4, 200), (0, 4));
        assert_eq!(visible_tab_range(3, 4, 200), (0, 4));
    }

    #[test]
    fn visible_tab_range_scrolls_minimally_to_keep_active_in_view() {
        // Room for 3 tabs (3*27-1=80 fits in 90), 10 tabs total.
        assert_eq!(visible_tab_range(0, 10, 90), (0, 3)); // active at the start
        assert_eq!(visible_tab_range(2, 10, 90), (0, 3)); // still within the first window
        assert_eq!(visible_tab_range(5, 10, 90), (3, 6)); // scrolled to bring 5 into view
        assert_eq!(visible_tab_range(9, 10, 90), (7, 10)); // last tab: pinned to the end
    }

    #[test]
    fn visible_tab_range_never_leaves_dead_space_past_the_last_tab() {
        // 5 tabs, room for 3: clicking back to an early tab shouldn't scroll
        // past what's needed, but the window also never runs past total.
        assert_eq!(visible_tab_range(4, 5, 90), (2, 5));
        assert_eq!(visible_tab_range(0, 5, 90), (0, 3));
    }

    #[test]
    fn drag_scroll_direction_fires_only_past_the_margin() {
        // Pane spans pixel rows [20, 100); a 5px margin around it.
        let (top, bottom, margin) = (20.0, 100.0, 5.0);
        assert_eq!(drag_scroll_direction(60.0, top, bottom, margin), None); // well inside
        assert_eq!(drag_scroll_direction(16.0, top, bottom, margin), None); // inside the margin
        assert_eq!(drag_scroll_direction(14.0, top, bottom, margin), Some(true)); // past it: scroll up
        assert_eq!(drag_scroll_direction(104.0, top, bottom, margin), None); // inside the margin
        assert_eq!(drag_scroll_direction(106.0, top, bottom, margin), Some(false)); // past it: scroll down
    }

    #[cfg(not(windows))]
    #[test]
    fn shell_quote_wraps_and_escapes_single_quotes() {
        assert_eq!(shell_quote("/tmp/plain"), "'/tmp/plain'");
        assert_eq!(shell_quote("/tmp/with space"), "'/tmp/with space'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    #[test]
    fn fmt_runtime_formats_ranges() {
        use std::time::Duration;
        assert_eq!(fmt_runtime(Duration::from_millis(300)), "<1s");
        assert_eq!(fmt_runtime(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_runtime(Duration::from_secs(187)), "3m07s");
        assert_eq!(fmt_runtime(Duration::from_secs(7500)), "2h05m");
    }

    #[test]
    fn dock_grid_lists_blocks_newest_first_with_click_map() {
        use crate::core::AnsiParser;
        let mut g = crate::core::Grid::new(40, 8);
        let mut p = AnsiParser::new();
        p.advance(&mut g, b"$ make ok\r\n\x1b]133;C\x07fine\r\n\x1b]133;D;0\x07");
        p.advance(&mut g, b"$ make bad\r\n\x1b]133;C\x07boom\r\n\x1b]133;D;3\x07");
        let theme = crate::core::Theme::default();
        let (d, items) = super::build_dock_grid(&g, &theme, 30, 8);
        // Header on row 0; entries start on row 2, newest first.
        assert_eq!(d.viewport_cell(1, 2).ch, '\u{2717}', "newest (failed) block first");
        assert_eq!(d.viewport_cell(1, 2).fg, theme.palette16[1], "failure glyph is red");
        let label: String = (3..14).map(|c| d.viewport_cell(c, 2).ch).collect();
        assert_eq!(label.trim_end(), "$ make bad", "label is the prompt line above the block");
        assert_eq!(d.viewport_cell(1, 3).ch, '\u{2713}', "older success below");
        assert_eq!(d.viewport_cell(1, 3).fg, theme.palette16[2], "success glyph is green");
        // Click map: row 2 jumps to the failed block's start, row 3 to the
        // successful one's; header/blank rows are inert.
        assert_eq!(items[2], Some(3));
        assert_eq!(items[3], Some(1));
        assert_eq!(items[0], None);
        assert_eq!(items[1], None);
    }

    #[test]
    fn dock_grid_empty_state_has_no_click_targets() {
        let g = crate::core::Grid::new(40, 8);
        let theme = crate::core::Theme::default();
        let (d, items) = super::build_dock_grid(&g, &theme, 30, 8);
        assert!(items.iter().all(Option::is_none));
        let msg: String = (1..17).map(|c| d.viewport_cell(c, 2).ch).collect();
        assert_eq!(msg.trim_end(), "No commands yet.");
    }

    #[test]
    fn read_git_branch_walks_up_and_parses_head() {
        let root = std::env::temp_dir().join(format!("rt_git_test_{}", std::process::id()));
        let sub = root.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        // A symbolic ref resolves to the branch name, from a subdirectory.
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/status-bar\n").unwrap();
        assert_eq!(read_git_branch(&sub).as_deref(), Some("feature/status-bar"));
        // A detached head shows the short hash.
        std::fs::write(root.join(".git/HEAD"), "0123456789abcdef0123456789abcdef01234567\n")
            .unwrap();
        assert_eq!(read_git_branch(&sub).as_deref(), Some("01234567"));
        // A worktree-style `.git` *file* follows its gitdir pointer.
        let wt = std::env::temp_dir().join(format!("rt_git_wt_{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", root.join(".git").display()))
            .unwrap();
        assert_eq!(read_git_branch(&wt).as_deref(), Some("01234567"));
        // Outside any repository: None.
        let bare = std::env::temp_dir().join(format!("rt_git_none_{}", std::process::id()));
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(read_git_branch(&bare), None);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn display_path_shortens_home_to_tilde() {
        // Only meaningful where a home directory is defined (CI always has one).
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        let Some(home) = home.map(std::path::PathBuf::from) else { return };
        assert_eq!(display_path(&home), "~");
        assert_eq!(
            display_path(&home.join("projects")),
            format!("~{}projects", std::path::MAIN_SEPARATOR)
        );
        // A path outside home passes through unchanged.
        let other = std::path::Path::new("/definitely/not/home");
        assert_eq!(display_path(other), other.display().to_string());
    }
}
