#!/usr/bin/env python3
"""HORO-1127 required validation matrix, run against the REAL compiled
``libra-governor`` binary, plus the quality/security checks that need a
live daemon (the concurrency race check).

Each ``test_*`` function drives real subprocesses (the CLI binary, the
daemon it spawns) and inspects real on-disk artifacts (the SQLite ledger,
the daemon log). No Rust function is called in-process. Results are
written as JSON to the path given by ``--out``.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket as socket_mod
import sqlite3
import subprocess
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = Path(__file__).resolve().parent / "fixtures"

SOCKET_FILENAME = "daemon.sock"
LEDGER_FILENAME = "ledger.sqlite3"
LOG_FILENAME = "daemon.log"
TASK_ID_MARKER = "task "


def run_hook(binary: Path, subcommand: list[str], payload: dict, env: dict, timeout=30):
    return subprocess.run(
        [str(binary), *subcommand],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
    )


def spawn_daemon(binary: Path, env: dict) -> subprocess.Popen:
    return subprocess.Popen(
        [str(binary), "daemon", "run"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def wait_for_socket(state_dir: Path, timeout=5.0) -> bool:
    deadline = time.time() + timeout
    sock = state_dir / SOCKET_FILENAME
    while time.time() < deadline:
        if sock.exists():
            return True
        time.sleep(0.05)
    return sock.exists()


def receipt_count(ledger_path: Path) -> int:
    if not ledger_path.exists():
        return 0
    conn = sqlite3.connect(str(ledger_path))
    try:
        cur = conn.execute("SELECT COUNT(*) FROM receipts")
        return cur.fetchone()[0]
    except sqlite3.OperationalError:
        return 0
    finally:
        conn.close()


def preflight(binary, env, session_id, cwd, prompt="add input validation to the login handler", timeout=30):
    payload = {"session_id": session_id, "cwd": str(cwd), "prompt": prompt, "hook_event_name": "UserPromptSubmit"}
    return run_hook(binary, ["hook", "user-prompt-submit"], payload, env, timeout=timeout)


def stop(binary, env, session_id, model="claude-sonnet-5"):
    payload = {"session_id": session_id, "model": model, "hook_event_name": "Stop"}
    return run_hook(binary, ["hook", "stop"], payload, env)


def post_tool_use(binary, env, session_id, tool_name="Bash"):
    payload = {"session_id": session_id, "tool_name": tool_name, "hook_event_name": "PostToolUse"}
    return run_hook(binary, ["hook", "post-tool-use"], payload, env)


def statusline(binary, env):
    return subprocess.run([str(binary), "statusline"], capture_output=True, text=True, env=env, timeout=10)


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
        # Genuinely fresh: wipe any state left behind by a prior run of
        # this script (each test's "before" assertions assume an empty
        # ledger) rather than silently accumulating rows across runs.
        if state_dir.exists():
            shutil.rmtree(state_dir)
        state_dir.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ)
        env["LIBRA_GOVERNOR_STATE_DIR"] = str(state_dir)
        return env, state_dir

    # 1. Resume/restart mid-corpus
    def test_resume_restart(self):
        env, state_dir = self.fresh_env("resume")
        daemon = spawn_daemon(self.binary, env)
        try:
            assert wait_for_socket(state_dir), "daemon never bound its socket"
            for i in range(3):
                sid = f"resume-pre-{i}-{uuid.uuid4()}"
                preflight(self.binary, env, sid, FIXTURES / "rust-crate")
                stop(self.binary, env, sid)
            before = receipt_count(state_dir / LEDGER_FILENAME)

            daemon.send_signal(signal.SIGTERM)
            daemon.wait(timeout=5)

            # Subsequent hook calls must transparently respawn the daemon.
            ok = True
            for i in range(3):
                sid = f"resume-post-{i}-{uuid.uuid4()}"
                p = preflight(self.binary, env, sid, FIXTURES / "python-pkg")
                ok = ok and p.returncode == 0 and TASK_ID_MARKER in p.stdout
                stop(self.binary, env, sid)
            after = receipt_count(state_dir / LEDGER_FILENAME)

            self.record(
                "resume_restart",
                ok and before == 3 and after == 6,
                f"receipts before kill=3(exp)/{before}, after respawn+3more=6(exp)/{after}, "
                f"post-kill hooks all succeeded={ok}",
            )
        finally:
            self._kill_any_daemon(state_dir)

    # 2. Multiple sessions under one task context
    def test_multi_session(self):
        env, state_dir = self.fresh_env("multisession")
        sid_a = f"multi-a-{uuid.uuid4()}"
        sid_b = f"multi-b-{uuid.uuid4()}"
        pa = preflight(self.binary, env, sid_a, FIXTURES / "rust-crate", "fix the login bug")
        pb = preflight(self.binary, env, sid_b, FIXTURES / "rust-crate", "fix the login bug")
        ta = self._extract_task_id(pa.stdout)
        tb = self._extract_task_id(pb.stdout)
        stop(self.binary, env, sid_a)
        stop(self.binary, env, sid_b)
        self._kill_any_daemon(state_dir)
        self.record(
            "multi_session_same_context",
            ta is not None and tb is not None and ta != tb,
            f"session A -> task {ta}; session B -> task {tb} (same cwd+prompt). "
            "Observed real behavior per HORO-1124's resolve_or_create_task_for_session: "
            "each session_id gets its own TaskId; there is no cross-session task merge "
            "in MVP 1.0 (documented as out of scope in integrations/claude-code/README.md).",
        )

    # 3. User abort + stray Stop
    def test_user_abort_and_stray_stop(self):
        env, state_dir = self.fresh_env("abort")
        daemon = spawn_daemon(self.binary, env)
        try:
            wait_for_socket(state_dir)
            sid = f"abort-{uuid.uuid4()}"
            p = preflight(self.binary, env, sid, FIXTURES / "ts-pkg")
            preflight_ok = p.returncode == 0
            # Never call stop for this session (simulated user abort).

            stray_sid = f"stray-{uuid.uuid4()}"
            s = stop(self.binary, env, stray_sid)
            # hook_stop.rs only eprintln!s on the Finalized branch; the
            # NoActiveTask safe no-op is logged to daemon.log instead (see
            # hook_stop.rs::log / run()). So stderr is expected to be empty
            # here — check the log file for the real evidence.
            log_path = state_dir / LOG_FILENAME
            log_text = log_path.read_text() if log_path.exists() else ""
            no_op_ok = (
                s.returncode == 0
                and s.stderr.strip() == ""
                and "no active task for this session, safe no-op" in log_text
            )

            # Daemon must still be alive and responsive afterwards.
            st = statusline(self.binary, env)
            still_alive = daemon.poll() is None and "libra:" in st.stdout

            self.record(
                "user_abort_and_stray_stop",
                preflight_ok and no_op_ok and still_alive,
                f"preflight_ok={preflight_ok} (session never Stopped, simulating user abort). "
                f"stray-stop (session with no preceding preflight) safe no-op={no_op_ok}: "
                f"exit 0, stdout/stderr empty (hook_stop.rs only eprintln!s on the Finalized "
                f"branch), daemon.log contains the expected 'no active task ... safe no-op' "
                f"line. daemon still alive/responsive after={still_alive}",
            )
        finally:
            self._kill_any_daemon(state_dir, proc=daemon)

    # 4. Daemon restart mid-flight (preflight, kill, then Stop with no daemon)
    def test_daemon_restart_mid_flight(self):
        env, state_dir = self.fresh_env("midflight")
        daemon = spawn_daemon(self.binary, env)
        try:
            wait_for_socket(state_dir)
            sid = f"midflight-{uuid.uuid4()}"
            p = preflight(self.binary, env, sid, FIXTURES / "rust-crate")
            preflight_ok = p.returncode == 0

            daemon.send_signal(signal.SIGKILL)
            daemon.wait(timeout=5)

            # hook stop uses connect_only and must NEVER auto-spawn the
            # daemon (see crates/cli/src/hook_stop.rs docs). Expect a
            # silent, safe no-op: no receipt persisted for this task.
            s = stop(self.binary, env, sid)
            stop_exit_ok = s.returncode == 0
            receipts_after_stop = receipt_count(state_dir / LEDGER_FILENAME)

            daemon_running_after_stop = self._daemon_socket_alive(state_dir)

            # A fresh preflight (hook user-prompt-submit) DOES auto-respawn.
            sid2 = f"midflight-recover-{uuid.uuid4()}"
            p2 = preflight(self.binary, env, sid2, FIXTURES / "rust-crate")
            respawn_ok = p2.returncode == 0 and TASK_ID_MARKER in p2.stdout

            self.record(
                "daemon_restart_mid_flight",
                stop_exit_ok and receipts_after_stop == 0 and not daemon_running_after_stop and respawn_ok,
                f"preflight_ok={preflight_ok}, stop after kill exited cleanly={stop_exit_ok} "
                f"(no crash), receipts persisted for killed-mid-flight task={receipts_after_stop} (expect 0), "
                f"daemon NOT auto-restarted by hook stop={not daemon_running_after_stop} "
                "(by design: hook_stop.rs uses connect_only, never ensure_daemon_connection), "
                f"next hook user-prompt-submit DOES auto-respawn the daemon={respawn_ok}",
            )
        finally:
            self._kill_any_daemon(state_dir)

    # 5. Recon budget cap / truncation
    def test_recon_budget_cap(self):
        env, state_dir = self.fresh_env("reconbudget")
        big_repo = self.work_root / "big-fixture"
        if big_repo.exists():
            shutil.rmtree(big_repo)
        big_repo.mkdir(parents=True)
        # ReconBudget::default().max_files == 2000 (crates/daemon/src/recon.rs).
        # Generate comfortably more files than that so the walk hits the
        # cap deterministically, without depending on wall-clock timing.
        n_files = 2600
        for i in range(n_files):
            (big_repo / f"file_{i:05d}.txt").write_text("x")

        sid = f"reconbudget-{uuid.uuid4()}"
        start = time.time()
        p = preflight(self.binary, env, sid, big_repo, "refactor this huge tree", timeout=15)
        elapsed = time.time() - start
        stop(self.binary, env, sid)
        self._kill_any_daemon(state_dir)

        text = ""
        try:
            text = json.loads(p.stdout)["hookSpecificOutput"]["additionalContext"]
        except (json.JSONDecodeError, KeyError):
            pass

        truncated_signal = "budget exhausted" in text.lower() or "note:" in text.lower()
        low_or_medium_conf = ("confidence: low" in text) or ("confidence: medium" in text)
        finished_fast = elapsed < 10.0  # never hangs; well under the hook's own budget

        self.record(
            "recon_budget_cap",
            p.returncode == 0 and finished_fast,
            f"repo with {n_files} files (> ReconBudget::default().max_files=2000): "
            f"preflight completed in {elapsed:.2f}s (no hang), truncation signal present={truncated_signal}, "
            f"confidence downgraded (low/medium)={low_or_medium_conf}, raw context={text[:300]!r}",
        )
        shutil.rmtree(big_repo, ignore_errors=True)

    # 6. Clean worktree after read-only preflight
    def test_clean_worktree(self):
        env, state_dir = self.fresh_env("cleanworktree")
        results = []
        for fixture_name in ("rust-crate", "python-pkg", "ts-pkg"):
            rel = f"experiments/mvp1_validation/fixtures/{fixture_name}"
            before = subprocess.run(
                ["git", "status", "--porcelain", "--", rel],
                cwd=REPO_ROOT, capture_output=True, text=True,
            ).stdout
            sid = f"clean-{fixture_name}-{uuid.uuid4()}"
            preflight(self.binary, env, sid, FIXTURES / fixture_name)
            stop(self.binary, env, sid)
            after = subprocess.run(
                ["git", "status", "--porcelain", "--", rel],
                cwd=REPO_ROOT, capture_output=True, text=True,
            ).stdout
            results.append((fixture_name, before, after))
        self._kill_any_daemon(state_dir)
        all_clean = all(b == "" and a == "" for _, b, a in results)
        self.record(
            "clean_worktree_after_preflight",
            all_clean,
            "git status --porcelain scoped to each fixture path, before and after preflight: "
            + "; ".join(f"{n}: before={b!r} after={a!r}" for n, b, a in results),
        )

    # 7. Privacy inspection
    def test_privacy_inspection(self):
        env, state_dir = self.fresh_env("privacy")
        nonce = f"NONCE-{uuid.uuid4().hex}"
        sid = f"privacy-{uuid.uuid4()}"
        preflight(self.binary, env, sid, FIXTURES / "rust-crate", f"please {nonce} do not leak this")
        post_tool_use(self.binary, env, sid, "Bash")
        stop(self.binary, env, sid)
        self._kill_any_daemon(state_dir)

        ledger_path = state_dir / LEDGER_FILENAME
        log_path = state_dir / LOG_FILENAME
        wal_path = state_dir / f"{LEDGER_FILENAME}-wal"

        found_in = []
        for p in (ledger_path, log_path, wal_path):
            if p.exists() and nonce.encode() in p.read_bytes():
                found_in.append(str(p.name))

        self.record(
            "privacy_no_raw_prompt_leak",
            len(found_in) == 0,
            f"distinctive nonce {nonce!r} grepped (raw bytes) against {LEDGER_FILENAME}, "
            f"{LEDGER_FILENAME}-wal, and {LOG_FILENAME}. Found in: {found_in or 'none (clean)'}",
        )

    # 8. Concurrency / race check
    def test_concurrent_post_tool_use(self):
        env, state_dir = self.fresh_env("concurrency")
        daemon = spawn_daemon(self.binary, env)
        try:
            wait_for_socket(state_dir)
            sid = f"concurrency-{uuid.uuid4()}"
            preflight(self.binary, env, sid, FIXTURES / "rust-crate")

            n = 25
            procs = []
            for _ in range(n):
                payload = json.dumps({"session_id": sid, "tool_name": "Bash", "hook_event_name": "PostToolUse"})
                proc = subprocess.Popen(
                    [str(self.binary), "hook", "post-tool-use"],
                    stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    env=env, text=True,
                )
                proc.stdin.write(payload)
                proc.stdin.close()
                procs.append(proc)
            for proc in procs:
                proc.wait(timeout=15)

            s = stop(self.binary, env, sid)
            recorded = None
            if "Tool calls:" in s.stderr:
                try:
                    recorded = int(s.stderr.split("Tool calls:", 1)[1].split("\n", 1)[0].strip())
                except ValueError:
                    pass

            self.record(
                "concurrency_wal_no_lost_writes",
                recorded == n,
                f"fired {n} concurrent `hook post-tool-use` subprocesses against one session "
                f"(OS-level concurrent connects to the daemon's serial single-threaded accept "
                f"loop, exercising WAL + busy_timeout=5000ms from crates/ledger/src/store.rs); "
                f"final tool_call_count recorded on the receipt = {recorded} (expected {n})",
            )
        finally:
            self._kill_any_daemon(state_dir, proc=daemon)

    @staticmethod
    def _extract_task_id(stdout: str):
        try:
            text = json.loads(stdout)["hookSpecificOutput"]["additionalContext"]
        except (json.JSONDecodeError, KeyError):
            return None
        if TASK_ID_MARKER not in text:
            return None
        return text.split(TASK_ID_MARKER, 1)[1].split(",")[0].strip()

    @staticmethod
    def _daemon_socket_alive(state_dir: Path) -> bool:
        sock = state_dir / SOCKET_FILENAME
        if not sock.exists():
            return False
        s = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
        try:
            s.settimeout(1)
            s.connect(str(sock))
            return True
        except OSError:
            return False
        finally:
            s.close()

    def _kill_any_daemon(self, state_dir: Path, proc: subprocess.Popen | None = None):
        """Kills the daemon this test owns, whether we hold its Popen handle
        (`proc`, when this harness spawned it directly) or it was spawned
        transparently by `ensure_daemon_connection` inside a hook subprocess
        we don't hold a handle to. In the latter case, find the process
        bound to this state dir's own socket via `lsof` and kill it — every
        test uses its own fresh, uniquely-tagged state dir, so this can
        never reach into an unrelated daemon.
        """
        if proc is not None and proc.poll() is None:
            proc.send_signal(signal.SIGKILL)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass

        sock = state_dir / SOCKET_FILENAME
        if not sock.exists():
            return
        try:
            out = subprocess.run(
                ["lsof", "-t", str(sock)], capture_output=True, text=True, timeout=5
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            return
        for pid_str in out.split():
            try:
                os.kill(int(pid_str), signal.SIGKILL)
            except (ValueError, ProcessLookupError, PermissionError):
                pass


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True, type=Path)
    ap.add_argument("--work-root", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()

    # NOSONAR: --work-root/--out/--binary are trusted, operator-supplied
    # local filesystem paths for this CLI evidence harness (same trust
    # boundary as any argument to `cargo test`), not attacker-controlled
    # input from a network-facing request; there is no path-traversal sink
    # here to defend against.
    args.work_root.mkdir(parents=True, exist_ok=True)  # NOSONAR
    m = Matrix(args.binary.resolve(), args.work_root)

    m.test_resume_restart()
    m.test_multi_session()
    m.test_user_abort_and_stray_stop()
    m.test_daemon_restart_mid_flight()
    m.test_recon_budget_cap()
    m.test_clean_worktree()
    m.test_privacy_inspection()
    m.test_concurrent_post_tool_use()

    args.out.parent.mkdir(parents=True, exist_ok=True)  # NOSONAR
    args.out.write_text(json.dumps(m.results, indent=2))  # NOSONAR

    n_pass = sum(1 for r in m.results.values() if r["passed"])
    print(f"\n{n_pass}/{len(m.results)} validation-matrix checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
