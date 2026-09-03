pub mod cli;
pub mod domain;
pub mod emit;
pub mod formats;
pub mod fs;
pub mod launcher;
pub mod migrate;
pub mod picker;
pub mod relocate;
pub mod sessions;
pub mod tmux;
pub mod sync;

use anyhow::Result;

pub fn run() -> Result<()> {
    cli::run()
}
