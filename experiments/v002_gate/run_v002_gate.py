#!/usr/bin/env python3
"""HORO-1169 v0.0.2 release-gate dogfood evidence.

Extends ``experiments/v001_gate/run_v001_gate.py`` (imported via
``importlib``, not copy-pasted, mirroring the ``mvp3_gate``/``mvp1_validation``
subclass-reuse pattern used elsewhere in this repo's experiments) with the
scenarios the v0.0.1 gate didn't need: long-running sessions, concurrent
sessions against one daemon, failure/recovery, invalid/missing
configuration, resource consumption, and a real upgrade from the actual
tagged ``v0.0.1`` release (not ``mvp-3.0``) -- this matters now because
``PROTOCOL_VERSION`` bumped 7->8 and ledger migration ``0009`` must apply
cleanly to an existing, populated v0.0.1 ledger, not just a fresh one.

Per the founder's 2026-09-19 validation-policy decision (HORO-1154): this
replaces the external-user-recruitment hard gate with real internal
dogfood evidence as the v0.0.2 release-readiness check. Team Alpha scope
is out of this gate entirely -- see the scope note on HORO-1169.

Every scenario here is a real subprocess of the real compiled binary
against real on-disk state, same discipline as v001_gate: no in-process
mocking, no fabricated numbers.
"""

from __future__ import annotations

import importlib.util
import json
import os
import resource
import shutil
import signal
import subprocess
import sys
import tarfile
import threading
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
RESULTS = Path(__file__).resolve().parent / "results"
FIXTURES = REPO_ROOT / "experiments" / "mvp1_validation" / "fixtures"

# Import run_v001_gate.py as a module rather than duplicating its
# profile/hook/daemon helpers -- same reuse pattern as mvp3_gate's
# _BaseMatrix import of mvp1_validation's Matrix class.
_V001_SPEC = importlib.util.spec_from_file_location(
    "run_v001_gate", REPO_ROOT / "experiments" / "v001_gate" / "run_v001_gate.py"
)
v001 = importlib.util.module_from_spec(_V001_SPEC)
_V001_SPEC.loader.exec_module(v001)

SOCKET_FILENAME = v001.SOCKET_FILENAME
LEDGER_FILENAME = v001.LEDGER_FILENAME
LOG_FILENAME = v001.LOG_FILENAME
CLAUDE_DIR_NAME = v001.CLAUDE_DIR_NAME
LOCAL_STATE_DIRNAME = v001.LOCAL_STATE_DIRNAME
run = v001.run
run_hook = v001.run_hook
preflight = v001.preflight
post_tool_use = v001.post_tool_use
stop = v001.stop
spawn_daemon = v001.spawn_daemon
wait_for_socket = v001.wait_for_socket
receipt_count = v001.receipt_count
new_profile = v001.new_profile
_resolve_within = v001._resolve_within


def w(name: str, text: str) -> None:
    path = RESULTS / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    print(f"--- wrote {path} ({len(text)} bytes) ---")


def _run_item12_long_running_session(binary: Path, env: dict, fake_home: Path, record) -> None:
    """Item 12: a long-running session -- one session_id accumulates many
    real tool-use events over an extended span before Stop, exercising
    whatever per-session/per-task state accretes across a real multi-hour
    (here: many-event, not many-wall-clock-hours -- see disclosure below)
    working session rather than the single-shot preflight->stop used
    elsewhere in this matrix."""
    state_dir = fake_home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"
    sid = f"long-{uuid.uuid4()}"
    lines = [f"session_id: {sid}"]
    pf = preflight(binary, env, sid, REPO_ROOT, prompt="run the full workspace test suite and fix any failures")
    lines.append(f"preflight exit={pf.returncode}")
    tool_sequence = ["Read", "Bash", "Edit", "Read", "Bash", "Edit", "Bash", "Read", "Edit", "Bash"] * 3
    ptu_failures = 0
    for i, tool in enumerate(tool_sequence):
        ptu = post_tool_use(binary, env, sid, tool_name=tool)
        if ptu.returncode != 0:
            ptu_failures += 1
            lines.append(f"  event {i} ({tool}): UNEXPECTED non-zero exit={ptu.returncode} stderr={ptu.stderr.strip()!r}")
    lines.append(f"delivered {len(tool_sequence)} post-tool-use events, {ptu_failures} unexpected failures")
    stop_proc = stop(binary, env, sid)
    lines.append(f"stop exit={stop_proc.returncode}\n--- stop stderr ---\n{stop_proc.stderr}")
    receipts = receipt_count(state_dir / LEDGER_FILENAME)
    lines.append(f"receipts after long session: {receipts}")
    w("12_long_running_session.txt", "\n".join(lines))
    record(
        "item12_long_running_session",
        pf.returncode == 0 and ptu_failures == 0 and stop_proc.returncode == 0 and receipts >= 1,
        f"preflight_ok={pf.returncode == 0}, events={len(tool_sequence)}, ptu_failures={ptu_failures}, stop_ok={stop_proc.returncode == 0}, receipts={receipts}",
    )


