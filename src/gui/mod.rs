//! Native window backend — the `tcore-font` / `tcore-app` fork.
//!
//! Renders the terminal into a standalone OS window instead of into a host
//! terminal, encoding real key events natively rather than relaying them. Built
//! behind the `gui` feature so the default terminal stays library-light.
//!
//! Layers:
//! - [`access`] — AccessKit accessibility-tree integration (C20): exposes the
//!   visible screen text and cursor position to assistive technology.
//! - [`font`] — monospace glyph rasterization (`ab_glyph`), shared by renderers.
//! - [`cpu`] — software compositor: a [`crate::core::Grid`] into a pixel buffer
//!   (presented via `softbuffer`); pure and headless-testable.
//! - [`render`] — the `Renderer` trait + CPU (`softbuffer`) presenter.
//! - [`settings`] — the in-app settings page model (values + persistence).
//! - [`gpu`] — wgpu glyph-atlas renderer (behind `gui-gpu`).
//! - [`input`] — native key encoding (winit key → terminal bytes).
//! - [`window`] — the `winit` event loop + window tying it together.

mod access;
mod boxdraw;
pub mod control;
mod cpu;
mod font;
#[cfg(feature = "gui-gpu")]
mod gpu;
mod hotkey;
pub(crate) mod input;
mod layout;
mod mouse;
mod render;
mod settings;
mod shape;
mod taskbar;
mod window;

pub use window::run;
