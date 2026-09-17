# MVP 1.0 release-gate evidence corpus (HORO-1127)

Drives the real compiled `libra-governor` binary as a subprocess through
realistic Claude Code hook invocations (`hook user-prompt-submit` ->
`hook post-tool-use`* -> `hook stop`) against a handful of synthetic fixture
repos, plus the required lifecycle/integration validation matrix from the
ticket. Nothing here calls into the Rust crates in-process — every
observation comes from parsing the CLI's real stdout/stderr or inspecting
the daemon's real on-disk state (SQLite ledger, log file) after the fact.

## Layout

- `fixtures/` — small synthetic-but-realistic repos (`rust-crate`,
  `python-pkg`, `ts-pkg`), each with a `login`- and `pagination`-named
  source file so bounded reconnaissance has real signal to match against.
  Not part of the Cargo workspace (excluded in the root `Cargo.toml`).
- `run_corpus.py` — drives the 20-50 task corpus (preflight -> tool calls
  -> stop) and records per-task results.
- `run_validation_matrix.py` — drives the required validation matrix:
  resume/restart, multi-session, user abort + stray Stop, daemon restart
  mid-flight, recon budget cap, clean worktree, privacy inspection, and the
  concurrency/race check.
- `fresh_clone_quality_check.sh` — clones the branch fresh and runs
  `cargo fmt --check` / `clippy -D warnings` / `build --release` /
  `test --workspace` plus a secret scan, to catch "works on my machine"
  issues.
- `render_summary.py` — renders the two JSON result files into
  `results/HORO-1127_evidence_summary.md`.
- `results/` — the committed evidence artifact for the HORO-1127 release
  gate: `corpus_results.json`, `validation_matrix_results.json`, and the
  rendered Markdown summary.

## Reproducing

```bash
cargo build --workspace --release
BIN=target/release/libra-governor
python3 experiments/mvp1_validation/run_corpus.py \
  --binary "$BIN" --state-dir /tmp/horo1127-corpus-state \
  --out experiments/mvp1_validation/results/corpus_results.json
python3 experiments/mvp1_validation/run_validation_matrix.py \
  --binary "$BIN" --work-root /tmp/horo1127-matrix-work \
  --out experiments/mvp1_validation/results/validation_matrix_results.json
python3 experiments/mvp1_validation/render_summary.py \
  --corpus experiments/mvp1_validation/results/corpus_results.json \
  --matrix experiments/mvp1_validation/results/validation_matrix_results.json \
  --out experiments/mvp1_validation/results/HORO-1127_evidence_summary.md
```