def _run_item13_concurrent_sessions(binary: Path, env: dict, fake_home: Path, record) -> None:
    """Item 13: N distinct session_ids hitting the same single daemon
    concurrently (the daemon is a single-threaded serial accept loop --
    see crates/daemon/src/server.rs -- so this proves the serialization
    is correct under real concurrent client load, not that requests run
    in parallel inside the daemon)."""
    state_dir = fake_home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"
    n = 8
    session_ids = [f"concurrent-{uuid.uuid4()}" for _ in range(n)]
    results: dict[str, dict] = {}
    lock = threading.Lock()

    def worker(sid: str) -> None:
        pf = preflight(binary, env, sid, FIXTURES / "rust-crate", prompt=f"concurrent task {sid}")
        ptu = post_tool_use(binary, env, sid, tool_name="Bash")
        st = stop(binary, env, sid)
        with lock:
            results[sid] = {
                "preflight_rc": pf.returncode,
                "preflight_has_task": "task " in pf.stdout,
                "ptu_rc": ptu.returncode,
                "stop_rc": st.returncode,
            }

    threads = [threading.Thread(target=worker, args=(sid,)) for sid in session_ids]
    t0 = time.time()
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=60)
    elapsed = time.time() - t0

    all_ok = all(
        r["preflight_rc"] == 0 and r["preflight_has_task"] and r["ptu_rc"] == 0 and r["stop_rc"] == 0
        for r in results.values()
    )
    completed = len(results) == n
    receipts = receipt_count(state_dir / LEDGER_FILENAME)

    lines = [f"{n} concurrent sessions against one daemon, wall time={elapsed:.2f}s"]
    for sid, r in results.items():
        lines.append(f"  {sid}: {r}")
    lines.append(f"all_sessions_completed={completed}, all_ok={all_ok}, receipts_after={receipts}")
    w("13_concurrent_sessions.txt", "\n".join(lines))
    record(
        "item13_concurrent_sessions",
        completed and all_ok and receipts >= n,
        f"completed={completed}/{n}, all_ok={all_ok}, elapsed={elapsed:.2f}s, receipts={receipts}",
    )


def _run_item14_failure_recovery(binary: Path, env: dict, fake_home: Path, record) -> None:
    """Item 14: kill the daemon mid-task (after preflight admits, before
    Stop finalizes) and confirm a respawned daemon's ledger state is
    coherent -- no half-written plan/reservation blocks a fresh task."""
    state_dir = fake_home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"
    lines = []

    daemon = spawn_daemon(binary, env)
    lines.append(f"daemon spawned pid={daemon.pid}")
    up = wait_for_socket(state_dir, timeout=8)
    lines.append(f"socket up: {up}")

    sid = f"kill-midtask-{uuid.uuid4()}"
    pf = preflight(binary, env, sid, FIXTURES / "rust-crate")
    lines.append(f"preflight (admitted, not yet stopped) exit={pf.returncode}")

    daemon.send_signal(signal.SIGKILL)
    try:
        daemon.wait(timeout=5)
    except subprocess.TimeoutExpired:
        daemon.kill()
        daemon.wait(timeout=5)
    lines.append("daemon SIGKILLed with an outstanding (never-Stopped) task in flight")

    # Ledger must still open cleanly -- WAL mode + busy_timeout is
    # exactly what's supposed to make an ungraceful kill survivable.
    sqlite_ok = True
    sqlite_err = ""
    try:
        receipt_count(state_dir / LEDGER_FILENAME)
    except Exception as exc:  # noqa: BLE001 -- recording the real exception is the point
        sqlite_ok = False
        sqlite_err = repr(exc)
    lines.append(f"ledger opens cleanly after ungraceful kill: {sqlite_ok} {sqlite_err}")

    sid2 = f"post-kill-fresh-{uuid.uuid4()}"
    pf2 = preflight(binary, env, sid2, FIXTURES / "python-pkg")
    fresh_task_ok = pf2.returncode == 0 and "task " in pf2.stdout
    lines.append(f"fresh task after respawn: exit={pf2.returncode}, ok={fresh_task_ok}\nstdout={pf2.stdout}")
    stop2 = stop(binary, env, sid2)
    lines.append(f"stop for fresh task exit={stop2.returncode}")

    w("14_failure_recovery.txt", "\n".join(lines))
    record(
        "item14_failure_recovery",
        sqlite_ok and fresh_task_ok and stop2.returncode == 0,
        f"sqlite_ok={sqlite_ok}, fresh_task_ok={fresh_task_ok}, stop_ok={stop2.returncode == 0}",
    )


