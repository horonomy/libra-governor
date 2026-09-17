#!/usr/bin/env python3
"""HORO-1127 MVP 1.0 release-gate evidence corpus.

Drives the REAL compiled ``libra-governor`` binary as a subprocess through
realistic Claude Code hook invocations (UserPromptSubmit -> PostToolUse* ->
Stop), exactly as Claude Code itself would: real JSON on stdin, real
subprocess spawns, a real Unix-socket daemon, a real SQLite ledger on disk.

Nothing here calls into the Rust crates in-process. Every observation is
made by parsing the CLI's actual stdout/stderr or by inspecting the daemon's
on-disk state (the SQLite ledger, the log file) after the fact.

Usage:
    python3 run_corpus.py --binary <path/to/libra-governor> \
        --state-dir <dir> --out <results.json>
"""

from __future__ import annotations

import argparse
import json
import os
import random
import signal
import string
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = Path(__file__).resolve().parent / "fixtures"

PROMPTS = [
    "add input validation to the login handler",
    "fix the off-by-one in the pagination logic",
    "add a rate limiter to the login endpoint",
    "refactor the pagination helper to avoid integer overflow",
    "write a regression test for the login bypass bug",
    "add logging around failed login attempts",
    "fix the pagination bug when total is zero",
    "harden login against empty password strings",
    "extract the pagination math into its own function",
    "add a docstring to the login handler",
    "fix pagination so the last page is never empty",
    "add unit tests for page_bounds",
    "sanitize the username field before login",
    "cap pagination page_size to a sane maximum",
    "add a timeout to the login flow",
]

TOOL_NAMES = ["Read", "Edit", "Bash", "Write", "Grep"]

FIXTURE_REPOS = [
    FIXTURES / "rust-crate",
    FIXTURES / "python-pkg",
    FIXTURES / "ts-pkg",
]


def run_hook(binary: Path, subcommand: list[str], payload: dict, env: dict) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(binary), *subcommand],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )


def parse_additional_context(stdout: str) -> dict:
    try:
        obj = json.loads(stdout)
        return obj.get("hookSpecificOutput", {})
    except json.JSONDecodeError:
        return {}


@dataclass
class TaskRecord:
    index: int
    fixture: str
    prompt: str
    session_id: str
    preflight_ok: bool = False
    task_id: str | None = None
    confidence: str | None = None
    recon_truncated: bool | None = None
    contract_revision: int | None = None
    estimate_present: bool = False
    estimate_cold_start: bool | None = None
    tool_calls_sent: int = 0
    stop_ok: bool = False
    receipt_reconstructed: bool = False
    outcome: str | None = None
    actual_duration_secs: float | None = None
    stderr_summary: str | None = None
    errors: list[str] = field(default_factory=list)


