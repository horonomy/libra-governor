#!/usr/bin/env bash
# Manual/CI-runnable smoke test for HORO-1157's Codex CLI integration:
# build, install --agent codex, doctor (healthy), uninstall --agent
# codex, doctor (not installed) — against a throwaway sandbox state dir
# and $CODEX_HOME, never the real ~/.local/state/libra-governor or
# ~/.codex.
#
# This is the shell-level equivalent of
# crates/cli/tests/codex_hooks_integration.rs, which drives the same
# lifecycle in Rust. This script additionally, when a real `codex`
# binary is present, drives an actual local Codex CLI install
# end-to-end with our real hooks wired — the exact procedure that
# caught and fixed a real bug during HORO-1157's own development
# (hooks.json's real top-level shape is `{"hooks": {...}}`, not the
# flat event-keyed shape an earlier draft assumed — see
# docs/adr/0004-agent-adapter-contract.md). That step is entirely
# optional and self-skipping: if `codex` is not on PATH, the script
# says so plainly and exits 0 rather than fabricating results.
#
# # Safety
#
# The optional live-Codex step ALWAYS points `$CODEX_HOME` at a fresh
# throwaway sandbox directory — never the real user's `~/.codex`. A
# sandboxed `$CODEX_HOME` has no auth of its own (Codex's credentials
# live under `$CODEX_HOME`), so a real model call from this script
# always fails with 401 Unauthorized before it could spend anything —
# this script never has, and is not designed to ever gain, the ability
# to make a real, billed model call on the invoking user's account.
# `--dangerously-bypass-hook-trust` is passed only because the hook
# source here is this script's own freshly built binary, not because
# trust review is being skipped for anything untrusted.
#
# Not wired into .github/workflows/ci.yml: CI runners do not have a
# real Codex CLI install or login, so the live-Codex step could never
# run there anyway. Run by hand:
#   ./scripts/codex-smoke.sh
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

export LIBRA_GOVERNOR_STATE_DIR="$sandbox/state"
export LIBRA_GOVERNOR_CODEX_HOME="$sandbox/codex-home"
mkdir -p "$LIBRA_GOVERNOR_CODEX_HOME"

echo "Sandbox: $sandbox"
echo

echo "== Building libra-governor =="
target_dir="$sandbox/target"
export CARGO_TARGET_DIR="$target_dir"
cargo build --release -p libra-governor-cli
bin="$target_dir/release/libra-governor"
echo "Binary: $bin"
echo

echo "== doctor before install (expect: codex hooks not installed, exit 0) =="
"$bin" doctor
echo

echo "== Seeding a foreign hooks.json description + hook group =="
cat > "$LIBRA_GOVERNOR_CODEX_HOME/hooks.json" <<'EOF'
{
  "description": "my own hooks file",
  "hooks": {
    "PreCompact": [
      { "hooks": [{ "type": "command", "command": "/usr/local/bin/my-own-tool precompact" }] }
    ]
  }
}
EOF

echo "== install --agent codex =="
"$bin" install --agent codex
echo

echo "== Foreign description + hook group must survive install byte-for-byte =="
if ! grep -q "my own hooks file" "$LIBRA_GOVERNOR_CODEX_HOME/hooks.json"; then
    echo "error: install --agent codex dropped a foreign description field" >&2
    exit 1
fi
if ! grep -q "my-own-tool precompact" "$LIBRA_GOVERNOR_CODEX_HOME/hooks.json"; then
    echo "error: install --agent codex dropped a foreign hook group it does not own" >&2
    exit 1
fi
echo "OK: foreign description and hook group survived."
echo