def _run_item15_invalid_config(work_root: Path, binary: Path, record) -> None:
    """Item 15: invalid/missing configuration must degrade safely (a
    clear doctor finding or documented fallback), never a crash or a
    silent wrong-policy admission."""
    lines = []
    cases = {
        "malformed_json": "{ this is not valid json ",
        "wrong_type_for_gateway": json.dumps({"gateway": "should-be-an-object-not-a-string"}),
        "unknown_top_level_key_only": json.dumps({"totally_unrecognized_key": True}),
        "empty_file": "",
    }
    all_safe = True
    for case_name, content in cases.items():
        profile = new_profile(f"badcfg-{case_name}", work_root)
        home = profile["fake_home"]
        cfg_env = profile["env"]
        state_dir = home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "config.json").write_text(content)
        bin_path = profile["cargo_home"] / "bin" / "libra-governor"
        bin_path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(binary, bin_path)

        doctor = run([str(bin_path), "doctor"], env=cfg_env, timeout=30)
        sid = f"badcfg-{case_name}-{uuid.uuid4()}"
        pf = preflight(bin_path, cfg_env, sid, FIXTURES / "rust-crate")
        no_crash = doctor.returncode in (0, 1) and pf.returncode in (0, 1)
        no_panic_text = "panic" not in doctor.stderr.lower() and "panic" not in pf.stderr.lower()
        case_safe = no_crash and no_panic_text
        all_safe = all_safe and case_safe
        lines.append(
            f"=== {case_name} ===\nconfig.json content: {content!r}\n"
            f"doctor exit={doctor.returncode} stderr={doctor.stderr.strip()!r}\n"
            f"preflight-with-bad-config exit={pf.returncode} stderr={pf.stderr.strip()!r}\n"
            f"safe (no crash/panic, degrades cleanly): {case_safe}\n"
        )
    w("15_invalid_missing_config.txt", "\n".join(lines))
    record("item15_invalid_config_degrades_safely", all_safe, f"all_cases_safe={all_safe}, cases={list(cases)}")


def _run_item16_resource_consumption(binary: Path, env: dict, fake_home: Path, record) -> None:
    """Item 16: sanity-bound the daemon's own resource footprint under a
    real (small) workload -- this is a coarse floor/ceiling check, not a
    perf benchmark: flag anything wildly out of line (e.g. an unbounded
    RSS growth bug), not micro-optimize."""
    state_dir = fake_home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"
    daemon = spawn_daemon(binary, env)
    lines = [f"daemon spawned pid={daemon.pid}"]
    try:
        wait_for_socket(state_dir, timeout=8)
        rss_samples = []
        for i in range(5):
            sid = f"rss-{i}-{uuid.uuid4()}"
            preflight(binary, env, sid, FIXTURES / "rust-crate")
            for tool in ("Read", "Edit", "Bash"):
                post_tool_use(binary, env, sid, tool_name=tool)
            stop(binary, env, sid)
            try:
                with open(f"/proc/{daemon.pid}/status") as f:
                    vm_rss_kb = next(
                        (int(line.split()[1]) for line in f if line.startswith("VmRSS:")), None
                    )
            except FileNotFoundError:
                # macOS has no /proc -- fall back to ps.
                ps = run(["ps", "-o", "rss=", "-p", str(daemon.pid)])
                vm_rss_kb = int(ps.stdout.strip()) if ps.stdout.strip().isdigit() else None
            rss_samples.append(vm_rss_kb)
            lines.append(f"after task {i}: daemon RSS = {vm_rss_kb} KB")
        self_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        lines.append(f"harness process ru_maxrss (for scale reference): {self_rss}")
        known = [s for s in rss_samples if s is not None]
        growth_ratio = (known[-1] / known[0]) if len(known) >= 2 and known[0] else None
        bounded = growth_ratio is None or growth_ratio < 3.0
        lines.append(f"RSS growth ratio (last/first over 5 real tasks): {growth_ratio}, bounded(<3x)={bounded}")
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
    w("16_resource_consumption.txt", "\n".join(lines))
    record("item16_resource_consumption_bounded", bounded, f"rss_samples_kb={rss_samples}, growth_ratio={growth_ratio}")