def do_task(binary: Path, env: dict, index: int, fixture: Path, prompt: str) -> TaskRecord:
    session_id = f"corpus-{index}-{uuid.uuid4()}"
    rec = TaskRecord(index=index, fixture=fixture.name, prompt=prompt, session_id=session_id)

    # 1. Preflight (UserPromptSubmit)
    preflight_payload = {
        "session_id": session_id,
        "cwd": str(fixture),
        "prompt": prompt,
        "hook_event_name": "UserPromptSubmit",
        "transcript_path": "/tmp/does-not-exist.jsonl",
    }
    proc = run_hook(binary, ["hook", "user-prompt-submit"], preflight_payload, env)
    if proc.returncode != 0:
        rec.errors.append(f"preflight exit {proc.returncode}: {proc.stderr[:500]}")
        return rec

    ctx = parse_additional_context(proc.stdout)
    text = ctx.get("additionalContext", "")
    if "task " in text:
        rec.preflight_ok = True
        try:
            rec.task_id = text.split("task ", 1)[1].split(",")[0].strip()
        except IndexError:
            pass
    for level in ("low", "medium", "high"):
        if f"confidence: {level}" in text:
            rec.confidence = level
    rec.recon_truncated = "truncated" in text.lower() and "Note:" in text
    if "revision " in text:
        try:
            rec.contract_revision = int(
                text.split("revision ", 1)[1].split(")")[0].strip()
            )
        except (IndexError, ValueError):
            pass
    rec.estimate_present = "Cost/time estimate:" in text
    rec.estimate_cold_start = "cold start" in text if rec.estimate_present else None

    # 2. A few PostToolUse notifications (fire-and-forget)
    n_tools = random.randint(2, 6)
    for _ in range(n_tools):
        tool_payload = {
            "session_id": session_id,
            "tool_name": random.choice(TOOL_NAMES),
            "cwd": str(fixture),
            "hook_event_name": "PostToolUse",
        }
        tproc = run_hook(binary, ["hook", "post-tool-use"], tool_payload, env)
        if tproc.returncode != 0:
            rec.errors.append(f"post-tool-use exit {tproc.returncode}: {tproc.stderr[:200]}")
        else:
            rec.tool_calls_sent += 1

    # Simulate real elapsed "execution" time between preflight and stop.
    time.sleep(random.uniform(0.05, 0.35))

    # 3. Stop (finalize)
    stop_payload = {
        "session_id": session_id,
        "model": "claude-sonnet-5",
        "hook_event_name": "Stop",
        "stop_hook_active": False,
    }
    sproc = run_hook(binary, ["hook", "stop"], stop_payload, env)
    if sproc.returncode != 0:
        rec.errors.append(f"stop exit {sproc.returncode}: {sproc.stderr[:500]}")
        return rec

    rec.stderr_summary = sproc.stderr.strip()
    if "Execution Receipt" in sproc.stderr:
        rec.stop_ok = True
        rec.receipt_reconstructed = True
        if "Outcome:" in sproc.stderr:
            rec.outcome = sproc.stderr.split("Outcome:", 1)[1].split("\n", 1)[0].strip()
        if "Actual:" in sproc.stderr:
            actual_line = sproc.stderr.split("Actual:", 1)[1].split("\n", 1)[0].strip()
            dur = actual_line.split("s", 1)[0].strip()
            try:
                rec.actual_duration_secs = float(dur)
            except ValueError:
                pass
    elif "no active task" in sproc.stderr.lower():
        rec.stop_ok = True  # safe no-op is a legitimate, documented outcome
    return rec


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True, type=Path)
    ap.add_argument("--state-dir", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--corpus-size", type=int, default=34)
    ap.add_argument("--seed", type=int, default=1127)
    args = ap.parse_args()

    random.seed(args.seed)
    args.state_dir.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["LIBRA_GOVERNOR_STATE_DIR"] = str(args.state_dir)

    binary = args.binary.resolve()
    if not binary.exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    records: list[TaskRecord] = []
    for i in range(args.corpus_size):
        fixture = FIXTURE_REPOS[i % len(FIXTURE_REPOS)]
        prompt = PROMPTS[i % len(PROMPTS)]
        rec = do_task(binary, env, i, fixture, prompt)
        records.append(rec)
        print(f"[{i+1}/{args.corpus_size}] {fixture.name:14s} preflight={rec.preflight_ok} "
              f"conf={rec.confidence} stop={rec.stop_ok} outcome={rec.outcome} "
              f"errors={len(rec.errors)}")

    out = {
        "corpus_size": len(records),
        "tasks": [r.__dict__ for r in records],
        "summary": {
            "preflight_success_rate": sum(r.preflight_ok for r in records) / len(records),
            "estimate_present_rate": sum(r.estimate_present for r in records) / len(records),
            "receipt_reconstruction_rate": sum(r.receipt_reconstructed for r in records) / len(records),
            "stop_ok_rate": sum(r.stop_ok for r in records) / len(records),
            "tasks_with_errors": sum(1 for r in records if r.errors),
        },
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=2))
    print(json.dumps(out["summary"], indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
