#!/bin/bash
# demo.sh — Full Docksmith demo sequence.
# Run from the repo root after building: cargo build --release

set -e

DOCKSMITH="./target/release/docksmith"
SAMPLE="./sample-app"

# Allow override of state dir for clean demo runs
export DOCKSMITH_HOME="${DOCKSMITH_HOME:-$HOME/.docksmith}"

echo "========================================"
echo "  Docksmith Demo"
echo "  State dir: $DOCKSMITH_HOME"
echo "========================================"
echo

# ── Setup ────────────────────────────────────────────────────────────────────
echo "--- SETUP: checking for alpine:3.18 base image ---"
if ! $DOCKSMITH images 2>/dev/null | grep -q "alpine"; then
  echo "Base image not found. Import it first:"
  echo "  docker export \$(docker create alpine:3.18) > alpine-3.18.tar"
  echo "  $DOCKSMITH import alpine-3.18.tar alpine:3.18"
  exit 1
fi
echo "Base image found."
echo

# ── Demo 1: Cold build ────────────────────────────────────────────────────────
echo "=== Demo 1: Cold build (all CACHE MISS) ==="
$DOCKSMITH rmi myapp:latest 2>/dev/null || true
$DOCKSMITH build -t myapp:latest "$SAMPLE"
echo

# ── Demo 2: Warm rebuild ──────────────────────────────────────────────────────
echo "=== Demo 2: Warm rebuild (all CACHE HIT) ==="
$DOCKSMITH build -t myapp:latest "$SAMPLE"
echo

# ── Demo 3: Partial invalidation ─────────────────────────────────────────────
echo "=== Demo 3: Edit source file → partial cache invalidation ==="
echo "# modified" >> "$SAMPLE/app.sh"
$DOCKSMITH build -t myapp:latest "$SAMPLE"
# Restore
sed -i '/^# modified/d' "$SAMPLE/app.sh"
echo

# ── Demo 4: docksmith images ──────────────────────────────────────────────────
echo "=== Demo 4: docksmith images ==="
$DOCKSMITH images
echo

# ── Demo 5: docksmith run ─────────────────────────────────────────────────────
echo "=== Demo 5: docksmith run myapp:latest ==="
$DOCKSMITH run myapp:latest
echo

# ── Demo 6: env override ──────────────────────────────────────────────────────
echo "=== Demo 6: docksmith run -e GREETING=Howdy myapp:latest ==="
$DOCKSMITH run -e GREETING=Howdy myapp:latest
echo

# ── Demo 7: Isolation check ───────────────────────────────────────────────────
echo "=== Demo 7: Isolation check ==="
SENTINEL="/tmp/docksmith_isolation_test_$$"
echo "Writing $SENTINEL inside the container..."
$DOCKSMITH run myapp:latest sh -c "touch $SENTINEL && echo 'wrote $SENTINEL inside container'"
if [ -f "$SENTINEL" ]; then
  echo "FAIL: $SENTINEL exists on host — isolation broken!"
  exit 1
else
  echo "PASS: $SENTINEL does not exist on host — isolation works."
fi
echo

# ── Demo 8: docksmith rmi ─────────────────────────────────────────────────────
echo "=== Demo 8: docksmith rmi myapp:latest ==="
$DOCKSMITH rmi myapp:latest
$DOCKSMITH images
echo

echo "========================================"
echo "  All demos complete."
echo "========================================"