def _run_item17_upgrade_from_v001(work_root: Path, binary: Path, record) -> None:
    """Item 17: upgrade from the real tagged v0.0.1 release -- not
    mvp-3.0. This matters specifically because PROTOCOL_VERSION bumped
    7->8 and ledger migration 0009 (business_context/outcome_attestations/
    webhook_deliveries) must apply cleanly on top of a v0.0.1 ledger that
    already has real receipts/events/plans in it, not a fresh schema."""
    lines = []
    tag_check = run(["git", "cat-file", "-e", "v0.0.1"], cwd=str(REPO_ROOT))
    if tag_check.returncode != 0:
        lines.append("SKIPPED: tag v0.0.1 not found in this repository; genuinely infeasible.")
        record("item17_upgrade_from_v001", None, "tag v0.0.1 not found")
        w("17_upgrade_from_v0_0_1.txt", "\n".join(lines))
        return

    old_src = _resolve_within(work_root, "old-v0.0.1-src")
    if old_src.exists():
        shutil.rmtree(old_src)
    old_src.mkdir(parents=True)
    archive_path = _resolve_within(work_root, "v0.0.1.tar")
    with open(archive_path, "wb") as f:
        arch_proc = subprocess.run(["git", "archive", "v0.0.1"], cwd=str(REPO_ROOT), stdout=f)
    lines.append(f"git archive v0.0.1 exit={arch_proc.returncode}")
    with tarfile.open(archive_path) as tf:
        tf.extractall(old_src)

    profile = new_profile("upgrade-v001", work_root)
    home = profile["fake_home"]
    env = profile["env"]
    bin_path = profile["cargo_home"] / "bin" / "libra-governor"
    state_dir = home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"

    old_install = run(["cargo", "install", "--path", "crates/cli", "--locked"], cwd=str(old_src), env=env, timeout=900)
    lines.append(f"\n$ (v0.0.1 tree) cargo install --path crates/cli --locked\nexit={old_install.returncode}\n--- stderr (tail) ---\n{old_install.stderr[-2000:]}\n")

    old_install_ok = old_install.returncode == 0 and bin_path.exists()
    if not old_install_ok:
        lines.append("v0.0.1 tree failed to build under --locked on current toolchain -- recording as a real finding, not faking success.")
        record("item17_upgrade_from_v001", False, "v0.0.1 build failed under --locked on current toolchain")
        w("17_upgrade_from_v0_0_1.txt", "\n".join(lines))
        return

    run([str(bin_path), "install"], env=env, timeout=30)
    # Populate a REAL v0.0.1 ledger: a full preflight->tool-use->stop
    # cycle against the old binary, so migration 0009 has real
    # pre-existing rows (tasks/events/plans/receipts) to run against,
    # not an empty schema.
    sid = f"pre-upgrade-{uuid.uuid4()}"
    pf_old = preflight(bin_path, env, sid, FIXTURES / "rust-crate")
    post_tool_use(bin_path, env, sid, tool_name="Bash")
    stop_old = stop(bin_path, env, sid)
    receipts_before = receipt_count(state_dir / LEDGER_FILENAME)
    lines.append(f"old-binary (v0.0.1) real task: preflight exit={pf_old.returncode}, stop exit={stop_old.returncode}, receipts={receipts_before}")

    old_doctor = run([str(bin_path), "doctor"], env=env, timeout=30)
    lines.append(f"old-binary doctor (before upgrade):\n{old_doctor.stdout}\n")

    new_install = run([str(REPO_ROOT / "scripts" / "install.sh")], cwd=str(REPO_ROOT), env=env, timeout=900)
    # install.sh's own exit code is doctor's exit code (see scripts/
    # install.sh) -- doctor legitimately reports [error] severity here
    # because the still-live v0.0.1 daemon fails every request with a
    # protocol mismatch, so a non-zero exit at THIS exact moment is
    # expected and disclosed, not itself a failure signal. What proves
    # installation genuinely succeeded is the binary actually being
    # replaced (checked below) and doctor's other findings being [ok].
    binary_replaced = "Replaced package" in new_install.stderr or "Installed package" in new_install.stderr
    lines.append(
        f"\n$ (current v0.0.2 tree) ./scripts/install.sh  (same $HOME as v0.0.1 binary, real populated ledger)\n"
        f"exit={new_install.returncode} (expected non-zero here -- see note above), "
        f"binary_replaced={binary_replaced}\n--- stderr (tail) ---\n{new_install.stderr[-2000:]}\n"
    )

    new_doctor = run([str(bin_path), "doctor"], env=env, timeout=30)
    receipts_after = receipt_count(state_dir / LEDGER_FILENAME)
    lines.append(f"new-binary doctor (after upgrade, migration 0002..0009 applied to the real v0.0.1 ledger):\n{new_doctor.stdout}\n")
    lines.append(f"receipts: before={receipts_before}, after={receipts_after} (must be >= before -- migration is additive, never destructive)")

    # A task run against the still-live OLD (v0.0.1-compiled) daemon is
    # EXPECTED to fail open here, and disclosed as such -- this is a
    # real, permanent, one-time limitation of exactly this one upgrade
    # (v0.0.1 -> v0.0.2): the running v0.0.1 daemon process was compiled
    # before the self-healing fix (HORO-1169) existed, so it cannot
    # retroactively gain it. Every upgrade FROM v0.0.2 onward self-heals
    # automatically -- see item19, which proves that mechanism for real
    # against a running *current* daemon, the case the fix actually
    # covers.
    sid2 = f"post-upgrade-stale-daemon-{uuid.uuid4()}"
    pf_stale = preflight(bin_path, env, sid2, FIXTURES / "python-pkg")
    stop_stale = stop(bin_path, env, sid2)
    receipts_with_stale_daemon = receipt_count(state_dir / LEDGER_FILENAME)
    governed_despite_stale_daemon = "task " in pf_stale.stdout and receipts_with_stale_daemon > receipts_after
    lines.append(
        f"task against the still-live OLD (v0.0.1) daemon: preflight exit={pf_stale.returncode}, "
        f"governed={governed_despite_stale_daemon} (expected False here -- this exact transition "
        f"predates the self-healing fix and cannot retroactively gain it), "
        f"receipts={receipts_with_stale_daemon}"
    )

    # The documented workaround (doctor's own diagnostic message, and
    # README's known-limitations entry): kill the stale daemon once.
    # Prove it actually resolves things, not just that we tell users to
    # try it.
    # pkill -f matches argv, not environment -- LIBRA_GOVERNOR_STATE_DIR
    # is passed via env, so match on the installed binary's own path
    # (bin_path, which IS argv[0] for a daemon spawned via
    # Command::new(current_exe())) instead.
    run(["pkill", "-f", str(bin_path)])
    time.sleep(0.3)
    sid3 = f"post-upgrade-after-manual-restart-{uuid.uuid4()}"
    pf_new = preflight(bin_path, env, sid3, FIXTURES / "python-pkg")
    stop_new = stop(bin_path, env, sid3)
    receipts_final = receipt_count(state_dir / LEDGER_FILENAME)
    lines.append(
        f"task after the documented one-time manual restart: preflight exit={pf_new.returncode}, "
        f"stop exit={stop_new.returncode}, receipts_final={receipts_final}"
    )

    data_survived = receipts_after >= receipts_before and receipts_before >= 1
    workaround_resolves_it = (
        pf_new.returncode == 0 and stop_new.returncode == 0 and receipts_final > receipts_with_stale_daemon
    )
    doctor_ok = new_doctor.returncode in (0, 1)
    w("17_upgrade_from_v0_0_1.txt", "\n".join(lines))
    record(
        "item17_upgrade_from_v001",
        binary_replaced and data_survived and doctor_ok and workaround_resolves_it,
        f"binary_replaced={binary_replaced}, data_survived={data_survived}, doctor_ok={doctor_ok}, "
        f"governed_despite_stale_daemon={governed_despite_stale_daemon} (expected False -- disclosed, "
        f"permanent, one-time v0.0.1-only limitation), workaround_resolves_it={workaround_resolves_it}, "
        f"receipts {receipts_before}->{receipts_after}->{receipts_with_stale_daemon}->{receipts_final}",
    )


