# Building a quota-observation extension — builder prompt (HORO-1856)

A copy-pasteable template for a human or an LLM to follow when building
a new `quota-observation` one-shot stdio extension against the accepted
contract. See [`README.md`](README.md)'s status banner first:
experimental 0.x preview, observation-only, merged to `main`
(HORO-1854/1855). The operator CLI (HORO-1870,
`libra-governor quota-observation <register|approve|enable|disable|
uninstall|doctor|inspect>`) is merged too — see
[`author-guide.md`](author-guide.md)'s "Operator CLI" section for the
full reference.

---

## 1. Role and goal

Build a **one-shot stdio quota-observation extension**: a single
executable the host spawns per request, with no persistent process, no
daemon, no network server of its own. **Observation only** — this
extension never makes, informs, or participates in an
admission/pacing/enforcement decision. Its only job is to answer
`describe` and `snapshot` requests honestly, including answering
`missing`/`error` honestly when it cannot answer.

## 2. Inputs you (the author) must fill in before writing any code

- **Provider name** — what real system this extension reports quota
  for (your own naming; the host never resolves or validates it beyond
  treating `extension_id` as an opaque string).
- **How the extension gets its own credentials (E1)** — see
  [`trust-and-lifecycle.md`](trust-and-lifecycle.md)'s E1 section. The
  host spawns your registered executable directly with a cleared
  environment — there is no process of your own that runs first to set
  one up. A realistic option is a credential file at a fixed, absolute
  path your executable reads itself at runtime; **never** list that
  file in `runtime_files` (its hash would bind into the launch-tuple
  digest, breaking approval on every routine credential rotation — see
  `trust-and-lifecycle.md`'s E1 section for why).
- **Constraint list**: for each quota/usage limit you will report,
  decide its `id` (stable, you choose it), `scope`
  (`principal`/`account`/`team`/`organization`/`project`/
  `shared_pool`/`provider`, each with a `handle`), `metric`
  (`total_tokens`/`input_tokens`/`output_tokens`/`requests`/`spend`/
  `credit_usage`/`utilization`), and `unit` (`tokens`/`requests`/
  `usd_cents`/`percent_basis_points`/`opaque_credit` with a
  `namespace`).
- **`account_fingerprint` source**: a non-secret, deterministic
  identifier for the account/entity this reading is about — must be
  **computed from something real**, must be **the same value every
  time** for the same underlying account (the host binds it against
  the registration's expected value; a wrong or randomly-regenerated
  fingerprint moves every constraint into `missing` with
  `account_fingerprint_mismatch`, not a crash).
- **Timeout budget**: pick a `timeout_ms` your extension can reliably
  answer within, in the registration — not in your own code: the host
  enforces `100..=10000` and bounds your **entire** run (start + stdin
  read + your processing + stdout write + exit) by it.

## 3. Hard rules (non-negotiable — see `author-guide.md` for detail)

- Environment is cleared to exactly `LANG=C.UTF-8`/`LC_ALL=C.UTF-8` —
  **absolute paths only**, including your own shebang interpreter; no
  `PATH`, no `HOME`.
- Working directory is `/`. stderr is discarded — never rely on it.
- Exactly one request frame in, exactly one response frame out
  (optional trailing `\n`), exit 0.
- **Never emit an extra JSON field.** Every struct is
  `#[serde(deny_unknown_fields)]` — one unexpected key fails the parse
  of your entire response.
- **Never echo a credential, and never echo a raw upstream error
  message.** If your real provider's API returns an error that might
  contain a credential-shaped string (a token embedded in a URL, an
  auth header dump), translate it to one of this protocol's own
  `ErrorCode` values — never pass the raw text through.
- **No background children.** A child you background and leave running
  is a known, currently-unfixed host limitation on the **success**
  path (the host does not kill your process group if you exit 0 on
  your own — only timeout/cancel paths do) — write as if this will be
  fixed under you: don't rely on the host cleaning up after you, ever.
- **Design for limits the host does not enforce yet**, so your
  extension keeps working once these gaps close rather than silently
  breaking:
  - Nesting depth **<= 12** in any JSON value you emit (gap **G4**: no
    nesting-depth cap exists today, but a future host version may add
    one).
  - **<= 64 items** in any array/collection field you emit (gap **G5**:
    same reasoning — `constraint_ids`/`capabilities`-shaped fields).
  - `window.declared_reset_at`, if you set it, must be **strictly after
    the matching constraint's own `observed_at`** (gap **G7**: no
    cross-check exists today).
  - Your `metric`/`unit` pairing per constraint must stay
    semantically compatible, and a `remaining` reading's `value` must
    never exceed its own `limit` (gap **G8**: no cross-check exists
    today for either).

