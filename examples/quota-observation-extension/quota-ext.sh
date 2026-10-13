#!/bin/sh
# Reference quota-observation extension sample (HORO-1856).
#
# This is a clean, minimal REFERENCE implementation of the
# quota-observation one-shot stdio protocol described in
# docs/quota-observation/author-guide.md. It answers both `describe`
# and `snapshot` requests against a single static, hardcoded response
# each — it never calls out to a real provider, never reads a real
# credential, and must never be mistaken for a production integration.
#
# It is deliberately NOT a copy of
# crates/quota-observation/tests/fixtures/fictional-ext/quota-ext.sh —
# that script is a black-box test fixture driven by its own harness
# (argv-mode-dispatched, template-file-driven) and its own header says
# plainly it must never be mistaken for a real integration either. This
# sample dispatches the way a real extension would: on the request's
# own `"operation"` field read from stdin, not on argv, since the host
# invokes the extension with the SAME fixed argv for every operation
# (argv is bound into the approval manifest — see
# docs/quota-observation/trust-and-lifecycle.md).
#
# Runtime contract this script is written to assume (see
# crates/quota-observation/src/runner.rs and the author-guide):
#   - No PATH. Every external command below is invoked by absolute
#     path, and this script's own shebang interpreter is also an
#     absolute path for the same reason.
#   - No HOME, no ambient environment beyond LANG=C.UTF-8/LC_ALL=C.UTF-8.
#   - Working directory is "/".
#   - stderr is discarded by the host — never rely on it being seen.
#   - Exactly one request JSON frame arrives on stdin; exactly one
#     response JSON frame (plus an optional trailing newline) must be
#     written to stdout, then this process must exit 0.

set -u

request="$(/bin/cat)"

# The request's own `"operation"` field selects describe vs. snapshot.
# Extraction is a plain `sed` capture, not a JSON parser — safe here
# only because the host always emits `request_id`/`operation` using a
# bounded, known-safe character set (see the author-guide's note on
# why a real extension should do the same for any value it re-embeds
# in a response: never round-trip an attacker-controlled string through
# an un-escaped `sed`/`printf` substitution).
operation=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"operation":"\([a-z_]*\)".*/\1/p')
request_id=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"request_id":"\([A-Za-z0-9_.-]*\)".*/\1/p')

case "$operation" in
  describe)
    bootstrap_version=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"bootstrap_version":"\([^"]*\)".*/\1/p')
    printf '{"bootstrap_version":"%s","request_id":"%s","result":{"extension_id":"org.example.sample-quota-ext","extension_version":"0.1.0","supported_versions":["libra.quota-observation/0.1"],"capabilities":["quota.snapshot"]}}\n' \
      "$bootstrap_version" "$request_id"
    exit 0
    ;;
  snapshot)
    protocol_version=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"protocol_version":"\([^"]*\)".*/\1/p')
    printf '{"protocol_version":"%s","request_id":"%s","extension_id":"org.example.sample-quota-ext","extension_version":"0.1.0","result":{"context":{"selector":"sample-session","handle":"sample-handle","principal":"sample-principal","epoch":"epoch-0001"},"authority":"sample-authority","coverage":"complete","constraints":[{"id":"tokens_month","scope":{"kind":"account","handle":"sample-account"},"metric":"total_tokens","unit":{"kind":"tokens"},"window":{"kind":"opaque","declared_reset_at":"2027-02-01T00:00:00Z"},"reading":{"kind":"used","value":"600000","limit":"1000000"},"observed_at":"2027-01-18T12:00:00Z","valid_until":"2027-01-18T12:05:00Z","upstream_enforcement":"unknown","account_fingerprint":"fp-smoke-0001"}],"missing":[]}}\n' \
      "$protocol_version" "$request_id"
    exit 0
    ;;
  *)
    # Unknown or unparseable operation: exit non-zero rather than
    # guessing. A real extension should do the same for anything it
    # does not recognize — never fail open with a fabricated success
    # envelope.
    exit 99
    ;;
esac