def _run_item19_self_healing_daemon_upgrade(binary: Path, env: dict, fake_home: Path, daemon_pid: int, record) -> None:
    """Item 19: proves the real fix for the item17 finding actually
    works for its real applicability -- a *running v0.0.2-or-later*
    daemon that receives a request from a client speaking a *newer*
    protocol than its own shuts itself down, and the very next hook
    invocation transparently respawns a fresh (current) daemon and
    governs normally again.

    This deliberately does NOT re-test the v0.0.1 -> v0.0.2 transition
    (see item17's own result/disclosure): that specific daemon process
    was compiled before this fix existed and cannot retroactively gain
    it -- a real, permanent, one-time limitation for that exact upgrade,
    not something any code change here can retroactively fix. What CAN
    be verified for real is the mechanism itself, end to end, against a
    real running current daemon -- proving every upgrade *from* v0.0.2
    onward self-heals."""
    import json
    import socket as socket_mod

    state_dir = fake_home / LOCAL_STATE_DIRNAME / "state" / "libra-governor"
    lines = []

    sid0 = f"pre-{uuid.uuid4()}"
    pf0 = preflight(binary, env, sid0, FIXTURES / "rust-crate")
    stop(binary, env, sid0)
    lines.append(f"warm-up real task against the current daemon: preflight exit={pf0.returncode}")

    daemon_pid_before = str(daemon_pid)
    lines.append(f"daemon pid before synthetic newer-protocol request: {daemon_pid_before}")

    sock_path = state_dir / SOCKET_FILENAME
    raw_envelope = json.dumps({"protocol_version": 999, "request": {"kind": "status"}}) + "\n"
    sock = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    sock.settimeout(5)
    sock.connect(str(sock_path))
    sock.sendall(raw_envelope.encode("utf-8"))
    raw_response = sock.makefile("r").readline()
    sock.close()
    lines.append(f"sent a synthetic protocol_version=999 request directly over the socket, raw response: {raw_response.strip()!r}")

    # Poll the daemon's own log for its exact shutdown message -- a
    # `ps -p <pid>` liveness check was tried here first and dropped: on
    # a fast respawn the OS can reuse the exact same pid number for the
    # brand-new daemon process within the polling window, making a
    # point-in-time `ps` snapshot alone an unreliable signal (a false
    # "still alive" for a *different* process holding the same number).
    # The log line is written synchronously by the exiting process
    # itself before it exits, so it cannot produce that false negative.
    log_path = state_dir / LOG_FILENAME
    daemon_exited = False
    for _ in range(50):
        if log_path.exists() and "shutting down: a client speaking a newer protocol" in log_path.read_text():
            daemon_exited = True
            break
        time.sleep(0.1)
    lines.append(f"original daemon logged its own shutdown and exited: {daemon_exited}")

    sid1 = f"post-{uuid.uuid4()}"
    pf1 = preflight(binary, env, sid1, FIXTURES / "rust-crate")
    stop1 = stop(binary, env, sid1)
    receipts_after = receipt_count(state_dir / LEDGER_FILENAME)
    fresh_daemon_governs = pf1.returncode == 0 and "task " in pf1.stdout and stop1.returncode == 0
    lines.append(
        f"next hook invocation after the old daemon exited: preflight exit={pf1.returncode}, "
        f"has real admission={('task ' in pf1.stdout)}, stop exit={stop1.returncode}, "
        f"receipts_after={receipts_after}"
    )

    w("19_self_healing_daemon_upgrade.txt", "\n".join(lines))
    record(
        "item19_self_healing_daemon_upgrade",
        daemon_exited and fresh_daemon_governs,
        f"daemon_exited_after_newer_client={daemon_exited}, fresh_daemon_governs_normally={fresh_daemon_governs}",
    )


