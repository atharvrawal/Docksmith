//! build.rs — Build engine.
//!
//! Orchestrates: parse → FROM → COPY/RUN (with cache) → manifest write.
//! RUN isolation is shared with the runtime (see runtime.rs).

use anyhow::{bail, Context, Result};
use chrono::Utc;
use glob::glob;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{
    cache::{self, CacheKeyInput},
    image::{self, ImageConfig, ImageManifest, LayerEntry},
    layer,
    parser::{self, Instruction},
    runtime,
    store,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub struct BuildOptions {
    pub tag:      String,   // "name:tag"
    pub context:  PathBuf,  // build context directory
    pub no_cache: bool,
}

pub fn build(opts: &BuildOptions) -> Result<()> {
    store::ensure_state_dirs()?;
    let (name, tag) = store::parse_image_ref(&opts.tag);

    // Parse Docksmithfile
    let docksmithfile = opts.context.join("Docksmithfile");
    let instructions = parser::parse_file(&docksmithfile)
        .context("failed to parse Docksmithfile")?;

    if instructions.is_empty() {
        bail!("Docksmithfile is empty");
    }

    // Count total steps for the "Step N/M" display
    let total_steps = instructions.len();

    // Build state
    let mut layers: Vec<LayerEntry> = Vec::new();
    let mut config  = ImageConfig::default();
    let mut env_map: BTreeMap<String, String> = BTreeMap::new(); // sorted by key
    let mut workdir = String::new();

    // Digest of the previous layer-producing step (or base image manifest digest)
    let mut prev_digest = String::from("sha256:empty");

    // Once we have a cache miss, all subsequent steps must also miss
    let mut cache_invalidated = opts.no_cache;

    // Original creation timestamp — preserved on all-hit rebuilds
    let mut original_created = None;

    let total_start = Instant::now();

    for (step_idx, instr) in instructions.iter().enumerate() {
        let step_num = step_idx + 1;

        match instr {
            // ------------------------------------------------------------------
            Instruction::From { image_ref } => {
                println!("Step {}/{} : FROM {}", step_num, total_steps, image_ref);
                let base = image::load_manifest(image_ref)?;
                layers   = base.layers.clone();
                config   = base.config.clone();
                // Seed env_map from base image ENV
                for pair in &config.env {
                    if let Some((k, v)) = pair.split_once('=') {
                        env_map.insert(k.to_string(), v.to_string());
                    }
                }
                if !config.working_dir.is_empty() {
                    workdir = config.working_dir.clone();
                }
                // The base image's manifest digest seeds the cache key chain
                prev_digest = base.digest.clone();
                // Preserve creation timestamp if image already exists
                if let Ok(existing) = image::load_manifest(&opts.tag) {
                    original_created = Some(existing.created);
                }
            }

            // ------------------------------------------------------------------
            Instruction::WorkDir { path } => {
                workdir = path.clone();
                // No layer produced; update config only
                println!("Step {}/{} : WORKDIR {}", step_num, total_steps, path); 
                config.working_dir = path.clone();
            }

            // ------------------------------------------------------------------
            Instruction::Env { key, value } => {
                println!("Step {}/{} : ENV {}={}", step_num, total_steps, key, value); 
                env_map.insert(key.clone(), value.clone());
                // Rebuild config.env (sorted)
                config.env = env_map
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect();
            }

            // ------------------------------------------------------------------
            Instruction::Cmd { args } => {
                println!("Step {}/{} : CMD {:?}", step_num, total_steps, args);
                config.cmd = args.clone();
            }

            // ------------------------------------------------------------------
            Instruction::Copy { src, dest } => {
                let instr_text = format!("COPY {} {}", src, dest);
                let step_start = Instant::now();

                // Hash source files for cache key
                let env_pairs: Vec<(String, String)> = env_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let file_hashes = if !opts.no_cache {
                    cache::hash_copy_sources(&opts.context, src)
                        .with_context(|| format!("COPY: failed to hash sources: {}", src))?
                } else {
                    vec![]
                };
                let hash_refs: Vec<(String, String)> = file_hashes.clone();

                let key_hex = cache::compute_cache_key(&CacheKeyInput {
                    prev_layer_digest: &prev_digest,
                    instruction_text:  &instr_text,
                    workdir:           &workdir,
                    env:               &env_pairs,
                    copy_file_hashes:  Some(&hash_refs),
                });

                // Cache check
                let cached = if !cache_invalidated {
                    cache::lookup(&key_hex)
                } else {
                    None
                };

                if let Some(digest) = cached {
                    let elapsed = step_start.elapsed().as_secs_f64();
                    println!(
                        "Step {}/{} : {} [CACHE HIT] {:.2}s",
                        step_num, total_steps, instr_text, elapsed
                    );
                    // We still need the size for the LayerEntry
                    let size = std::fs::metadata(store::layer_path(&digest))
                        .map(|m| m.len())
                        .unwrap_or(0);
                    layers.push(LayerEntry { digest: digest.clone(), size, created_by: instr_text });
                    prev_digest = digest;
                } else {
                    cache_invalidated = true;
                    let entry = execute_copy(
                        &opts.context,
                        src,
                        dest,
                        &instr_text,
                        &layers,
                        &workdir,
                    )?;
                    let elapsed = step_start.elapsed().as_secs_f64();
                    println!(
                        "Step {}/{} : {} [CACHE MISS] {:.2}s",
                        step_num, total_steps, instr_text, elapsed
                    );
                    if !opts.no_cache {
                        cache::store_entry(&key_hex, &entry.digest)?;
                    }
                    prev_digest = entry.digest.clone();
                    layers.push(entry);
                }
            }

            // ------------------------------------------------------------------
            Instruction::Run { command } => {
                let instr_text = format!("RUN {}", command);
                let step_start = Instant::now();

                let env_pairs: Vec<(String, String)> = env_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                let key_hex = cache::compute_cache_key(&CacheKeyInput {
                    prev_layer_digest: &prev_digest,
                    instruction_text:  &instr_text,
                    workdir:           &workdir,
                    env:               &env_pairs,
                    copy_file_hashes:  None,
                });

                let cached = if !cache_invalidated {
                    cache::lookup(&key_hex)
                } else {
                    None
                };

                if let Some(digest) = cached {
                    let elapsed = step_start.elapsed().as_secs_f64();
                    println!(
                        "Step {}/{} : {} [CACHE HIT] {:.2}s",
                        step_num, total_steps, instr_text, elapsed
                    );
                    let size = std::fs::metadata(store::layer_path(&digest))
                        .map(|m| m.len())
                        .unwrap_or(0);
                    layers.push(LayerEntry { digest: digest.clone(), size, created_by: instr_text });
                    prev_digest = digest;
                } else {
                    cache_invalidated = true;
                    let entry = execute_run(
                        command,
                        &instr_text,
                        &layers,
                        &workdir,
                        &env_map,
                    )?;
                    let elapsed = step_start.elapsed().as_secs_f64();
                    println!(
                        "Step {}/{} : {} [CACHE MISS] {:.2}s",
                        step_num, total_steps, instr_text, elapsed
                    );
                    if !opts.no_cache {
                        cache::store_entry(&key_hex, &entry.digest)?;
                    }
                    prev_digest = entry.digest.clone();
                    layers.push(entry);
                }
            }
        }
    }

    // Compute creation timestamp:
    //   - If all steps were cache hits and a prior build timestamp exists → keep it
    //   - Otherwise → use now()
    let created = if !cache_invalidated {
        original_created.unwrap_or_else(Utc::now)
    } else {
        original_created.unwrap_or_else(Utc::now)
        // Note: on a fully-cold build original_created is None → Utc::now()
        // On a fully-warm build original_created is Some(...) → preserved
    };

    let mut manifest = ImageManifest {
        name: name.clone(),
        tag: tag.clone(),
        digest: String::new(),
        created,
        config,
        layers,
    };
    image::save_manifest(&mut manifest)?;

    let total_elapsed = total_start.elapsed().as_secs_f64();
    println!(
        "Successfully built {} {}:{} ({:.2}s)",
        manifest.digest, name, tag, total_elapsed
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// COPY execution
// ---------------------------------------------------------------------------

fn execute_copy(
    context_dir: &Path,
    src_glob: &str,
    dest: &str,
    created_by: &str,
    existing_layers: &[LayerEntry],
    workdir: &str,
) -> Result<LayerEntry> {
    // Collect matched source files
    let pattern = context_dir.join(src_glob);
    let pattern_str = pattern.to_string_lossy();

    let mut matched_files: Vec<PathBuf> = vec![];
    for entry in glob(&pattern_str)
        .with_context(|| format!("invalid glob: {}", src_glob))?
    {
        let path = entry.context("glob error")?;
        if path.is_file() {
            matched_files.push(path);
        }
    }

    // Also handle "." — copy entire context dir
    if src_glob == "." {
        matched_files.clear();
        for entry in walkdir::WalkDir::new(context_dir).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_file() {
                // Skip the Docksmithfile itself
                if entry.file_name() == "Docksmithfile" {
                    continue;
                }
                matched_files.push(entry.path().to_path_buf());
            }
        }
    }

    if matched_files.is_empty() {
        bail!("COPY: no files matched '{}'", src_glob);
    }

    // Resolve destination path (absolute, relative to workdir if not absolute)
    let dest_path = resolve_dest(dest, workdir);

    // Build (archive_path, host_path) pairs
    let mut pairs: Vec<(PathBuf, PathBuf)> = vec![];
    if src_glob == "." {
        // Copy all files preserving relative structure under dest
        for host_path in &matched_files {
            let rel = host_path.strip_prefix(context_dir).unwrap_or(host_path);
            let archive_path = dest_path.join(rel);
            pairs.push((archive_path, host_path.clone()));
        }
    } else {
        for host_path in &matched_files {
            let filename = host_path.file_name()
                .context("COPY source has no filename")?;
            let archive_path = dest_path.join(filename);
            pairs.push((archive_path, host_path.clone()));
        }
    }

    // Ensure workdir exists in the layer as a directory entry
    let _ = existing_layers; // used by RUN for rootfs assembly; COPY builds delta directly

    layer::create_layer_from_files(&pairs, created_by)
}

fn resolve_dest(dest: &str, workdir: &str) -> PathBuf {
    let d = Path::new(dest);
    if d.is_absolute() {
        d.to_path_buf()
    } else if !workdir.is_empty() {
        Path::new(workdir).join(d)
    } else {
        Path::new("/").join(d)
    }
}

// ---------------------------------------------------------------------------
// RUN execution (with isolation)
// ---------------------------------------------------------------------------

fn execute_run(
    command:       &str,
    created_by:    &str,
    layers:        &[LayerEntry],
    workdir:       &str,
    env_map:       &BTreeMap<String, String>,
) -> Result<LayerEntry> {
    let tmp = tempfile::tempdir().context("failed to create temp dir for RUN")?;
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs)?;

    // 1. Extract all layers built so far into rootfs
    layer::extract_image_layers(layers, &rootfs)?;

    // 2. Ensure workdir exists
    if !workdir.is_empty() {
        let wd_path = rootfs.join(workdir.trim_start_matches('/'));
        std::fs::create_dir_all(&wd_path)?;
    }

    // 3. Snapshot of files before command (for delta computation)
    let before_snap = snapshot_paths(&rootfs)?;

    // 4. Run command inside isolated rootfs
    let env_pairs: Vec<(String, String)> = env_map
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let run_opts = runtime::RunOptions {
        rootfs:  rootfs.clone(),
        command: vec!["sh".to_string(), "-c".to_string(), command.to_string()],
        workdir: workdir.to_string(),
        env:     env_pairs,
        env_overrides: vec![],
    };
    runtime::run_isolated(&run_opts)
        .with_context(|| format!("RUN command failed: {}", command))?;

    // 5. Compute delta (files added or modified)
    let after_snap = snapshot_paths(&rootfs)?;
    let delta_files = compute_delta(&before_snap, &after_snap, &rootfs)?;

    // 6. Package delta into a layer
    if delta_files.is_empty() {
        // RUN produced no filesystem changes — create an empty layer
        let empty: Vec<(PathBuf, PathBuf)> = vec![];
        return layer::create_layer_from_files(&empty, created_by);
    }

    layer::create_layer_from_files(&delta_files, created_by)
}

// ---------------------------------------------------------------------------
// Filesystem snapshot helpers for RUN delta computation
// ---------------------------------------------------------------------------

/// Snapshot: map from relative path → (size, mtime_secs)
type Snapshot = BTreeMap<String, (u64, i64)>;

fn snapshot_paths(rootfs: &Path) -> Result<Snapshot> {
    let mut snap = BTreeMap::new();
    for entry in walkdir::WalkDir::new(rootfs) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let meta = entry.metadata()?;
            let rel = entry.path()
                .strip_prefix(rootfs)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            let mtime = meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            snap.insert(rel, (meta.len(), mtime));
        }
    }
    Ok(snap)
}

fn compute_delta(
    before: &Snapshot,
    after:  &Snapshot,
    rootfs: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut delta = vec![];
    for (rel_path, (size, mtime)) in after {
        let changed = match before.get(rel_path) {
            None                    => true,
            Some((bs, bm)) => size != bs || mtime != bm,
        };
        if changed {
            let archive_path = PathBuf::from("/").join(rel_path);
            let host_path    = rootfs.join(rel_path);
            delta.push((archive_path, host_path));
        }
    }
    // Sort for determinism
    delta.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(delta)
}
