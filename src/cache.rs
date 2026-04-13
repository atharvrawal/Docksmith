//! cache.rs — Build cache (cache key → layer digest mapping).
//!
//! Cache key components (all hashed together with SHA-256):
//!   1. Previous layer digest (or base image manifest digest for first step)
//!   2. Full instruction text as written
//!   3. Current WORKDIR at the time of execution
//!   4. Current ENV state, sorted by key lexicographically
//!   5. (COPY only) SHA-256 of each source file, sorted by path

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::Path;

use crate::store;

// ---------------------------------------------------------------------------
// Cache entry stored on disk
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key:    String,   // "sha256:<hex>" of the cache key material
    pub digest: String,   // "sha256:<hex>" of the resulting layer
}

// ---------------------------------------------------------------------------
// Cache key computation
// ---------------------------------------------------------------------------

/// All inputs that go into the cache key for one build step.
pub struct CacheKeyInput<'a> {
    /// Digest of the layer produced by the immediately preceding COPY/RUN,
    /// or the base image's manifest digest for the very first layer-producing step.
    pub prev_layer_digest: &'a str,
    /// The raw instruction text (e.g. "RUN pip install -r requirements.txt")
    pub instruction_text:  &'a str,
    /// Current WORKDIR value (empty string if not set)
    pub workdir:           &'a str,
    /// All ENV pairs accumulated so far, in any order (we sort them)
    pub env: &'a [(String, String)],
    /// For COPY only: list of (sorted path, sha256) of every source file
    pub copy_file_hashes:  Option<&'a [(String, String)]>,
}

/// Compute a deterministic SHA-256 cache key from the given inputs.
/// Returns the hex string (without "sha256:" prefix).
pub fn compute_cache_key(input: &CacheKeyInput) -> String {
    let mut h = Sha256::new();

    // 1. Previous layer digest
    h.update(input.prev_layer_digest.as_bytes());
    h.update(b"\0");

    // 2. Instruction text
    h.update(input.instruction_text.as_bytes());
    h.update(b"\0");

    // 3. WORKDIR
    h.update(input.workdir.as_bytes());
    h.update(b"\0");

    // 4. ENV sorted by key
    let mut env_sorted: Vec<&(String, String)> = input.env.iter().collect();
    env_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in &env_sorted {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update(b"\0");
    }
    h.update(b"\0");

    // 5. COPY file hashes (sorted by path — caller must provide already sorted)
    if let Some(files) = input.copy_file_hashes {
        for (path, hash) in files {
            h.update(path.as_bytes());
            h.update(b"=");
            h.update(hash.as_bytes());
            h.update(b"\0");
        }
    }

    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// Lookup and store
// ---------------------------------------------------------------------------

/// Look up a cache entry. Returns the stored layer digest if present
/// AND the layer file exists on disk. A missing layer file counts as a miss.
pub fn lookup(key_hex: &str) -> Option<String> {
    let path = store::cache_entry_path(key_hex);
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&data).ok()?;
    // Verify layer still exists
    if store::layer_exists(&entry.digest) {
        Some(entry.digest)
    } else {
        None
    }
}

/// Write a cache entry mapping key_hex → layer_digest.
pub fn store_entry(key_hex: &str, layer_digest: &str) -> Result<()> {
    let entry = CacheEntry {
        key:    format!("sha256:{}", key_hex),
        digest: layer_digest.to_string(),
    };
    let json = serde_json::to_string(&entry)
        .context("failed to serialize cache entry")?;
    let path = store::cache_entry_path(key_hex);
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write cache entry {:?}", path))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Source file hashing helper (used by build engine for COPY)
// ---------------------------------------------------------------------------

/// Compute SHA-256 of every file matched by a glob, returned as a sorted
/// vec of (relative_path_string, sha256_hex).
pub fn hash_copy_sources(
    context_dir: &Path,
    src_glob: &str,
) -> Result<Vec<(String, String)>> {
    use glob::glob;
    use sha2::Digest as Sha256Digest;

    // Build the glob pattern relative to context_dir
    let pattern = context_dir.join(src_glob);
    let pattern_str = pattern.to_string_lossy();

    let mut results: Vec<(String, String)> = Vec::new();

    for entry in glob(&pattern_str)
        .with_context(|| format!("invalid glob pattern: {}", src_glob))?
    {
        let path = entry.context("glob error")?;
        if path.is_file() {
            let data = std::fs::read(&path)
                .with_context(|| format!("cannot read {:?}", path))?;
            let hash = hex::encode(Sha256::digest(&data));
            // Use the path relative to context_dir as the key
            let rel = path.strip_prefix(context_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            results.push((rel, hash));
        }
    }

    // Sort by path for determinism
    results.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input<'a>() -> CacheKeyInput<'a> {
        CacheKeyInput {
            prev_layer_digest: "sha256:base",
            instruction_text:  "COPY . /app",
            workdir:           "/app",
            env:               &[],
            copy_file_hashes:  None,
        }
    }

    #[test]
    fn key_is_deterministic() {
        let k1 = compute_cache_key(&base_input());
        let k2 = compute_cache_key(&base_input());
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_changes_on_instruction_change() {
        let k1 = compute_cache_key(&base_input());
        let k2 = compute_cache_key(&CacheKeyInput {
            instruction_text: "COPY src /app",
            ..base_input()
        });
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_changes_on_prev_digest_change() {
        let k1 = compute_cache_key(&base_input());
        let k2 = compute_cache_key(&CacheKeyInput {
            prev_layer_digest: "sha256:different",
            ..base_input()
        });
        assert_ne!(k1, k2);
    }

    #[test]
    fn env_order_does_not_affect_key() {
        // ENV pairs are sorted, so order of input shouldn't matter
        let env1 = vec![
            ("Z_VAR".to_string(), "1".to_string()),
            ("A_VAR".to_string(), "2".to_string()),
        ];
        let env2 = vec![
            ("A_VAR".to_string(), "2".to_string()),
            ("Z_VAR".to_string(), "1".to_string()),
        ];
        let k1 = compute_cache_key(&CacheKeyInput { env: &env1, ..base_input() });
        let k2 = compute_cache_key(&CacheKeyInput { env: &env2, ..base_input() });
        assert_eq!(k1, k2, "env order must not affect the cache key");
    }

    #[test]
    fn cache_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("DOCKSMITH_HOME", tmp.path());
        store::ensure_state_dirs().unwrap();

        // Fake a layer file so the existence check passes
        let fake_digest = "sha256:deadbeef";
        std::fs::write(store::layer_path(fake_digest), b"fake").unwrap();

        let key = compute_cache_key(&base_input());
        store_entry(&key, fake_digest).unwrap();

        let hit = lookup(&key);
        assert_eq!(hit, Some(fake_digest.to_string()));
    }

    #[test]
    fn cache_miss_when_layer_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("DOCKSMITH_HOME", tmp.path());
        store::ensure_state_dirs().unwrap();

        let key = compute_cache_key(&base_input());
        // Write a cache entry pointing to a layer that doesn't exist
        store_entry(&key, "sha256:nonexistent").unwrap();

        // Should be a miss because the layer file isn't there
        let hit = lookup(&key);
        assert!(hit.is_none(), "must be a miss when layer file is absent");
    }
}
