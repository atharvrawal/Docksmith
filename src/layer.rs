//! layer.rs — Tar delta layer creation and extraction.
//!
//! A layer is a tar archive of *only* the files added or modified by one
//! build instruction (COPY or RUN).  Layers are content-addressed: the file
//! on disk is named by the SHA-256 of its raw bytes.
//!
//! Reproducibility rules (MUST be followed everywhere):
//!   - All tar entries are added in lexicographically sorted path order.
//!   - Every entry's mtime is zeroed (unix timestamp 0).
//!   - uid/gid are set to 0 (root) for determinism.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use tar::{Archive, Builder, Header};
use walkdir::WalkDir;

use crate::{image::LayerEntry, store};

// ---------------------------------------------------------------------------
// Create a layer from a set of (archive_path, source_path) pairs
// ---------------------------------------------------------------------------

/// Build a tar layer from an explicit list of files.
///
/// Each element is (path_inside_tar, actual_file_on_host).
/// The list is sorted by archive path before packing to ensure determinism.
/// Returns a LayerEntry with the digest, size, and description.
pub fn create_layer_from_files(
    files: &[(PathBuf, PathBuf)],   // (archive path, host path)
    created_by: &str,
) -> Result<LayerEntry> {
    // Sort by archive path for determinism
    let mut files: Vec<&(PathBuf, PathBuf)> = files.iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut builder = Builder::new(&mut buf);

        for (archive_path, host_path) in &files {
            append_path_deterministic(&mut builder, host_path, archive_path)
                .with_context(|| format!("failed to add {:?} to layer", archive_path))?;
        }

        builder.finish().context("failed to finalise tar")?;
    }

    store_layer_buf(&buf, created_by)
}

/// Build a layer from an entire directory tree rooted at `dir`.
/// The archive paths are relative to `prefix` (e.g. "/app").
///
/// Used by COPY to package everything copied into the image.
pub fn create_layer_from_dir(
    dir: &Path,
    prefix: &Path,
    created_by: &str,
) -> Result<LayerEntry> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut builder = Builder::new(&mut buf);

        // Collect all entries first so we can sort
        let mut entries: Vec<(PathBuf, PathBuf)> = vec![];

        // Add the prefix directory itself first (if not root)
        if prefix != Path::new("/") {
            entries.push((prefix.to_path_buf(), dir.to_path_buf()));
        }

        for entry in WalkDir::new(dir).sort_by_file_name() {
            let entry = entry.context("walkdir error")?;
            let rel = entry.path().strip_prefix(dir)
                .context("strip_prefix failed")?;
            if rel == Path::new("") {
                continue;  // skip the root itself (added above)
            }
            let archive_path = if prefix == Path::new("/") {
                Path::new("/").join(rel)
            } else {
                prefix.join(rel)
            };
            entries.push((archive_path, entry.path().to_path_buf()));
        }

        // Sort by archive path for determinism
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (archive_path, host_path) in &entries {
            append_path_deterministic(&mut builder, host_path, archive_path)
                .with_context(|| format!("failed to add {:?}", archive_path))?;
        }

        builder.finish().context("failed to finalise tar")?;
    }

    store_layer_buf(&buf, created_by)
}

/// Build a layer from a rootfs delta directory produced by a RUN command.
///
/// `delta_dir` contains only the files that were created or modified inside
/// the container during this RUN step.  We pack everything under `/`.
pub fn create_layer_from_rootfs_delta(
    delta_dir: &Path,
    created_by: &str,
) -> Result<LayerEntry> {
    create_layer_from_dir(delta_dir, Path::new("/"), created_by)
}

// ---------------------------------------------------------------------------
// Internal: deterministic tar entry appender
// ---------------------------------------------------------------------------

