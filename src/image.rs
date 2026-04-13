//! image.rs — Image manifest type, digest computation, load/save.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single layer entry inside the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerEntry {
    /// "sha256:<hex>" — content-addressed tar file in layers/
    pub digest: String,
    /// Byte size of the tar file on disk
    pub size: u64,
    /// Human-readable description of what produced this layer
    #[serde(rename = "createdBy")]
    pub created_by: String,
}

/// The image configuration section (runtime environment).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageConfig {
    /// Environment variables as "KEY=value" strings
    #[serde(rename = "Env", default)]
    pub env: Vec<String>,
    /// Default command to run
    #[serde(rename = "Cmd", default)]
    pub cmd: Vec<String>,
    /// Default working directory
    #[serde(rename = "WorkingDir", default)]
    pub working_dir: String,
}

/// The top-level image manifest written to images/<name>_<tag>.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageManifest {
    pub name: String,
    pub tag: String,
    /// "sha256:<hex>" of the canonical JSON (with digest="")
    pub digest: String,
    /// ISO-8601 creation timestamp — set once on first build, preserved on cache-hit rebuilds
    pub created: DateTime<Utc>,
    pub config: ImageConfig,
    pub layers: Vec<LayerEntry>,
}

// ---------------------------------------------------------------------------
// Digest computation
// ---------------------------------------------------------------------------

/// Compute the manifest digest:
/// 1. Clone the manifest and set digest = ""
/// 2. Serialize to JSON (fields in declaration order — deterministic)
/// 3. SHA-256 the bytes
/// 4. Return "sha256:<hex>"
pub fn compute_manifest_digest(manifest: &ImageManifest) -> Result<String> {
    let mut m = manifest.clone();
    m.digest = String::new();
    let json = serde_json::to_vec(&m)
        .context("failed to serialize manifest for digest")?;
    let hash = Sha256::digest(&json);
    Ok(format!("sha256:{}", hex::encode(hash)))
}

/// Compute SHA-256 of arbitrary bytes and return "sha256:<hex>".
pub fn sha256_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("sha256:{}", hex::encode(hash))
}

/// Compute SHA-256 of a file on disk and return "sha256:<hex>".
pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    let data = std::fs::read(path)
        .with_context(|| format!("failed to read {:?} for hashing", path))?;
    Ok(sha256_digest(&data))
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Save a manifest to disk. Recomputes and sets the digest field before writing.
pub fn save_manifest(manifest: &mut ImageManifest) -> Result<()> {
    store::ensure_state_dirs()?;
    manifest.digest = compute_manifest_digest(manifest)?;
    let path = store::manifest_path(&manifest.name, &manifest.tag);
    let json = serde_json::to_string_pretty(manifest)
        .context("failed to serialize manifest")?;
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write manifest to {:?}", path))?;
    Ok(())
}

/// Load a manifest from disk by name:tag reference.
pub fn load_manifest(image_ref: &str) -> Result<ImageManifest> {
    let (name, tag) = store::parse_image_ref(image_ref);
    let path = store::manifest_path(&name, &tag);
    if !path.exists() {
        bail!("image not found: {}:{} (run `docksmith images` to list available images)", name, tag);
    }
    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read manifest {:?}", path))?;
    let manifest: ImageManifest = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse manifest {:?}", path))?;
    Ok(manifest)
}

/// Delete a manifest file. Does not remove layers (caller's responsibility).
pub fn delete_manifest(name: &str, tag: &str) -> Result<()> {
    let path = store::manifest_path(name, tag);
    if !path.exists() {
        bail!("image not found: {}:{}", name, tag);
    }
    std::fs::remove_file(&path)
        .with_context(|| format!("failed to delete manifest {:?}", path))?;
    Ok(())
}

