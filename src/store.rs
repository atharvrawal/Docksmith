//! store.rs — State directory layout and path helpers.
//!
//! All state lives under ~/.docksmith/ (or $DOCKSMITH_HOME if set):
//!
//!   images/  — one JSON manifest per image, named "<name>_<tag>.json"
//!   layers/  — content-addressed tar files, named by sha256 digest
//!   cache/   — one JSON file per cache entry, named by sha256 of the cache key

use anyhow::{Context, Result};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Root directory
// ---------------------------------------------------------------------------

/// Returns the root state directory, honouring $DOCKSMITH_HOME for tests.
pub fn state_dir() -> PathBuf {
    if let Ok(h) = std::env::var("DOCKSMITH_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".docksmith")
}

pub fn images_dir() -> PathBuf { state_dir().join("images") }
pub fn layers_dir() -> PathBuf { state_dir().join("layers") }
pub fn cache_dir()  -> PathBuf { state_dir().join("cache") }

/// Create the three subdirectories if they don't exist yet.
/// Called at the start of every CLI command.
pub fn ensure_state_dirs() -> Result<()> {
    std::fs::create_dir_all(images_dir())
        .context("failed to create images dir")?;
    std::fs::create_dir_all(layers_dir())
        .context("failed to create layers dir")?;
    std::fs::create_dir_all(cache_dir())
        .context("failed to create cache dir")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Image manifest paths
// ---------------------------------------------------------------------------

/// Convert "name:tag" into the filename used on disk.
/// "myapp:latest" → "myapp_latest.json"
/// Colons are not valid in filenames on all filesystems, so we replace with _.
pub fn manifest_filename(name: &str, tag: &str) -> String {
    format!("{}_{}.json", name, tag)
}

/// Full path to a manifest file.
pub fn manifest_path(name: &str, tag: &str) -> PathBuf {
    images_dir().join(manifest_filename(name, tag))
}

/// Parse "name:tag" — if no colon, tag defaults to "latest".
pub fn parse_image_ref(image_ref: &str) -> (String, String) {
    match image_ref.split_once(':') {
        Some((name, tag)) => (name.to_string(), tag.to_string()),
        None              => (image_ref.to_string(), "latest".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Layer paths
// ---------------------------------------------------------------------------

/// Full path to a layer tar file, given its digest string ("sha256:<hex>").
pub fn layer_path(digest: &str) -> PathBuf {
    // digest is "sha256:<hex>"; use just the hex part as the filename
    let filename = digest.strip_prefix("sha256:").unwrap_or(digest);
    layers_dir().join(filename)
}

/// Return true if the layer tar file exists on disk.
pub fn layer_exists(digest: &str) -> bool {
    layer_path(digest).exists()
}

// ---------------------------------------------------------------------------
// Cache entry paths
// ---------------------------------------------------------------------------

/// Full path to a cache entry file, given the cache key's hex digest.
pub fn cache_entry_path(key_hex: &str) -> PathBuf {
    cache_dir().join(format!("{}.json", key_hex))
}

// ---------------------------------------------------------------------------
// Listing helpers
// ---------------------------------------------------------------------------

/// Return all manifest paths under images/.
pub fn list_manifest_paths() -> Result<Vec<PathBuf>> {
    let dir = images_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut paths = vec![];
    for entry in std::fs::read_dir(&dir).context("failed to read images dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();   // deterministic order for `docksmith images`
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_image_ref_with_tag() {
        let (name, tag) = parse_image_ref("alpine:3.18");
        assert_eq!(name, "alpine");
        assert_eq!(tag, "3.18");
    }

    #[test]
    fn test_parse_image_ref_no_tag() {
        let (name, tag) = parse_image_ref("myapp");
        assert_eq!(name, "myapp");
        assert_eq!(tag, "latest");
    }

    #[test]
    fn test_manifest_filename() {
        assert_eq!(manifest_filename("myapp", "latest"), "myapp_latest.json");
        assert_eq!(manifest_filename("alpine", "3.18"), "alpine_3.18.json");
    }

    #[test]
    fn test_layer_path_strips_prefix() {
        std::env::set_var("DOCKSMITH_HOME", "/tmp/docksmith_test");
        let p = layer_path("sha256:abcdef1234");
        assert!(p.to_str().unwrap().ends_with("abcdef1234"));
    }
}
