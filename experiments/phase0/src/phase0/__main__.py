"""phase0 CLI: fetch | extract | freeze-splits | run | report.

See README.md for the single "make it go" command.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
import math
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import numpy as np
import pandas as pd

from phase0 import pipeline, report, runner, splits
from phase0.config import load_config

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_RAW_DIR = REPO_ROOT / "data" / "raw"
DEFAULT_INTERIM_PARQUET = REPO_ROOT / "data" / "interim" / "trajectories.parquet"
DEFAULT_SPLITS_DIR = REPO_ROOT / "data" / "splits"
DEFAULT_CACHE_DIR = REPO_ROOT / "data" / "cache"
DEFAULT_RESULTS_JSON = REPO_ROOT / "results" / "phase0_results.json"
DEFAULT_REPORT_MD = REPO_ROOT / "results" / "phase0_report.md"


def _git_sha() -> str:
    try:
        return (
            subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, stderr=subprocess.DEVNULL
            )
            .decode()
            .strip()
        )
    except Exception:
        return "unknown"


def _file_sha256(path: Path) -> str:
    if not path.exists():
        return "sha256:absent"
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


def _package_versions() -> dict[str, str]:
    versions = {}
    for pkg in ["pandas", "numpy", "scikit-learn", "pyarrow", "pyyaml", "requests", "datasets"]:
        try:
            versions[pkg] = importlib.metadata.version(pkg)
        except importlib.metadata.PackageNotFoundError:
            versions[pkg] = "unknown"
    return versions


def _json_sanitize(value: Any) -> Any:
    """Recursively converts numpy scalars to native Python and non-finite
    floats (NaN/Inf) to None, so json.dumps produces strict, valid JSON.
    Non-finite floats and numpy scalar types are both guaranteed to occur in
    this artifact (e.g. coverage_censored on an empty stratum, np.float64
    metric values) -- this is not a defensive no-op.
    """
    if isinstance(value, dict):
        return {k: _json_sanitize(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_sanitize(v) for v in value]
    if isinstance(value, (np.floating,)):
        value = float(value)
    elif isinstance(value, (np.integer,)):
        return int(value)
    elif isinstance(value, (np.bool_,)):
        return bool(value)
    if isinstance(value, float) and not math.isfinite(value):
        return None
    return value


def cmd_fetch(args: argparse.Namespace) -> None:
    cfg = load_config(args.config)
    pipeline.fetch_all(cfg, DEFAULT_RAW_DIR, include_optional=args.include_optional)
    print(f"fetch complete -> {DEFAULT_RAW_DIR}")


def cmd_extract(args: argparse.Namespace) -> None:
    cfg = load_config(args.config)
    df = pipeline.extract_dataframe(cfg, DEFAULT_RAW_DIR, include_optional=args.include_optional)
    DEFAULT_INTERIM_PARQUET.parent.mkdir(parents=True, exist_ok=True)
    df.to_parquet(DEFAULT_INTERIM_PARQUET, index=False)

    n_uncensored = int((~df["censored"]).sum()) if not df.empty else 0
    min_required = int(cfg["min_uncensored_rows"])
    print(
        f"extract complete -> {DEFAULT_INTERIM_PARQUET} ({len(df)} rows, {n_uncensored} uncensored)"
    )
    if n_uncensored < min_required:
        print(
            f"WARNING: only {n_uncensored} uncensored rows (< {min_required} required). "
            f"This is the HORO-1120 escalation condition -- see PROVENANCE.md. "
            f"`phase0 run` will refuse to proceed until this is resolved or explicitly overridden."
        )


def cmd_freeze_splits(args: argparse.Namespace) -> None:
    if not DEFAULT_INTERIM_PARQUET.exists():
        raise SystemExit(f"{DEFAULT_INTERIM_PARQUET} does not exist. Run `phase0 extract` first.")
    df = pd.read_parquet(DEFAULT_INTERIM_PARQUET)
    for name in splits.SPLIT_CONFIGS:
        path = splits.freeze_split(df, name, DEFAULT_SPLITS_DIR)
        print(f"froze {name} -> {path}")


def cmd_run(args: argparse.Namespace) -> None:
    cfg = load_config(args.config)
    if not DEFAULT_INTERIM_PARQUET.exists():
        raise SystemExit(f"{DEFAULT_INTERIM_PARQUET} does not exist. Run `phase0 extract` first.")
    df = pd.read_parquet(DEFAULT_INTERIM_PARQUET)

    n_uncensored = int((~df["censored"]).sum()) if not df.empty else 0
    min_required = int(cfg["min_uncensored_rows"])
    if n_uncensored < min_required and not args.force:
        raise SystemExit(
            f"ESCALATION: only {n_uncensored} uncensored rows, below the "
            f"min_uncensored_rows={min_required} threshold in configs/phase0.yaml. "
            f"Per the HORO-1120 spec, this is a dataset-adequacy judgment for a "
            f"human / HORO-1123, not something to route around. Stopping before "
            f"model fitting. Pass --force to override for a debug run (results "
            f"will still be tagged with the shortfall)."
        )

    run_output = runner.run_all(cfg, df, DEFAULT_SPLITS_DIR, DEFAULT_CACHE_DIR)

    results = {
        "schema_version": "1.0",
        "run": {
            "seed": int(cfg["seed"]),
            "git_sha": _git_sha(),
            "config_sha256": cfg.sha256,
            "dataset_fingerprint": _file_sha256(DEFAULT_INTERIM_PARQUET),
            "package_versions": _package_versions(),
            "generated_at": datetime.now(timezone.utc).isoformat(),
        },
        "dataset": {
            "n_rows": int(len(df)),
            "n_instances": int(df["instance_id"].nunique()) if not df.empty else 0,
            "n_submissions": int(df["submission"].nunique()) if not df.empty else 0,
            "censoring_rate": float(df["censored"].mean()) if not df.empty else float("nan"),
            "resolved_rate": float(df["resolved"].mean()) if not df.empty else float("nan"),
            "field_coverage": {
                "wall_clock_seconds": 0.0,  # always a proxy, per spec -- never measured.
                "instance_cost_usd": float(df["instance_cost_usd"].notna().mean())
                if not df.empty
                else 0.0,
                "difficulty": float(df["difficulty"].notna().mean()) if not df.empty else 0.0,
            },
            "escalation_triggered": n_uncensored < min_required,
            "n_uncensored_rows": n_uncensored,
            "min_uncensored_rows": min_required,
        },
        **run_output,
    }

    DEFAULT_RESULTS_JSON.parent.mkdir(parents=True, exist_ok=True)
    results = _json_sanitize(results)
    DEFAULT_RESULTS_JSON.write_text(
        json.dumps(results, indent=2, sort_keys=True, allow_nan=False), encoding="utf-8"
    )
    print(f"run complete -> {DEFAULT_RESULTS_JSON}")


def cmd_report(args: argparse.Namespace) -> None:
    if not DEFAULT_RESULTS_JSON.exists():
        raise SystemExit(f"{DEFAULT_RESULTS_JSON} does not exist. Run `phase0 run` first.")
    results = json.loads(DEFAULT_RESULTS_JSON.read_text(encoding="utf-8"))
    report_md = report.render_report(results)
    DEFAULT_REPORT_MD.parent.mkdir(parents=True, exist_ok=True)
    DEFAULT_REPORT_MD.write_text(report_md, encoding="utf-8")
    print(f"report complete -> {DEFAULT_REPORT_MD}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="phase0")
    sub = parser.add_subparsers(dest="command", required=True)

    p_fetch = sub.add_parser(
        "fetch", help="Network: download S3 trajs + results.json + HF metadata"
    )
    p_fetch.add_argument("--config", default=None)
    p_fetch.add_argument("--include-optional", action="store_true")
    p_fetch.set_defaults(func=cmd_fetch)

    p_extract = sub.add_parser(
        "extract", help="Network-free: distill data/raw/ -> trajectories.parquet"
    )
    p_extract.add_argument("--config", default=None)
    p_extract.add_argument("--include-optional", action="store_true")
    p_extract.set_defaults(func=cmd_extract)

    p_freeze = sub.add_parser("freeze-splits", help="Write data/splits/*.json with content hashes")
    p_freeze.add_argument("--config", default=None)
    p_freeze.set_defaults(func=cmd_freeze_splits)

    p_run = sub.add_parser(
        "run", help="Network-free: fit all models, write results/phase0_results.json"
    )
    p_run.add_argument("--config", default=None)
    p_run.add_argument(
        "--force",
        action="store_true",
        help="Override the min_uncensored_rows escalation check (debug only)",
    )
    p_run.set_defaults(func=cmd_run)

    p_report = sub.add_parser("report", help="Turn phase0_results.json into phase0_report.md")
    p_report.set_defaults(func=cmd_report)

    return parser


def main(argv: list[str] | None = None) -> None:
    parser = build_parser()
    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    sys.exit(main() or 0)
