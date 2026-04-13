mod cli;
mod store;
mod image;
mod layer;
mod cache;
mod parser;
mod build;
mod runtime;

use anyhow::Result;

fn main() -> Result<()> {
    cli::run()
}
