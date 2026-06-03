//! Native window backend — the `tcore-font` / `tcore-app` fork.
//!
//! Renders the terminal into a standalone OS window instead of into a host
//! terminal, encoding real key events natively rather than relaying them. Built
//! behind the `gui` feature so the default terminal stays library-light.
//!
//! Layers:
//! - [`font`] — monospace glyph rasterization (`ab_glyph`), shared by renderers.
//! - [`cpu`] — software compositor: a [`crate::core::Grid`] into a pixel buffer
//!   (presented via `softbuffer`); pure and headless-testable.
//! - [`render`] — the `Renderer` trait + CPU (`softbuffer`) presenter.
//! - [`gpu`] — wgpu glyph-atlas renderer (behind `gui-gpu`).
//! - [`input`] — native key encoding (winit key → terminal bytes).
//! - [`window`] — the `winit` event loop + window tying it together.

mod cpu;
mod font;
#[cfg(feature = "gui-gpu")]
mod gpu;
mod input;
mod render;
mod window;

pub use window::run;
