//! The windowed front-end: a `winit` event loop driving a real OS window, with
//! `softbuffer` CPU presentation.
//!
//! A reader thread pumps the PTY through the parser into the shared [`Grid`] and
//! wakes the loop to repaint; window key events are encoded natively (see
//! [`super::input`]) and written to the PTY; window resizes are translated to a
//! new cell grid + `TIOCSWINSZ`.
//!
//! Note: this drives a live window and so cannot be exercised in a headless
//! environment — the render, input-encoding, and font layers it composes are
//! unit-tested independently. Mouse drag-selection, clipboard copy/paste
//! (Ctrl+Shift+C / Ctrl+Shift+V), the block cursor, and a Windows child-exit
//! watcher are wired here; mouse *reporting* to the child and DECCKM
//! application-cursor tracking remain documented gaps.

use std::sync::Arc;

use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::backend::{Backend, BackendHandle};
use crate::config::Config;
use crate::core::{AnsiParser, Grid, Selection, Theme};
use super::font::{self, FontCache, GlyphSource};
use super::render::{CpuRenderer, Renderer};

/// Built-in defaults, overridable via the config file (`[window]` section).
const FONT_PX: f32 = 18.0;
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;

/// Wakeups sent from the PTY reader thread into the winit loop.
enum UserEvent {
    /// New output was parsed into the grid; repaint.
    Redraw,
    /// The child exited; close the window.
    Exit,
    /// The config file changed on disk; reload and apply what can change live.
    ConfigChanged,
}

/// Launch the windowed terminal. Blocks until the window closes or the child
/// exits. Returns an error if the window/PTY/font can't be set up.
pub fn run(backend: &dyn Backend, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    // The child renders through us now, so advertise a real terminal identity.
    unsafe {
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
    }

    let font_bytes =
        font::load_default_font(config.font.as_deref()).ok_or("no monospace font found")?;
    let font_px = config.font_size.unwrap_or(FONT_PX);
    let font = FontCache::new(font_bytes, font_px).ok_or("font failed to parse")?;
    let (cell_w, cell_h) = font.cell_size();

    let init_cols = config.cols.unwrap_or(INIT_COLS);
    let init_rows = config.rows.unwrap_or(INIT_ROWS);
    let handle = backend.spawn_shell(init_cols, init_rows, config.shell.as_deref())?;
    let mut g = Grid::new(init_cols as usize, init_rows as usize);
    if let Some(max) = config.scrollback {
        g.set_scrollback_max(max);
    }
    g.apply_theme(&config.theme);
    let grid = Arc::new(Mutex::new(g));
    // The parser is shared between the reader thread (PTY bytes) and the
    // winit loop (live config reload retheming it); contention is nil — the
    // loop only takes it on a config save.
    let parser = Arc::new(Mutex::new(AnsiParser::with_theme(config.theme)));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Reader thread: PTY -> parser -> grid, writing replies back and waking the
    // loop. Uses independent handle clones so it shares no lock with the loop.
    let read_handle = handle.try_clone()?;
    let reply_handle = handle.try_clone()?;
    let reader_grid = Arc::clone(&grid);
    let reader_proxy = proxy.clone();
    let reader_parser = Arc::clone(&parser);
    std::thread::spawn(move || {
        reader_loop(read_handle, reply_handle, reader_grid, reader_proxy, reader_parser)
    });

    // Config live reload: watch the file and wake the loop on changes.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = crate::config::Config::file_path(&args);
    if let Some(path) = config_path.clone() {
        let watch_proxy = proxy.clone();
        crate::config::watch(path, move || {
            let _ = watch_proxy.send_event(UserEvent::ConfigChanged);
        });
    }

    // Child-exit watcher. On Windows the ConPTY output pipe only EOFs at
    // teardown, not when the shell exits, so read-EOF can't close the window;
    // block on the child process handle instead. `None` where read-EOF already
    // signals exit (Unix), keeping this a no-op there.
    if let Some(wait) = handle.exit_token() {
        std::thread::spawn(move || {
            wait();
            let _ = proxy.send_event(UserEvent::Exit);
        });
    }

    let mut app = App {
        grid,
        parser,
        config_path,
        writer: handle,
        font,
        cell_w: cell_w.max(1),
        cell_h: cell_h.max(1),
        window: None,
        renderer: None,
        mods: ModifiersState::empty(),
        cols: init_cols,
        rows: init_rows,
        theme: config.theme,
        clipboard: arboard::Clipboard::new().ok(),
        mouse_pos: (0.0, 0.0),
        selecting: false,
        sel_anchor: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// PTY reader loop (own thread): parse output into the grid, send replies
/// (DA/DSR/structured-channel) back to the child, and wake the window.
fn reader_loop(
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
                if proxy.send_event(UserEvent::Redraw).is_err() {
                    break; // loop gone
                }
            }
            Err(_) => break,
        }
    }
    let _ = proxy.send_event(UserEvent::Exit);
}

