# Local extension provider example (HORO-1174)

A real, runnable reference implementation of a Libra Governor extension
provider — the process an operator points `config.json`'s `[extensions]`
block at. See `docs/api/libra-extension-v1.yaml` for the full wire
contract and `docs/adr/0005-local-extension-points.md` for the
architecture this implements.

## Files

| File | Purpose |
|---|---|
| `libra_example_provider.py` | Stdlib-only HTTP server implementing all three outbound surfaces: business-context, policy-webhook, events. |
| `tickets.json` | Fixture: ticket key -> priority/deadline/cost-center/advisory-criteria, keyed by the ticket key found in the task's git branch name. |
| `cost_caps.json` | Fixture: cost center -> a token cap the policy-webhook route enforces for real. |
| `report_outcome.sh` | CLI wrapper an Outcome Provider shells out to, piping a JSON payload into `libra-governor outcome record`. |

## Running the provider

```bash
# The shared HMAC signing secret. In this example every surface uses the
# same secret file for simplicity; config.json lets each surface point at
# a different secret_command if you want them to differ.
printf 'sk-fake-example-signing-secret' > /tmp/libra-example-secret.txt

python3 libra_example_provider.py \
    --port 8787 \
    --secret-file /tmp/libra-example-secret.txt \
    --events-log /tmp/libra-example-events.jsonl
```

The server listens on `127.0.0.1:8787` and exposes:

- `POST /libra/business-context`
- `POST /libra/policy-decision`
- `POST /libra/events`

## Pointing the daemon at it

Write `<state_dir>/config.json` (the same file
`crates/daemon/src/config_file.rs` already reads for `[policy]` and
`[gateway]` overrides) with an `extensions` block:

```json
{
  "extensions": {
    "business_context_provider": {
      "url": "http://127.0.0.1:8787/libra/business-context",
      "timeout_ms": 500,
      "secret_command": "cat",
      "secret_args": ["/tmp/libra-example-secret.txt"]
    },
    "policy_webhook": {
      "url": "http://127.0.0.1:8787/libra/policy-decision",
      "timeout_ms": 700,
      "secret_command": "cat",
      "secret_args": ["/tmp/libra-example-secret.txt"]
    },
    "events": {
      "url": "http://127.0.0.1:8787/libra/events",
      "timeout_ms": 3000,
      "max_attempts": 5,
      "kinds": ["admission", "replan", "approval", "outcome"],
      "secret_command": "cat",
      "secret_args": ["/tmp/libra-example-secret.txt"]
    }
  }
}
```

`secret_command`/`secret_args` are executed once per daemon process and
the resulting stdout bytes become the signing secret — see
`crates/extension/src/secret.rs`. `cat <file>` is one valid resolver; any
command that writes the secret to stdout works (an `op read`/`pass show`
invocation in a real deployment, for instance).

Every URL must use scheme `http` and host `127.0.0.1` or `::1` — see
`crates/extension/src/config.rs` for why `localhost` is deliberately
refused.

### Accepted exception: plain HTTP, no TLS (python:S5332)

`libra_example_provider.py` speaks plain HTTP, not HTTPS. This is a
reviewed and accepted SonarCloud exception (HORO-1174 evidence), not an
oversight: `--host` is hard-validated to a loopback literal
(`require_loopback_host` in the provider, exercised by
`test_libra_example_provider.py`) so this server can never bind a
network-reachable interface, and it is reference/demo code for local
manual testing — not shipped product code. Adding real TLS here would
require the daemon's own extension HTTP client
(`crates/extension/src/client.rs`) to trust a self-signed loopback
certificate, a materially larger change to the daemon's TLS trust model
that is out of scope for this file.

## What the example provider actually does (not stubs)

- **Business context**: runs `git -C <task cwd> rev-parse --abbrev-ref
  HEAD` against the task's real working directory, extracts a ticket key
  from the branch name (e.g. `v0.0.2/HORO-1174/extension_contracts` ->
  `HORO-1174`), and looks it up in `tickets.json`. No match -> a minimal
  response with no fields set (the daemon's fail-open path exercises the
  same code either way).
- **Policy decision**: remembers which cost center a task belongs to
  from its earlier business-context response, then approves or rejects
  based on a real comparison against that cost center's cap in
  `cost_caps.json`.
- **Events**: verifies the signature, tracks `event_id` values it has
  already accepted so a dispatcher retry is deduped (not double-recorded
  as a separate event), and appends every accepted delivery to an
  append-only JSONL log.
- **Signing**: every route recomputes the HMAC-SHA256 exactly as
  `crates/extension/src/sign.rs` does, checks `x-libra-timestamp` against
  a configurable clock-skew window, and rejects a reused
  `x-libra-nonce` as a replay.

## Reporting an outcome back

An external Outcome Provider (CI system, human reviewer tooling, etc.)
never speaks the daemon's Unix-socket protocol directly — it shells out
to the `libra-governor` binary:

```bash
./report_outcome.sh \
    --task-id "$TASK_ID" \
    --source-id example-provider \
    --idempotency-key "ci-run-42" \
    --kind completed \
    --evidence "https://ci.example.com/runs/42"
```

This is exactly the shape `crates/cli/src/outcome_cmd.rs::OutcomeInput`
expects on stdin — see `docs/api/examples/outcome_record_stdin.json`.

## End-to-end

`experiments/horo-1174/run_extension_e2e.py` drives this exact provider
against a real compiled `libra-governor` daemon and binary, exercising
all three outbound surfaces plus `outcome record`, end to end.
