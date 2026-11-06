# S11a: Claude Code gateway request shape (host evidence)

Slice S11a of `docs/design/s11-gateway-governor-held-hard-cap.md` (HORO-2274). Docs only, no
product code. Evidence is **structural**: header names, type tags, byte counts and opaque
equality results. No prompt text, response text, header value, token or key was stored.

**Who ran it:** producer-run by the Libra v0.0.5 coordinator subagent, not independent QA.

## Run facts

| Item | Value |
|---|---|
| Date | 2026-11-06 |
| Claude Code CLI | **2.1.306** (`claude -p`, API-key mode, default model tag `claude-opus-5-5`) |
| Candidate binary | `libra-governor-14ca589`, sha256 `5d163bba...cb354b` verified; **not in the request path** (see Deviation) |
| Isolation | `HOME` and `CLAUDE_CONFIG_DIR` under `/private/tmp/claude-501/s11a-home`; invented dummy API key; no real credentials or `~/.claude` |
| Egress | macOS `sandbox-exec` profile denying all non-loopback outbound (verified: under the fixed profile `curl` to an external host exited 6 and to a bare IP exited 7, loopback worked). `ANTHROPIC_BASE_URL` = loopback fake. Zero real Anthropic traffic, zero spend |
| Nonessential traffic | `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, autoupdater, telemetry off |
| Upstream | a ~150-line loopback fake that records structure in memory and replies with a minimal valid streaming Messages response, scripting `tool_use` where a scenario needs a tool |
| Hooks | a probe hook (events `UserPromptSubmit`, `PreToolUse`, `SubagentStop`, `Stop`, `PreCompact`) that logs only event name, payload key names and a sha256 prefix of `session_id` / `agent_id`. The repo's own hooks were not installed (they need the daemon) |
| Equality method | the fake hashed the header value, the hook hashed `session_id`; only the yes/no comparison is reported |

**Deviation from the S11a text.** The candidate daemon cannot be pointed at a loopback fake:
`config.json` `[gateway]` (`RawGatewayConfig`, crates/daemon/src/config_file.rs) accepts only
`bind_addr`, `token_path`, `credential_mode`, `credential_command/args` and
`upstream_host_allowlist`. The upstream URL and `allow_plaintext_loopback_upstream` are not
configurable, so the real gateway would forward to the real API. Rather than rebuild or risk
real egress, the real `claude` was pointed straight at the equivalent loopback fake. This
proves the **request shape Claude Code emits**; it does not exercise the gateway's own
acceptance of that shape (S11e integration tests must). The capability-token header the
gateway expects was therefore not exercised either.

## Request counts per scenario (18 POSTs, all `POST /v1/messages`)

| Scenario | Requests | Notes |
|---|---|---|
| single prompt | 1 | |
| subagent (Agent tool) | 4 | main, subagent, two main follow-ups |
| WebSearch | 4 | includes 1 inner server-tool request |
| WebFetch | 3 | no secondary request observed |
| image (Read of a PNG) | 2 | |
| PDF (Read of a PDF) | 2 | |
| `/compact` (after a seed prompt, `--continue`) | 1 + 1 | |

No other path (`count_tokens`, batches, files) was called. No GET egress was observed.

## Envelope inventory (all 18 requests unless stated)

- **Stream:** `stream: true` on 18/18. Non-stream NOT OBSERVED.
- **`max_tokens`:** present on 18/18, and the value was the same on all of them (128000 for the
  default model tag).
- **Top-level keys:** `model`, `messages`, `system`, `tools`, `max_tokens`, `stream`, `metadata`
  (key `user_id` only), `output_config` (18), `context_management` (17), `thinking` (17,
  type `adaptive`), `tool_choice` (1, type `auto`, the inner web-search request).
- **Absent in every request:** `speed`, `inference_geo`, `service_tier`, `container`,
  `mcp_servers`, `betas`.
- **Header names (22 distinct):** `accept`, `accept-encoding`, `anthropic-beta`,
  `anthropic-dangerous-direct-browser-access`, `anthropic-version`, `connection`,
  `content-length`, `content-type`, `host`, `user-agent`, `x-api-key`, `x-app`,
  `x-claude-code-session-id`, `x-stainless-{arch,lang,os,package-version,retry-count,runtime,runtime-version,timeout}`,
  plus `x-claude-code-agent-id` on subagent requests only.
- **`anthropic-beta` flag names:** `claude-code-20250219`, `context-management-2025-06-27`,
  `effort-2025-11-24`, `interleaved-thinking-2025-05-14`,
  `mid-conversation-tool-changes-2026-07-01`, `per-turn-control-2026-07-01`,
  `prompt-caching-scope-2026-01-05`, `thinking-token-count-2026-05-13` (all 18);
  `advanced-tool-use-2025-11-20` (17, absent on the inner web-search request);
  `fallback-credit-2026-06-01` (1, the `/compact` request). No fast-mode or geo flag.
- **`tools[].type`:** 17 requests carry only untyped (custom) tools, with keys `name`,
  `description`, `input_schema`, `defer_loading`. One request carries
  `web_search_20250305` (keys `type`, `name`, `max_uses`). Main-lane `tools[]` has 12-13
  entries and grows mid-conversation when ToolSearch loads a deferred tool.
- **`cache_control`:** on 17 of 18 requests, 3 blocks each (51 uses), all `{type: ephemeral}` with **no `ttl` field**. `ttl: "1h"`
  NOT OBSERVED. None on `tools[]`.
- **Content block tags:** `system[]`: `text`. `messages[]` roles `user`, `assistant` **and
  `system`** (see Surprises). Block tags seen: `text`, `tool_use`, `tool_result`, plus inside
  `tool_result.content`: `text`, `image`, `document`, `tool_reference`. String-form content
  also appears (`system` and `user` messages).
- **`source.type`:** `base64` only (image `image/png`, document `application/pdf`). `url` and
  `file` NOT OBSERVED.
- **Image and PDF placement:** both arrive **nested inside a `tool_result`**, not as a
  top-level user block.

## [H] and [I] verdicts (CLI 2.1.306)

| # | Design-doc statement | Verdict | Evidence |
|---|---|---|---|
| §2.1 | Claude Code sends the session header (default `x-claude-code-session-id`) | **CONFIRMED** | present on 18/18 requests, including subagent and `/compact` requests |
| §2.1 | The header equals the hook `session_id` | **CONFIRMED** | equal on 18/18 requests; subagent requests carry the parent's session id; the id is unchanged across `/compact` (seed and compact run share it) |
| §2.1 / §2.2 | Subagent requests carry a distinguishing header [H] | **CONFIRMED** | Claude Code sends `x-claude-code-agent-id` on the subagent request (1 of 4 in that scenario) and its value equals the hook `agent_id` (opaque equality: yes). All other header values are equal between main and subagent requests. The [V] "no agent id on the wire" describes the proxy code, which reads only the session header and currently ignores this header; it stays true |
| §2.2 / R1 | Gateway mode must refuse when the header is absent | NOT TRIGGERED | header always present here; the refusal path is untested against real Claude Code |
| §1.3 | `web_search_*` server tool breaks Claude Code WebSearch [I] | **CONFIRMED** | WebSearch issues a separate `POST /v1/messages` with `tools: [web_search_20250305]`; it matches the S11b denylist, so hard mode would 403 it. Also user-visible: `screen_unbounded_inputs` treats a base64 `application/pdf` document block as unbounded (`is_base64_pdf`), so hard mode would 403 Claude Code reading a PDF too |
| §1.3 | `web_fetch_*` server tool | **NOT OBSERVED** | WebFetch is a custom client-side tool in 2.1.306; no secondary request seen. The target was loopback and no GET was recorded, so a pre-flight block is likely but not shown; this is a weak negative |
| §1.3 | `code_execution_*`, `mcp_servers`, `container`, `container_upload` | **NOT OBSERVED** | absent in all 18 requests; no MCP server was configured |
| §1.3 | `source.type` url/file | **NOT OBSERVED** | only `base64` seen; the Read tool emits base64 |
| §1.3 | base64 inline images and documents exist and carry `source.data` | **CONFIRMED** | one image and one document block, both `base64`, nested in `tool_result` |
| §1.3 | 1-hour cache tier | **NOT OBSERVED** | no `ttl` on any `cache_control` (API-key mode; subscription mode not tested) |
| §1.3 | fast mode, `inference_geo`, service tier | **NOT OBSERVED** | no `speed`/`inference_geo`/`service_tier` key, no fast-mode beta flag |
| §1.1 | `max_tokens` always present | **CONFIRMED** (18/18) | |
| §1.2 | requests are streaming | **CONFIRMED** (18/18) | non-stream not observed |
| §1.2 | intermediate cumulative `message_delta` on real streams [I] | **NOT OBSERVABLE** | the fake controls the response |
| §1.2 | ESC interrupt is a client disconnect [I] | **NOT OBSERVED** | interactive interrupt not exercised in `-p` mode |
| ADR checklist 2 | post-`/compact` identity | **PARTIAL** | request-side session id and hook `session_id` equal and unchanged across `/compact`; post-compact transcript still unverified |

## Surprises and new shapes (findings)

None of these is a denylisted type, but several change what S11b must parse.

1. **No new unbounded-input server-tool shape found.** The only non-custom tool type was
   `web_search_20250305`, already on the denylist. Findings below are about parsing, not
   new unbounded input.
2. **`messages[].role == "system"` exists** (17 requests, all but the inner web-search one, carry system-role blocks, 9 in string
   form), tied to the `mid-conversation-tool-changes` beta. An envelope walker that assumes
   only `user`/`assistant` roles will mis-handle them. Their text counts toward input.
3. **Images and PDFs are nested in `tool_result.content`.** `image_block_count` and
   `has_unbounded_source` in crates/gateway/src/cost.rs already recurse at any depth, so
   nesting is handled; the observation confirms they must keep recursing. The reservation is
   the whole body length plus `IMAGE_MAX_TOKENS` per image (`source.data` is not excluded).
   Consequence: a base64 PDF is treated as unbounded, so hard mode 403s Claude Code reading
   a PDF, like WebSearch (founder-visible).
4. **`tool_reference` is a new content block tag** inside `tool_result` (ToolSearch results,
   4 requests, echoing 2 distinct ToolSearch results). It names a deferred tool and carries no payload. Treat as zero-payload, not as
   an unknown refusal trigger.
5. **`tools[]` changes between turns** (12 to 13 entries after ToolSearch). A per-session
   tool-prompt constant cannot be cached from the first request.
6. **Subagent requests have a smaller `tools[]` (8 entries, ~14 KB) and `system` (~3 KB).**
7. **`x-claude-code-agent-id` may make per-agent gateway attribution possible**, which the
   design currently rules out (`unattributed` under the session). Caveat: only the subagent
   scenario was correlated. The `/compact` run fired a `SubagentStop` hook with an
   `agent_id`, yet its single request had no `x-claude-code-agent-id` header, so one
   hook-visible agent had no agent header on the wire. This is a design option for the
   founder/architect, not an S11a decision.
8. **The candidate daemon cannot reach a loopback upstream through config** (Deviation above).
   Any live gateway rehearsal needs either a test-only build or a config surface.
9. **`fallback-credit-2026-06-01` beta flag** appeared on the `/compact` request only.

## S11b constants (observed sizes only)

- **`TOOL_PROMPT_TOKENS = 1000`:** the constant covers an API-injected tool-use system prompt,
  which does not apply to custom tools. Observed `tools[]` JSON was 14,279 B (subagent) to
  29,401 B (main with deferred tools loaded), `system` 3.1-6.1 KB, whole body 25-51 KB. Those
  bytes are already inside the request body, so at `bytes / 1.0` they reserve roughly
  25k-51k input tokens per request regardless of the constant. The constant is adequate for
  the one server-tool request seen (body 892 B) but that request is denylisted anyway.
  Real token counts were not measured (the fake returns canned usage), so the over-reservation
  ratio is not quantified here.
- **`IMAGE_MAX_TOKENS = 8000`:** NOT ASSESSABLE. The synthetic image was 64x64 (224 base64
  chars) and the PDF one blank page (416 chars); these show only the shape. Claude Code's
  client-side downscaling and a large-screenshot or many-page-PDF case were not observed.
  Re-run with a realistic screenshot before trusting the constant.
- **Reservation driver:** `max_tokens` was 128000 on every request, so the output side of
  the worst-case reservation dominates at the per-model output price. S11b/S11d sizing and
  the cap headroom should assume one request reserves a 128k-token output.

## Reproduction notes

The fake upstream, probe hook and sandbox profile live outside the repo at
`/private/tmp/claude-501/s11a-home` (`fake.py`, `hook.sh`, `net.sb`, `run.sh`). They are
scratch tooling, deliberately not committed. A first sandbox profile did not block egress
(reconstructed and re-probed: `curl` exit 0 for both an external host and a bare IP; the
fixed profile gave exits 6 and 7; log in scratch `out/probes.log`); that run was discarded and every scenario was
re-run under the corrected profile before any number above was taken.

Not covered: subscription (OAuth) auth mode, interactive (non-`-p`) mode, Bedrock/Vertex
paths, an MCP server, url/file sources, prompt caching beyond `ephemeral` default, and the
gateway's own acceptance of these requests.