## 4. Operations and field reference

Full field-by-field reference: [`author-guide.md`](author-guide.md)'s
"Operations" section, sourced directly from
`crates/quota-observation/src/wire.rs`. For exact canonical wire shapes
of real, host-recognized responses, read
`crates/quota-observation/tests/fixtures/scenarios/*.json` directly —
every scenario the host's own conformance suite tests against.

## 5. Self-test checklist (run these against your own extension before registering it)

Using the same invocation shape as `author-guide.md`'s real examples
(substitute your own executable path and a request matching your own
constraint ids):

```bash
# describe
cd / && printf '%s\n' '{"bootstrap_version":"libra.quota-bootstrap/1","request_id":"selftest-describe","operation":"describe","supported_versions":["libra.quota-observation/0.1"]}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /abs/path/to/your-extension
echo "exit=$?"   # must be 0

# snapshot, one of your real constraint ids
cd / && printf '%s\n' '{"protocol_version":"libra.quota-observation/0.1","request_id":"selftest-snapshot","operation":"snapshot","context_selector":"<your selector>","constraint_ids":["<your constraint id>"],"max_age_seconds":300,"budget_ms":3000}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /abs/path/to/your-extension
echo "exit=$?"   # must be 0

# each of your own simulated failure/error scenarios
cd / && printf '%s\n' '{"protocol_version":"libra.quota-observation/0.1","request_id":"selftest-error","operation":"snapshot","context_selector":"<your selector>","constraint_ids":["<a constraint id that should fail>"],"max_age_seconds":300,"budget_ms":3000}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /abs/path/to/your-extension
echo "exit=$?"   # must still be 0 — a logical error is a valid *envelope*, not a process failure
```

For each run, check:
1. **Exit code is 0** in every case, including an error envelope — a
   non-zero exit is a transport failure regardless of what you wrote to
   stdout.
2. **Exactly one JSON value** came out, parseable as the matching
   `BootstrapResponse`/`SnapshotResponse` shape, with no extra trailing
   bytes after it (or exactly one trailing `\n`).
3. **No unexpected field** — deserialize your own output against the
   real structs in `crates/quota-observation/src/wire.rs` if you have a
   Rust toolchain handy; `deny_unknown_fields` will catch anything
   extra immediately.

## 6. Deliverables

1. **The executable** itself (absolute, stable install path).
2. **The `runtime_files` list** your registration needs — empty only if
   your executable is a single file with no separately-loaded
   interpreter modules (see `trust-and-lifecycle.md`'s note on
   interpreted extensions).
3. **A registration template with an empty `approval` block** —
   `approved: false`, empty digest strings, your real `runtime_files`
   list (digests still blank — they get filled at approval time), a
   real non-empty `endpoint_owner`, and a `consent` object present
   (even if its values are placeholders pending the actual approval
   step). See
   [`examples/quota-observation-extension/registration.template.json`](../../examples/quota-observation-extension/registration.template.json)
   for a complete worked example of this shape.

**Reminder**: approving a registration is a real two-command operator
flow today —

```bash
libra-governor quota-observation register <registration-file>
libra-governor quota-observation approve <registration-id>
```

`register` never trusts a candidate file's self-declared
`approval.approved`/digest fields — it force-clears them to the
unapproved state on write regardless of what the file claims, so the
"empty `approval` block" shape above is already what `register`
enforces even if your template claimed otherwise. `approve` then
measures the real executable/manifest/launch-tuple digests fresh from
disk and writes them in — see
[`author-guide.md`](author-guide.md)'s "Operator CLI" section for a
full, verified example of this sequence end to end.