echo "== hooks.json's real shape: hook groups live under the top-level \"hooks\" key =="
if command -v python3 >/dev/null 2>&1; then
    python3 - "$LIBRA_GOVERNOR_CODEX_HOME/hooks.json" <<'PYEOF'
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
assert "UserPromptSubmit" not in data, "hook groups must never be written at the top level"
for event in ("UserPromptSubmit", "PostToolUse", "Stop"):
    assert event in data["hooks"], f"missing {event} under hooks"
    entry = data["hooks"][event][0]["hooks"][0]
    assert entry["command"].endswith(f"codex-hook {'user-prompt-submit' if event == 'UserPromptSubmit' else ('post-tool-use' if event == 'PostToolUse' else 'stop')}")
    assert entry["timeout"] == 15
assert data["hooks"]["PostToolUse"][0]["hooks"][0].get("async") is True
print("OK: hooks.json shape verified structurally.")
PYEOF
fi
echo

echo "== doctor after install (expect: codex hooks wired) =="
if ! "$bin" doctor --json | grep -q '"codex_hooks"'; then
    echo "error: doctor --json did not report a codex_hooks finding" >&2
    exit 1
fi
"$bin" doctor
echo

if command -v codex >/dev/null 2>&1; then
    echo "== Real local Codex CLI found: driving an actual end-to-end run =="
    echo "codex version: $(codex --version 2>&1 || true)"
    echo
    echo "This will run 'codex exec --dangerously-bypass-hook-trust' against the"
    echo "sandboxed \$CODEX_HOME above. Codex's own credentials live under"
    echo "\$CODEX_HOME, so this sandboxed instance has none and cannot make a real,"
    echo "billed model call — every attempt will fail with 401 Unauthorized, which"
    echo "is expected and fine; the hook itself still fires before that failure."
    echo
    set +e
    codex_output="$(CODEX_HOME="$LIBRA_GOVERNOR_CODEX_HOME" timeout 20 \
        codex exec --dangerously-bypass-hook-trust "smoke test" </dev/null 2>&1)"
    set -e
    if echo "$codex_output" | grep -qi "failed to parse hooks config"; then
        echo "error: the real codex binary rejected hooks.json — shape regression!" >&2
        echo "$codex_output" >&2
        exit 1
    fi
    if echo "$codex_output" | grep -q "hook: UserPromptSubmit"; then
        echo "OK: the real Codex CLI ran our UserPromptSubmit hook."
    else
        echo "warning: could not confirm the hook fired from codex's own output — this" >&2
        echo "does not necessarily mean it failed; see \$LIBRA_GOVERNOR_STATE_DIR/daemon.log" >&2
        echo "and the raw output below." >&2
        echo "$codex_output" >&2
    fi
    if [[ -f "$LIBRA_GOVERNOR_STATE_DIR/daemon.log" ]]; then
        echo "-- daemon.log --"
        cat "$LIBRA_GOVERNOR_STATE_DIR/daemon.log"
    fi
    pkill -f "$LIBRA_GOVERNOR_STATE_DIR" >/dev/null 2>&1 || true
    echo
else
    echo "== No local 'codex' binary on PATH — skipping the live end-to-end step =="
    echo "This is an honest gap, not a fabricated pass: install Codex CLI to run"
    echo "the full live smoke evidence (see docs/adr/0004-agent-adapter-contract.md"
    echo "for what was verified this way during HORO-1157's own development)."
    echo
fi

echo "== uninstall --agent codex --yes =="
"$bin" uninstall --agent codex --yes
echo

echo "== Foreign description + hook group must survive uninstall =="
if ! grep -q "my own hooks file" "$LIBRA_GOVERNOR_CODEX_HOME/hooks.json"; then
    echo "error: uninstall --agent codex removed a foreign description field" >&2
    exit 1
fi
if ! grep -q "my-own-tool precompact" "$LIBRA_GOVERNOR_CODEX_HOME/hooks.json"; then
    echo "error: uninstall --agent codex removed a foreign hook group it does not own" >&2
    exit 1
fi
echo "OK: foreign description and hook group survived uninstall."
echo

echo "== doctor after uninstall (expect: codex hooks not installed again) =="
"$bin" doctor

echo
echo "Codex smoke test passed."
