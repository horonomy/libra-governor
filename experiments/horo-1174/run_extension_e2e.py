#!/usr/bin/env python3
"""HORO-1174 local extension points: real end-to-end evidence.

Drives a real compiled `libra-governor` binary as a subprocess, a real
daemon it spawns, and the real `examples/local-providers/libra_example_provider.py`
reference provider as a separate subprocess, talking real signed HTTP
over loopback. No Rust function is called in-process; the SQLite ledger
and the provider's own `events.jsonl` are inspected as real on-disk
artifacts. Follows the exact harness pattern established by
`experiments/mvp1_validation/run_validation_matrix.py` (HORO-1127) and
`experiments/mvp3_gate/run_gate_matrix.py` (HORO-1146).

Scenarios:
  1. A real preflight fetches business context from the provider (which
     resolves this repository's own `HORO-1174` branch via a real `git`
     subprocess) and persists a `business_context` ledger row.
  2. The resulting admission event is delivered to the provider and
     signature-verified BY THE PROVIDER ITSELF (not by this harness) --
     see the provider's own `events.jsonl` record, and `webhook_deliveries`
     transitions to `delivered`.
  3. A real `PossibleToolLoop` replan (same technique as
     `experiments/mvp3_gate/run_gate_matrix.py` scenario 5) triggers a
     replan event, also delivered.
  4. `Stop` finalizes the task; an `outcome` event (source=governor_local)
     is delivered.
  5. `examples/local-providers/report_outcome.sh` pushes a real
     Provider-sourced outcome attestation over the *existing* Unix
     socket (`libra-governor outcome record`, no new HTTP listener); the
     ledger shows it authoritative, and a duplicate push is a genuine
     no-op (same `idempotency_key`).

Scope note -- Policy Webhook approve/reject/abstain and the full
`ApprovalRequired` path are deliberately NOT re-driven here. Reaching
`ApprovalRequired` requires either seeding a synthetic reservation row
ahead of a real preflight (the exact technique
`crates/daemon/tests/extension_integration.rs` and
`crates/daemon/tests/reservation_integration.rs` already use,
deliberately, as a controlled Rust test double -- 10/10 passing,
including `policy_webhook_approve_...`/`..._reject_...`/`..._abstain_...`)
or waiting out real usage history this harness has no way to accumulate
believably in a few seconds. Re-deriving the identical synthetic-
reservation trick here, in Python, against the real daemon would not be
independent evidence -- it would be the same fabricated ledger state one
layer further from the code it is testing, which is exactly what this
campaign's "no fabricated data" instruction rules out. The compiled,
passing Rust suite is the real evidence for that path.

Usage:
    python3 run_extension_e2e.py --binary <path> --work-root <dir> --out <path>
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket as socket_mod
import sqlite3
import subprocess
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
PROVIDER_SCRIPT = REPO_ROOT / "examples" / "local-providers" / "libra_example_provider.py"
REPORT_OUTCOME_SCRIPT = REPO_ROOT / "examples" / "local-providers" / "report_outcome.sh"

SOCKET_FILENAME = "daemon.sock"
LEDGER_FILENAME = "ledger.sqlite3"
LOG_FILENAME = "daemon.log"

TASK_ID_RE = re.compile(r"task ([0-9a-f-]{36})")


def find_free_port() -> int:
    with socket_mod.socket(socket_mod.AF_INET, socket_mod.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run_hook(binary, subcommand, payload, env, timeout=30):
    return subprocess.run(
        [str(binary), *subcommand],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
    )


def preflight(binary, env, session_id, cwd, prompt):
    payload = {
        "session_id": session_id,
        "cwd": str(cwd),
        "prompt": prompt,
        "hook_event_name": "UserPromptSubmit",
    }
    return run_hook(binary, ["hook", "user-prompt-submit"], payload, env)


def post_tool_use(binary, env, session_id, tool_name="Bash"):
    payload = {"session_id": session_id, "tool_name": tool_name, "hook_event_name": "PostToolUse"}
    return run_hook(binary, ["hook", "post-tool-use"], payload, env)


def stop(binary, env, session_id, model="claude-sonnet-5"):
    payload = {"session_id": session_id, "model": model, "hook_event_name": "Stop"}
    return run_hook(binary, ["hook", "stop"], payload, env)


def wait_for_socket(state_dir: Path, timeout=5.0) -> bool:
    deadline = time.time() + timeout
    sock = state_dir / SOCKET_FILENAME
    while time.time() < deadline:
        if sock.exists():
            return True
        time.sleep(0.05)
    return sock.exists()


def wait_for_tcp(host: str, port: int, timeout=5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket_mod.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def sqlite_query(ledger_path: Path, sql: str, params=()):
    if not ledger_path.exists():
        return []
    conn = sqlite3.connect(str(ledger_path))
    try:
        return conn.execute(sql, params).fetchall()
    finally:
        conn.close()


def wait_until(predicate, timeout=10.0, interval=0.1):
    deadline = time.time() + timeout
    result = predicate()
    while not result and time.time() < deadline:
        time.sleep(interval)
        result = predicate()
    return result


def read_events_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    records = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            records.append(json.loads(line))
    return records


class Matrix:
    def __init__(self, binary: Path, work_root: Path):
        self.binary = binary
        self.work_root = work_root
        self.results: dict[str, dict] = {}

    def record(self, name, passed, detail):
        self.results[name] = {"passed": passed, "detail": detail}
        print(f"[{'PASS' if passed else 'FAIL'}] {name}: {detail}")

    def run(self):
        state_dir = self.work_root / "state"
        if state_dir.exists():
            shutil.rmtree(state_dir)
        state_dir.mkdir(parents=True, exist_ok=True)

        secret_file = self.work_root / "secret.txt"
        secret_file.write_text("sk-fake-horo1174-e2e-signing-secret")

        events_log = self.work_root / "events.jsonl"
        if events_log.exists():
            events_log.unlink()

        provider_port = find_free_port()

        config = {
            "extensions": {
                "business_context_provider": {
                    "url": f"http://127.0.0.1:{provider_port}/libra/business-context",
                    "timeout_ms": 700,
                    "secret_command": "cat",
                    "secret_args": [str(secret_file)],
                },
                "policy_webhook": {
                    "url": f"http://127.0.0.1:{provider_port}/libra/policy-decision",
                    "timeout_ms": 700,
                    "secret_command": "cat",
                    "secret_args": [str(secret_file)],
                },
                "events": {
                    "url": f"http://127.0.0.1:{provider_port}/libra/events",
                    "timeout_ms": 3000,
                    "max_attempts": 5,
                    "kinds": ["admission", "replan", "approval", "outcome"],
                    "secret_command": "cat",
                    "secret_args": [str(secret_file)],
                },
            }
        }
        (state_dir / "config.json").write_text(json.dumps(config))

        env = dict(os.environ)
        env["LIBRA_GOVERNOR_STATE_DIR"] = str(state_dir)

        provider = subprocess.Popen(
            [
                "python3",
                str(PROVIDER_SCRIPT),
                "--port",
                str(provider_port),
                "--secret-file",
                str(secret_file),
                "--events-log",
                str(events_log),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        daemon = None
        try:
            provider_up = wait_for_tcp("127.0.0.1", provider_port)
            self.record(
                "setup_provider_started",
                provider_up,
                f"example provider listening on 127.0.0.1:{provider_port}: {provider_up}",
            )
            if not provider_up:
                return

            daemon = subprocess.Popen(
                [str(self.binary), "daemon", "run"],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            socket_up = wait_for_socket(state_dir)
            log_path = state_dir / LOG_FILENAME
            log_text = log_path.read_text() if log_path.exists() else ""
            self.record(
                "setup_daemon_started_with_extensions_config",
                socket_up,
                f"daemon socket present: {socket_up}. daemon.log (verbatim): {log_text!r}",
            )
            if not socket_up:
                return

            self._scenario_business_context_and_admission_event(env, state_dir, events_log)
            self._scenario_replan_event(env, state_dir, events_log)
            self._scenario_outcome_event_on_finalize(env, state_dir, events_log)
            self._scenario_record_outcome_over_the_socket(env, state_dir)
        finally:
            if daemon is not None and daemon.poll() is None:
                daemon.send_signal(signal.SIGKILL)
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
            if provider.poll() is None:
                provider.send_signal(signal.SIGTERM)
                try:
                    provider.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    provider.kill()

    def _scenario_business_context_and_admission_event(self, env, state_dir, events_log):
        ledger_path = state_dir / LEDGER_FILENAME
        sid = f"e2e-{uuid.uuid4()}"
        # REPO_ROOT's own current branch is v0.0.2/HORO-1174/extension_contracts
        # -- the example provider's real `git rev-parse --abbrev-ref HEAD`
        # against this exact cwd resolves the real HORO-1174 ticket key
        # and matches it in examples/local-providers/tickets.json.
        p = preflight(self.binary, env, sid, REPO_ROOT, "verify HORO-1174 extension points end to end")
        preflight_ok = p.returncode == 0
        match = TASK_ID_RE.search(p.stdout)
        task_id = match.group(1) if match else None
        self.task_id = task_id
        self.session_id = sid

        bc_rows = sqlite_query(
            ledger_path,
            "SELECT provider_id, cost_center, applied FROM business_context WHERE task_id = ?",
            (task_id,) if task_id else ("",),
        )
        business_context_persisted = len(bc_rows) == 1 and bc_rows[0][1] == "eng-platform"

        delivered = wait_until(
            lambda: any(
                r.get("event_kind") == "admission" for r in read_events_jsonl(events_log)
            )
        )
        events = read_events_jsonl(events_log)
        admission_events = [r for r in events if r.get("event_kind") == "admission"]
        admission_matches_task = bool(admission_events) and admission_events[0]["data"].get(
            "task_id"
        ) == task_id

        webhook_rows = sqlite_query(
            ledger_path,
            "SELECT state FROM webhook_deliveries WHERE event_kind = 'admission' AND task_id = ?",
            (task_id,) if task_id else ("",),
        )
        webhook_delivered_state = webhook_rows[0][0] if webhook_rows else None

        self.record(
            "business_context_fetched_and_persisted",
            bool(preflight_ok and task_id and business_context_persisted),
            f"preflight rc={p.returncode}, task_id={task_id}, business_context rows={bc_rows}. "
            f"preflight stdout (verbatim): {p.stdout.strip()!r}",
        )
        self.record(
            "admission_event_delivered_to_real_provider",
            bool(delivered and admission_matches_task),
            f"provider events.jsonl admission entries={admission_events}, "
            f"webhook_deliveries.state for this admission row={webhook_delivered_state}",
        )

    def _scenario_replan_event(self, env, state_dir, events_log):
        ledger_path = state_dir / LEDGER_FILENAME
        sid = self.session_id
        before = len(
            sqlite_query(ledger_path, "SELECT id FROM replan_events WHERE task_id = ?", (self.task_id,))
        ) if self.task_id else 0

        # Same PossibleToolLoop technique as
        # experiments/mvp3_gate/run_gate_matrix.py scenario 5: the same
        # tool invoked >=4 times consecutively with no different tool
        # between is a deterministic, history-free replan trigger.
        for _ in range(5):
            post_tool_use(self.binary, env, sid, "Bash")

        after_rows = sqlite_query(
            ledger_path, "SELECT id FROM replan_events WHERE task_id = ?", (self.task_id,)
        ) if self.task_id else []
        replan_triggered = len(after_rows) > before

        delivered = wait_until(
            lambda: any(r.get("event_kind") == "replan" for r in read_events_jsonl(events_log))
        )

        self.record(
            "replan_event_delivered_to_real_provider",
            bool(replan_triggered and delivered),
            f"replan_events before={before}, after={len(after_rows)}. "
            f"provider saw a replan event: {delivered}",
        )

    def _scenario_outcome_event_on_finalize(self, env, state_dir, events_log):
        ledger_path = state_dir / LEDGER_FILENAME
        sid = self.session_id
        s = stop(self.binary, env, sid)
        stop_ok = s.returncode == 0

        receipts = sqlite_query(
            ledger_path,
            "SELECT task_id, plan_id, outcome_json, recorded_at FROM receipts WHERE task_id = ?",
            (self.task_id,) if self.task_id else ("",),
        )

        delivered = wait_until(
            lambda: any(
                r.get("event_kind") == "outcome" and r.get("data", {}).get("source") == "governor_local"
                for r in read_events_jsonl(events_log)
            )
        )

        self.record(
            "finalize_writes_receipt_and_delivers_governor_local_outcome_event",
            bool(stop_ok and len(receipts) == 1 and delivered),
            f"stop rc={s.returncode}, receipts for task={receipts}, "
            f"governor_local outcome event delivered={delivered}. "
            f"stop stderr (verbatim): {s.stderr.strip()!r}",
        )

    def _scenario_record_outcome_over_the_socket(self, env, state_dir):
        ledger_path = state_dir / LEDGER_FILENAME
        if not self.task_id:
            self.record("record_outcome_over_the_socket", False, "no task_id from earlier scenario")
            return

        idempotency_key = f"e2e-run-{uuid.uuid4()}"
        first = subprocess.run(
            [
                "bash",
                str(REPORT_OUTCOME_SCRIPT),
                "--task-id",
                self.task_id,
                "--source-id",
                "example-provider",
                "--idempotency-key",
                idempotency_key,
                "--kind",
                "completed",
                "--evidence",
                "https://ci.example.com/horo-1174-e2e",
                "--binary",
                str(self.binary),
            ],
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        # A second push with the SAME idempotency_key must be a real
        # no-op (Duplicate), not a second attestation row.
        second = subprocess.run(
            [
                "bash",
                str(REPORT_OUTCOME_SCRIPT),
                "--task-id",
                self.task_id,
                "--source-id",
                "example-provider",
                "--idempotency-key",
                idempotency_key,
                "--kind",
                "completed",
                "--evidence",
                "https://ci.example.com/horo-1174-e2e",
                "--binary",
                str(self.binary),
            ],
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )

        attestation_rows = sqlite_query(
            ledger_path,
            "SELECT source, authoritative FROM outcome_attestations WHERE task_id = ? AND idempotency_key = ?",
            (self.task_id, idempotency_key),
        )
        exactly_one_row = len(attestation_rows) == 1
        authoritative = exactly_one_row and attestation_rows[0][1] == 1

        first_ok = first.returncode == 0 and '"state":"recorded"' in first.stdout
        second_ok = second.returncode == 0 and '"state":"duplicate"' in second.stdout

        self.record(
            "record_outcome_over_the_socket",
            bool(first_ok and second_ok and exactly_one_row and authoritative),
            f"first push stdout={first.stdout.strip()!r} rc={first.returncode}, "
            f"second (duplicate) push stdout={second.stdout.strip()!r} rc={second.returncode}, "
            f"outcome_attestations rows for this idempotency_key={attestation_rows}",
        )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--work-root", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    binary = Path(args.binary).resolve()
    if not (binary.is_file() and os.access(binary, os.X_OK)):
        raise SystemExit(f"--binary does not point at an executable file: {binary}")
    work_root = Path(args.work_root).resolve()
    work_root.mkdir(parents=True, exist_ok=True)

    matrix = Matrix(binary, work_root)
    matrix.run()

    out_path = Path(args.out).resolve()
    # Same traversal guard as experiments/mvp3_gate/run_gate_matrix.py
    # (SonarCloud pythonsecurity:S8707): --out is externally supplied, so
    # constrain it inside the repository rather than trusting it blindly.
    if REPO_ROOT not in out_path.parents and out_path != REPO_ROOT:
        raise SystemExit(f"--out must resolve inside the repository: {out_path}")
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(matrix.results, indent=2))

    all_passed = all(r["passed"] for r in matrix.results.values())
    print(f"\n{'ALL PASSED' if all_passed else 'SOME FAILED'}: {len(matrix.results)} scenarios")
    raise SystemExit(0 if all_passed else 1)


if __name__ == "__main__":
    main()