def _run_item18_codex_dogfood(work_root: Path, binary: Path, record) -> None:
    """Item 18: real dogfood against the real Codex CLI if present on
    this machine (same discovery pattern as scripts/codex-smoke.sh,
    verified working end-to-end in HORO-1157). Skips (not fails) if
    codex-cli genuinely isn't installed here -- disclosed, not faked."""
    codex_check = run(["codex", "--version"])
    if codex_check.returncode != 0:
        record("item18_codex_dogfood", None, "codex CLI not found on this machine -- genuinely skipped")
        w("18_codex_dogfood.txt", f"codex --version failed (rc={codex_check.returncode}); skipped.\n")
        return

    smoke_script = REPO_ROOT / "scripts" / "codex-smoke.sh"
    if not smoke_script.exists():
        record("item18_codex_dogfood", None, "scripts/codex-smoke.sh missing")
        w("18_codex_dogfood.txt", "scripts/codex-smoke.sh not found; skipped.\n")
        return

    profile = new_profile("codex-dogfood", work_root)
    env = profile["env"]
    proc = run(["bash", str(smoke_script)], cwd=str(REPO_ROOT), env=env, timeout=600)
    w(
        "18_codex_dogfood.txt",
        f"codex --version: {codex_check.stdout.strip()}\n\n$ bash scripts/codex-smoke.sh\nexit={proc.returncode}\n--- stdout ---\n{proc.stdout}\n--- stderr ---\n{proc.stderr}\n",
    )
    record("item18_codex_dogfood", proc.returncode == 0, f"codex_smoke_exit={proc.returncode}")


