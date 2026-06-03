//! Renderer abstraction: the windowed front-end paints through a [`Renderer`].
//!
//! The CPU implementation here presents via `softbuffer`; the GPU one
//! ([`super::gpu`], behind `gui-gpu`) presents via `wgpu`. Both consume the same
//! [`FontCache`] and the shared [`Grid`], so the window code is backend-agnostic.

use std::num::NonZeroU32;
use std::sync::Arc;

use winit::window::Window;

use crate::core::Grid;

use super::cpu;
use super::font::FontCache;

/// A present target: paint one frame of `grid` at the given pixel size.
pub(crate) trait Renderer {
    fn render(&mut self, grid: &Grid, font: &mut FontCache, width: u32, height: u32);
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
    fn render(&mut self, grid: &Grid, font: &mut FontCache, width: u32, height: u32) {
        let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };
        cpu::render(grid, font, &mut buffer, width as usize, height as usize);
        let _ = buffer.present();
    }
}
