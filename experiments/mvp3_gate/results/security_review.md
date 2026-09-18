# MVP 3.0 Release Gate — Security Review

Each item below states the exact command run, the exact real output
observed, and a PASS/FAIL/CANNOT-VERIFY verdict. No item is a
source-reading-only assertion without a corresponding real command run
against real compiled code, except where marked CANNOT-VERIFY.

## 1. Credential isolation — fake test credential never appears in persisted state

**Command** (`crates/daemon/tests/mvp3_gate_evidence.rs::security_evidence_credential_and_prompt_content_never_touch_persisted_state`):

```
cargo test -p libra-governor-daemon --test mvp3_gate_evidence \
  security_evidence_credential_and_prompt_content_never_touch_persisted_state -- --nocapture
```

Real daemon dispatch, real gateway config with
`GovernorHeld { credential: CredentialCommand("/bin/sh", ["-c", "printf 'sk-fake-test-key-mvp3-security-9f3c7a1e'"]) }`,
real Preflight with a nonce-bearing prompt
(`NONCE-MVP3-7c4c1a8e-do-not-leak-this-prompt-text`), and a real
`GatewayRequestRecord` written through the real `LedgerRequestRecorder` — to
a persisted (non-tempdir, not auto-deleted) directory so it could be
grepped after the test process exited.

**Output** (in-process check, from `--nocapture`):
```
security evidence persisted at: /var/folders/.../T/libra-horo1146-mvp3-security-evidence
in-process grep: no credential or prompt-nonce leakage found in /var/folders/.../T/libra-horo1146-mvp3-security-evidence
```

