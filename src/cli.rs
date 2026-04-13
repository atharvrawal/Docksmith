//! cli.rs — CLI commands: build, images, run, rmi, import.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::{build, image, runtime, store};

// ---------------------------------------------------------------------------
// Top-level CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "docksmith",
    about = "A simplified Docker-like build and runtime system",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build an image from a Docksmithfile
    Build {
        /// Image name and tag (name:tag)
        #[arg(short = 't', long = "tag")]
        tag: String,

        /// Build context directory (must contain a Docksmithfile)
        #[arg(default_value = ".")]
        context: PathBuf,

        /// Skip all cache lookups and writes
        #[arg(long = "no-cache")]
        no_cache: bool,
    },

    /// List all images in the local store
    Images,

    /// Run a container from an image
    Run {
        /// Image to run (name:tag)
        image: String,

        /// Override image CMD
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,

        /// Override or add environment variables (repeatable)
        #[arg(short = 'e', value_name = "KEY=VALUE")]
        env: Vec<String>,
    },

    /// Remove an image and all its layers
    Rmi {
        /// Image to remove (name:tag)
        image: String,
    },

    /// Import a tar file as a base image (run once before builds)
    Import {
        /// Path to the tar file
        tar: PathBuf,
        /// Image reference to assign (name:tag)
        image_ref: String,
    },
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    store::ensure_state_dirs()?;

    match cli.command {
        Command::Build { tag, context, no_cache } => {
            cmd_build(tag, context, no_cache)
        }
        Command::Images => {
            cmd_images()
        }
        Command::Run { image, cmd, env } => {
            cmd_run(image, cmd, env)
        }
        Command::Rmi { image } => {
            cmd_rmi(image)
        }
        Command::Import { tar, image_ref } => {
            cmd_import(tar, image_ref)
        }
    }
}

// ---------------------------------------------------------------------------
// `docksmith build`
// ---------------------------------------------------------------------------

fn cmd_build(tag: String, context: PathBuf, no_cache: bool) -> Result<()> {
    if !context.exists() {
        bail!("context directory does not exist: {:?}", context);
    }
    let docksmithfile = context.join("Docksmithfile");
    if !docksmithfile.exists() {
        bail!("no Docksmithfile found in {:?}", context);
    }
    build::build(&build::BuildOptions {
        tag,
        context,
        no_cache,
    })
}

// ---------------------------------------------------------------------------
// `docksmith images`
// ---------------------------------------------------------------------------

fn cmd_images() -> Result<()> {
    let manifests = image::list_manifests()?;

    if manifests.is_empty() {
        println!("No images found. Use `docksmith import` to add a base image.");
        return Ok(());
    }

    // Column widths
    println!("{:<20} {:<12} {:<15} {}", "NAME", "TAG", "ID", "CREATED");
    println!("{}", "-".repeat(72));

    for m in &manifests {
        // ID = first 12 chars of digest hex (strip "sha256:")
        let id = m.digest
            .strip_prefix("sha256:")
            .unwrap_or(&m.digest)
            .get(..12)
            .unwrap_or(&m.digest);

        println!(
            "{:<20} {:<12} {:<15} {}",
            m.name,
            m.tag,
            id,
            m.created.format("%Y-%m-%d %H:%M"),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `docksmith run`
// ---------------------------------------------------------------------------

fn cmd_run(image_ref: String, cmd_override: Vec<String>, env_flags: Vec<String>) -> Result<()> {
    let manifest = image::load_manifest(&image_ref)?;

    // Parse -e KEY=VALUE flags
    let mut env_overrides: Vec<(String, String)> = Vec::new();
    for flag in &env_flags {
        match flag.split_once('=') {
            Some((k, v)) => env_overrides.push((k.to_string(), v.to_string())),
            None => bail!("-e '{}' must be in KEY=VALUE format", flag),
        }
    }

    let cmd = if cmd_override.is_empty() { None } else { Some(cmd_override) };

    runtime::launch_container(&manifest, cmd, env_overrides)
}

// ---------------------------------------------------------------------------
// `docksmith rmi`
// ---------------------------------------------------------------------------

fn cmd_rmi(image_ref: String) -> Result<()> {
    let (name, tag) = store::parse_image_ref(&image_ref);
    let manifest = image::load_manifest(&image_ref)?;

    // Remove layer files belonging to this image
    let mut removed_layers = 0usize;
    for layer in &manifest.layers {
        let path = store::layer_path(&layer.digest);
        if path.exists() {
            std::fs::remove_file(&path).ok(); // best-effort
            removed_layers += 1;
        }
    }

    image::delete_manifest(&name, &tag)?;

    println!(
        "Removed {}:{} ({} layer{} deleted)",
        name, tag,
        removed_layers,
        if removed_layers == 1 { "" } else { "s" }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `docksmith import`
// ---------------------------------------------------------------------------

fn cmd_import(tar: PathBuf, image_ref: String) -> Result<()> {
    if !tar.exists() {
        bail!("tar file not found: {:?}", tar);
    }
    image::import_image(&tar, &image_ref)
}
