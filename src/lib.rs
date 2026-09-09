//! Session catalog and format adapters for AI coding assistants.
//!
//! The public API is session-only: parse, discover/list/search, convert/emit,
//! migrate, and relocate. CLI launchers, tmux, sync, and fzf helpers are
//! compiled in the same crate but kept crate-private for the `al` binary.
//!
//! Library consumers that only need `domain` / `formats` / `emit` can depend
//! with `default-features = false`. `cli` pulls in `clap` plus the catalog
//! (`rusqlite`) used by Agent session parsing.

pub mod domain;
pub mod emit;
pub mod formats;
pub mod fs;
pub mod migrate;
pub mod relocate;
pub mod sessions;

#[cfg(feature = "cli")]
pub(crate) mod cli;
#[cfg(feature = "cli")]
pub(crate) mod launcher;
#[cfg(feature = "cli")]
pub(crate) mod live;
#[cfg(feature = "cli")]
pub(crate) mod new;
#[cfg(feature = "cli")]
pub(crate) mod picker;
#[cfg(feature = "cli")]
pub(crate) mod sync;
#[cfg(feature = "cli")]
pub(crate) mod tmux;

/// Run the `al` CLI binary entrypoint.
#[cfg(feature = "cli")]
pub fn run() -> anyhow::Result<()> {
    cli::run()
}
