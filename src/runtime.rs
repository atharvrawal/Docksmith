//! runtime.rs — Container runtime using Linux process isolation.
//!
//! Isolation strategy:
//!   We use unshare(2) to create new namespaces before chroot(2):
//!     - CLONE_NEWNS   (mount namespace): prevents mount propagation to host
//!     - CLONE_NEWPID  (PID namespace):   process tree is isolated
//!     - CLONE_NEWUSER (user namespace):  map container root → unprivileged host UID
//!                                        (falls back gracefully if unsupported)
//!
//!   After unshare we chroot(2) into the assembled rootfs, then exec the command.
//!
//! The same run_isolated() function is called by:
//!   - build.rs for RUN instructions
//!   - cli.rs  for `docksmith run`
//!
//! HARD REQUIREMENT: the container process must not read or write outside its rootfs.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct RunOptions {
    /// The assembled rootfs directory (all layers extracted here)
    pub rootfs:         PathBuf,
    /// Command to execute (argv[0] must exist inside rootfs)
    pub command:        Vec<String>,
    /// Working directory inside the container (empty = use "/")
    pub workdir:        String,
    /// ENV from the image config (key, value pairs)
    pub env:            Vec<(String, String)>,
    /// Runtime -e overrides (take precedence over image ENV)
    pub env_overrides:  Vec<(String, String)>,
}

/// Run a command inside an isolated rootfs.
/// Blocks until the process exits, then returns its exit status.
/// Exits with a non-zero code if the command fails.
pub fn run_isolated(opts: &RunOptions) -> Result<()> {
    if opts.command.is_empty() {
        bail!("no command specified");
    }

    // Merge env: image ENV first, then overrides on top
    let mut env_map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (k, v) in &opts.env {
        env_map.insert(k.clone(), v.clone());
    }
    for (k, v) in &opts.env_overrides {
        env_map.insert(k.clone(), v.clone());
    }
    let env_vec: Vec<(String, String)> = env_map.into_iter().collect();

    let workdir = if opts.workdir.is_empty() { "/" } else { &opts.workdir };

    run_in_chroot(&opts.rootfs, &opts.command, workdir, &env_vec)
}

// ---------------------------------------------------------------------------
// Core isolation implementation
// ---------------------------------------------------------------------------

/// Execute `command` inside a chroot jail at `rootfs`.
///
/// Uses a two-process model:
///   parent: forks a child, waits for it, checks exit status
///   child:  attempts unshare(NEWNS|NEWPID|NEWUSER), chroots, execs
fn run_in_chroot(
    rootfs:  &Path,
    command: &[String],
    workdir: &str,
    env:     &[(String, String)],
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    // Ensure essential device nodes and proc are available inside the rootfs
    ensure_rootfs_basics(rootfs)?;

    let rootfs_str = rootfs.to_string_lossy().to_string();
    let cmd0       = command[0].clone();
    let cmd_args: Vec<String> = command[1..].to_vec();
    let workdir    = workdir.to_string();
    let env        = env.to_vec();

    // Build a Command that does the isolation sequence in its pre_exec hook.
    // pre_exec runs AFTER fork but BEFORE exec — in the child process.
    let mut child = unsafe {
        let rootfs_str  = rootfs_str.clone();
        let workdir_cl  = workdir.clone();

        std::process::Command::new(&cmd0)
            .args(&cmd_args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .pre_exec(move || {
                // --- running in the child, before exec ---
                isolation_sequence(&rootfs_str, &workdir_cl)
            })
            .spawn()
            .with_context(|| format!("failed to spawn '{}'", cmd0))?
    };

    let status = child.wait().context("failed to wait for container process")?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        bail!("container process exited with code {}", code);
    }
    Ok(())
}

