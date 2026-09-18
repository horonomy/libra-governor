#!/usr/bin/env python3
"""HORO-1146 (MVP 3.0 release gate) E2E scenarios that ARE reachable
through the real compiled ``libra-governor`` CLI binary as a subprocess,
against a real daemon, a real SQLite ledger, and real hook payloads.

Follows the exact pattern established by
``experiments/mvp1_validation/run_validation_matrix.py`` (HORO-1127):
real subprocesses, real on-disk artifacts, no in-process Rust calls, no
fabricated numbers.

Scenarios NOT covered here (balanced-policy scenarios only) are covered
instead by real Rust integration tests — see
``experiments/mvp3_gate/README.md`` for the full scenario-to-evidence map.

NOTE (HORO-1146 defect #3, fixed): at the time this harness was written,
the shipped CLI's ``daemon run`` hardcoded ``policy:
default_admission_policy()`` and ``gateway: None`` with no CLI/env
override, which is why this harness never drives a non-``balanced``
policy or the gateway. That gap is now fixed —
``crates/daemon/src/config_file.rs`` reads an optional
``<state_dir>/config.json`` — but this harness has not been rewritten to
exercise it; see ``experiments/mvp3_gate/README.md``'s "Load-bearing
methodology note" for why the scenario matrix below is unchanged.

Usage:
    python3 run_gate_matrix.py --binary <path> --work-root <dir> --out <path>
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = REPO_ROOT / "experiments" / "mvp1_validation" / "fixtures"

SOCKET_FILENAME = "daemon.sock"
LEDGER_FILENAME = "ledger.sqlite3"
LOG_FILENAME = "daemon.log"


def run_hook(binary, subcommand, payload, env, timeout=30):
    return subprocess.run(
        [str(binary), *subcommand],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
    )


def spawn_daemon(binary, env):
    return subprocess.Popen(
        [str(binary), "daemon", "run"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def wait_for_socket(state_dir, timeout=5.0):
    deadline = time.time() + timeout
    sock = state_dir / SOCKET_FILENAME
    while time.time() < deadline:
        if sock.exists():
            return True
        time.sleep(0.05)
    return sock.exists()


def daemon_socket_alive(state_dir):
    import socket as socket_mod

    sock_path = state_dir / SOCKET_FILENAME
    if not sock_path.exists():
        return False
    s = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    try:
        s.settimeout(1.0)
        s.connect(str(sock_path))
        return True
    except OSError:
        return False
    finally:
        s.close()


def sqlite_query(ledger_path, sql, params=()):
    if not ledger_path.exists():
        return []
    conn = sqlite3.connect(str(ledger_path))
    try:
        cur = conn.execute(sql, params)
        return cur.fetchall()
    finally:
        conn.close()


def sqlite_exec(ledger_path, sql, params=()):
    conn = sqlite3.connect(str(ledger_path))
    try:
        conn.execute(sql, params)
        conn.commit()
    finally:
        conn.close()


def preflight(binary, env, session_id, cwd, prompt="add input validation to the login handler"):
    payload = {
        "session_id": session_id,
        "cwd": str(cwd),
        "prompt": prompt,
        "hook_event_name": "UserPromptSubmit",
    }
    return run_hook(binary, ["hook", "user-prompt-submit"], payload, env)


def stop(binary, env, session_id, model="claude-sonnet-5"):
    payload = {"session_id": session_id, "model": model, "hook_event_name": "Stop"}
    return run_hook(binary, ["hook", "stop"], payload, env)


def post_tool_use(binary, env, session_id, tool_name="Bash"):
    payload = {"session_id": session_id, "tool_name": tool_name, "hook_event_name": "PostToolUse"}
    return run_hook(binary, ["hook", "post-tool-use"], payload, env)


def statusline(binary, env):
    return subprocess.run([str(binary), "statusline"], capture_output=True, text=True, env=env, timeout=10)


def kill_any_daemon(state_dir, proc=None, sig=signal.SIGKILL):
    if proc is not None and proc.poll() is None:
        proc.send_signal(sig)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
        return
    sock = state_dir / SOCKET_FILENAME
    if not sock.exists():
        return
    try:
        out = subprocess.run(["lsof", "-t", str(sock)], capture_output=True, text=True, timeout=5)
        for pid in out.stdout.split():
            os.kill(int(pid), sig)
    except Exception:
        pass


class Matrix:
    def __init__(self, binary: Path, work_root: Path):
        self.binary = binary
        self.work_root = work_root
        self.results: dict[str, dict] = {}

    def record(self, name, passed, detail):
        self.results[name] = {"passed": passed, "detail": detail}
        print(f"[{'PASS' if passed else 'FAIL'}] {name}: {detail}")

    def fresh_env(self, tag):
        state_dir = self.work_root / f"state-{tag}"
        if state_dir.exists():
            shutil.rmtree(state_dir)
        state_dir.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ)
        env["LIBRA_GOVERNOR_STATE_DIR"] = str(state_dir)
        return env, state_dir

    # --- Scenario 1: balanced policy, normal on-budget completion -----
    def scenario_1_balanced_on_budget_completion(self):
        env, state_dir = self.fresh_env("s1-balanced")
        daemon = spawn_daemon(self.binary, env)
        try:
            assert wait_for_socket(state_dir), "daemon never bound its socket"
            sid = f"s1-{uuid.uuid4()}"
            p = preflight(self.binary, env, sid, FIXTURES / "rust-crate", "add input validation")
            preflight_ok = p.returncode == 0 and "task " in p.stdout
            admitted = "Preflight complete" in p.stdout

            for i, tool in enumerate(["Read", "Edit", "Bash"]):
                post_tool_use(self.binary, env, sid, tool)

            s = stop(self.binary, env, sid)
            stop_ok = s.returncode == 0
            receipt_has_outcome = "Execution Receipt" in s.stderr and "Outcome:" in s.stderr

            receipts = sqlite_query(
                state_dir / LEDGER_FILENAME,
                "SELECT tool_call_count, actual_duration_secs FROM receipts LIMIT 1",
            )
            self.record(
                "s1_balanced_on_budget_completion",
                preflight_ok and admitted and stop_ok and receipt_has_outcome and len(receipts) == 1,
                f"preflight_ok={preflight_ok}, admitted={admitted}, stop_ok={stop_ok}, "
                f"receipt written with tool_call_count={receipts[0][0] if receipts else 'n/a'}, "
                f"actual_duration_secs={receipts[0][1] if receipts else 'n/a'}. "
                f"preflight additionalContext (verbatim): {p.stdout.strip()!r}. "
                f"stop stderr (verbatim): {s.stderr.strip()!r}",
            )
        finally:
            kill_any_daemon(state_dir, proc=daemon)

    # --- Scenario 5: material unexpected failure -> real replan -------
    def scenario_5_material_event_triggers_a_real_replan(self):
        env, state_dir = self.fresh_env("s5-replan")
        daemon = spawn_daemon(self.binary, env)
        try:
            assert wait_for_socket(state_dir), "daemon never bound its socket"
            sid = f"s5-{uuid.uuid4()}"
            preflight(self.binary, env, sid, FIXTURES / "rust-crate", "refactor the pagination module")

            before_events = sqlite_query(state_dir / LEDGER_FILENAME, "SELECT COUNT(*) FROM replan_events")
            before_count = before_events[0][0] if before_events else 0

            # PossibleToolLoop: same tool invoked >= 4 times consecutively
            # with no different tool between (ReplanTriggerKind::PossibleToolLoop,
            # crates/domain/src/replan.rs::possible_tool_loop, threshold=4).
            # This is the deterministic, history-free material-event trigger:
            # no bucket seeding required, unlike ToolCallCountExceeded.
            for _ in range(5):
                post_tool_use(self.binary, env, sid, "Bash")

            st = statusline(self.binary, env)

            after_events = sqlite_query(
                state_dir / LEDGER_FILENAME,
                "SELECT id, trigger, detail FROM replan_events ORDER BY created_at",
            )
            after_count = len(after_events)

            replan_visible = "replanned" in st.stdout

            self.record(
                "s5_material_event_triggers_a_real_replan",
                after_count > before_count and replan_visible,
                f"replan_events before={before_count}, after={after_count} (rows: {after_events}). "
                f"statusline (verbatim): {st.stdout.strip()!r}",
            )
        finally:
            kill_any_daemon(state_dir, proc=daemon)

    # --- Scenario 9: daemon crash mid-reservation, restart, reconcile -
    def scenario_9_daemon_crash_restart_reconciliation(self):
        env, state_dir = self.fresh_env("s9-crash")
        daemon = spawn_daemon(self.binary, env)
        ledger_path = state_dir / LEDGER_FILENAME
        try:
            assert wait_for_socket(state_dir), "daemon never bound its socket"
            sid = f"s9-{uuid.uuid4()}"
            p = preflight(self.binary, env, sid, FIXTURES / "rust-crate", "add pagination support")
            preflight_ok = p.returncode == 0

            # REAL FINDING (see scenario 5 / README "Defects found"): the
            # shipped CLI's hardcoded `balanced` admission policy requires
            # Confidence::Medium, but every cold-start estimate is
            # Confidence::Low, so the very first preflight for a brand
            # new task is always a real Deny with NO reservation written.
            # To get a real *active* reservation to crash mid-flight (the
            # actual point of this scenario), we drive the same real
            # material-event replan trigger as scenario 5 -- which, per
            # that same finding, reserves capacity for this task despite
            # its own admission having been Denied.
            for _ in range(5):
                post_tool_use(self.binary, env, sid, "Bash")

            before_kill = sqlite_query(
                ledger_path,
                "SELECT id, task_id, state, expires_at FROM reservations",
            )

            # Real SIGKILL of the real daemon process, mid-reservation
            # (the reservation row is already durably committed via
            # SQLite WAL — killing the process cannot roll it back).
            daemon.send_signal(signal.SIGKILL)
            daemon.wait(timeout=5)
            daemon_dead = not daemon_socket_alive(state_dir)

            # The 900s reservation TTL is hardcoded (DEFAULT_RESERVATION_TTL_SECS,
            # crates/cli/src/daemon_cmd.rs) with no CLI/env override, so we
            # cannot honestly wait out a real 900s window in this harness.
            # Instead we directly rewrite the ALREADY-REAL, already-committed
            # reservation row's own `expires_at` column to a past timestamp
            # -- simulating elapsed wall-clock time on a real row, not
            # fabricating the reservation itself. This is disclosed here and
            # in the README, not hidden.
            sqlite_exec(
                ledger_path,
                "UPDATE reservations SET expires_at = '2020-01-01T00:00:00Z' WHERE state = 'active'",
            )

            reserve_before_reconcile = sqlite_query(
                ledger_path, "SELECT state FROM reservations WHERE state='active'"
            )

            # A fresh `hook user-prompt-submit` DOES auto-respawn the daemon
            # (client::ensure_daemon_connection) -- unlike `hook stop`/
            # `statusline`, which use connect_only and never respawn.
            # reconcile_stale_reservations runs once at daemon startup AND
            # at the top of every Preflight (crates/daemon/src/server.rs).
            sid2 = f"s9-recover-{uuid.uuid4()}"
            p2 = preflight(self.binary, env, sid2, FIXTURES / "python-pkg", "add pagination support")
            respawn_ok = p2.returncode == 0 and "task " in p2.stdout

            after_reconcile = sqlite_query(
                ledger_path, "SELECT id, state FROM reservations ORDER BY id"
            )
            reclaimed = any(row[1] == "expired" for row in after_reconcile)

            budget_rows = sqlite_query(
                ledger_path,
                "SELECT task_id, completion_reserve, initial_completion_reserve FROM task_budgets",
            )
            reserve_restored = all(r[1] == r[2] for r in budget_rows if r[0] == before_kill[0][1]) if before_kill else False

            self.record(
                "s9_daemon_crash_restart_reconciliation",
                preflight_ok
                and daemon_dead
                and respawn_ok
                and reclaimed
                and reserve_restored,
                f"preflight_ok={preflight_ok}, reservations_before_kill={before_kill}, "
                f"daemon confirmed dead after SIGKILL={daemon_dead}, "
                f"active reservations right before reconcile (expires_at rewritten to the past)="
                f"{reserve_before_reconcile}, next hook auto-respawned the daemon={respawn_ok}, "
                f"reservations after respawn+reconcile={after_reconcile}, "
                f"stale reservation reclaimed to state=expired={reclaimed}, "
                f"completion_reserve restored to initial value for the crashed task={reserve_restored}, "
                f"task_budgets rows={budget_rows}",
            )
        finally:
            kill_any_daemon(state_dir)

    def run_all(self):
        self.scenario_1_balanced_on_budget_completion()
        self.scenario_5_material_event_triggers_a_real_replan()
        self.scenario_9_daemon_crash_restart_reconciliation()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--work-root", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    binary = Path(args.binary).resolve()
    work_root = Path(args.work_root).resolve()
    work_root.mkdir(parents=True, exist_ok=True)

    matrix = Matrix(binary, work_root)
    matrix.run_all()

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(matrix.results, indent=2))

    all_passed = all(r["passed"] for r in matrix.results.values())
    print(f"\n{'ALL PASSED' if all_passed else 'SOME FAILED'}: {len(matrix.results)} scenarios")
    raise SystemExit(0 if all_passed else 1)


if __name__ == "__main__":
    main()
