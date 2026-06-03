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
//! unit-tested independently. Mouse reporting, clipboard (OSC 52), and DECCKM
//! application-cursor tracking are not yet wired (documented gaps, not stubs).

use std::sync::Arc;

use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::backend::{Backend, BackendHandle};
use crate::core::{AnsiParser, Grid};
use super::font::{self, FontCache, GlyphSource};
use super::render::{CpuRenderer, Renderer};

const FONT_PX: f32 = 18.0;
const INIT_COLS: u16 = 80;
const INIT_ROWS: u16 = 24;

/// Wakeups sent from the PTY reader thread into the winit loop.
enum UserEvent {
    /// New output was parsed into the grid; repaint.
    Redraw,
    /// The child exited; close the window.
    Exit,
}

/// Launch the windowed terminal. Blocks until the window closes or the child
/// exits. Returns an error if the window/PTY/font can't be set up.
pub fn run(backend: &dyn Backend) -> Result<(), Box<dyn std::error::Error>> {
    // The child renders through us now, so advertise a real terminal identity.
    unsafe {
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
    }

    let font_bytes = font::load_default_font().ok_or("no monospace font found")?;
    let font = FontCache::new(font_bytes, FONT_PX).ok_or("font failed to parse")?;
    let (cell_w, cell_h) = font.cell_size();

    let handle = backend.spawn_shell(INIT_COLS, INIT_ROWS)?;
    let grid = Arc::new(Mutex::new(Grid::new(INIT_COLS as usize, INIT_ROWS as usize)));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Reader thread: PTY -> parser -> grid, writing replies back and waking the
    // loop. Uses independent handle clones so it shares no lock with the loop.
    let read_handle = handle.try_clone()?;
    let reply_handle = handle.try_clone()?;
    let reader_grid = Arc::clone(&grid);
    std::thread::spawn(move || reader_loop(read_handle, reply_handle, reader_grid, proxy));

    let mut app = App {
        grid,
        writer: handle,
        font,
        cell_w: cell_w.max(1),
        cell_h: cell_h.max(1),
        window: None,
        renderer: None,
        mods: ModifiersState::empty(),
        cols: INIT_COLS,
        rows: INIT_ROWS,
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
) {
    let mut parser = AnsiParser::new();
    loop {
        match reader.read() {
            Ok(data) if data.is_empty() => break, // EOF: child exited
            Ok(data) => {
                let response = {
                    let mut g = grid.lock();
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
    writer: Box<dyn BackendHandle>,
    font: FontCache,
    cell_w: usize,
    cell_h: usize,
    window: Option<Arc<Window>>,
    renderer: Option<Box<dyn Renderer>>,
    mods: ModifiersState,
    cols: u16,
    rows: u16,
}

impl App {
    /// Recompute the cell grid from the window's pixel size and inform the child.
    fn apply_size(&mut self, px_w: u32, px_h: u32) {
        let cols = ((px_w as usize / self.cell_w).max(1)) as u16;
        let rows = ((px_h as usize / self.cell_h).max(1)) as u16;
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.grid.lock().resize(cols as usize, rows as usize);
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
                if event.state == ElementState::Pressed
                    && let Some(bytes) = super::input::encode(&event.logical_key, self.mods, false)
                {
                    let _ = self.writer.write(&bytes);
                }
            }
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
        }
    }
}