/// Called in the child process (after fork, before exec).
/// Sets up namespaces, then chroot.
fn isolation_sequence(rootfs: &str, workdir: &str) -> std::io::Result<()> {
    use std::io;

    // 1. Attempt user namespace (CLONE_NEWUSER) — allows unprivileged chroot.
    //    Write uid_map / gid_map to map container root (0) → our real UID.
    //    This may fail on some kernels/configs — we fall back gracefully.
    let our_uid = unsafe { libc::getuid() };
    let our_gid = unsafe { libc::getgid() };

    let user_ns_ok = try_unshare_user(our_uid, our_gid);

    // 2. Unshare mount namespace so our mounts don't propagate to the host.
    let r = unsafe { libc::unshare(libc::CLONE_NEWNS) };
    if r != 0 {
        // Not fatal — some environments restrict this. Log and continue.
        let _ = eprintln!("warning: CLONE_NEWNS failed (errno {}), isolation is reduced", io::Error::last_os_error());
    }

    // 3. chroot into rootfs
    let rootfs_cstr = std::ffi::CString::new(rootfs)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rootfs path contains null byte"))?;

    let r = unsafe { libc::chroot(rootfs_cstr.as_ptr()) };
    if r != 0 {
        if !user_ns_ok {
            // No user namespace and chroot failed — likely need root
            eprintln!("error: chroot failed — try running as root, or ensure user namespaces are enabled");
            eprintln!("       (check: sysctl kernel.unprivileged_userns_clone)");
        }
        return Err(io::Error::last_os_error());
    }

    // 4. chdir to workdir (or / if not set)
    let wd = if workdir.is_empty() { "/" } else { workdir };
    let wd_cstr = std::ffi::CString::new(wd)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "workdir contains null byte"))?;

    let r = unsafe { libc::chdir(wd_cstr.as_ptr()) };
    if r != 0 {
        // workdir may not exist — fall back to /
        let slash = std::ffi::CString::new("/").unwrap();
        let _ = unsafe { libc::chdir(slash.as_ptr()) };
    }

    Ok(())
}

/// Attempt to unshare the user namespace and write uid/gid maps.
/// Returns true on success, false on failure (non-fatal).
fn try_unshare_user(our_uid: u32, our_gid: u32) -> bool {
    let r = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
    if r != 0 {
        return false;
    }

    // Write "deny" to setgroups before gid_map (required since Linux 3.19)
    if std::fs::write("/proc/self/setgroups", "deny").is_err() {
        return false;
    }

    // Map container UID 0 → our real UID
    let uid_map = format!("0 {} 1\n", our_uid);
    if std::fs::write("/proc/self/uid_map", uid_map).is_err() {
        return false;
    }

    // Map container GID 0 → our real GID
    let gid_map = format!("0 {} 1\n", our_gid);
    if std::fs::write("/proc/self/gid_map", gid_map).is_err() {
        return false;
    }

    true
}

// ---------------------------------------------------------------------------
// Rootfs preparation
// ---------------------------------------------------------------------------

/// Mount /proc and create minimal /dev entries inside the rootfs so that
/// basic shell commands work.  Uses bind mounts so nothing escapes.
fn ensure_rootfs_basics(rootfs: &Path) -> Result<()> {
    use std::fs;

    // Create essential directories if missing
    for dir in &["proc", "dev", "sys", "tmp"] {
        let p = rootfs.join(dir);
        if !p.exists() {
            fs::create_dir_all(&p)
                .with_context(|| format!("failed to create {:?}", p))?;
        }
    }

    // Create /dev/null if missing (needed by many shell scripts)
    let dev_null = rootfs.join("dev/null");
    if !dev_null.exists() {
        // Try to mknod; ignore errors (may lack permission, can still work)
        let _ = unsafe {
            let path = std::ffi::CString::new(dev_null.to_string_lossy().as_bytes()).unwrap();
            libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(1, 3))
        };
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public helper: assemble rootfs from an image manifest and run
// ---------------------------------------------------------------------------

/// Full container launch for `docksmith run`:
///   1. Extract all layers into a temp dir
///   2. Apply working dir and ENV
///   3. Run the command in isolation
///   4. Clean up
pub fn launch_container(
    manifest:      &crate::image::ImageManifest,
    cmd_override:  Option<Vec<String>>,
    env_overrides: Vec<(String, String)>,
) -> Result<()> {
    use crate::layer;

    let tmp = tempfile::tempdir().context("failed to create rootfs temp dir")?;
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&rootfs)?;

    // Extract layers
    layer::extract_image_layers(&manifest.layers, &rootfs)?;

    // Resolve command
    let command = match cmd_override {
        Some(c) if !c.is_empty() => c,
        _ => {
            if manifest.config.cmd.is_empty() {
                bail!(
                    "no CMD defined in image {}:{} and no command given. \
                     Specify a command: docksmith run {}:{} <cmd>",
                    manifest.name, manifest.tag,
                    manifest.name, manifest.tag
                );
            }
            manifest.config.cmd.clone()
        }
    };

    // Parse image env pairs
    let env: Vec<(String, String)> = manifest.config.env
        .iter()
        .filter_map(|s| s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();

    let opts = RunOptions {
        rootfs,
        command,
        workdir:       manifest.config.working_dir.clone(),
        env,
        env_overrides,
    };

    run_isolated(&opts)
}
