//! Session catalog and format adapters for AI coding assistants.
//!
//! The public API is session-only: parse, discover/list/search, convert/emit,
//! migrate, and relocate. CLI launchers, tmux, sync, and fzf helpers are
//! compiled in the same crate but kept crate-private for the `al` binary.

pub mod domain;
pub mod emit;
pub mod formats;
pub mod fs;
pub mod migrate;
pub mod relocate;
pub mod sessions;

pub(crate) mod cli;
pub(crate) mod launcher;
pub(crate) mod picker;
pub(crate) mod sync;
pub(crate) mod tmux;

use anyhow::Result;

/// Run the `al` CLI binary entrypoint.
pub fn run() -> Result<()> {
    cli::run()
}
