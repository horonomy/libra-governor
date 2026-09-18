# README.md documentation review (HORO-1152)

Followed `README.md`'s "Install" and "Quickstart" sections literally,
step by step, in the isolated fresh profile from item 1
(`/tmp/libra-horo1152-work/home-main`), plus targeted checks of the
"Diagnostics", "Troubleshooting", "Security & privacy", and "Uninstall"
sections against what items 2–10 above actually observed.

## Install

> ```
> git clone https://github.com/horonomy/libra-governor.git
> cd libra-governor
> ./scripts/install.sh
> ```

Not literally re-cloned from GitHub (this gate already works inside a
checkout of this repository at the current commit, per this ticket's
instructions to work only in this existing worktree) — the equivalent
`./scripts/install.sh` run against this checkout, in the isolated
fake-`$HOME` profile, is `experiments/v001_gate/results/01_install_from_documented_path.txt`.
**Worked exactly as documented**: built, installed to
`$CARGO_HOME/bin/libra-governor`, wired `~/.claude/settings.json`, ran
`doctor`. No undocumented prerequisite encountered beyond what the
script's own `--help` text and this gate's environment notes already
require (a working Rust toolchain).

The "equivalent manual steps" (`cargo install --path crates/cli
--locked`, `libra-governor install`, `libra-governor doctor`) were also
exercised directly, separately from `install.sh`, in items 3a/3b/7/8/9 —
**all worked as documented**.

## Quickstart (5 minutes)

1. Install steps — verified above.
2. "Restart Claude Code... so it picks up the settings.json change" —
   not literally testable without a running Claude Code instance in
   this environment; the underlying claim (the hook/statusline entries
   are actually present in `settings.json` after install) is directly
   verified in items 3a/3b/9.
3. "Open any project in Claude Code and submit a prompt" — the
   equivalent real action, `hook user-prompt-submit` invoked directly
   against a fixture repo, is item 4 and reproduced again below.
4. "Watch the statusline... update within a couple of seconds" —
   reproduced directly:

   ```
   $ libra-governor statusline        # before any prompt
   libra: -

   $ echo '{"session_id":"...","cwd":".../fixtures/rust-crate","prompt":"doc review quickstart check","hook_event_name":"UserPromptSubmit"}' \
       | libra-governor hook user-prompt-submit
   {"hookSpecificOutput":{"additionalContext":"...[libra-governor] Preflight complete (task 9b00e464..., confidence: medium, recon: 0.00s)...

   $ libra-governor statusline        # after the prompt
   libra: task 9b00e464 | plan 823fc81e | preflight: medium | recon: 0.0s | remaining P90: unknown | stable
   ```

   Matches the documented example shape
   (`libra: task <id> | plan <id> | preflight: <status> | recon: <t> |
   remaining P90: <t> | <replan state>`) field-for-field.
   **Worked as documented.**
5. `libra-governor doctor` — verified repeatedly throughout (items 2,
   7, 8, 10).

## Diagnostics section

The documented `doctor` behavior list (daemon availability/protocol
version, ledger schema version, Claude Code wiring, active policy
preset, gateway config/tier, `config.json` presence/validity, stale
config, telemetry posture) — every one of these finding categories was
directly observed in real `doctor` output across items 2, 7, 8, 10:
`state_dir_permissions`, `config_file_present`, `gateway_token_present`,
`claude_settings`, `daemon_reachable`, `schema_version`,
`policy_preset`, `telemetry`. **Matches documentation.**

## Troubleshooting table — verified vs. flagged as unverified

The ticket asks specifically to flag speculative troubleshooting
entries rather than keep them dressed as real. Cross-checked each row
against what this gate actually reproduced:

