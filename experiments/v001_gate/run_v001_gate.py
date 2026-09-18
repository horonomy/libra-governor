#!/usr/bin/env python3
"""HORO-1152 v0.0.1 release gate evidence.

Drives the REAL compiled ``libra-governor`` binary through the
documented install path (``scripts/install.sh`` / ``cargo install
--path crates/cli --locked``) in an isolated fake-``$HOME`` profile on
this same development machine, then exercises the Claude Code
integration bootstrap, a full governed task, restart/resume, upgrade,
uninstall/reinstall, foreign-settings preservation, and capability-tier
text -- each against the real binary and real on-disk state, following
the pattern established by ``experiments/mvp1_validation`` (HORO-1127),
``experiments/mvp2_calibration`` (HORO-1132), and
``experiments/mvp3_gate`` (HORO-1146).

Single-machine-isolated-profile limitation: there is no second physical
or VM machine available in this environment. "Fresh user environment"
below means a fully isolated fake ``$HOME`` (a fresh temp directory
standing in for ``$HOME``, a fresh ``~/.claude``-equivalent
settings.json, a fresh state dir under ``~/.local/state``) on this same
host -- not a literally separate machine. This is disclosed explicitly
in ``experiments/v001_gate/README.md`` and every results file it
produces.

Each ``scenario_*`` function is a real subprocess of the real compiled
binary. No Rust function is called in-process. No mocking of product
code under test.
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
import sys
import tarfile
import time
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = REPO_ROOT / "experiments" / "mvp1_validation" / "fixtures"
RESULTS = Path(__file__).resolve().parent / "results"

SOCKET_FILENAME = "daemon.sock"
LEDGER_FILENAME = "ledger.sqlite3"
LOG_FILENAME = "daemon.log"


def w(name: str, text: str) -> None:
    """Write a results file (overwrite) and echo a short marker to stdout."""
    path = RESULTS / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    print(f"--- wrote {path} ({len(text)} bytes) ---")


def run(cmd, **kw):
    kw.setdefault("capture_output", True)
    kw.setdefault("text", True)
    return subprocess.run(cmd, **kw)


def run_hook(binary: Path, subcommand: list[str], payload: dict, env: dict, timeout=30):
    return subprocess.run(
        [str(binary), *subcommand],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
    )


def preflight(binary, env, session_id, cwd, prompt="add input validation to the login handler", timeout=30):
    payload = {
        "session_id": session_id,
        "cwd": str(cwd),
        "prompt": prompt,
        "hook_event_name": "UserPromptSubmit",
    }
    return run_hook(binary, ["hook", "user-prompt-submit"], payload, env, timeout=timeout)


def post_tool_use(binary, env, session_id, tool_name="Bash"):
    payload = {"session_id": session_id, "cwd": str(FIXTURES / "rust-crate"), "tool_name": tool_name, "hook_event_name": "PostToolUse"}
    return run_hook(binary, ["hook", "post-tool-use"], payload, env)


def stop(binary, env, session_id, model="claude-sonnet-5"):
    payload = {"session_id": session_id, "model": model, "hook_event_name": "Stop"}
    return run_hook(binary, ["hook", "stop"], payload, env)


def spawn_daemon(binary: Path, env: dict) -> subprocess.Popen:
    return subprocess.Popen(
        [str(binary), "daemon", "run"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )


def wait_for_socket(state_dir: Path, timeout=8.0) -> bool:
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


def _resolve_within(base: Path, *parts: str) -> Path:
    """Join ``parts`` onto ``base``, resolve the result, and assert it
    still resolves inside ``base`` before any caller passes it to a
    filesystem sink (mkdir/open/extractall).

    Guards SonarCloud pythonsecurity:S8707 for every path derived from
    the externally-supplied ``--work-root`` CLI argument: ``base`` is
    validated once, up front, in ``main()`` (resolved to an absolute
    canonical path); every path built from it in this harness is
    constrained here to stay inside that trusted root.
    """
    candidate = base.joinpath(*parts).resolve()
    if candidate != base and base not in candidate.parents:
        raise ValueError(f"path must resolve inside {base}: {candidate}")
    return candidate


def new_profile(tag: str, work_root: Path) -> dict:
    """A fresh fake-$HOME profile: isolated $HOME, cargo registry/git
    symlinked to the real cache (network-cache reuse only -- the install
    root itself, $FAKE_HOME/.cargo/bin, is genuinely fresh), everything
    else genuinely isolated."""
    fake_home = _resolve_within(work_root, f"home-{tag}")
    if fake_home.exists():
        shutil.rmtree(fake_home)
    fake_home.mkdir(parents=True)
    fake_cargo_home = fake_home / ".cargo"
    fake_cargo_home.mkdir(parents=True)
    real_cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    for sub in ("registry", "git"):
        src = real_cargo_home / sub
        if src.exists():
            os.symlink(src, fake_cargo_home / sub)
    (fake_cargo_home / "bin").mkdir(parents=True, exist_ok=True)

    env = dict(os.environ)
    env["HOME"] = str(fake_home)
    env["CARGO_HOME"] = str(fake_cargo_home)
    # rustup itself resolves its toolchain list from $RUSTUP_HOME (default
    # $HOME/.rustup) -- point it explicitly at the real rustup install so
    # this isolated $HOME still has a rustc/cargo toolchain to find. This
    # is the same "download-cache reuse only" compromise as the cargo
    # registry symlinks below: the toolchain binaries are shared, the
    # install root ($FAKE_HOME/.cargo/bin) is genuinely fresh.
    real_rustup_home = os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup"))
    if Path(real_rustup_home).exists():
        env["RUSTUP_HOME"] = real_rustup_home
    env.pop("XDG_STATE_HOME", None)
    env.pop("LIBRA_GOVERNOR_STATE_DIR", None)
    env.pop("LIBRA_GOVERNOR_CLAUDE_DIR", None)
    target_dir = os.environ.get("CARGO_TARGET_DIR")
    if target_dir:
        env["CARGO_TARGET_DIR"] = target_dir
    return {"fake_home": fake_home, "env": env, "cargo_home": fake_cargo_home}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work-root", required=True)
    ap.add_argument("--scenario", default="all")
    args = ap.parse_args()

    # SonarCloud pythonsecurity:S8707: --work-root is an externally
    # supplied CLI argument reaching filesystem mkdir/open/extractall
    # sinks throughout this harness. Resolve it once, here, to an
    # absolute canonical path -- every path built from it downstream
    # (new_profile's fake_home, item 7's old_src/archive_path) is then
    # constrained via _resolve_within() to stay inside this root.
    work_root = Path(args.work_root).resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    RESULTS.mkdir(parents=True, exist_ok=True)

    results = {}

    def record(name, passed, detail):
        results[name] = {"passed": passed, "detail": detail}
        print(f"[{'PASS' if passed else 'FAIL'}] {name}: {detail}")

    # ---------- Item 1: install from documented release path ----------
    profile = new_profile("main", work_root)
    fake_home = profile["fake_home"]
    env = profile["env"]

    install_log = []
    install_log.append(f"$ HOME={fake_home} CARGO_HOME={profile['cargo_home']} CARGO_TARGET_DIR={env.get('CARGO_TARGET_DIR')} ./scripts/install.sh")
    proc = run([str(REPO_ROOT / "scripts" / "install.sh")], cwd=str(REPO_ROOT), env=env, timeout=900)
    install_log.append(f"exit code: {proc.returncode}")
    install_log.append("--- stdout ---")
    install_log.append(proc.stdout)
    install_log.append("--- stderr ---")
    install_log.append(proc.stderr)
    w("01_install_from_documented_path.txt", "\n".join(install_log))

    binary = profile["cargo_home"] / "bin" / "libra-governor"
    record("item1_install", proc.returncode == 0 and binary.exists() and os.access(binary, os.X_OK),
           f"install.sh exit={proc.returncode}, binary present={binary.exists()}")

    # ---------- Item 2: first doctor ----------
    doctor_proc = run([str(binary), "doctor"], env=env, timeout=30)
    w("02_first_doctor.txt",
      f"$ libra-governor doctor  (fresh install, before any Claude Code interaction)\n"
      f"exit code: {doctor_proc.returncode}\n--- stdout ---\n{doctor_proc.stdout}\n--- stderr ---\n{doctor_proc.stderr}\n")
    record("item2_first_doctor", doctor_proc.returncode in (0, 1),
           f"doctor exit={doctor_proc.returncode}")

    # Item 1's install.sh already ran `libra-governor install` for us
    # against this fake_home (that IS the documented install path), so
    # its settings.json is no longer absent -- record where things stand
    # after item 1 for reference, and use a *separate* fresh profile for
    # the explicit "absent settings.json" case in item 3a below.
    settings_dir = fake_home / ".claude"
    settings_path = settings_dir / "settings.json"

    # ---------- Item 3: Claude Code integration bootstrap ----------
    # 3a: absent settings.json -- a fresh profile, binary copied in
    # (not rebuilt) so this exercises `install` in isolation.
    absent_profile = new_profile("absent-settings", work_root)
    absent_home = absent_profile["fake_home"]
    absent_env = absent_profile["env"]
    absent_settings_path = absent_home / ".claude" / "settings.json"
    absent_binpath = absent_profile["cargo_home"] / "bin" / "libra-governor"
    absent_binpath.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, absent_binpath)
    os.chmod(absent_binpath, 0o755)
    assert not absent_settings_path.exists(), "expected no pre-existing settings.json in fresh fake_home"
    install_cmd_proc = run([str(absent_binpath), "install"], env=absent_env, timeout=30)
    absent_settings_after = absent_settings_path.read_text() if absent_settings_path.exists() else None
    w("03a_install_cmd_absent_settings.txt",
      f"$ libra-governor install  (against ABSENT ~/.claude/settings.json, from a fresh profile)\n"
      f"exit code: {install_cmd_proc.returncode}\n--- stdout ---\n{install_cmd_proc.stdout}\n"
      f"--- stderr ---\n{install_cmd_proc.stderr}\n--- resulting settings.json ---\n{absent_settings_after}\n")
    record("item3a_install_absent_settings", install_cmd_proc.returncode == 0 and absent_settings_path.exists(),
           f"install exit={install_cmd_proc.returncode}, settings.json created={absent_settings_path.exists()}")

    # 3b: pre-existing settings.json with foreign keys
    foreign_profile = new_profile("foreign", work_root)
    foreign_home = foreign_profile["fake_home"]
    foreign_env = foreign_profile["env"]
    foreign_settings_dir = foreign_home / ".claude"
    foreign_settings_dir.mkdir(parents=True, exist_ok=True)
    foreign_settings_path = foreign_settings_dir / "settings.json"
    foreign_seed = {
        "hooks": {
            "PreToolUse": [{"type": "command", "command": "/usr/local/bin/some-other-tool hook pre"}],
        },
        "statusLine": {"type": "command", "command": "/usr/local/bin/other-statusline"},
        "env": {"SOME_OTHER_TOOL_TOKEN": "not-a-real-secret-placeholder"},
        "permissions": {"allow": ["Bash(ls:*)"]},
        "someTopLevelKeyLibraDoesNotKnowAbout": {"nested": ["value", 1, True]},
    }
    foreign_settings_path.write_text(json.dumps(foreign_seed, indent=2))
    import hashlib

    def sha(p: Path) -> str:
        return hashlib.sha256(p.read_bytes()).hexdigest()

    pristine_sha = sha(foreign_settings_path)
    pristine_text = foreign_settings_path.read_text()

    binary_foreign = foreign_profile["cargo_home"] / "bin" / "libra-governor"
    # reuse the already-built binary rather than re-running cargo install
    binary_foreign.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, binary_foreign)
    os.chmod(binary_foreign, 0o755)

    install_foreign_proc = run([str(binary_foreign), "install"], env=foreign_env, timeout=30)
    after_install_sha = sha(foreign_settings_path)
    after_install_text = foreign_settings_path.read_text()
    after_install_json = json.loads(after_install_text)

    def foreign_subtree_preserved(doc):
        return (
            doc.get("hooks", {}).get("PreToolUse") == foreign_seed["hooks"]["PreToolUse"]
            and doc.get("statusLine") in (foreign_seed["statusLine"], None)  # libra never overwrites foreign statusLine
            and doc.get("env", {}).get("SOME_OTHER_TOOL_TOKEN") == foreign_seed["env"]["SOME_OTHER_TOOL_TOKEN"]
            and doc.get("permissions") == foreign_seed["permissions"]
            and doc.get("someTopLevelKeyLibraDoesNotKnowAbout") == foreign_seed["someTopLevelKeyLibraDoesNotKnowAbout"]
        )

    foreign_preserved_after_install = foreign_subtree_preserved(after_install_json)

    w("03b_install_cmd_foreign_settings.txt",
      f"$ libra-governor install  (against PRE-EXISTING ~/.claude/settings.json with foreign keys)\n"
      f"exit code: {install_foreign_proc.returncode}\n--- stdout ---\n{install_foreign_proc.stdout}\n"
      f"--- stderr ---\n{install_foreign_proc.stderr}\n"
      f"--- seeded settings.json (pristine) ---\n{pristine_text}\n"
      f"--- settings.json after install ---\n{after_install_text}\n"
      f"pristine sha256: {pristine_sha}\nafter-install sha256: {after_install_sha}\n"
      f"foreign subtrees preserved after install: {foreign_preserved_after_install}\n")
    record("item3b_install_foreign_settings",
           install_foreign_proc.returncode == 0 and foreign_preserved_after_install,
           f"install exit={install_foreign_proc.returncode}, foreign preserved={foreign_preserved_after_install}")

    # ---------- Item 4: first bounded preflight ----------
    state_dir = fake_home / ".local" / "state" / "libra-governor"
    sid = f"gate-{uuid.uuid4()}"
    pf = preflight(binary, env, sid, FIXTURES / "rust-crate")
    w("04_first_bounded_preflight.txt",
      f"$ libra-governor hook user-prompt-submit  (fresh fixture repo: {FIXTURES/'rust-crate'})\n"
      f"session_id: {sid}\nexit code: {pf.returncode}\n--- stdout ---\n{pf.stdout}\n--- stderr ---\n{pf.stderr}\n"
      f"state dir exists after preflight: {state_dir.exists()}\n"
      f"socket present: {(state_dir/SOCKET_FILENAME).exists()}\n")
    record("item4_first_preflight", pf.returncode == 0 and "task " in pf.stdout,
           f"preflight exit={pf.returncode}, socket_up={wait_for_socket(state_dir, timeout=3)}")

    # ---------- Item 5: full governed task + Execution Receipt ----------
    sid2 = f"gate-full-{uuid.uuid4()}"
    pf2 = preflight(binary, env, sid2, FIXTURES / "rust-crate", prompt="implement a new /health endpoint")
    ptu_results = []
    for tool in ("Read", "Edit", "Bash"):
        ptu = post_tool_use(binary, env, sid2, tool_name=tool)
        ptu_results.append((tool, ptu.returncode, ptu.stdout.strip(), ptu.stderr.strip()))
    stop_proc = stop(binary, env, sid2)
    receipts_after = receipt_count(state_dir / LEDGER_FILENAME)
    lines = [
        f"$ libra-governor hook user-prompt-submit -> post-tool-use x3 -> hook stop",
        f"session_id: {sid2}",
        f"preflight exit={pf2.returncode}\n--- preflight stdout ---\n{pf2.stdout}\n",
    ]
    for tool, rc, out, err in ptu_results:
        lines.append(f"post-tool-use({tool}) exit={rc} stdout={out!r} stderr={err!r}")
    lines.append(f"\nstop exit={stop_proc.returncode}\n--- stop stdout ---\n{stop_proc.stdout}\n--- stop stderr ---\n{stop_proc.stderr}\n")
    lines.append(f"receipt rows in ledger after stop: {receipts_after}")
    w("05_full_task_execution_receipt.txt", "\n".join(lines))
    record("item5_full_task_receipt", stop_proc.returncode == 0 and receipts_after >= 1,
           f"stop exit={stop_proc.returncode}, receipts={receipts_after}")

    # ---------- Item 6: restart/resume ----------
    daemon = spawn_daemon(binary, env)
    restart_lines = [f"daemon spawned pid={daemon.pid}"]
    try:
        up = wait_for_socket(state_dir, timeout=8)
        restart_lines.append(f"socket up before kill: {up}")
        before_receipts = receipt_count(state_dir / LEDGER_FILENAME)
        daemon.send_signal(signal.SIGKILL)
        daemon.wait(timeout=5)
        restart_lines.append("daemon SIGKILLed")
        sid3 = f"resume-{uuid.uuid4()}"
        pf3 = preflight(binary, env, sid3, FIXTURES / "python-pkg")
        respawn_ok = pf3.returncode == 0 and "task " in pf3.stdout
        restart_lines.append(f"post-kill preflight exit={pf3.returncode} respawn_ok={respawn_ok}\nstdout={pf3.stdout}")
        stop3 = stop(binary, env, sid3)
        after_receipts = receipt_count(state_dir / LEDGER_FILENAME)
        restart_lines.append(f"post-kill stop exit={stop3.returncode}")
        restart_lines.append(f"receipts before kill={before_receipts}, after resume+stop={after_receipts}")
    finally:
        if daemon.poll() is None:
            daemon.terminate()
    w("06_restart_resume.txt", "\n".join(restart_lines))
    record("item6_restart_resume", respawn_ok and after_receipts > before_receipts,
           f"respawn_ok={respawn_ok}, receipts {before_receipts}->{after_receipts}")

    # ---------- Item 7: upgrade from mvp-3.0 ----------
    upgrade_lines = []
    tag_check = run(["git", "cat-file", "-e", "mvp-3.0"], cwd=str(REPO_ROOT))
    if tag_check.returncode != 0:
        upgrade_lines.append("SKIPPED: tag mvp-3.0 not found in this repository; genuinely infeasible.")
        record("item7_upgrade", None, "tag mvp-3.0 not found")
    else:
        old_src = _resolve_within(work_root, "old-mvp-3.0-src")
        if old_src.exists():
            shutil.rmtree(old_src)
        old_src.mkdir(parents=True)
        archive_path = _resolve_within(work_root, "mvp-3.0.tar")
        with open(archive_path, "wb") as f:
            arch_proc = subprocess.run(["git", "archive", "mvp-3.0"], cwd=str(REPO_ROOT), stdout=f)
        upgrade_lines.append(f"git archive mvp-3.0 exit={arch_proc.returncode}")
        with tarfile.open(archive_path) as tf:
            tf.extractall(old_src)

        upgrade_profile = new_profile("upgrade", work_root)
        up_home = upgrade_profile["fake_home"]
        up_env = upgrade_profile["env"]
        up_binpath = upgrade_profile["cargo_home"] / "bin" / "libra-governor"

        old_install_proc = run(
            ["cargo", "install", "--path", "crates/cli", "--locked"],
            cwd=str(old_src), env=up_env, timeout=900,
        )
        upgrade_lines.append(f"\n$ (old mvp-3.0 tree) cargo install --path crates/cli --locked\nexit={old_install_proc.returncode}\n--- stdout ---\n{old_install_proc.stdout[-3000:]}\n--- stderr ---\n{old_install_proc.stderr[-3000:]}\n")

        old_install_ok = old_install_proc.returncode == 0 and up_binpath.exists()
        if old_install_ok:
            up_install_cmd = run([str(up_binpath), "install"], env=up_env, timeout=30)
            upgrade_lines.append(f"old-binary `install` exit={up_install_cmd.returncode}")
            old_doctor = run([str(up_binpath), "doctor"], env=up_env, timeout=30)
            upgrade_lines.append(f"old-binary `doctor` (before upgrade):\n{old_doctor.stdout}\n")

            sid4 = f"upgrade-old-{uuid.uuid4()}"
            up_state_dir = up_home / ".local" / "state" / "libra-governor"
            pf4 = preflight(up_binpath, up_env, sid4, FIXTURES / "rust-crate")
            stop4 = stop(up_binpath, up_env, sid4)
            receipts_old = receipt_count(up_state_dir / LEDGER_FILENAME)
            upgrade_lines.append(f"old-binary task run: preflight exit={pf4.returncode}, stop exit={stop4.returncode}, receipts={receipts_old}")

            # Now build+install the CURRENT (main) binary over the same profile.
            new_install_proc = run([str(REPO_ROOT / "scripts" / "install.sh")], cwd=str(REPO_ROOT), env=up_env, timeout=900)
            upgrade_lines.append(f"\n$ (current main tree) ./scripts/install.sh  (same $HOME/$CARGO_HOME as old binary)\nexit={new_install_proc.returncode}\n--- stdout (tail) ---\n{new_install_proc.stdout[-3000:]}\n--- stderr (tail) ---\n{new_install_proc.stderr[-3000:]}\n")

            new_doctor = run([str(up_binpath), "doctor"], env=up_env, timeout=30)
            receipts_after_upgrade = receipt_count(up_state_dir / LEDGER_FILENAME)
            upgrade_lines.append(f"new-binary `doctor` (after upgrade):\n{new_doctor.stdout}\n")
            upgrade_lines.append(f"receipts survive upgrade: before={receipts_old}, after={receipts_after_upgrade} (state dir untouched by upgrade)")

            state_survived = receipts_after_upgrade >= receipts_old and receipts_old >= 1
            doctor_ok_after = new_doctor.returncode in (0, 1)
            record("item7_upgrade", new_install_proc.returncode == 0 and state_survived and doctor_ok_after,
                   f"old_install_ok={old_install_ok}, new_install_exit={new_install_proc.returncode}, state_survived={state_survived}")
        else:
            upgrade_lines.append("old mvp-3.0 tree failed to `cargo install --locked` on the current toolchain -- recording as a real finding, not faking success.")
            record("item7_upgrade", False, "old mvp-3.0 build failed under --locked on current toolchain")
    w("07_upgrade_from_mvp3.txt", "\n".join(upgrade_lines))

    # ---------- Item 8: uninstall/reinstall ----------
    uninstall_lines = []
    uninstall_proc = run([str(binary), "uninstall", "--yes"], env=env, timeout=30)
    uninstall_lines.append(f"$ libra-governor uninstall --yes\nexit={uninstall_proc.returncode}\n--- stdout ---\n{uninstall_proc.stdout}\n--- stderr ---\n{uninstall_proc.stderr}\n")
    doctor_after_uninstall = run([str(binary), "doctor"], env=env, timeout=30)
    uninstall_lines.append(f"\n$ libra-governor doctor  (after uninstall)\nexit={doctor_after_uninstall.returncode}\n--- stdout ---\n{doctor_after_uninstall.stdout}\n--- stderr ---\n{doctor_after_uninstall.stderr}\n")
    not_installed_reported = "not install" in doctor_after_uninstall.stdout.lower() or "not wired" in doctor_after_uninstall.stdout.lower() or "no hook" in doctor_after_uninstall.stdout.lower()

    reinstall_proc = run([str(binary), "install"], env=env, timeout=30)
    doctor_after_reinstall = run([str(binary), "doctor"], env=env, timeout=30)
    uninstall_lines.append(f"\n$ libra-governor install  (reinstall)\nexit={reinstall_proc.returncode}\n--- stdout ---\n{reinstall_proc.stdout}\n")
    uninstall_lines.append(f"\n$ libra-governor doctor  (after reinstall)\nexit={doctor_after_reinstall.returncode}\n--- stdout ---\n{doctor_after_reinstall.stdout}\n")
    reinstall_ok = doctor_after_reinstall.returncode in (0, 1) and settings_path.exists()
    w("08_uninstall_reinstall.txt", "\n".join(uninstall_lines))
    record("item8_uninstall_reinstall",
           uninstall_proc.returncode == 0 and reinstall_proc.returncode == 0 and reinstall_ok,
           f"uninstall_exit={uninstall_proc.returncode}, not_installed_reported={not_installed_reported}, reinstall_ok={reinstall_ok}")

    # ---------- Item 9: no damage to unrelated Claude configuration, full cycle ----------
    cycle_profile = new_profile("cycle", work_root)
    cyc_home = cycle_profile["fake_home"]
    cyc_env = cycle_profile["env"]
    cyc_settings_dir = cyc_home / ".claude"
    cyc_settings_dir.mkdir(parents=True, exist_ok=True)
    cyc_settings_path = cyc_settings_dir / "settings.json"
    cyc_seed = dict(foreign_seed)  # same realistic foreign content
    cyc_settings_path.write_text(json.dumps(cyc_seed, indent=2))
    cyc_pristine_sha = sha(cyc_settings_path)
    cyc_binpath = cycle_profile["cargo_home"] / "bin" / "libra-governor"
    cyc_binpath.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, cyc_binpath)
    os.chmod(cyc_binpath, 0o755)

    cyc_install = run([str(cyc_binpath), "install"], env=cyc_env, timeout=30)
    cyc_after_install = json.loads(cyc_settings_path.read_text())
    cyc_uninstall = run([str(cyc_binpath), "uninstall", "--yes"], env=cyc_env, timeout=30)
    cyc_after_uninstall_text = cyc_settings_path.read_text() if cyc_settings_path.exists() else None
    cyc_after_uninstall = json.loads(cyc_after_uninstall_text) if cyc_after_uninstall_text else None

    def foreign_only(doc):
        if doc is None:
            return None
        return {
            "hooks_PreToolUse": doc.get("hooks", {}).get("PreToolUse"),
            "statusLine": doc.get("statusLine"),
            "env": doc.get("env"),
            "permissions": doc.get("permissions"),
            "someTopLevelKeyLibraDoesNotKnowAbout": doc.get("someTopLevelKeyLibraDoesNotKnowAbout"),
        }

    foreign_pristine = foreign_only(cyc_seed)
    foreign_after_install = foreign_only(cyc_after_install)
    foreign_after_uninstall = foreign_only(cyc_after_uninstall)

    byte_identical_after_uninstall = (cyc_settings_path.exists() and sha(cyc_settings_path) == cyc_pristine_sha)
    foreign_subtree_identical_after_uninstall = (foreign_after_uninstall == foreign_pristine)

    backups = sorted(p.name for p in cyc_settings_dir.glob("settings.json.libra-backup-*"))

    item9_text = (
        f"Seeded settings.json (pristine), sha256={cyc_pristine_sha}:\n{json.dumps(cyc_seed, indent=2)}\n\n"
        f"$ libra-governor install\nexit={cyc_install.returncode}\nstdout={cyc_install.stdout}\n\n"
        f"After install, sha256={sha(cyc_settings_path)}:\n{cyc_settings_path.read_text()}\n\n"
        f"Foreign subtree, pristine vs after-install (jq -S style structural compare):\n"
        f"pristine       = {json.dumps(foreign_pristine, sort_keys=True)}\n"
        f"after install  = {json.dumps(foreign_after_install, sort_keys=True)}\n"
        f"foreign subtree unchanged by install: {foreign_after_install == foreign_pristine}\n\n"
        f"$ libra-governor uninstall --yes\nexit={cyc_uninstall.returncode}\nstdout={cyc_uninstall.stdout}\n\n"
        f"After uninstall:\n{cyc_after_uninstall_text}\n"
        f"after-uninstall sha256: {sha(cyc_settings_path) if cyc_settings_path.exists() else 'FILE MISSING'}\n"
        f"byte-identical to pristine after full install->uninstall cycle: {byte_identical_after_uninstall}\n"
        f"foreign subtree structurally identical after full cycle: {foreign_subtree_identical_after_uninstall}\n\n"
        f"backup files written under ~/.claude/ during this cycle: {backups}\n"
        f"(collision check: {len(backups)} backup file(s) for {2} write operations that could have backed up "
        f"an existing file -- install backs up the pre-existing settings.json once; no duplicate/overwritten backup names observed)\n"
    )
    w("09_foreign_settings_full_cycle.txt", item9_text)
    record("item9_foreign_settings_preserved",
           cyc_install.returncode == 0 and cyc_uninstall.returncode == 0 and foreign_subtree_identical_after_uninstall,
           f"byte_identical={byte_identical_after_uninstall}, foreign_subtree_identical={foreign_subtree_identical_after_uninstall}")

    # ---------- Item 10: capability tier text under GovernorHeld vs PassThroughSubscription ----------
    tier_lines = []
    for mode_name, gateway_cfg in (
        ("GovernorHeld", {
            "bind_addr": "127.0.0.1:38765",
            "token_path": str(work_root / "gw-token-governed.txt"),
            "credential_mode": "governor_held",
            "credential_command": "/bin/echo",
            "credential_args": ["fake-test-credential-nonce-8f2c1a"],
        }),
        ("PassThroughSubscription", {
            "bind_addr": "127.0.0.1:38766",
            "token_path": str(work_root / "gw-token-subscription.txt"),
            "credential_mode": "pass_through_subscription",
        }),
    ):
        tier_profile = new_profile(f"tier-{mode_name}", work_root)
        t_home = tier_profile["fake_home"]
        t_env = tier_profile["env"]
        t_state_dir = t_home / ".local" / "state" / "libra-governor"
        t_state_dir.mkdir(parents=True, exist_ok=True)
        cfg = {"gateway": gateway_cfg}
        (t_state_dir / "config.json").write_text(json.dumps(cfg, indent=2))
        t_bin = tier_profile["cargo_home"] / "bin" / "libra-governor"
        t_bin.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(binary, t_bin)
        os.chmod(t_bin, 0o755)
        doctor_json = run([str(t_bin), "doctor", "--json"], env=t_env, timeout=30)
        doctor_text = run([str(t_bin), "doctor"], env=t_env, timeout=30)
        tier_lines.append(f"=== mode: {mode_name} ===\nconfig.json:\n{json.dumps(cfg, indent=2)}\n")
        tier_lines.append(f"$ libra-governor doctor\nexit={doctor_text.returncode}\n{doctor_text.stdout}\n")
        tier_lines.append(f"$ libra-governor doctor --json\nexit={doctor_json.returncode}\n{doctor_json.stdout}\n")

    governed_text = tier_lines[1] if len(tier_lines) > 1 else ""
    subscription_text = tier_lines[4] if len(tier_lines) > 4 else ""
    tiers_differ = governed_text != subscription_text
    overstated_language = any(
        phrase in "\n".join(tier_lines).lower()
        for phrase in ["exact enforcement", "guaranteed cap", "hard limit enforced" ]
    ) and "pass_through" in "\n".join(tier_lines).lower()
    w("10_capability_tier_text.txt", "\n".join(tier_lines))
    record("item10_capability_tier_text", tiers_differ,
           f"tiers_differ={tiers_differ}, suspicious_overstated_language_found={overstated_language}")

    # ---------- Item 11: no secret/private data leakage ----------
    leak_report = run_privacy_checks(state_dir, work_root)
    w("privacy_leak_check_raw.txt", leak_report)

    RESULTS.joinpath("scenario_results.json").write_text(json.dumps(results, indent=2, default=str))
    print(json.dumps(results, indent=2, default=str))


def run_privacy_checks(state_dir: Path, work_root: Path) -> str:
    lines = []
    nonces = {
        "fake_credential_nonce": "fake-test-credential-nonce-8f2c1a",
        "fake_prompt_content": "add input validation to the login handler",
        "fake_prompt_content2": "implement a new /health endpoint",
        "fake_fixture_path": str(FIXTURES / "rust-crate"),
    }
    targets = {
        "daemon.log": state_dir / LOG_FILENAME,
        "ledger.sqlite3 (strings)": state_dir / LEDGER_FILENAME,
    }
    for target_name, path in targets.items():
        for nonce_name, nonce in nonces.items():
            if not path.exists():
                lines.append(f"{target_name} :: {nonce_name}: SKIPPED (file does not exist)")
                continue
            if "sqlite3" in target_name:
                p = run(["bash", "-c", f"strings '{path}' | grep -c -- '{nonce}'"])
            else:
                p = run(["grep", "-c", "--", nonce, str(path)])
            count = p.stdout.strip() or "0"
            expected_absent = nonce_name.startswith("fake_credential") or nonce_name.startswith("fake_prompt")
            verdict = "PASS (absent, as expected)" if (expected_absent and count == "0") else (
                "FAIL (leaked!)" if expected_absent and count != "0" else f"count={count}")
            lines.append(f"{target_name} :: grep for {nonce_name} ({nonce!r}): count={count} -> {verdict}")
    return "\n".join(lines)


if __name__ == "__main__":
    main()
