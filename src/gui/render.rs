//! Renderer abstraction: the windowed front-end paints through a [`Renderer`].
//!
//! The CPU implementation here presents via `softbuffer`; the GPU one
//! ([`super::gpu`], behind `gui-gpu`) presents via `wgpu`. Both consume the same
//! [`FontCache`] and the shared [`Grid`], so the window code is backend-agnostic.

use std::num::NonZeroU32;
use std::sync::Arc;

use winit::window::Window;

use crate::core::{Cell, Grid};

use super::cpu;
use super::font::{FontCache, GlyphSource};

/// One pane to paint this frame: its `grid` at cell offset `(col0, row0)`,
/// sized to the grid's own `cols × rows`. `focused` gates the cursor/IME
/// preedit (only the focused pane shows them); `cursor_on` is the blink phase.
pub(crate) struct PaneFrame<'a> {
    pub grid: &'a Grid,
    pub col0: usize,
    pub row0: usize,
    pub focused: bool,
    pub cursor_on: bool,
    /// Cursor-trail ghosts (G36): faded cursor blocks at these cells, alpha
    /// `0.0..=1.0`, blended in the cursor color. Empty when the trail is off
    /// or expired.
    pub trail: Vec<(usize, usize, f32)>,
}

/// A present target: paint one frame of the tab's `panes` at the given pixel
/// size. `chrome` is the window's own top bar (tabs + caption buttons) as one
/// pre-laid cell row at the top; `divider` fills the gaps between panes.
pub(crate) trait Renderer {
    fn render(
        &mut self,
        panes: &[PaneFrame],
        chrome: &[Cell],
        font: &mut FontCache,
        width: u32,
        height: u32,
        divider: u32,
    );

    /// Set the window background opacity (`[window] opacity`), `0.0`-`1.0`.
    /// A no-op by default: the CPU (`softbuffer`) presentation path has no
    /// alpha channel to composite through, so only the GPU renderer overrides
    /// this — see `GpuRenderer`'s impl and `GpuCore::alpha_mode`.
    fn set_opacity(&mut self, _opacity: f32) {}
}

/// CPU compositor presented through `softbuffer`.
pub(crate) struct CpuRenderer {
    // The context must outlive the surface, so it is held alongside it.
    _context: softbuffer::Context<Arc<Window>>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
}

impl CpuRenderer {
    pub(crate) fn new(window: Arc<Window>) -> Result<Self, Box<dyn std::error::Error>> {
        let context = softbuffer::Context::new(window.clone())?;
        let surface = softbuffer::Surface::new(&context, window)?;
        Ok(CpuRenderer { _context: context, surface })
    }
}

impl Renderer for CpuRenderer {
    fn render(
        &mut self,
        panes: &[PaneFrame],
        chrome: &[Cell],
        font: &mut FontCache,
        width: u32,
        height: u32,
        divider: u32,
    ) {
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };
        buffer.fill(divider); // gaps between panes show the divider color
        let (w, h) = (width as usize, height as usize);
        if !chrome.is_empty() {
            let (cw, ch) = font.cell_size();
            cpu::draw_chrome(&mut buffer, w, h, chrome, font, cw, ch);
        }
        for p in panes {
            cpu::draw_grid(&mut buffer, w, h, p.grid, p.col0, p.row0, p.focused, p.cursor_on, font);
            cpu::draw_trail(&mut buffer, w, h, p.grid, p.col0, p.row0, &p.trail, font);
        }
        let _ = buffer.present();
    }
}