**External grep, after the test process exited** (separate shell, separate
process, reading the file from disk, not relying on the test's own claim):

```
$ grep -c "sk-fake-test-key-mvp3-security-9f3c7a1e" ledger.sqlite3 daemon.log
ledger.sqlite3:0
daemon.log:0
$ grep -c "NONCE-MVP3-7c4c1a8e" ledger.sqlite3 daemon.log
ledger.sqlite3:0
daemon.log:0
```

**Verdict: PASS.** Neither the fake credential nor the prompt-content nonce
appears anywhere in the persisted SQLite ledger or daemon log, confirmed by
both the test's own in-process byte-scan and an independent external `grep`.
Note the credential never reaches the ledger by *construction* too —
`GatewayRequestRecord` has no credential field at all (see
`a_gateway_request_row_is_written_and_carries_no_body_or_credential` in
`scenario10_provider_retry_and_stream_interruption.txt`'s corroborating
evidence) — this test confirms that holds on real persisted disk state, not
just in the type signature.

## 2. Gateway open-proxy / SSRF — real running gateway, real rejected requests

**Command**:
```
cargo test -p libra-governor-gateway --test ssrf_and_config -- --nocapture --test-threads=1
```

**Output** (`scenario_security_ssrf_open_proxy.txt`, verbatim):
```
running 13 tests
test a_gateway_bound_to_a_non_loopback_address_never_starts ... ok
test a_gateway_whose_upstream_is_not_configuration_never_starts ... ok
test a_name_that_merely_looks_like_loopback_is_not_treated_as_loopback ... ok
test a_subscription_deployment_may_not_claim_a_usd_hard_cap ... ok
test a_traversal_path_is_refused_rather_than_normalised_into_the_upstream_url ... ok
test an_absolute_form_request_uri_cannot_redirect_the_outbound_call ... ok
test an_upstream_redirect_is_relayed_rather_than_followed ... ok
test the_configured_upstream_is_the_only_destination_any_request_reaches ... ok
test the_plaintext_loopback_opt_in_cannot_be_turned_into_an_exfiltration_path ... ok
[+ 4 fake_upstream self-tests, all ok]

test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

These are real, running-process assertions, not merely construction-time
config validation: `an_upstream_redirect_is_relayed_rather_than_followed`
sends a real HTTP request through a real running gateway to a real fake
upstream configured to respond with a `302` to `https://evil.example.com/...`,
and asserts the redirect is relayed to the caller verbatim, never followed
by the gateway itself.
`an_absolute_form_request_uri_cannot_redirect_the_outbound_call` and
`a_traversal_path_is_refused_rather_than_normalised_into_the_upstream_url`
send real malformed/off-allowlist-shaped requests
(`POST http://evil.example.com/v1/messages` absolute-form target, path
traversal like `/v1/messages/../../internal`) against the real gateway and
confirm real rejection (404/misdirected, never normalized into a different
outbound destination).
`the_configured_upstream_is_the_only_destination_any_request_reaches`
confirms no request the gateway ever sends can be redirected to a
destination other than its one configured, allowlisted upstream.

**Verdict: PASS.**

## 3. Forged/malformed/out-of-range daemon protocol requests

**Command**: a Python script driving raw Unix-socket requests against a
real, live `libra-governor daemon run` subprocess (see
`security_malformed_protocol_requests.txt` for the full script and output).

**Output** (verbatim):
```
1. malformed JSON -> {"protocol_version":6,"response":{"kind":"error","message":"malformed request"}}
2. wrong protocol_version (999) -> {"protocol_version":6,"response":{"kind":"error","message":"protocol version mismatch: daemon speaks 6, client sent 999"}}
3. unknown request kind, forged reservation-shaped payload -> {"protocol_version":6,"response":{"kind":"error","message":"malformed request"}}
4. legitimate ToolInvoked request with a smuggled out-of-range 'amount' field -> {"protocol_version":6,"response":{"kind":"ack"}}
5. daemon still alive & answers Status correctly after all of the above -> {"protocol_version":6,"response":{"kind":"status","current_task":null}}
```

**Finding, disclosed rather than hidden**: the daemon's wire protocol
(`crates/protocol/src/messages.rs`, `PROTOCOL_VERSION = 6`) has exactly 6
`Request` variants (`Preflight`, `Status`, `ToolInvoked`, `Finalize`,
`CalibrationReport`, `GatewayStatus`), **none of which carries a
resource/reservation amount field at all**. There is therefore no
"malformed reservation amount" to smuggle through this socket — item 4
above demonstrates a smuggled `amount` field on a legitimate request is
simply ignored by `serde` (unknown fields are dropped, not interpreted).
Reservations are created only server-side, computed from policy/estimate
state the client never supplies directly. Every genuinely malformed input
(bad JSON, wrong protocol version, unknown request kind) was rejected with
a structured `Response::Error` and never crashed or corrupted the daemon,
which remained fully responsive afterward.

**Verdict: PASS** (no forgeable reservation-amount surface exists to
exploit; every malformed input was cleanly rejected).

## 4. Reservation races / double settlement

**Commands**:
```
cargo test -p libra-governor-daemon --test gateway_enforcement settling_the_same_reservation_twice_does_not_double_charge -- --nocapture
cargo test -p libra-governor-gateway --test proxy_lifecycle a_client_retry_is_a_fresh_reservation_not_a_double_settlement -- --nocapture
cargo test -p libra-governor-ledger --test reservation_concurrency -- --nocapture
```

**Output** (`scenario10_provider_retry_and_stream_interruption.txt`,
`scenario7_completion_reserve_protected.txt` — verbatim `ok` for all):
```
test settling_the_same_reservation_twice_does_not_double_charge ... ok
test a_client_retry_is_a_fresh_reservation_not_a_double_settlement ... ok
test a_reservation_left_active_by_a_crashed_process_is_reclaimed_on_reconciliation ... ok
test concurrent_optional_reservations_never_double_spend_the_shared_envelope ... ok
test concurrent_required_work_reservations_never_over_draw_the_completion_reserve ... ok
```

Real concurrent OS threads (8 and 10 respectively) attempting real
concurrent settlement/reservation against the real SQLite ledger (WAL mode,
`busy_timeout=5000`) confirm exactly the expected grant/deny split with no
double-spend (`Σ active == expected` exactly in both cases) — this gate's
own scenario 8 test additionally confirmed the same property with 12 real
threads against `LedgerSpendAuthority` (`scenario2_deadline_first_and_scenario8_concurrent_subagents.txt`):
`Σ active_optional_total == granted × per_request_amount` exactly.

**Verdict: PASS.**

## 5. Local socket / access control file permissions

**Commands** (real daemon run, real files, `stat` after the process
started):
```
$ stat -f "%N %OLp" <state_dir> <state_dir>/d.sock <state_dir>/ledger.sqlite3
<state_dir>                 755
<state_dir>/d.sock          755
<state_dir>/ledger.sqlite3  644

$ LIBRA_GOVERNOR_STATE_DIR=<state_dir> libra-governor gateway token
9a8f700b5943eef5f1a1b5bd81582bbea6f8842531b6bc48e0e3844b78803b17
$ stat -f "%N %OLp" <state_dir>/gateway.token
<state_dir>/gateway.token  600
```

**Finding, disclosed rather than hidden**: `gateway.token` (the local
capability token gating gateway HTTP access) is deliberately written at
mode `0600` (`crates/gateway/src/credential.rs::write_private`). Everything
else — the state directory itself, the Unix domain socket, and the SQLite
ledger file — is created with **no explicit mode set anywhere in this
codebase**, inheriting whatever the process umask yields (`755`/`644` on
this machine's default umask `022`). On a shared multi-user machine with a
permissive umask, another local user could read the ledger file directly
(SQL query, not just a socket connection) or connect to the daemon's Unix
socket, though connecting alone grants no privilege beyond what any local
process could already request over the socket protocol (§3 above shows the
protocol itself has no way to forge a reservation). This is a real,
disclosed gap worth the coordinator's judgment on severity for a
local-machine-trust-model product — not assessed as a severity level by
this review.

**Verdict: PASS/FAIL split** — gateway token handling: **PASS** (explicitly
hardened). State dir / socket / ledger file permissions: **FAIL** against
an implicit "no unintended local read access" expectation, though not
necessarily a regression from any documented invariant (none exists in the
codebase specifying these should be restricted) — reported as a finding for
the coordinator to weigh.

## 6. Log/receipt/statusline privacy leakage

Covered by item 1 above (same test, same grep) for the gateway-request-path
credential/prompt-content check. This mirrors
`experiments/mvp1_validation/run_validation_matrix.py::test_privacy_inspection`'s
established nonce-grep technique from MVP 1.0, re-run here for the
gateway/credential path specifically (MVP 1.0's version covered the
daemon-only path without a gateway configured).

Additionally, `crates/daemon/tests/gateway_enforcement.rs::a_gateway_request_row_is_written_and_carries_no_body_or_credential`
and `crates/gateway/tests/proxy_lifecycle.rs::a_provenance_row_never_carries_a_body_or_a_credential`
(both `ok` in the full workspace run — see `full_workspace_test_run.txt`)
independently confirm this holds structurally (the `GatewayRequestRecord`
type has no body/credential field to leak in the first place), not merely
for this run's specific values.

**Verdict: PASS.**

## 7. Bypass attempt — can traffic skip the gateway/daemon entirely?

**Method**: read ADR 0001 (`docs/adr/0001-initial-architecture.md`) and
confirm its claims against the real code, rather than simulating an actual
external bypass (there is nothing to "run" here — the question is an
architectural guarantee, not a runtime behavior to trigger).

**Finding, confirmed true, not just claimed**: per ADR 0001, exactly one of
this system's five components is a genuine hard enforcement boundary — the
gateway, because it sits directly in the real HTTP network path between the
agent and the provider. The daemon/hooks path is explicitly, by design,
**cooperative, not enforcing**: `EnforcementCapabilities::for_tier(HooksOnly)`
(`crates/domain/src/capability.rs`) is a pure, closed-set mapping that
reports `pre_spend_refusal: false` and `credential_custody: NotApplicable`
for the hooks-only tier — there is no code path anywhere that lets a
hooks-only deployment claim it is enforcing spend, and `gateway status`'s
`render_status()` explicitly prints `"Monetary cap: NOT ENFORCED"` for that
tier (confirmed passing in the full test suite:
`gateway_cmd::tests::a_hooks_only_deployment_says_there_is_no_pre_spend_refusal`).
MCP is explicitly rejected as an enforcement surface in the ADR's own
"Alternatives considered" section. **If Claude Code (or any agent) simply
never calls the hooks, or calls a provider directly bypassing the gateway
process, nothing in the daemon/hooks layer can prevent that spend** — this
is a real, inherent architecture limitation, not a bug, and the codebase
does not claim otherwise anywhere in the paths this review checked
(`gateway status` rendering, `EnforcementCapabilities::for_tier`, the ADR
itself).

The one layer that genuinely cannot be bypassed once configured is the
gateway itself, *if* the agent is actually configured to route provider
traffic through it (e.g. via `apiKeyHelper` pointing at the gateway's local
capability token) — traffic that goes around the gateway process entirely
(calls the provider directly with a raw API key) is unpreventable by
anything in this codebase, by design, and this is documented honestly here
rather than claimed as a guarantee that doesn't exist.

**Verdict: CANNOT-VERIFY as a "prevented" property (correctly so — it is
not designed to be preventable), CONFIRMED as an honestly-documented,
real architectural limitation.** No code path claims a guarantee this
review's checks contradict.

## Summary

| # | Item | Verdict |
|---|---|---|
| 1 | Credential isolation | PASS |
| 2 | Gateway SSRF / open-proxy | PASS |
| 3 | Forged/malformed daemon protocol requests | PASS |
| 4 | Reservation races / double settlement | PASS |
| 5 | Local socket/state-dir file permissions | PASS (token) / FAIL (state dir, socket, ledger — disclosed gap) |
| 6 | Log/receipt/statusline privacy leakage | PASS |
| 7 | Bypass attempt (MCP/hook absence) | Confirmed real, honestly-documented, by-design limitation — not a defect |
