//! rusty_term as a library: the same modules the binary is built from,
//! exposed so out-of-tree harnesses (the `fuzz/` coverage-guided targets)
//! can drive the parser/decoder stack through its public surface. The
//! binary (`main.rs`) is a thin CLI over this.

pub mod backend;
pub mod config;
pub mod core;
#[cfg(feature = "gui")]
pub mod gui;
pub mod input;
pub mod keymap;
pub mod render;
pub mod runtime;
pub mod shells;
pub mod term;
#[cfg(feature = "web-bridge")]
pub mod web_bridge;
