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

/// Pixels of strip-colored band above the chrome bar's tabs, so a tab's top
/// edge is visibly distinct from the window's (the tabs otherwise run flush
/// into the frame and read as sliced off). Bounded by the window padding so
/// the pushed-down bar can never overlap the grid, which starts a full cell
/// row plus `pad` below the top.
pub(super) fn bar_inset(pad: usize, cell_h: usize) -> usize {
    pad.min(cell_h / 6)
}

/// Search-match highlight, shared by both renderers: amber for a match, orange
/// for the active one, with a dark glyph so text stays legible on either.
pub(super) const SEARCH_BG: u32 = 0xFFD24A;
pub(super) const SEARCH_CUR_BG: u32 = 0xFF7A1A;
pub(super) const SEARCH_FG: u32 = 0x101010;

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
    /// A Ctrl-hovered hyperlink's `(row, start_col, end_col)` (inclusive),
    /// underlined for the click affordance — `None` when nothing's hovered
    /// or Ctrl isn't held. Cell coordinates, in this pane's own frame.
    pub hover_link: Option<(usize, usize, usize)>,
    /// Command gutter marks: `(viewport row, color)` for rows inside an
    /// OSC 133 command block, painted as a thin stripe just left of the
    /// pane's text (green success / red failure / accent while running).
    /// Empty when the feature is off or nothing is marked.
    pub marks: Vec<(usize, u32)>,
}

/// A present target: paint one frame of the tab's `panes` at the given pixel
/// size. `chrome` is the window's own top bar (tabs + caption buttons) as one
/// pre-laid cell row at the top; `status` is the bottom status ribbon (cwd,
/// git branch, exit pill, …) as one pre-laid cell row flush with the window's
/// bottom edge (empty when disabled); `divider` fills the gaps between panes;
/// `bg` fills everything else — the `pad`-pixel band inset around the pane
/// area (`[window] padding`) and any right/bottom slack, so content reads as
/// floating on the terminal background. Both bars stay flush.
pub(crate) trait Renderer {
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        panes: &[PaneFrame],
        chrome: &[Cell],
        status: &[Cell],
        font: &mut FontCache,
        width: u32,
        height: u32,
        divider: u32,
        bg: u32,
        bar_bg: u32,
        pad: usize,
    );

    /// Set the window background opacity (`[window] opacity`), `0.0`-`1.0`.
    /// A no-op by default: the CPU (`softbuffer`) presentation path has no
    /// alpha channel to composite through, so only the GPU renderer overrides
    /// this — see `GpuRenderer`'s impl and `GpuCore::alpha_mode`.
    fn set_opacity(&mut self, _opacity: f32) {}

    /// Whether [`set_opacity`](Self::set_opacity) actually does anything.
    /// The settings page uses this to render the opacity row as disabled
    /// (instead of silently inert) when the CPU renderer is presenting.
    fn supports_opacity(&self) -> bool {
        false
    }
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
        Ok(CpuRenderer {
            _context: context,
            surface,
        })
    }
}

impl Renderer for CpuRenderer {
    fn render(
        &mut self,
        panes: &[PaneFrame],
        chrome: &[Cell],
        status: &[Cell],
        font: &mut FontCache,
        width: u32,
        height: u32,
        divider: u32,
        bg: u32,
        bar_bg: u32,
        pad: usize,
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
        buffer.fill(bg); // the padding band (and any slack) is background
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = font.cell_size();
        // The chrome bar sits `inset` px below the window's top edge (the
        // strip band shows above it); the grid shifts down by the same
        // amount so the pad-sized gap between bar and content survives —
        // the bottom pad absorbs the difference.
        let inset = if chrome.is_empty() {
            0
        } else {
            bar_inset(pad, ch)
        };
        let oy = pad + inset;
        // Gaps between panes show the divider: fill the panes' bounding box
        // with it, then let the grids overpaint everything but the gaps.
        if let (Some(x1), Some(y1)) = (
            panes.iter().map(|p| p.col0 + p.grid.cols).max(),
            panes.iter().map(|p| p.row0 + p.grid.rows).max(),
        ) {
            let y0 = panes.iter().map(|p| p.row0).min().unwrap_or(0) * ch + oy;
            for y in y0..(y1 * ch + oy).min(h) {
                let base = y * w;
                for x in pad..(x1 * cw + pad).min(w) {
                    buffer[base + x] = divider;
                }
            }
        }
        if !chrome.is_empty() {
            cpu::draw_chrome(&mut buffer, w, h, chrome, font, cw, ch, inset, bar_bg);
        }
        // The status ribbon sits flush with the window's bottom edge; the
        // grid was sized one row shorter, so it never paints under it.
        if !status.is_empty() && h >= ch {
            cpu::draw_bar(&mut buffer, w, h, status, font, cw, ch, h - ch);
        }
        for p in panes {
            cpu::draw_grid(
                &mut buffer,
                w,
                h,
                p.grid,
                p.col0,
                p.row0,
                pad,
                oy,
                p.focused,
                p.cursor_on,
                p.hover_link,
                font,
            );
            cpu::draw_trail(
                &mut buffer,
                w,
                h,
                p.grid,
                p.col0,
                p.row0,
                pad,
                oy,
                &p.trail,
                font,
            );
            cpu::draw_marks(&mut buffer, w, h, p.col0, p.row0, pad, oy, &p.marks, font);
        }
        let _ = buffer.present();
    }
}
