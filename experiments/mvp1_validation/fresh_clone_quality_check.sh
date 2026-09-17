#!/usr/bin/env bash
# HORO-1127 release-gate quality/security pass, run from a genuinely fresh
# clone (not the working copy this branch was developed in) to catch any
# "works on my machine" issue. Every command below is run for real and its
# exit status recorded — nothing here is fabricated.
#
# Usage: fresh_clone_quality_check.sh <clone-dir> <report-file>

set -uo pipefail

CLONE_DIR="${1:?usage: fresh_clone_quality_check.sh <clone-dir> <report-file>}"
REPORT="${2:?usage: fresh_clone_quality_check.sh <clone-dir> <report-file>}"
TARGET_DIR="${CARGO_TARGET_DIR:-${CLONE_DIR}/target}"

rm -rf "$CLONE_DIR"
gh repo clone horonomy/libra-governor "$CLONE_DIR" -- --branch mvp-1.0/HORO-1127/release_gate_evidence 2>&1 | tee -a "$REPORT"

cd "$CLONE_DIR" || exit 1
export CARGO_TARGET_DIR="$TARGET_DIR"

{
  echo "=== fresh clone: $(git rev-parse HEAD) ==="
  echo "=== cargo fmt --all -- --check ==="
  cargo fmt --all -- --check
  echo "exit: $?"

  echo "=== cargo clippy --workspace --all-targets -- -D warnings ==="
  cargo clippy --workspace --all-targets -- -D warnings
  echo "exit: $?"

  echo "=== cargo build --workspace --release ==="
  cargo build --workspace --release
  echo "exit: $?"

  echo "=== cargo test --workspace ==="
  cargo test --workspace
  echo "exit: $?"

  echo "=== fresh-clone binary standalone check ==="
  "$TARGET_DIR/release/libra-governor" 2>&1
  echo "exit (expected 2, usage message): $?"

  echo "=== secret scan: git log --all -p | grep -iE 'api[_-]?key|secret|password|token' ==="
  git log --all -p | grep -iE "api[_-]?key|secret|password|token" | head -40

} >> "$REPORT" 2>&1

echo "report written to $REPORT"
