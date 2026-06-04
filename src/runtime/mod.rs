//! Runtime: a single tokio reactor drives the [`Grid`](crate::core::Grid) over
//! the [`Backend`](crate::backend::Backend) on every platform, via [`tokio_rt`].
//!
//! On Unix the PTY master and a fresh `/dev/tty` open are registered with the
//! reactor (tokio's `AsyncFd`) and `SIGWINCH` arrives as a signal stream. On
//! Windows ConPTY's pipes are synchronous, so blocking reader/writer/stdin
//! threads bridge into tokio channels and a timer polls the console size for
//! resizes. Both expose the same [`run`] entry point so `main` is agnostic.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::backend::Backend;
use crate::core::Grid;

mod tokio_rt;
pub use tokio_rt::run;

/// The signature every runtime's `run` must satisfy: take ownership of the
/// backend and the shared grid, plus the host's initial size, and drive the
/// terminal until the child exits or input ends. Documented here as the
/// contract; the active runtime provides the concrete `run`.
#[allow(dead_code)]
type RunFn = fn(Box<dyn Backend>, Arc<Mutex<Grid>>, u16, u16) -> std::io::Result<()>;