struct App {
    grid: Arc<Mutex<Grid>>,
    /// Shared with the reader thread; the loop takes it only on config reload.
    parser: Arc<Mutex<AnsiParser>>,
    /// The config file in effect, for the open shortcut + reload re-reads.
    config_path: Option<std::path::PathBuf>,
    writer: Box<dyn BackendHandle>,
    font: FontCache,
    cell_w: usize,
    cell_h: usize,
    window: Option<Arc<Window>>,
    renderer: Option<Box<dyn Renderer>>,
    mods: ModifiersState,
    cols: u16,
    rows: u16,
    /// The theme in effect, mirrored onto the window chrome (title bar/border).
    theme: Theme,
    /// System clipboard for copy/paste; `None` if unavailable (e.g. headless).
    clipboard: Option<arboard::Clipboard>,
    /// Last pointer position in physical pixels, for hit-testing selection.
    mouse_pos: (f64, f64),
    /// Whether the left button is held (a drag-selection is in progress).
    selecting: bool,
    /// Cell where the current drag-selection began.
    sel_anchor: Option<(usize, usize)>,
}

impl App {
    /// Recompute the cell grid from the window's pixel size and inform the child.
    fn apply_size(&mut self, px_w: u32, px_h: u32) {
        let cols = ((px_w as usize / self.cell_w).max(1)) as u16;
        let rows = ((px_h as usize / self.cell_h).max(1)) as u16;
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            let mut g = self.grid.lock();
            g.resize(cols as usize, rows as usize);
            g.selection = None; // viewport changed; old selection coords are stale
            drop(g);
            let _ = self.writer.set_winsize(cols, rows);
        }
    }

    /// Paint the current grid through the active renderer.
    fn redraw(&mut self) {
        let (Some(renderer), Some(window)) = (self.renderer.as_mut(), self.window.as_ref()) else {
            return;
        };
        let size = window.inner_size();
        let g = self.grid.lock();
        window.set_title(if g.title.is_empty() { "rusty_term" } else { &g.title });
        renderer.render(&g, &mut self.font, size.width, size.height);
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

    /// Map a physical pixel position to a clamped `(col, row)` cell.
    fn cell_at(&self, px: f64, py: f64) -> (usize, usize) {
        let col = (px.max(0.0) as usize / self.cell_w).min((self.cols as usize).saturating_sub(1));
        let row = (py.max(0.0) as usize / self.cell_h).min((self.rows as usize).saturating_sub(1));
        (col, row)
    }

    /// Copy the current selection to the system clipboard (Ctrl+Shift+C).
    fn copy_selection(&mut self) {
        let Some(cb) = self.clipboard.as_mut() else { return };
        if let Some(text) = self.grid.lock().selected_text() {
            let _ = cb.set_text(text);
        }
    }

    /// Paste the system clipboard into the child (Ctrl+Shift+V).
    fn paste(&mut self) {
        let Some(cb) = self.clipboard.as_mut() else { return };
        let Ok(text) = cb.get_text() else { return };
        if text.is_empty() {
            return;
        }
        let bracketed = self.grid.lock().bracketed_paste;
        let _ = self.writer.write(&encode_paste(&text, bracketed));
    }

    /// Open the config file in the user's editor (Ctrl+Shift+,), creating it
    /// from the commented template first if needed. The live-reload watcher
    /// then applies any save the user makes.
    fn open_config(&self) {
        let Some(path) = &self.config_path else { return };
        if let Err(e) = crate::config::open_in_editor(path) {
            eprintln!("rusty_term: open config {}: {e}", path.display());
        }
    }

    /// Re-read the config file and apply what can change live: theme (parser
    /// palette + grid recolor + window chrome) and scrollback cap. Shell, font, and window
    /// size are launch-time choices — a saved change to those takes effect on
    /// the next start. Parse warnings go to stderr, same as at startup.
    fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        let args = vec!["--config".to_string(), path.to_string_lossy().into_owned()];
        let (new, warnings) = crate::config::Config::load(&args);
        for w in &warnings {
            eprintln!("rusty_term: {w}");
        }
        let mut g = self.grid.lock();
        let old = self.parser.lock().retheme(new.theme);
        if old != new.theme {
            g.retheme(&old, &new.theme);
        }
        g.set_scrollback_max(new.scrollback.unwrap_or(crate::core::SCROLLBACK_MAX));
        drop(g);
        self.theme = new.theme;
        if let Some(window) = &self.window {
            apply_chrome(window, &self.theme);
            window.request_redraw();
        }
    }
}