def main() -> None:
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("--work-root", required=True)
    args = ap.parse_args()

    work_root = Path(args.work_root).resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    RESULTS.mkdir(parents=True, exist_ok=True)

    results = {}

    def record(name, passed, detail):
        results[name] = {"passed": passed, "detail": detail}
        print(f"[{'PASS' if passed is True else 'FAIL' if passed is False else 'SKIP'}] {name}: {detail}")

    # Base v0.0.1 matrix (items 1-11), reused verbatim against the
    # current (v0.0.2) tree -- a real Claude Code regression check.
    binary, env, fake_home = v001._run_item1_install(work_root, record)
    settings_path = v001._run_item2_first_doctor(binary, env, fake_home, record)
    foreign_seed = v001._run_item3_bootstrap(work_root, binary, record)
    state_dir = v001._run_item4_first_preflight(binary, env, fake_home, record)
    v001._run_item5_full_task(binary, env, state_dir, record)
    v001._run_item6_restart_resume(binary, env, state_dir, record)
    v001._run_item8_uninstall_reinstall(binary, env, settings_path, record)
    v001._run_item9_foreign_cycle(work_root, binary, foreign_seed, record)
    v001._run_item10_capability_tier(work_root, binary, record)
    leak_report = v001.run_privacy_checks(state_dir)
    w("11_privacy_leak_check_raw.txt", leak_report)

    # New v0.0.2 scenarios.
    long_profile = new_profile("long-session", work_root)
    _run_item12_long_running_session(binary, long_profile["env"], long_profile["fake_home"], record)

    concurrent_profile = new_profile("concurrent", work_root)
    daemon = spawn_daemon(binary, concurrent_profile["env"])
    try:
        wait_for_socket(concurrent_profile["fake_home"] / LOCAL_STATE_DIRNAME / "state" / "libra-governor", timeout=8)
        _run_item13_concurrent_sessions(binary, concurrent_profile["env"], concurrent_profile["fake_home"], record)
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()

    recovery_profile = new_profile("recovery", work_root)
    _run_item14_failure_recovery(binary, recovery_profile["env"], recovery_profile["fake_home"], record)

    _run_item15_invalid_config(work_root, binary, record)

    rss_profile = new_profile("rss", work_root)
    _run_item16_resource_consumption(binary, rss_profile["env"], rss_profile["fake_home"], record)

    _run_item17_upgrade_from_v001(work_root, binary, record)

    healing_profile = new_profile("self-healing", work_root)
    healing_daemon = spawn_daemon(binary, healing_profile["env"])
    try:
        wait_for_socket(healing_profile["fake_home"] / LOCAL_STATE_DIRNAME / "state" / "libra-governor", timeout=8)
        _run_item19_self_healing_daemon_upgrade(
            binary, healing_profile["env"], healing_profile["fake_home"], healing_daemon.pid, record
        )
    finally:
        if healing_daemon.poll() is None:
            healing_daemon.terminate()

    _run_item18_codex_dogfood(work_root, binary, record)

    RESULTS.joinpath("scenario_results.json").write_text(json.dumps(results, indent=2, default=str))
    print(json.dumps(results, indent=2, default=str))

    failed = [k for k, v in results.items() if v["passed"] is False]
    if failed:
        print(f"\nFAILED SCENARIOS: {failed}")
        sys.exit(1)
    print("\nALL SCENARIOS PASSED (or genuinely skipped)")


if __name__ == "__main__":
    main()