| Row | Status |
|---|---|
| `doctor` reports a protocol version mismatch → `pkill -f "libra-governor daemon run"` | **VERIFIED — reproduced live.** This is exactly the failure hit and fixed in item 7 (upgrade): a stale old-protocol daemon after an in-place binary upgrade. The documented fix worked verbatim; see `07_upgrade_from_mvp3.txt`. |
| Statusline shows `libra: -` when daemon not running | **VERIFIED — reproduced live** above (before any prompt). |
| `install` says an existing `statusLine` was left untouched | **VERIFIED — reproduced live** in item 3b (`03b_install_cmd_foreign_settings.txt`) and in `scripts/smoke-test.sh`'s own run (`smoke_test_run.txt`): "OK: foreign statusLine survived install." |
| `doctor` reports `config.json ... rejected` | **NOT reproduced by this gate.** This gate only exercised valid `config.json` fixtures (item 10). Flagging as **unverified by this gate** — plausible from reading `crates/daemon/src/config_file.rs`'s error handling, but not empirically confirmed here. |
| `doctor` reports `stale_config` | **NOT reproduced by this gate** — would require editing `config.json` under a live daemon and re-running doctor without restarting; not exercised. **Unverified by this gate.** |
| `doctor` reports the ledger schema is ahead of this binary | **NOT reproduced** — would require deliberately downgrading the binary against a newer-schema ledger. **Unverified by this gate.** |
| Gateway configured but `doctor` says `not running` | **NOT reproduced** — this gate's gateway scenarios (item 10) used `doctor`/`doctor --json` reading `config.json` directly, without starting a live daemon+gateway process. **Unverified by this gate.** |
| A gateway request refused with HTTP 403 | **NOT reproduced** — no live gateway proxy was started (see the privacy-leak-check scope note on why). **Unverified by this gate.** |

Recommendation to the coordinator: the three unverified-by-this-gate
rows involving `stale_config`, ledger-schema-ahead, and live gateway
behavior are not necessarily wrong — HORO-1146's own evidence set
already exercises the gateway path via Rust-integration tests — but
this gate specifically did not re-verify them against the shipped CLI
end-to-end, and the ticket asked that distinction be made explicit
rather than blurred.

## Security & privacy section

- "full prompt text, source code, and raw tool output never leave your
  machine" / "`daemon.log` never receives raw prompt text or hook
  payload content" — **directly verified**, see
  `results/privacy_leak_check.md` (item 11): grepped `daemon.log` and
  `ledger.sqlite3` for real prompt content used in this gate's own
  runs; none found.
- "no telemetry code path anywhere" — consistent with `doctor`'s
  `telemetry: off (local-only; no telemetry code path exists)` finding
  observed in item 7's post-upgrade doctor output.
- "a forwarded subscription credential gets exact token observation but
  no monetary cap" — checked in item 10 for overstated language; no
  "exact enforcement"/"guaranteed cap"/"hard limit enforced" phrase
  found paired with the subscription/pass-through mode in real `doctor`
  output. **Consistent with documentation.**

## Uninstall section — one real discrepancy found

> "Every other key in that file... is left untouched... its test suite
> that seeds foreign content and asserts it survives byte-for-byte."

Item 9's full install→uninstall cycle found this claim **slightly
overstated relative to observed CLI behavior**: every foreign key's
**value** survives the full cycle (structurally verified — see
`09_foreign_settings_full_cycle.txt`), but the file is **not
byte-identical** afterward — JSON re-serialization reorders top-level
and nested object keys (e.g. `hooks`/`statusLine`/`env` alphabetized to
`env`/`hooks`/`permissions`/.../`statusLine`; `{"type":...,
"command":...}` reordered to `{"command":..., "type":...}`). This is a
real, minor, reproducible defect: the module-level unit tests referenced
in the doc comment likely assert value-level (not byte-level)
preservation, while the doc's own prose promises "byte-for-byte." See
the final report's "defects found" section for the recommendation.

Backup-file collision behavior (recent commits: "Never delete the
cargo-installed binary directly", "fix backup filename collision") —
**verified working**: two separate backups
(`settings.json.libra-backup-<ts>-<pid>-...`) were written across the
install→uninstall cycle with no name collision, and `uninstall` never
attempted to delete the `cargo install`-managed binary, correctly
printing `cargo uninstall libra-governor-cli` instead (see item 8 and
`smoke_test_run.txt`).

## `scripts/smoke-test.sh`

Run for real (not just read) — `results/smoke_test_run.txt`. Passed
cleanly end-to-end (`Smoke test passed.`), independently corroborating
items 3b, 8, and 9's findings via a different (env-var-redirected
rather than fake-`$HOME`) isolation mechanism.

## Summary

No documented install/quickstart step failed or required an
undocumented prerequisite. One real prose-vs-behavior discrepancy found
(byte-for-byte claim in "Uninstall", see above). Five troubleshooting
table rows are plausible-but-unverified-by-this-specific-gate rather
than confirmed; three rows were directly reproduced live.
