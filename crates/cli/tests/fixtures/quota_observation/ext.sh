#!/bin/sh
# Minimal quota-observation test fixture extension (HORO-1870 PR2
# `crates/cli/tests/quota_observation_cli.rs`). Deliberately NOT a copy of
# examples/quota-observation-extension/quota-ext.sh (that sample answers
# a real-looking extension author would write) nor of
# crates/quota-observation/tests/fixtures/fictional-ext/quota-ext.sh (that
# fixture is driven by its own harness) — this one exists solely to make
# this test file's `doctor --probe`/`inspect` assertions deterministic.
#
# Dispatches on the request's own `"operation"` field read from stdin
# (never on argv — the host invokes this script with the same fixed argv
# for every operation; argv is bound into the approval manifest). Answers
# `describe` and `snapshot` each against static canned data, except
# `observed_at`, which is generated fresh via `/bin/date -u` on every
# invocation so a freshness check against this test's own
# `max_staleness_secs` never goes stale between the test being written
# and it being run. `/bin/date -u +FORMAT` (no arithmetic, no `-d`/`-v`
# flags) is identical between BSD date (macOS CI) and GNU date (Ubuntu
# CI) — this script intentionally never needs a future timestamp, so it
# never has to reconcile those two date dialects.
#
# Runtime contract assumed (see crates/quota-observation/src/runner.rs):
# no PATH (every external command below is an absolute path, including
# this script's own shebang), no HOME, no ambient environment beyond
# LANG=C.UTF-8/LC_ALL=C.UTF-8, cwd is "/", stderr is discarded by the
# host, exactly one request frame in and one response frame out.

set -u

request="$(/bin/cat)"

operation=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"operation":"\([a-z_]*\)".*/\1/p')
request_id=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"request_id":"\([A-Za-z0-9_.-]*\)".*/\1/p')
if [ -z "$request_id" ]; then
  exit 98
fi

case "$operation" in
  describe)
    bootstrap_version=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"bootstrap_version":"\([^"]*\)".*/\1/p')
    if [ -z "$bootstrap_version" ]; then
      exit 98
    fi
    printf '{"bootstrap_version":"%s","request_id":"%s","result":{"extension_id":"org.example.horo1870-fixture","extension_version":"1.0.0","supported_versions":["libra.quota-observation/0.1"],"capabilities":["quota.snapshot"]}}\n' \
      "$bootstrap_version" "$request_id"
    exit 0
    ;;
  snapshot)
    protocol_version=$(printf '%s' "$request" | /usr/bin/sed -n 's/.*"protocol_version":"\([^"]*\)".*/\1/p')
    if [ -z "$protocol_version" ]; then
      exit 98
    fi
    observed_at=$(/bin/date -u +%Y-%m-%dT%H:%M:%SZ)
    printf '{"protocol_version":"%s","request_id":"%s","extension_id":"org.example.horo1870-fixture","extension_version":"1.0.0","result":{"context":{"selector":"sel","handle":"handle","principal":"principal","epoch":"epoch-0001"},"authority":"authority","coverage":"complete","constraints":[{"id":"c1","scope":{"kind":"account","handle":"acct"},"metric":"total_tokens","unit":{"kind":"tokens"},"window":{"kind":"opaque"},"reading":{"kind":"used","value":"600","limit":"1000"},"observed_at":"%s","upstream_enforcement":"unknown","account_fingerprint":"fp"}],"missing":[]}}\n' \
      "$protocol_version" "$request_id" "$observed_at"
    exit 0
    ;;
  *)
    exit 99
    ;;
esac
