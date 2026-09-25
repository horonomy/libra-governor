#!/usr/bin/env bash
# Libra Governor cross-product canonicalization-vector agreement journey
# (HORO-1381 sub-ticket 6).
#
# Implements tools/dogfood-conformance/JOURNEY-CONTRACT.md
# (horonomy/internal-docs). Reads this repo's own bundled copy of
# vectors/canon-v1-vector-001.json, builds and runs the new
# crates/evidence-adapter/examples/canon_vector.rs against it (which
# calls the real, shipped
# libra_governor_evidence_adapter::canon::content_hash_hex), and
# compares the result against the vector's own bundled `content_hash`
# pin. Agreement here plus the same agreement independently reproduced
# by circinus, ophiuchus, horologium, and eltanin against the SAME pin
# is what proves five-way cross-product canonicalization agreement.
#
# stdout: ONLY one NDJSON check row. Everything else -> stderr.
#
# This script never modifies crates/evidence-adapter/src/ or
# Cargo.toml -- it only builds and drives the new, non-shipping
# `examples/canon_vector.rs` target.
#
# Canonical-string availability note (pin, do not "fix"):
# content_hash_hex only exposes the hex digest, not the canonical
# string it hashed internally -- the example always reports
# "canonical": null. A mismatch here is diagnosed by bisecting against
# the other four products' canonical strings, not a string diff.
#
# Infrastructure-failure rule (pin, do not deviate): a `cargo run
# --example` build failure or a missing vector file produces empty
# stdout and a nonzero exit -- an infrastructure failure, never a
# fabricated FAILED row.

set -uo pipefail

log() { printf '%s\n' "$*" >&2; }
die() { log "FATAL: $*"; exit 1; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VECTOR_PATH="${REPO_ROOT}/scripts/dogfood-journey/vectors/canon-v1-vector-001.json"

if ! command -v cargo >/dev/null 2>&1; then
    die "cargo not found on PATH"
fi

if [[ ! -f "${VECTOR_PATH}" ]]; then
    die "vector file missing: ${VECTOR_PATH}"
fi

EXAMPLE_STDERR="$(mktemp "${TMPDIR:-/tmp}/libra_canon_vector.stderr.XXXXXX")"
trap 'rm -f "${EXAMPLE_STDERR}"' EXIT

EXAMPLE_OUTPUT="$(cd "${REPO_ROOT}" && cargo run -q -p libra-governor-evidence-adapter --example canon_vector < "${VECTOR_PATH}" 2>"${EXAMPLE_STDERR}")"
EXAMPLE_EXIT=$?

if [[ -s "${EXAMPLE_STDERR}" ]]; then
    log "example stderr: $(cat "${EXAMPLE_STDERR}")"
fi

if [[ ${EXAMPLE_EXIT} -ne 0 ]]; then
    die "cargo run --example canon_vector exited ${EXAMPLE_EXIT} (build/setup failure -- infrastructure failure, not a check result)"
fi

if [[ -z "${EXAMPLE_OUTPUT}" ]]; then
    die "cargo run --example canon_vector produced no output"
fi

VECTOR_PATH="${VECTOR_PATH}" EXAMPLE_OUTPUT="${EXAMPLE_OUTPUT}" python3 - <<'PYEOF'
import json
import os
import sys

vector_path = os.environ["VECTOR_PATH"]
example_output = os.environ["EXAMPLE_OUTPUT"]

try:
    with open(vector_path, encoding="utf-8") as fh:
        vector = json.load(fh)
except (OSError, json.JSONDecodeError) as exc:
    print(f"FATAL: could not read/parse vector file: {exc}", file=sys.stderr)
    sys.exit(1)

try:
    observed = json.loads(example_output.strip().splitlines()[-1])
except (json.JSONDecodeError, IndexError) as exc:
    print(f"FATAL: could not parse example stdout as JSON: {exc}; raw={example_output!r}", file=sys.stderr)
    sys.exit(1)

expected_hash = vector["content_hash"]
observed_hash = observed.get("content_hash")

hash_matches = observed_hash == expected_hash

row = {
    "product": "libra_governor",
    "dfc_id": "DFC-SCHEMA-09",
    "virtual_time": False,
}

if hash_matches:
    row["result"] = "PROVEN"
    row["assertion"] = (
        "libra-governor-evidence-adapter's shipped canon::content_hash_hex "
        "reproduces pinned vector canon-v1-vector-001's content_hash "
        "(hash-only; this product exposes no canonical-string accessor)"
    )
    row["rationale"] = f"computed {observed_hash['value']}, matches the vector's bundled pin"
else:
    row["result"] = "FAILED"
    row["assertion"] = (
        "libra-governor-evidence-adapter's shipped canon::content_hash_hex "
        "reproduces pinned vector canon-v1-vector-001's content_hash "
        "(hash-only; this product exposes no canonical-string accessor)"
    )
    row["rationale"] = (
        f"mismatch: expected content_hash={expected_hash!r}; observed content_hash={observed_hash!r}"
    )

print(json.dumps(row))
PYEOF
