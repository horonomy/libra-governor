#!/usr/bin/env bash
# libra-governor install script (HORO-1150 Developer Preview).
#
# Honest about what this actually does: this repository's CI
# (.github/workflows/ci.yml) does not publish any release binary or
# artifact anywhere. There is no `curl | sh` binary-download path to
# offer, because no such binary exists. This script builds and installs
# from a local clone of this repository with `cargo install`, then runs
# this tool's own `install` and `doctor` subcommands to wire Claude Code
# and verify the result — it does not assert its own success, it asks
# the binary it just built to report on itself.
#
# Usage: run from the root of a clone of this repository:
#   ./scripts/install.sh
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/install.sh [--help]

Builds and installs the libra-governor binary from this repository
clone (via `cargo install --path crates/cli --locked`), wires it into
Claude Code's ~/.claude/settings.json, and runs a diagnostic to confirm
the result.

Requires: a Rust toolchain (cargo, rustc) already installed. See
https://rustup.rs if you do not have one.

Never runs as root, never uses sudo, and only ever writes to
$CARGO_HOME/bin (cargo's own default install location),
$HOME/.claude/settings.json (Claude Code's own settings file), and this
tool's own state directory (see crates/daemon/src/paths.rs).
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ ! -f "Cargo.toml" || ! -d "crates/cli" ]]; then
    echo "error: scripts/install.sh must be run from a clone of the libra-governor repository" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    echo "error: no Rust toolchain found (cargo/rustc). Install one first: https://rustup.rs" >&2
    exit 1
fi

echo "Using toolchain: $(rustc --version)"
echo

echo "Building and installing libra-governor from $repo_root ..."
cargo install --path crates/cli --locked
echo

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
bin_path="$cargo_home/bin/libra-governor"
if [[ ! -x "$bin_path" ]]; then
    echo "error: expected an installed binary at $bin_path but did not find one" >&2
    exit 1
fi
echo "Installed: $bin_path"
echo

echo "Wiring Claude Code integration (~/.claude/settings.json) ..."
"$bin_path" install
echo

echo "Running diagnostics ..."
set +e
"$bin_path" doctor
doctor_exit=$?
set -e

echo
echo "Install complete. Next steps:"
echo "  1. Restart Claude Code (or open a new session) so it picks up the settings change."
echo "  2. Submit any prompt — the statusline should update within a couple of seconds."
echo "  3. Run 'libra-governor doctor' any time to check on things."
echo "  4. See README.md and integrations/claude-code/README.md for policy presets,"
echo "     the optional enforcement gateway, and troubleshooting."

exit "$doctor_exit"
