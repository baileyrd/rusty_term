//! Runtime selection. Two interchangeable event-loop implementations drive the
//! same [`Grid`](crate::core::Grid) over the same [`Backend`](crate::backend::Backend):
//!
//! - [`threaded`] — one OS thread each for parse / input / render, coordinated
//!   by a condvar. The default; no async dependencies.
//! - [`tokio_rt`] — a single tokio reactor driving the PTY master, host stdin,
//!   the SIGWINCH stream, and render coalescing. Unix-only.
//!
//! Exactly one is compiled, chosen by the `threaded` / `tokio-runtime` Cargo
//! features. Both expose the same [`run`] entry point so `main` is agnostic.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::backend::Backend;
use crate::core::Grid;

// When `tokio-runtime` is enabled on a supported (Unix) platform it takes
// precedence over the default `threaded`, so `--features tokio-runtime` "just
// works" without `--no-default-features`. Threaded is used otherwise, including
// any non-Unix build that requested tokio.

#[cfg(not(any(feature = "threaded", feature = "tokio-runtime")))]
compile_error!("no runtime selected: enable the `threaded` (default) or `tokio-runtime` feature");

#[cfg(all(feature = "tokio-runtime", not(unix)))]
compile_error!(
    "the `tokio-runtime` feature is only supported on Unix (it drives the PTY \
     master with tokio's AsyncFd); build the default `threaded` runtime instead"
);

#[cfg(all(feature = "tokio-runtime", unix))]
mod tokio_rt;
#[cfg(all(feature = "tokio-runtime", unix))]
pub use tokio_rt::run;

#[cfg(all(feature = "threaded", not(all(feature = "tokio-runtime", unix))))]
mod threaded;
#[cfg(all(feature = "threaded", not(all(feature = "tokio-runtime", unix))))]
pub use threaded::run;

/// The signature every runtime's `run` must satisfy: take ownership of the
/// backend and the shared grid, plus the host's initial size, and drive the
/// terminal until the child exits or input ends. Documented here as the
/// contract; the active runtime provides the concrete `run`.
#[allow(dead_code)]
type RunFn = fn(Box<dyn Backend>, Arc<Mutex<Grid>>, u16, u16) -> std::io::Result<()>;