/// Load all manifests in the images/ directory.
pub fn list_manifests() -> Result<Vec<ImageManifest>> {
    let paths = store::list_manifest_paths()?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let json = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {:?}", path))?;
        match serde_json::from_str::<ImageManifest>(&json) {
            Ok(m)  => out.push(m),
            Err(e) => eprintln!("warning: skipping corrupt manifest {:?}: {}", path, e),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Import helper (for `docksmith import`)
// ---------------------------------------------------------------------------

/// Import an OCI/Docker-exported tarball as a base image.
///
/// This is deliberately simple: the tarball is stored verbatim as a single
/// layer and a manifest is synthesized.  For a real multi-layer OCI image you
/// would unpack the index.json and iterate layers — that is out of scope here.
///
/// Expected workflow:
///   docker export <container_id> > alpine.tar
///   docksmith import alpine.tar alpine:3.18
pub fn import_image(tar_path: &std::path::Path, image_ref: &str) -> Result<()> {
    store::ensure_state_dirs()?;
    let (name, tag) = store::parse_image_ref(image_ref);

    // Read the tar and hash it
    let data = std::fs::read(tar_path)
        .with_context(|| format!("cannot read {:?}", tar_path))?;
    let digest = sha256_digest(&data);
    let size = data.len() as u64;

    // Store layer
    let layer_dest = store::layer_path(&digest);
    if !layer_dest.exists() {
        std::fs::write(&layer_dest, &data)
            .with_context(|| format!("failed to write layer {:?}", layer_dest))?;
    }

    // Build manifest
    let mut manifest = ImageManifest {
        name: name.clone(),
        tag: tag.clone(),
        digest: String::new(),   // filled in by save_manifest
        created: Utc::now(),
        config: ImageConfig::default(),
        layers: vec![LayerEntry {
            digest,
            size,
            created_by: format!("imported from {}", tar_path.display()),
        }],
    };

    save_manifest(&mut manifest)?;
    println!("Imported {}:{} ({})", name, tag, manifest.digest);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> ImageManifest {
        ImageManifest {
            name: "test".to_string(),
            tag: "latest".to_string(),
            digest: String::new(),
            created: DateTime::from_timestamp(0, 0).unwrap(),
            config: ImageConfig {
                env: vec!["PATH=/usr/bin".to_string()],
                cmd: vec!["sh".to_string()],
                working_dir: "/".to_string(),
            },
            layers: vec![LayerEntry {
                digest: "sha256:abc123".to_string(),
                size: 1024,
                created_by: "COPY . /app".to_string(),
            }],
        }
    }

    #[test]
    fn digest_is_deterministic() {
        let m = sample_manifest();
        let d1 = compute_manifest_digest(&m).unwrap();
        let d2 = compute_manifest_digest(&m).unwrap();
        assert_eq!(d1, d2);
        assert!(d1.starts_with("sha256:"));
    }

    #[test]
    fn digest_changes_when_content_changes() {
        let m1 = sample_manifest();
        let mut m2 = sample_manifest();
        m2.config.working_dir = "/app".to_string();
        let d1 = compute_manifest_digest(&m1).unwrap();
        let d2 = compute_manifest_digest(&m2).unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn digest_field_does_not_affect_digest() {
        // Setting digest to some existing value must not change the computed hash
        let mut m1 = sample_manifest();
        let mut m2 = sample_manifest();
        m2.digest = "sha256:somepreviousvalue".to_string();
        m1.digest = String::new();
        let d1 = compute_manifest_digest(&m1).unwrap();
        let d2 = compute_manifest_digest(&m2).unwrap();
        assert_eq!(d1, d2, "digest field must be excluded from hash computation");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("DOCKSMITH_HOME", tmp.path());
        store::ensure_state_dirs().unwrap();

        let mut m = sample_manifest();
        save_manifest(&mut m).unwrap();
        assert!(!m.digest.is_empty());

        let loaded = load_manifest("test:latest").unwrap();
        assert_eq!(loaded.name, "test");
        assert_eq!(loaded.tag, "latest");
        assert_eq!(loaded.digest, m.digest);
        assert_eq!(loaded.config.cmd, vec!["sh"]);
    }
}