fn append_path_deterministic<W: Write>(
    builder: &mut Builder<W>,
    host_path: &Path,
    archive_path: &Path,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(host_path)
        .with_context(|| format!("cannot stat {:?}", host_path))?;

    // Strip leading slash from archive path (tar convention)
    let archive_path_str = archive_path
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string();

    let mut header = Header::new_gnu();
    header.set_metadata(&meta);
    header.set_mtime(0);     // zeroed for reproducibility
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("")?;
    header.set_groupname("")?;

    if meta.is_dir() {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_cksum();
        builder.append_data(&mut header, &archive_path_str, std::io::empty())
            .context("failed to append dir entry")?;
    } else if meta.is_symlink() {
        let target = std::fs::read_link(host_path)
            .with_context(|| format!("cannot read symlink {:?}", host_path))?;
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name(&target)?;
        header.set_size(0);
        header.set_cksum();
        builder.append_data(&mut header, &archive_path_str, std::io::empty())
            .context("failed to append symlink entry")?;
    } else {
        // Regular file
        let data = std::fs::read(host_path)
            .with_context(|| format!("cannot read {:?}", host_path))?;
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, &archive_path_str, data.as_slice())
            .context("failed to append file entry")?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Store a finished tar buffer as a content-addressed layer
// ---------------------------------------------------------------------------

fn store_layer_buf(buf: &[u8], created_by: &str) -> Result<LayerEntry> {
    let hash = Sha256::digest(buf);
    let digest = format!("sha256:{}", hex::encode(hash));
    let size = buf.len() as u64;

    let dest = store::layer_path(&digest);
    if !dest.exists() {
        std::fs::write(&dest, buf)
            .with_context(|| format!("failed to write layer {:?}", dest))?;
    }

    Ok(LayerEntry {
        digest,
        size,
        created_by: created_by.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Extract layers into a rootfs directory
// ---------------------------------------------------------------------------

/// Extract a single layer tar into `dest_dir`.
/// Later layers overwrite earlier ones at the same path.
pub fn extract_layer(digest: &str, dest_dir: &Path) -> Result<()> {
    let layer_path = store::layer_path(digest);
    let file = std::fs::File::open(&layer_path)
        .with_context(|| format!("cannot open layer {:?}", layer_path))?;

    let mut archive = Archive::new(file);
    archive.set_overwrite(true);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(false);   // don't restore zeroed mtimes

    archive.unpack(dest_dir)
        .with_context(|| format!("failed to extract layer {} into {:?}", digest, dest_dir))?;

    Ok(())
}

/// Extract all layers of an image in order into `dest_dir`.
pub fn extract_image_layers(
    layers: &[LayerEntry],
    dest_dir: &Path,
) -> Result<()> {
    for entry in layers {
        if !store::layer_exists(&entry.digest) {
            anyhow::bail!(
                "layer {} is missing from the store (image may be corrupt)",
                entry.digest
            );
        }
        extract_layer(&entry.digest, dest_dir)
            .with_context(|| format!("failed to extract layer {}", entry.digest))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn layer_is_deterministic() {
        let tmp = tempdir().unwrap();
        std::env::set_var("DOCKSMITH_HOME", tmp.path().join("state"));
        store::ensure_state_dirs().unwrap();

        // Write two identical files
        let src1 = tmp.path().join("src1");
        let src2 = tmp.path().join("src2");
        std::fs::write(&src1, b"hello world").unwrap();
        std::fs::write(&src2, b"hello world").unwrap();

        let files1 = vec![(PathBuf::from("/app/hello.txt"), src1.clone())];
        let files2 = vec![(PathBuf::from("/app/hello.txt"), src2.clone())];

        let e1 = create_layer_from_files(&files1, "COPY test").unwrap();
        let e2 = create_layer_from_files(&files2, "COPY test").unwrap();

        assert_eq!(e1.digest, e2.digest, "same content must produce same digest");
    }

    #[test]
    fn layer_differs_on_content_change() {
        let tmp = tempdir().unwrap();
        std::env::set_var("DOCKSMITH_HOME", tmp.path().join("state"));
        store::ensure_state_dirs().unwrap();

        let f1 = tmp.path().join("f1");
        let f2 = tmp.path().join("f2");
        std::fs::write(&f1, b"aaa").unwrap();
        std::fs::write(&f2, b"bbb").unwrap();

        let e1 = create_layer_from_files(
            &[(PathBuf::from("/x"), f1)], "test").unwrap();
        let e2 = create_layer_from_files(
            &[(PathBuf::from("/x"), f2)], "test").unwrap();

        assert_ne!(e1.digest, e2.digest);
    }

    #[test]
    fn roundtrip_create_and_extract() {
        let tmp = tempdir().unwrap();
        std::env::set_var("DOCKSMITH_HOME", tmp.path().join("state"));
        store::ensure_state_dirs().unwrap();

        // Create a small file to layer
        let src_file = tmp.path().join("hello.txt");
        std::fs::write(&src_file, b"round trip test").unwrap();

        let entry = create_layer_from_files(
            &[(PathBuf::from("/app/hello.txt"), src_file)],
            "COPY test",
        ).unwrap();

        // Extract into a new directory
        let dest = tmp.path().join("rootfs");
        std::fs::create_dir_all(&dest).unwrap();
        extract_layer(&entry.digest, &dest).unwrap();

        // Verify the file exists and has the right contents
        let extracted = dest.join("app/hello.txt");
        assert!(extracted.exists(), "extracted file must exist");
        assert_eq!(std::fs::read(&extracted).unwrap(), b"round trip test");
    }
}
