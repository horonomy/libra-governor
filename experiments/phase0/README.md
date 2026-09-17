# Phase 0: Estimator Feasibility Replay Harness (HORO-1120)

```
uv sync --group dev
uv run phase0 fetch            # network: S3 + HuggingFace + GitHub, ~3.8 GB
uv run phase0 extract          # network-free: distill data/raw/ -> trajectories.parquet
uv run phase0 freeze-splits    # writes data/splits/*.json with content hashes
uv run phase0 run              # network-free: fits all models -> phase0_results.json
uv run phase0 report           # -> phase0_report.md
```

**Status of this PR**: the full pipeline above is implemented and was verified
end-to-end against a synthetic in-memory dataset (see commit history), and a
real `fetch` was started against the live S3 bucket + HuggingFace dataset
(confirmed working: real submissions listed, real `.traj` files downloaded and
distilled, real `results.json` fetched). It did not finish downloading the full
~3.8 GB across all 6 required submissions within this session -- the sandboxed
network in this environment sustains roughly 1-1.5 MB/s aggregate across 16
connections, which projects to on the order of an hour for the full fetch, not
minutes. Consequently **`data/interim/trajectories.parquet` and
`results/phase0_results.json`/`phase0_report.md` are NOT committed by this
PR** -- committing them would mean either fabricating data or committing a
partial/inconsistent extraction, both worse than being explicit about the gap.
What IS committed and real: the complete harness code, `tests/fixtures/mini.parquet`
(an honest small synthetic fixture, never presented as real trajectory data),
and a full passing test suite + CI job that exercises every code path
(extraction schema, leakage guard, censoring, metrics, splits, candidate
fitting) against that fixture with zero network access.

To produce the real evidence artifact, run the five commands above locally
with network access (`make phase0` also works once `data/interim/` and
`data/splits/` are populated, network-free from that point on). See
PROVENANCE.md for the full data-source writeup and known limitations.

## What this is

A standalone Python data-science harness, living outside the Rust workspace,
that answers one question: **can a simple, interpretable model predict a coding
agent's per-task cost well enough at admission time to beat a naive
per-repo-median baseline on a budget-admission decision?** See
`configs/phase0.yaml` and `docs/` (this ticket's design review) for the full
methodology; `PROVENANCE.md` for data sources, licenses, and known
limitations.

## Layout

- `src/phase0/` -- the package (`sources/`, `adapters/`, `models/`, `metrics/`).
- `configs/phase0.yaml` -- the single frozen source of truth for seeds, the
  submission list, the feature whitelist, quantiles, budget grid, and the
  wall-clock proxy constants.
- `data/interim/trajectories.parquet` -- **not committed in this PR** (see
  "Status of this PR" above); when produced, contains derived numeric
  features and IDs only, never raw issue text.
- `data/splits/{cold_start,history_assisted}.json` -- **not committed in this
  PR** for the same reason (there's no extracted dataset to freeze splits
  against yet). Format: frozen GroupKFold(5) splits with a content hash;
  `phase0 run` hard-errors if these are missing or have been hand-edited
  after freezing.
- `data/cache/llm_selfestimate.json` -- **not committed**. No LLM API access
  was available while building this harness, so there is no real
  self-estimate data to cache; every `llm_self_estimate` row in the results
  artifact is `status: "unavailable"` rather than a fabricated fixture. See
  PROVENANCE.md.
- `results/phase0_results.json` / `results/phase0_report.md` -- **not
  committed in this PR** (see "Status of this PR" above); produced by
  `phase0 run` / `phase0 report` once real data is extracted.
- `tests/fixtures/mini.parquet` -- a small, deterministic, synthetic fixture
  used by CI. Never real trajectory data.

## Falsification rule

If `candidate` does not beat `taskclass_repo` on `regret_usd` at matched
admit-rate, the hypothesis is not supported by this evidence. Coverage /
pinball / sharpness numbers are supporting evidence, not the headline
finding. See `results/phase0_report.md` for the actual verdict.

## Escalation condition

If the extracted dataset has fewer than `min_uncensored_rows` (configured in
`configs/phase0.yaml`, default 2000) uncensored rows, `phase0 run` refuses to
fit models and exits nonzero. This is a dataset-adequacy judgment for
HORO-1123, not something this harness routes around.
