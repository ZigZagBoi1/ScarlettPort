//! Shared plumbing for every ScarlettPort crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`logging`]: one tracing setup for all binaries.
//! * [`oom`]: the allocator that says so when memory runs out.

pub mod config;
pub mod env;
pub mod logging;
#[cfg(windows)]
pub mod oom;
pub mod policy;
pub mod saves;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};
