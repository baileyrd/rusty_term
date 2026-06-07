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
use super::font::FontCache;

/// A present target: paint one frame of `grid` at the given pixel size.
/// `chrome` is the window's own top bar (tabs + caption buttons) as one row of
/// pre-laid cells; when non-empty it occupies the first cell row and the grid
/// is painted one row below. Empty means no chrome (headless tests).
pub(crate) trait Renderer {
    fn render(&mut self, grid: &Grid, chrome: &[Cell], font: &mut FontCache, width: u32, height: u32, cursor_on: bool);
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
    fn render(&mut self, grid: &Grid, chrome: &[Cell], font: &mut FontCache, width: u32, height: u32, cursor_on: bool) {
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };
        cpu::render(grid, chrome, font, &mut buffer, width as usize, height as usize, cursor_on);
        let _ = buffer.present();
    }
}
