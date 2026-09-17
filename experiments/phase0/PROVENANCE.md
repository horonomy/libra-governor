# Data Provenance (HORO-1120)

## Sources

| Source | What | License | Access |
|---|---|---|---|
| `SWE-bench/experiments` (GitHub) | Per-submission `results.json` (`resolved`/`no_generation`/`no_logs` instance-id lists) at `evaluation/verified/<submission>/results/results.json` | MIT (repo license) | Anonymous `raw.githubusercontent.com` GET, no auth |
| `swe-bench-submissions` S3 bucket | Per-submission `.traj` trajectory files under `verified/<submission>/trajs/` | Same license terms as SWE-bench/experiments (submissions are that repo's data) | Anonymous S3 `ListObjectsV2` + GET, no auth |
| `princeton-nlp/SWE-bench_Verified` (HuggingFace) | Task metadata: `repo`, `problem_statement`, `created_at`, `difficulty`, etc. | MIT (per dataset card) | Public dataset, `datasets` library, no auth token |

All three were live-probed before implementation (listing prefixes, fetching
one `results.json`, checking the HF dataset's column list) rather than
assumed. See the HORO-1120 design review for probe transcripts.

## Submissions used

6 required (SWE-agent-family, `info.model_stats` cost-stats schema):

- `20240402_sweagent_claude3opus`
- `20240402_sweagent_gpt4`
- `20240620_sweagent_claude3.5sonnet`
- `20240728_sweagent_gpt4o`
- `20250511_sweagent_lm_32b`
- `20250804_codesweep_sweagent_kimi_k2_instruct`

1 optional/stretch (`llm_call_data` schema, best-effort adapter, not
independently live-verified): `epam-ai-run-claude-3-5-sonnet`.

Explicitly skipped (unparseable / no cost stats, per the design review's live
probe of all 182 verified submissions): `emergent`, `Lingxi`.

Of 182 verified submissions, only 13 publish `.traj` files at all; of those,
only 6 carry real `info.model_stats` cost stats -- these are exactly the 6
required submissions above. This is the full usable population, not a
sample of a larger one.

## Field coverage

| Field | Coverage | Notes |
|---|---|---|
| `instance_cost_usd` | 100% of extracted rows | Directly measured (`info.model_stats.instance_cost`) |
| `wall_clock_seconds` | 100% of extracted rows, but **`target_kind: "proxy"` always** | No wall-clock time field exists anywhere in the trajectory data. Derived as `base_seconds_per_call * api_calls + seconds_per_1k_tokens * (tokens_sent + tokens_received) / 1000` (see `configs/phase0.yaml` `proxy_time`), constants chosen as simple round numbers, not fit to any ground truth (none exists) |
| `difficulty` | 100% (HF field is fully populated in SWE-bench Verified) | Post-hoc human annotation -- used only as the `taskclass_difficulty` oracle-probe upper bound, never in `fit()` |
| `problem_statement` (raw text) | N/A -- never retained | Only derived counts (`features.derive_text_features`) are committed; raw text never enters `trajectories.parquet` |

## Censoring

`censored = "exit_cost" in exit_status or "exit_context" in exit_status`.
Censored rows are **kept** (see `tests/test_censoring.py`), never dropped --
their `instance_cost_usd` is a lower bound on true cost. Coverage/pinball
metrics are reported both aggregate and stratified (`resolved` /
`unresolved` / `exit_cost` as a distinct third stratum, not folded into
`unresolved`) so this bias is visible rather than averaged away.

## `llm_self_estimate` baseline: status "unavailable"

No LLM API access was available while building this harness, so
`data/cache/llm_selfestimate.json` is **not committed** -- there is no real
self-estimate data to cache, and the spec is explicit that a synthetic
fixture standing in for real LLM output is not an acceptable alternative
("do NOT fabricate fake 'real' LLM responses"). `LLMSelfEstimateEstimator`
already handles a missing cache file correctly: every row for this baseline
comes back `status: "unavailable"` in `results/phase0_results.json`, never a
fabricated or silently-skipped prediction. Populating this cache with real
cached LLM responses (keyed by `(instance_id, prompt_sha256, model_id)`,
per `LLMSelfEstimateEstimator.cache_key`) is future work once API access is
available, not part of this Phase 0 evidence.

## Known limitations

- **Sample size**: at most ~2,900 rows / ~500 unique tasks across 6
  submissions before censoring; see the dataset-adequacy note below for the
  actual count from this run.
- **Harness diversity is weak**: all 6 required submissions are SWE-agent
  variants (same scaffolding, different backing models). No conclusion here
  generalizes to a materially different agent harness.
- **Cost is per-instance, single-attempt**: no retry/subagent/multi-attempt
  signal exists in this data.
- **Time is a documented proxy, not a measurement** -- see field coverage
  table above. Any wall-clock finding in this harness is a proxy-target
  finding, reported as such.
- **Right-censoring biases upper quantiles downward**: a censored row's
  recorded cost is a lower bound; p80/p90 coverage numbers should be read
  with that in mind, which is why censored rows get their own stratum
  rather than being merged into "unresolved."
- **Effectively all-Python**: SWE-bench Verified is Python-only. No
  cross-language generalization claim is made or supported by this
  evidence.

## Dataset-adequacy escalation (HORO-1120 -> HORO-1123)

See `results/phase0_results.json`'s `dataset.escalation_triggered` /
`dataset.n_uncensored_rows` fields and `results/phase0_report.md` for
whether the `min_uncensored_rows` (2000, configured in
`configs/phase0.yaml`) threshold was met on the actual extracted dataset.
If it was not met, model fitting was intentionally skipped -- this is a
dataset-adequacy judgment routed to HORO-1123, not something worked around
here.