/// Paint the native window chrome — title bar background, caption text, and
/// border — with the theme's colors, so the frame reads as part of the
/// terminal instead of stock system chrome. Windows 11 only (DWM ignores the
/// attributes on Windows 10); a no-op on other platforms, where the system
/// titlebar theme is not per-window paintable through winit.
fn apply_chrome(window: &Window, theme: &Theme) {
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::{Color, WindowExtWindows};
        let c = |rgb: u32| Color::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        window.set_title_background_color(Some(c(theme.bg)));
        window.set_border_color(Some(c(theme.bg)));
        window.set_title_text_color(c(theme.fg));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window, theme);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let width = (self.cols as usize * self.cell_w) as u32;
        let height = (self.rows as usize * self.cell_h) as u32;
        let attrs = Window::default_attributes()
            .with_title("rusty_term")
            .with_inner_size(winit::dpi::PhysicalSize::new(width, height));
        let Ok(window) = event_loop.create_window(attrs) else {
            event_loop.exit();
            return;
        };
        let window = Arc::new(window);
        apply_chrome(&window, &self.theme);
        self.window = Some(window.clone());
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
                // Clipboard shortcuts are intercepted before native encoding.
                if self.mods.control_key()
                    && self.mods.shift_key()
                    && let PhysicalKey::Code(code) = event.physical_key
                {
                    match code {
                        KeyCode::KeyC => return self.copy_selection(),
                        KeyCode::KeyV => return self.paste(),
                        KeyCode::Comma => return self.open_config(),
                        _ => {}
                    }
                }
                if let Some(bytes) = super::input::encode(&event.logical_key, self.mods, false) {
                    let _ = self.writer.write(&bytes);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                if self.selecting
                    && let Some(anchor) = self.sel_anchor
                {
                    let head = self.cell_at(position.x, position.y);
                    self.grid.lock().selection = Some(Selection { anchor, head });
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    self.sel_anchor = Some(self.cell_at(self.mouse_pos.0, self.mouse_pos.1));
                    self.selecting = true;
                    self.grid.lock().selection = None; // cleared until the drag moves
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                ElementState::Released => self.selecting = false,
            },
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::Exit => event_loop.exit(),
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

#[cfg(test)]
mod tests {
    use super::encode_paste;

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
}
