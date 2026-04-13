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
./target/release/docksmith import alpine-3.18.tar alpine:3.18
```

> **Note:** If Docker requires elevated privileges, use `sudo docker export $(sudo docker create alpine:3.18) > alpine-3.18.tar`

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
./target/release/docksmith build -t name:tag <context_dir>
./target/release/docksmith build -t name:tag .          # context = current directory
./target/release/docksmith build --no-cache -t name:tag .
```

Parses the `Docksmithfile` in `<context_dir>`, executes all instructions,
writes the manifest. Prints each step with cache status and duration:

```
Step 1/7 : FROM alpine:3.18
Step 2/7 : WORKDIR /app
Step 3/7 : ENV GREETING=Hello
Step 4/7 : ENV APP_VERSION=1.0
Step 5/7 : COPY app.sh /app [CACHE MISS] 0.00s
Step 6/7 : RUN chmod +x /app/app.sh [CACHE MISS] 0.03s
Step 7/7 : CMD ["/app/app.sh"]
Successfully built sha256:cafb7feabfa6d667a5b779487034d042c404f4aceb04aaf81907e2872a5eaa84 myapp:latest (0.03s)
```

On a warm rebuild (all cache hits):

```
Step 1/7 : FROM alpine:3.18
Step 2/7 : WORKDIR /app
Step 3/7 : ENV GREETING=Hello
Step 4/7 : ENV APP_VERSION=1.0
Step 5/7 : COPY app.sh /app [CACHE HIT] 0.00s
Step 6/7 : RUN chmod +x /app/app.sh [CACHE HIT] 0.00s
Step 7/7 : CMD ["/app/app.sh"]
Successfully built sha256:cafb7feabfa6d667a5b779487034d042c404f4aceb04aaf81907e2872a5eaa84 myapp:latest (0.00s)
```

Note: the digest is identical across both builds — reproducible by design.

### `docksmith images`

```bash
./target/release/docksmith images
```

```
NAME                 TAG          ID              CREATED
------------------------------------------------------------------------
alpine               3.18         94be3862f23e    2026-04-13 16:27
myapp                latest       cafb7feabfa6    2026-04-13 16:28
```

### `docksmith run`

```bash
./target/release/docksmith run name:tag
./target/release/docksmith run name:tag <override_cmd>
./target/release/docksmith run -e KEY=VALUE name:tag
./target/release/docksmith run -e GREETING=Howdy myapp:latest
```

Assembles the filesystem, runs the container in the foreground, waits for exit.

### `docksmith rmi`

```bash
./target/release/docksmith rmi name:tag
```

Removes the image manifest and all associated layer files from disk.

### `docksmith import`

```bash
./target/release/docksmith import <tar_file> <name:tag>
```

Imports a rootfs tarball as a base image. Run once during initial setup.

---

## Running the Demo

```bash
# 1. Import base image (one-time)
docker export $(docker create alpine:3.18) > alpine-3.18.tar
./target/release/docksmith import alpine-3.18.tar alpine:3.18

# 2. Run the full demo script
./demo.sh
```

The demo script (`demo.sh`) runs all 8 demo scenarios automatically:

| # | Scenario | Expected result |
|---|---|---|
| 1 | Cold build | All layer-producing steps show `[CACHE MISS]` |
| 2 | Warm rebuild | All layer-producing steps show `[CACHE HIT]`, identical digest |
| 3 | Edit source file, rebuild | Affected step and all below show `[CACHE MISS]`, steps above show `[CACHE HIT]` |
| 4 | `docksmith images` | Image listed with Name, Tag, 12-char ID, Created |
| 5 | `docksmith run myapp:latest` | Container starts, produces output, exits cleanly |
| 6 | `docksmith run -e GREETING=Howdy myapp:latest` | ENV override applied inside container |
| 7 | Write file inside container, check host | `PASS` — file does not appear on host filesystem |
| 8 | `docksmith rmi myapp:latest` | Manifest and all layer files removed |

> **Kernel requirement:** User namespaces must be enabled for unprivileged use:
> ```bash
> sudo sysctl kernel.unprivileged_userns_clone=1
> ```
> Without this, run as root or prefix with `sudo DOCKSMITH_HOME=$HOME/.docksmith`.

---

## Build Cache

The cache key for each `COPY` or `RUN` step is a SHA-256 of:

1. Previous layer digest (or base image manifest digest for the first layer-producing step)
2. Full instruction text as written
3. Current `WORKDIR` value at the time the instruction is reached
4. All `ENV` pairs accumulated so far, sorted lexicographically by key
5. *(COPY only)* SHA-256 of each source file's bytes, sorted by path

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

Files written inside a container do not appear on the host filesystem (verified by demo step 7).

---

## Reproducible Builds

Same `Docksmithfile` + same source files = identical layer digests and manifest on every build:
- Tar entries are added in lexicographically sorted path order
- All file timestamps are zeroed (`mtime = 0`) in tar headers
- UID/GID are zeroed (`0/0`) in tar headers
- `ENV` pairs are sorted before inclusion in cache keys
- Manifest `created` timestamp is preserved on all-cache-hit rebuilds so the manifest digest is also identical

---

## Module Overview

| Module | Responsibility |
|---|---|
| `main.rs` | Entry point, module wiring |
| `cli.rs` | Clap CLI: build / images / run / rmi / import |
| `store.rs` | State directory paths and helpers (`$DOCKSMITH_HOME` override supported) |
| `image.rs` | Manifest type, digest computation, load/save, import |
| `layer.rs` — | Tar delta creation (sorted, zeroed timestamps) and extraction |
| `cache.rs` | Cache key computation and lookup |
| `parser.rs` | Docksmithfile parser — strict, line-number errors |
| `build.rs` | Build engine: orchestrates parser → cache → layer → manifest |
| `runtime.rs` | Container runtime: `unshare` + `chroot` isolation |
