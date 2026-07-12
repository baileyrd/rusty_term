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
use crate::config::{Config, LaunchMode};
use crate::core::{AnsiParser, Cell, Grid, Selection, Theme, WIDE_TRAILER, char_width};
use crate::keymap::{Action, Chord};
use crate::gui::mouse::{MouseButtonKind, MouseEvent, MousePoint, SgrEncoder};
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

/// One pane-resize keypress moves the split boundary by this ratio fraction.
const RESIZE_STEP: f32 = 0.05;
/// Scrollback lines moved per mouse-wheel notch.
const WHEEL_LINES: isize = 3;

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

    let mut app = App {
        backend,
        config: config.clone(),
        config_path,
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
    fn layout_panes(&mut self, ti: usize) {
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
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
            let frame = PaneFrame {
                grid: &page,
                col0: 0,
                row0: 1,
                focused: false,
                cursor_on: false,
                trail: Vec::new(),
            };
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
        let frames: Vec<PaneFrame> = held
            .iter()
            .map(|(g, r, foc)| PaneFrame {
                grid: g,
                col0: r.col,
                row0: r.row + 1,
                focused: *foc,
                cursor_on: blink,
                trail: if *foc { std::mem::take(&mut ghosts) } else { Vec::new() },
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
            let mode = if self.search_regex { " Find(re): " } else { " Find: " };
            put_text(&mut row, &mut hits, &mut col, limit, &format!("{mode}{query}"), self.theme.fg, bar_bg, Hit::Drag);
            let mut ccol = limit;
            put_text(&mut row, &mut hits, &mut ccol, cols, &count, self.theme.fg, bar_bg, Hit::Drag);
            self.hits = hits;
            return row;
        }
        if self.copy_mode.is_some() {
            let mut row = vec![Cell::blank(); cols];
            let bar_bg = mix(self.theme.bg, self.theme.fg, 45);
            for c in &mut row {
                c.fg = self.theme.fg;
                c.bg = bar_bg;
            }
            let mut hits = vec![Hit::Drag; cols];
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

        // The active tab of a focused window is being watched; its badge is
        // stale by definition.
        if self.focused
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.attention = false;
        }
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
            let (title, progress) = {
                let state = tab.focused().map(|p| {
                    let g = p.grid.lock();
                    (g.title.clone(), g.progress)
                });
                let (label, progress) = state.unwrap_or_default();
                (if label.is_empty() { format!("shell {}", i + 1) } else { label }, progress)
            };
            // OSC 9;4 progress rides the label: ` 42%`, `!42%` on error/
            // paused, `…` while indeterminate. An attention badge prepends
            // `•` in the alert color.
            let suffix = match progress {
                Some((2 | 4, pct)) => format!(" !{pct}%"),
                Some((3, _)) => " …".to_string(),
                Some((_, pct)) => format!(" {pct}%"),
                None => String::new(),
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
            if tab.attention {
                let alert = self.theme.palette16[1]; // ANSI red
                put_text(&mut row, &mut hits, &mut tcol, label_end, " •", alert, bg, Hit::Tab(i));
            }
            let cast = if is_active && self.broadcast { "⇉ " } else { "" };
            let label: String = format!(" {cast}{title}{suffix}")
                .chars()
                .take(label_end.saturating_sub(tcol))
                .collect();
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
            return self.overlay_click(y);
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
        // only fires when reporting is off; drag-selection still arms below
        // (the arrows only ever move within the prompt's own line).
        if self.config.click_to_move.unwrap_or(true)
            && let Some(p) = self.pane()
        {
            let moves = {
                let g = p.grid.lock();
                if g.mouse_modes.active() {
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
        self.selecting = self.click_streak == 1;
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
        let (col, row) = self.cell_at(px, py);
        if let Some(r) = self.focused_pane_rect() {
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
        let area = Rect::new(0, 0, self.cols as usize, self.rows as usize);
        let tab = self.tabs.get(self.active)?;
        tab.rects(area).into_iter().find(|(id, _)| *id == tab.focus).map(|(_, r)| r)
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
    fn service_clipboard(&mut self, id: u64) {
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
        if let Some(text) = set
            && let Some(cb) = self.clipboard.as_mut()
        {
            let _ = cb.set_text(text);
        }
        if let Some(text) = set_primary
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
        if query
            && let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok())
        {
            let reply = osc52_reply('c', &text);
            if let Some(p) = self.pane_by_id_mut(id) {
                let _ = p.writer.write(&reply);
            }
        }
        if query_primary
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
            Key::Character(s) if self.mods.control_key() && s.as_str() == "r" => {
                self.search_regex = !self.search_regex;
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
        let regex = self.search_regex;
        if let Some(p) = self.pane() {
            p.grid.lock().search_with(&q, regex);
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
        // Bidi: apps address cells in logical order; a click on a reordered
        // row reports the logical cell shown at the pointer's visual slot.
        let col = p.grid.lock().logical_col(col, row);
        let mut e = build(col, row);
        // SGR-pixel mode (`?1016`): the same SGR encoding, but the position
        // is the pointer's pixel offset within the focused pane's text area
        // (this pane is its own terminal, so pane-relative is the analogue of
        // xterm's text-area-relative pixels). The chrome bar above the grid
        // is one cell row tall, hence the +1 on the pane's row origin.
        if modes.extended & 8 != 0
            && let Some(r) = self.focused_pane_rect()
        {
            e.point = pane_pixel_point(
                self.mouse_pos,
                (r.col * self.cell_w, (r.row + 1) * self.cell_h),
                (r.cols * self.cell_w, r.rows * self.cell_h),
            );
        }
        let mut out = Vec::new();
        SgrEncoder::new(modes).write(e, &mut out);
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
        let url = {
            let g = p.grid.lock();
            let col = g.logical_col(col, row);
            // An explicit OSC 8 link wins; otherwise scan the cell's logical
            // line for a plain-text URL (G16) — most programs never emit OSC 8.
            g.link_at(col, row).map(str::to_owned).or_else(|| g.url_at(col, row))
        };
        url.is_some_and(|u| open_url(&u))
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
    /// for the focused pane right now: the mode is on *and* its alternate
    /// screen is active (mode 1007 only ever applies there — the primary
    /// screen keeps browsing rusty_term's own scrollback, same as xterm).
    fn alt_scroll_active(&self) -> bool {
        self.pane().is_some_and(|p| {
            let g = p.grid.lock();
            g.alt_scroll && g.in_alt_screen()
        })
    }

    /// Translate a wheel scroll into repeated Up/Down (DECCKM-aware) key
    /// presses for alternate scroll mode (`?1007`), so the wheel drives a
    /// pager (`less`, `man`, …) that never registered native mouse support.
    fn send_alt_scroll_keys(&mut self, lines: isize) {
        let app_cursor = self.pane().is_some_and(|p| p.grid.lock().app_cursor_keys);
        let seq: &[u8] = match (lines >= 0, app_cursor) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        if let Some(p) = self.pane_mut() {
            for _ in 0..lines.unsigned_abs() {
                let _ = p.writer.write(seq);
            }
        }
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

impl WindowState<'_> {
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
        let items = shell_menu_items(&self.config.profiles, &self.shells);
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
        // Keep every pane's XTWINOPS pixel-size answer (14t/16t) current.
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
        let width = (self.cols as usize * self.cell_w) as u32;
        // One extra cell row on top for the chrome bar.
        let height = ((self.rows as usize + 1) * self.cell_h) as u32;
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
            WindowEvent::ModifiersChanged(mods) => self.mods = mods.state(),
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
                // The edge band shows a resize cursor; over pane content (below
                // the chrome bar) the child's OSC 22 request wins if it made
                // one; everywhere else (the chrome bar itself) default.
                let icon = match self.resize_zone(position.x, position.y) {
                    Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => {
                        CursorIcon::NwseResize
                    }
                    Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => {
                        CursorIcon::NeswResize
                    }
                    Some(ResizeDirection::West | ResizeDirection::East) => CursorIcon::EwResize,
                    Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
                    None if (position.y.max(0.0) as usize) >= self.cell_h => self
                        .pane()
                        .and_then(|p| p.grid.lock().cursor_icon.as_deref().and_then(parse_cursor_icon))
                        .unwrap_or(CursorIcon::Default),
                    None => CursorIcon::Default,
                };
                if let Some(window) = &self.window {
                    window.set_cursor(icon);
                }
                if self.selecting
                    && let Some(anchor) = self.sel_anchor
                    && let Some(p) = self.pane()
                {
                    let (hc, hr) = self.cell_in_focused(position.x, position.y);
                    let mut g = p.grid.lock();
                    let hc = g.logical_col(hc, hr);
                    let head = (hc, g.abs_of_view_row(hr));
                    g.selection = Some(Selection { anchor, head });
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                // Motion reporting (`?1002` while a button is held, `?1003`
                // regardless) is independent of the local drag-selection
                // above — both can be active at once, same as other
                // terminals that don't force Shift to bypass app mouse mode.
                let (sh, al, ct) =
                    (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                let dragging = self.mouse_button_down.is_some();
                let button = self.mouse_button_down.unwrap_or_default();
                self.report_mouse(|c, r| {
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
                        if kind == MouseButtonKind::Middle
                            && !self.pane().is_some_and(|p| p.grid.lock().mouse_modes.active())
                        {
                            self.paste_primary();
                            return;
                        }
                        self.mouse_button_down = Some(kind);
                        let (sh, al, ct) =
                            (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                        self.report_mouse(|c, r| {
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
                        }
                        if self.mouse_button_down == Some(kind) {
                            self.mouse_button_down = None;
                        }
                        let (sh, al, ct) =
                            (self.mods.shift_key(), self.mods.alt_key(), self.mods.control_key());
                        self.report_mouse(|c, r| {
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
                if self.alt_scroll_active() {
                    self.send_alt_scroll_keys(lines);
                    return;
                }
                self.scroll_active(lines);
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
            tabs: Vec::new(),
            active: 0,
            next_id: self.next_id.clone(),
            proxy: self.proxy.clone(),
            font,
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            window: None,
            renderer: None,
            mods: ModifiersState::empty(),
            cols: self.config.cols.unwrap_or(INIT_COLS),
            rows: self.config.rows.unwrap_or(INIT_ROWS),
            theme: self.config.theme,
            clipboard: arboard::Clipboard::new().ok(),
            mouse_pos: (0.0, 0.0),
            selecting: false,
            focused: true,
            search_regex: false,
            broadcast: false,
            copy_mode: None,
            last_grid_click: None,
            click_streak: 0,
            mouse_button_down: None,
            sel_anchor: None,
            hits: Vec::new(),
            last_strip_click: None,
            cursor_blink_on: true,
            last_blink: Instant::now(),
            searching: None,
            shells: self.shells.clone(),
            overlay: None,
            font_px,
            closed: false,
            wants_new_window: false,
            cursor_prev: None,
            trail: None,
            quake,
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
/// paste-injection guard).
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
        text.into_bytes()
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
        Hit, MenuKind, arrow_presses, encode_paste, is_openable_url, mix, osc52_reply, pane_pixel_point,
        path_from_file_uri, put_text, shell_menu_items,
    };
    #[cfg(not(windows))]
    use super::shell_quote;
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

    #[cfg(not(windows))]
    #[test]
    fn shell_quote_wraps_and_escapes_single_quotes() {
        assert_eq!(shell_quote("/tmp/plain"), "'/tmp/plain'");
        assert_eq!(shell_quote("/tmp/with space"), "'/tmp/with space'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }
}
