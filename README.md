# Docksmith

A simplified Docker-like build and runtime system built from scratch in Rust.
No Docker, no runc, no containerd — just Linux primitives.

---

## Architecture

```
~/.docksmith/
  images/   — one JSON manifest per image  (name_tag.json)
  layers/   — content-addressed tar files  (named by sha256 digest)
  cache/    — cache key → layer digest     (sha256_hex.json)
```

Single binary, no daemon. Every command reads/writes the state directory directly.

---

## Building

```bash
cargo build --release
# Binary: target/release/docksmith
# Optionally install:
sudo cp target/release/docksmith /usr/local/bin/
```

Requires: Rust 1.75+, Linux (x86_64 or arm64).

---

## Setup: Importing a Base Image

Before any build, you must import a base image. This is done **once**.

```bash
# Export a minimal rootfs from Docker (one-time setup):
docker export $(docker create alpine:3.18) > alpine-3.18.tar

# Import into Docksmith's local store:
docksmith import alpine-3.18.tar alpine:3.18
```

Alternatively, download a pre-built rootfs tarball (e.g. from Alpine Linux's
mini rootfs releases) and import that directly.

---

## Docksmithfile Reference

Only these 6 instructions are supported. Any other instruction fails with a
clear error and line number.

| Instruction | Description |
|---|---|
| `FROM <image>[:<tag>]` | Base image (must exist in local store) |
| `COPY <src> <dest>` | Copy files from build context. Supports `*` and `**` globs. |
| `RUN <command>` | Execute shell command inside isolated rootfs. Creates a layer. |
| `WORKDIR <path>` | Set working directory for subsequent instructions. |
| `ENV key=value` | Set environment variable in image config. |
| `CMD ["exec","arg"]` | Default command. JSON array form required. |

**Example Docksmithfile:**
```dockerfile
FROM alpine:3.18
WORKDIR /app
ENV GREETING=Hello
ENV APP_VERSION=1.0
COPY app.sh /app
RUN chmod +x /app/app.sh
CMD ["/app/app.sh"]
```

---

## CLI Reference

### `docksmith build`

```bash
docksmith build -t name:tag <context_dir>
docksmith build -t name:tag .          # context = current directory
docksmith build --no-cache -t name:tag .
```

Parses the `Docksmithfile` in `<context_dir>`, executes all instructions,
writes the manifest. Prints each step with cache status and duration:

```
Step 1/4 : FROM alpine:3.18
Step 2/4 : WORKDIR /app
Step 3/4 : COPY app.sh /app [CACHE MISS] 0.01s
Step 4/4 : RUN chmod +x /app/app.sh [CACHE MISS] 0.18s
Successfully built sha256:a3f9b2c1... myapp:latest (0.19s)
```

On a warm rebuild (all cache hits):

```
Step 1/4 : FROM alpine:3.18
Step 2/4 : WORKDIR /app
Step 3/4 : COPY app.sh /app [CACHE HIT] 0.00s
Step 4/4 : RUN chmod +x /app/app.sh [CACHE HIT] 0.00s
Successfully built sha256:a3f9b2c1... myapp:latest (0.01s)
```

### `docksmith images`

```bash
docksmith images
```

```
NAME                 TAG          ID              CREATED
------------------------------------------------------------------------
alpine               3.18         d4167d3095f1    2025-01-15 10:30
myapp                latest       a3f9b2c1d5e8    2025-01-15 10:32
```

### `docksmith run`

```bash
docksmith run name:tag
docksmith run name:tag <override_cmd>
docksmith run -e KEY=VALUE name:tag
docksmith run -e GREETING=Howdy myapp:latest
```

Assembles the filesystem, runs the container in the foreground, waits for
exit, prints the exit code.

### `docksmith rmi`

```bash
docksmith rmi name:tag
```

Removes the image manifest and all associated layer files from disk.

### `docksmith import`

```bash
docksmith import <tar_file> <name:tag>
```

Imports a rootfs tarball as a base image. Run once during initial setup.

---

## Build Cache

The cache key for each `COPY` or `RUN` step is a SHA-256 of:

1. Previous layer digest (or base image manifest digest)
2. Full instruction text
3. Current `WORKDIR` value
4. All `ENV` pairs sorted lexicographically
5. *(COPY only)* SHA-256 of each source file, sorted by path

**Cache invalidation rules:**
- Any source file change → that step and all below become misses
- Any instruction text change → that step and all below
- `FROM` image changes → all layer-producing steps
- `WORKDIR` or `ENV` change → that step and all below
- Layer file missing from disk → that step and all below
- `--no-cache` flag → all steps

---

## Process Isolation

`RUN` (during build) and `docksmith run` use the **same** isolation mechanism:

1. All image layers are extracted into a temp directory
2. `unshare(CLONE_NEWUSER | CLONE_NEWNS)` creates new user + mount namespaces
3. `chroot(2)` jails the process into the assembled rootfs
4. The process cannot read or write outside its rootfs

**Kernel requirements:**
- User namespaces enabled: `sysctl kernel.unprivileged_userns_clone=1`
  (or run as root)
- If user namespaces are unavailable, docksmith falls back to plain `chroot`
  (requires root in that case)

---

## Reproducible Builds

Same `Docksmithfile` + same source files = identical digests on every build:
- Tar entries are added in lexicographically sorted path order
- All file timestamps are zeroed (`mtime = 0`) in tar headers
- UID/GID are zeroed (`0/0`) in tar headers
- `ENV` pairs are sorted before inclusion in cache keys

---

## Sample App

```bash
cd sample-app

# Cold build
docksmith build -t myapp:latest .

# Warm rebuild (all cache hits)
docksmith build -t myapp:latest .

# Run
docksmith run myapp:latest

# Override env
docksmith run -e GREETING=Howdy myapp:latest

# Isolation check: write a file inside, verify it's not on the host
docksmith run myapp:latest sh -c "touch /tmp/secret && echo 'wrote /tmp/secret'"
ls /tmp/secret   # must NOT exist on host

# List images
docksmith images

# Remove
docksmith rmi myapp:latest
```

---

## Module Overview

| Module | Responsibility |
|---|---|
| `main.rs` | Entry point, module wiring |
| `cli.rs` | Clap CLI: build / images / run / rmi / import |
| `store.rs` | State directory paths and helpers |
| `image.rs` | Manifest type, digest computation, load/save |
| `layer.rs` | Tar delta creation and extraction |
| `cache.rs` | Cache key computation and lookup |
| `parser.rs` | Docksmithfile parser |
| `build.rs` | Build engine: orchestrates parser → cache → layer → manifest |
| `runtime.rs` | Container runtime: chroot + namespace isolation |
