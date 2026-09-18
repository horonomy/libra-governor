#!/usr/bin/env bash
# Manual/CI-runnable lifecycle smoke test for HORO-1150: build, install,
# doctor (healthy), uninstall, doctor (not installed) — all against a
# throwaway sandbox state dir and Claude settings dir, never the real
# ~/.local/state/libra-governor or ~/.claude/settings.json.
#
# This is the shell-level equivalent of
# crates/cli/tests/doctor_uninstall_integration.rs (which drives the
# same lifecycle via Rust's `Command`, one scenario per #[test] so a
# failure names exactly which step broke). This script instead proves
# the *real* installed binary reachable via `cargo install` behaves the
# same way an end user would experience it, and it seeds a foreign
# settings.json key to prove uninstall does not touch it — the same
# guarantee `crates/cli/src/claude_settings.rs`'s unit tests already
# cover at the module level.
#
# Not wired into .github/workflows/ci.yml (see README.md's "Known
# limitations"): this repo's CI has no release job to attach it to, and
# adding a new CI job is a separate, reviewed change. Run by hand:
#   ./scripts/smoke-test.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

sandbox="$(mktemp -d)"
if [[ -z "$sandbox" || "$sandbox" != /tmp/* && "$sandbox" != /var/folders/* ]]; then
    echo "error: refusing to use a sandbox dir outside /tmp or /var/folders: $sandbox" >&2
    exit 1
fi
cleanup() {
    rm -rf "$sandbox"
}
trap cleanup EXIT

# Only the two directories this tool itself reads/writes are redirected
# — the real $HOME is left alone so cargo/rustup (which resolve their
# own toolchain config from $HOME/$CARGO_HOME/$RUSTUP_HOME) keep working
# unmodified.
export LIBRA_GOVERNOR_STATE_DIR="$sandbox/state"
export LIBRA_GOVERNOR_CLAUDE_DIR="$sandbox/claude"
mkdir -p "$LIBRA_GOVERNOR_CLAUDE_DIR"

echo "Sandbox: $sandbox"
echo

echo "== Building libra-governor =="
target_dir="$sandbox/target"
export CARGO_TARGET_DIR="$target_dir"
cargo build --release -p libra-governor-cli
bin="$target_dir/release/libra-governor"
echo "Binary: $bin"
echo

echo "== doctor before install (expect: not installed, exit 0) =="
"$bin" doctor
echo

echo "== Seeding foreign settings.json keys (apiKeyHelper + statusLine) =="
cat > "$LIBRA_GOVERNOR_CLAUDE_DIR/settings.json" <<'EOF'
{
  "apiKeyHelper": "/usr/local/bin/my-own-key-helper",
  "statusLine": { "type": "command", "command": "/usr/local/bin/my-own-statusline.py" }
}
EOF

echo "== install =="
"$bin" install
echo

echo "== Foreign statusLine must survive install (never overwritten) =="
if ! grep -q "my-own-statusline.py" "$LIBRA_GOVERNOR_CLAUDE_DIR/settings.json"; then
    echo "error: install overwrote a foreign statusLine it does not own" >&2
    exit 1
fi
echo "OK: foreign statusLine survived install."
echo

echo "== doctor after install (expect: hooks wired) =="
if ! "$bin" doctor --json | grep -q '"claude_settings"'; then
    echo "error: doctor --json did not report a claude_settings finding" >&2
    exit 1
fi
"$bin" doctor
echo

# uninstall never deletes the daemon binary itself (it only prints how
# to remove it via `cargo uninstall`, so it doesn't corrupt cargo's own
# package bookkeeping — see uninstall_cmd's module docs), so "$bin"
# stays usable for the post-uninstall doctor check below.

echo "== uninstall --yes =="
"$bin" uninstall --yes
echo

echo "== Foreign key must survive uninstall =="
if ! grep -q "my-own-key-helper" "$LIBRA_GOVERNOR_CLAUDE_DIR/settings.json"; then
    echo "error: uninstall removed a foreign apiKeyHelper it does not own" >&2
    exit 1
fi
echo "OK: foreign apiKeyHelper survived."
echo

if [[ ! -e "$bin" ]]; then
    echo "error: uninstall must never delete the daemon binary directly" >&2
    exit 1
fi
echo "OK: uninstall left the daemon binary in place."
echo

echo "== doctor after uninstall (expect: not installed again) =="
"$bin" doctor

echo
echo "Smoke test passed."
