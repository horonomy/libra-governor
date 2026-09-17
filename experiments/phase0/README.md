# Phase 0: Estimator Feasibility Replay Harness (HORO-1120)

```
uv sync --group dev && make phase0
```

That fits every baseline plus the candidate estimator against the already-committed
`data/interim/trajectories.parquet` + `data/splits/*.json`, writes
`results/phase0_results.json`, and renders `results/phase0_report.md`. No network
access required for this path.

To reproduce the full pipeline from scratch (network required -- lists and
downloads ~3.8 GB of SWE-agent trajectory files from a public S3 bucket, plus
task metadata from HuggingFace):

```
uv sync --group dev
uv run phase0 fetch            # network: S3 + HuggingFace + GitHub
uv run phase0 extract          # network-free: distill data/raw/ -> trajectories.parquet
uv run phase0 freeze-splits    # writes data/splits/*.json with content hashes
uv run phase0 run              # network-free: fits all models -> phase0_results.json
uv run phase0 report           # -> phase0_report.md
```

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
- `data/interim/trajectories.parquet` -- committed. Derived numeric features
  and IDs only, never raw issue text.
- `data/splits/{cold_start,history_assisted}.json` -- committed, frozen
  GroupKFold(5) splits with a content hash. `phase0 run` hard-errors if these
  are missing or have been hand-edited after freezing.
- `data/cache/llm_selfestimate.json` -- **not committed**. No LLM API access
  was available while building this harness, so there is no real
  self-estimate data to cache; every `llm_self_estimate` row in the results
  artifact is `status: "unavailable"` rather than a fabricated fixture. See
  PROVENANCE.md.
- `results/phase0_results.json` / `results/phase0_report.md` -- committed
  output artifacts.
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
