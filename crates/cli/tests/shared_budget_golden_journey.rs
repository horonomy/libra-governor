//! HORO-2217 (S8): the golden journey of the local monthly shared budget
//! (ADR-0026) against a REAL `libra-governor daemon run` process in an
//! isolated state directory: concurrent admission, attribution through the
//! real hook processes, soft limits that only warn, exhaustion, hot reload,
//! restart and SIGKILL recovery, month rollover, reconciliation and
//! retention/export.
//!
//! Evidence class: **TestOwnedLive** -- real hook and CLI processes and a
//! real daemon, but SYNTHETIC transcripts and no real Claude Code host.
//! Nothing here is `ProductionObserved`.
//!
//! Determinism: no sleep is relied on for correctness (the only waits are
//! bounded readiness polls). A running `daemon run` has no clock injection
//! point (and none is added for tests), so (g) is NOT a clock-driven
//! rollover: it proves period separation and retention against the real
//! daemon by booking charges to earlier periods through `event_time`. The
//! genuine injected-clock rollover (`ManualClock`) is covered in-process by
//! `crates/daemon/tests/shared_budget_core.rs`. Months are labelled from the
//! daemon's own reported period, but that period follows the real clock, so a
//! run straddling a UTC month boundary could still disagree with itself (a
//! known residual).
//!
//! Every child is owned by a guard that kills and reaps it on drop.

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_domain::{ChargeSource, ExecutorKind};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, SharedBudgetDenyReason,
    SharedBudgetOutcome, SharedBudgetRequest, SharedBudgetStatus, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

mod support;

const HOLD: u64 = 1_000;
/// 1,000,000 input tokens of this model = 300 cents in the pinned table.
const MODEL: &str = "claude-sonnet-4-5";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn config(cap: u64, soft: Option<(u64, u64)>) -> String {
    let soft = soft
        .map(|(session, agent)| {
            format!(
                r#","soft_limits":{{"per_session_usd_cents":{session},"per_agent_usd_cents":{agent}}}"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"UTC","turn_hold_usd_cents":{HOLD}{soft}}}"#
    )
}

// ---------------------------------------------------------------------
// The journey harness: one isolated state dir, one real daemon.
// ---------------------------------------------------------------------

struct Ran {
    stdout: String,
    code: Option<i32>,
}

impl Ran {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.stdout))
    }
}

struct Journey {
    state: support::DaemonState,
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Journey {
    /// Writes `config_json` as the budget file and starts the daemon.
    fn start(config_json: &str) -> Self {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        let mut journey = Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            repo,
            daemon: None,
        };
        journey.write_config(config_json);
        journey.spawn_daemon();
        journey
    }

    fn path(&self) -> &Path {
        self.state.path()
    }

    fn sock(&self) -> PathBuf {
        self.path().join("daemon.sock")
    }

    /// Replaces the budget file atomically (write + rename), mode 0600.
    fn write_config(&self, json: &str) {
        let tmp = self.path().join("shared-budget.json.tmp");
        std::fs::write(&tmp, json).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&tmp, self.path().join("shared-budget.json")).unwrap();
    }

    fn spawn_daemon(&mut self) {
        assert!(self.daemon.is_none(), "daemon already running");
        self.daemon = Some(
            Command::new(bin())
                .args(["daemon", "run"])
                .env("LIBRA_GOVERNOR_STATE_DIR", self.path())
                .env("HOME", self.home.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn daemon run"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while try_rpc(&self.sock(), SharedBudgetRequest::Status {}).is_none() {
            let exited = self
                .daemon
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten());
            assert!(exited.is_none(), "daemon exited during startup: {exited:?}");
            assert!(Instant::now() < deadline, "daemon never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pid(&self) -> u64 {
        let record: Value =
            serde_json::from_slice(&std::fs::read(self.path().join("daemon.pid")).unwrap())
                .unwrap();
        record["pid"].as_u64().unwrap()
    }

    /// SIGKILL: no shutdown path runs.
    fn kill9(&mut self) {
        let mut child = self.daemon.take().expect("daemon running");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The identity-checked `daemon stop`, then a bounded wait for exit.
    fn stop(&mut self) {
        assert!(self.cli(&["daemon", "stop"]).status.success());
        let mut child = self.daemon.take().expect("daemon running");
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "daemon did not exit after stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(bin());
        command
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", self.path())
            .env("HOME", self.home.path())
            .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
            .env_remove("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).stdin(Stdio::null()).output().unwrap()
    }

    fn hook(&self, sub: &[&str], payload: Value) -> Ran {
        let mut child = self
            .command(sub)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes());
        let out = child.wait_with_output().unwrap();
        Ran {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            code: out.status.code(),
        }
    }

    /// A prompt through the real `UserPromptSubmit` hook.
    fn prompt(&self, session: &str) -> Ran {
        self.hook(
            &["hook", "user-prompt-submit"],
            json!({"session_id": session, "cwd": self.repo.path(), "prompt": "go"}),
        )
    }

    fn rpc(&self, request: SharedBudgetRequest) -> SharedBudgetOutcome {
        try_rpc(&self.sock(), request).expect("daemon answers")
    }

    fn admit(&self, session: &str) -> SharedBudgetOutcome {
        self.rpc(SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
            provider: None,
        })
    }

    fn status(&self) -> SharedBudgetStatus {
        match self.rpc(SharedBudgetRequest::Status {}) {
            SharedBudgetOutcome::Status(s) => *s,
            other => panic!("expected Status, got {other:?}"),
        }
    }

    /// `shared-budget status --json`, the operator's view of the same state.
    fn status_json(&self) -> Value {
        let out = self.cli(&["shared-budget", "status", "--json"]);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// The legacy statusline. The budget segment has a 200 ms bound and is
    /// dropped silently when the machine is slow, so a line without it is
    /// retried (bounded; each attempt is a fresh process, no sleeps).
    fn statusline(&self) -> String {
        let mut line = String::new();
        for _ in 0..20 {
            let out = self.cli(&["statusline"]);
            assert!(out.status.success());
            line = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            if line.contains(" | budget ") {
                break;
            }
        }
        line
    }
}

impl Drop for Journey {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn try_rpc(socket: &Path, request: SharedBudgetRequest) -> Option<SharedBudgetOutcome> {
    let stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    wire::write_message(
        &stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request: Request::SharedBudget {
                shared_budget: request,
            },
        },
    )
    .ok()?;
    let envelope: ResponseEnvelope = wire::read_message(BufReader::new(stream)).ok()?;
    match envelope.response {
        Response::SharedBudget(outcome) => Some(*outcome),
        other => panic!("expected a SharedBudget response, got {other:?}"),
    }
}

fn is_allowed(outcome: &SharedBudgetOutcome) -> bool {
    matches!(outcome, SharedBudgetOutcome::Allowed { .. })
}

fn is_block(ran: &Ran) -> bool {
    ran.code == Some(0) && ran.stdout.contains(r#""decision":"block""#)
}

fn block_reason(ran: &Ran) -> String {
    assert!(is_block(ran), "expected a block, got {:?}", ran.stdout);
    ran.json()["reason"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------
// Synthetic transcripts.
// ---------------------------------------------------------------------

fn rec(secs_from_now: i64, id: &str, session: &str, tokens: u64, agent: Option<&str>) -> String {
    let ts = (OffsetDateTime::now_utc() + time::Duration::seconds(secs_from_now))
        .format(&Rfc3339)
        .unwrap();
    let side = match agent {
        Some(a) => format!(r#""isSidechain":true,"agentId":"{a}","#),
        None => String::new(),
    };
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{session}",{side}"message":{{"id":"{id}","model":"{MODEL}","usage":{{"input_tokens":{tokens},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

// ---------------------------------------------------------------------
// Reading the status back.
// ---------------------------------------------------------------------

type Rows = BTreeMap<(String, Option<String>, String), (u64, u64)>;

/// The executor table of a daemon status, keyed by (session, agent, kind).
fn rows(status: &SharedBudgetStatus) -> Rows {
    status
        .executors
        .iter()
        .map(|e| {
            (
                (
                    e.session_id.clone(),
                    e.agent_id.clone(),
                    e.executor_kind.as_str().to_string(),
                ),
                (e.used_usd_cents, e.reserved_usd_cents),
            )
        })
        .collect()
}

fn key(session: &str, agent: Option<&str>, kind: ExecutorKind) -> (String, Option<String>, String) {
    (
        session.to_string(),
        agent.map(str::to_string),
        kind.as_str().to_string(),
    )
}

/// sum(executors) == the pool total, for the daemon's status AND for the
/// operator-facing `status --json`, which must say the same.
fn assert_reconciled(journey: &Journey) {
    let status = journey.status();
    let used: u64 = status.executors.iter().map(|e| e.used_usd_cents).sum();
    let held: u64 = status.executors.iter().map(|e| e.reserved_usd_cents).sum();
    assert_eq!(used, status.used_usd_cents, "{:?}", status.executors);
    assert_eq!(held, status.reserved_usd_cents, "{:?}", status.executors);

    let view = journey.status_json();
    assert_eq!(view["state"], "reported", "{view}");
    assert_eq!(view["reconciliation"]["ok"], true, "{view}");
    assert!(view["reconciliation"]["discrepancy"].is_null());
    let amounts = &view["amounts_usd_cents"];
    assert_eq!(amounts["settled"], status.used_usd_cents);
    assert_eq!(amounts["held"], status.reserved_usd_cents);
    let listed: u64 = view["executors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["settled_usd_cents"].as_u64().unwrap())
        .sum();
    assert_eq!(listed, status.used_usd_cents, "operator view sums too");
}

// =====================================================================
// (a) 16 concurrent client processes race one small cap.
// =====================================================================

const WORKER_ENV: &str = "HORO2217_WORKER";
const RESULT_PREFIX: &str = "HORO2217_RESULT ";

/// Harness entrypoint, NOT a test: it appears in the test list only because
/// the parent re-executes this test binary to get real separate client
/// processes. It is the body of one admitting client process. A no-op
/// unless the parent re-executes this test binary with `HORO2217_WORKER`
/// set. spec = "<socket>|<ready file>|<gate file>|<session id>".
#[test]
fn golden_admit_worker_entrypoint() {
    let Ok(spec) = std::env::var(WORKER_ENV) else {
        return;
    };
    let parts: Vec<&str> = spec.split('|').collect();
    let (socket, ready, gate, session) = (parts[0], parts[1], parts[2], parts[3]);
    std::fs::write(ready, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !Path::new(gate).exists() {
        assert!(Instant::now() < deadline, "gate never opened");
        std::thread::sleep(Duration::from_millis(1));
    }
    let outcome = try_rpc(
        Path::new(socket),
        SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
            provider: None,
        },
    )
    .expect("daemon answered");
    println!(
        "{RESULT_PREFIX}{}",
        serde_json::to_string(&outcome).unwrap()
    );
}

struct Reaper(Vec<Child>);

impl Drop for Reaper {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn sixteen_concurrent_clients_never_over_allocate_and_every_refusal_says_why() {
    const CLIENTS: usize = 16;
    const CAP: u64 = 5_500; // fits exactly five holds of HOLD
    let journey = Journey::start(&config(CAP, None));

    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("go");
    let readies: Vec<PathBuf> = (0..CLIENTS)
        .map(|i| gate_dir.path().join(format!("ready-{i}")))
        .collect();
    let mut reaper = Reaper(Vec::new());
    for (i, ready) in readies.iter().enumerate() {
        let spec = format!(
            "{}|{}|{}|racer-{i}",
            journey.sock().display(),
            ready.display(),
            gate.display()
        );
        reaper.0.push(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "golden_admit_worker_entrypoint", "--nocapture"])
                .env(WORKER_ENV, spec)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn client process"),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while !readies.iter().all(|p| p.exists()) {
        assert!(Instant::now() < deadline, "clients never became ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    // One file creation releases every client at once.
    std::fs::write(&gate, b"").unwrap();

    let (mut granted, mut denied) = (0usize, 0usize);
    while !reaper.0.is_empty() {
        let child = reaper.0.remove(0);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "client failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|l| l.strip_prefix(RESULT_PREFIX))
            .unwrap_or_else(|| panic!("no result line in {stdout:?}"));
        match serde_json::from_str::<SharedBudgetOutcome>(line).unwrap() {
            SharedBudgetOutcome::Allowed { hold_usd_cents, .. } => {
                assert_eq!(hold_usd_cents, HOLD);
                granted += 1;
            }
            SharedBudgetOutcome::Denied {
                reason: SharedBudgetDenyReason::CapExhausted,
                remaining_usd_cents,
                period_end,
                ..
            } => {
                assert_eq!(remaining_usd_cents, Some(500), "names what is left");
                assert!(period_end.is_some(), "names when the period ends");
                denied += 1;
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    assert!(
        granted as u64 * HOLD <= CAP,
        "granted {granted} holds of {HOLD} against a cap of {CAP}"
    );
    assert_eq!(granted, 5, "exactly floor(cap / hold) clients are granted");
    assert_eq!(denied, CLIENTS - granted, "every other client is refused");

    let status = journey.status();
    assert_eq!(status.reserved_usd_cents, granted as u64 * HOLD);
    assert!(status.used_usd_cents + status.reserved_usd_cents <= CAP);
    assert_eq!(status.remaining_usd_cents, Some(500));
    assert_eq!(status.executors.len(), granted, "refusals reserve nothing");
    assert_reconciled(&journey);
}

// =====================================================================
// (b) Attribution: sessions, subagents (SubagentStop + Stop sweep),
//     unknown identity, no double charge, reconciliation.
// =====================================================================

#[test]
fn sessions_and_subagents_are_attributed_exactly_once_and_unknown_identity_is_kept() {
    let journey = Journey::start(&config(50_000, None));
    let project = journey.path().join("project");

    let (s1, s2) = ("sess-one", "sess-two");
    let s1_main = project.join(format!("{s1}.jsonl"));
    let s1_dir = project.join(s1).join("subagents");
    let a1 = s1_dir.join("agent-a1.jsonl");
    let a9z = s1_dir.join("agent-a9z.jsonl");
    // A subagent file outside the session directory: its owner cannot be
    // identified, so it must surface as `unattributed`, not vanish.
    let stray = journey.path().join("stray-subagent.jsonl");
    let s2_main = project.join(format!("{s2}.jsonl"));

    assert!(!is_block(&journey.prompt(s1)));
    assert!(!is_block(&journey.prompt(s2)));
    assert_eq!(journey.status().reserved_usd_cents, 2 * HOLD);

    write_lines(&s1_main, &[rec(1, "m1", s1, 2_000_000, None)]); // 600
    write_lines(&a1, &[rec(2, "x1", s1, 2_000_000, Some("a1"))]); // 600
    write_lines(&a9z, &[rec(2, "x2", s1, 2_000_000, Some("a9z"))]); // 600
    write_lines(&stray, &[rec(2, "x3", s1, 1_000_000, Some("zz"))]); // 300
    write_lines(&s2_main, &[rec(1, "n1", s2, 1_000_000, None)]); // 300

    let subagent_stop = |agent: Option<&str>, file: &Path| {
        let mut payload = json!({
            "session_id": s1, "hook_event_name": "SubagentStop",
            "agent_transcript_path": file, "transcript_path": s1_main,
        });
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
        }
        let ran = journey.hook(&["hook", "subagent-stop"], payload);
        assert_eq!(ran.code, Some(0));
        assert_eq!(ran.stdout, "", "a settlement hook never prints");
    };
    let stop = |session: &str, main: &Path| {
        let ran = journey.hook(
            &["hook", "stop"],
            json!({"session_id": session, "transcript_path": main}),
        );
        assert_eq!(ran.code, Some(0));
    };

    // The named subagent through SubagentStop; the id-less one through the
    // file-name family; the one nobody can place as unattributed.
    subagent_stop(Some("a1"), &a1);
    subagent_stop(None, &a9z);
    subagent_stop(None, &stray);
    // The turns' Stop hooks settle each main lane and sweep the subagent
    // files: files already charged add nothing.
    stop(s1, &s1_main);
    stop(s2, &s2_main);

    let expected: Rows = BTreeMap::from([
        (key(s1, None, ExecutorKind::AgentAbsent), (600, 0)),
        (key(s1, Some("a1"), ExecutorKind::Agent), (600, 0)),
        (key(s1, Some("a9z"), ExecutorKind::Agent), (600, 0)),
        (key(s1, None, ExecutorKind::Unattributed), (300, 0)),
        (key(s2, None, ExecutorKind::AgentAbsent), (300, 0)),
    ]);
    let status = journey.status();
    assert_eq!(rows(&status), expected, "attribution per executor");
    assert_eq!(status.used_usd_cents, 2_400);
    assert_eq!(status.reserved_usd_cents, 0, "both holds were released");

    // Replays charge nothing: the same hooks again, in another order.
    subagent_stop(None, &stray);
    subagent_stop(Some("a1"), &a1);
    subagent_stop(None, &a9z);
    stop(s1, &s1_main);
    stop(s2, &s2_main);
    let again = journey.status();
    assert_eq!(rows(&again), expected, "no double charge");
    assert_eq!(again.used_usd_cents, 2_400);

    assert_reconciled(&journey);
    let table =
        String::from_utf8_lossy(&journey.cli(&["shared-budget", "status"]).stdout).into_owned();
    assert!(table.contains("unattributed"), "{table}");
    assert!(table.contains("agent_absent"), "{table}");
    assert!(table.contains("Reconciliation: OK"), "{table}");
}

// =====================================================================
// (c) A soft limit is crossed: warn, never deny.
// =====================================================================

#[test]
fn a_crossed_soft_limit_warns_in_status_and_statusline_but_never_denies() {
    // per-session 700 < one hold (1,000): the first admission already
    // crosses it; per-agent 500.
    let journey = Journey::start(&config(50_000, Some((700, 500))));

    let first = journey.prompt("limit-1");
    assert!(!is_block(&first), "{:?}", first.stdout);
    assert!(
        first.stdout.contains("budget soft limit"),
        "the prompt carries the warning: {:?}",
        first.stdout
    );
    // A subagent over its own per-agent limit.
    match journey.rpc(SharedBudgetRequest::ChargeAgent {
        session_id: "limit-1".into(),
        agent_id: Some("busy".into()),
        amount_usd_cents: 600,
        idempotency_key: "busy-1".into(),
        charge_source: ChargeSource::SubagentTranscript,
        event_time: None,
        provider: None,
    }) {
        SharedBudgetOutcome::Charged { .. } => {}
        other => panic!("{other:?}"),
    }

    let status = journey.status();
    let scopes: Vec<String> = status
        .warnings
        .iter()
        .map(|w| format!("{:?}", w.scope))
        .collect();
    assert!(scopes.contains(&"PerSession".to_string()), "{scopes:?}");
    assert!(scopes.contains(&"PerAgent".to_string()), "{scopes:?}");

    let view = journey.status_json();
    assert_eq!(
        view["soft_limits"]["behaviour"],
        "warn-only; never denies admission"
    );
    assert!(!view["soft_limits"]["warnings"]
        .as_array()
        .unwrap()
        .is_empty());
    let human =
        String::from_utf8_lossy(&journey.cli(&["shared-budget", "status"]).stdout).into_owned();
    assert!(human.contains("WARN session"), "{human}");
    assert!(human.contains("WARN agent"), "{human}");
    assert!(human.contains("never deny admission"), "{human}");

    let line = journey.statusline();
    assert!(line.contains("budget $"), "{line}");
    assert!(line.ends_with("WARN"), "{line}");

    // Warn-only: the over-limit session is admitted again and so is a new one.
    assert!(is_allowed(&journey.admit("limit-1")));
    assert!(!is_block(&journey.prompt("limit-1")));
    assert!(!is_block(&journey.prompt("limit-2")));
    assert!(journey.status().reserved_usd_cents >= HOLD);
}

// =====================================================================
// (d) + (e) Exhaustion denies with the documented reason; a cap raised
//     in the file takes effect without a restart.
// =====================================================================

#[test]
fn an_exhausted_pool_denies_with_the_documented_reason_and_a_raised_cap_hot_reloads() {
    let mut journey = Journey::start(&config(2_000, None)); // two holds

    assert!(!is_block(&journey.prompt("e-1")));
    assert!(!is_block(&journey.prompt("e-2")));
    let line = journey.statusline();
    assert!(
        line.ends_with("budget $20.00/$20 (100%) WARN · budget low: new sessions blocked"),
        "{line}"
    );

    // (d) The third prompt is refused before any spend, with the reason.
    let denied = journey.prompt("e-3");
    let reason = block_reason(&denied);
    assert!(
        reason.starts_with("Libra shared monthly budget exhausted: $0.00 remaining"),
        "{reason}"
    );
    assert!(reason.contains("the period ends"), "{reason}");
    assert!(
        reason.contains("admission-bounded (cooperative), not provider quota"),
        "{reason}"
    );
    // A tool call with no hold is denied too (PreToolUse), a holder is not.
    let pre = |session: &str| {
        journey.hook(
            &["hook", "pre-tool-use"],
            json!({"session_id": session, "hook_event_name": "PreToolUse", "tool_name": "Bash",
                   "tool_input": {}}),
        )
    };
    assert!(pre("e-3").stdout.contains(r#""permissionDecision":"deny""#));
    assert_eq!(
        pre("e-1").stdout,
        "",
        "a lane that holds budget keeps working"
    );
    assert_eq!(
        journey.status().reserved_usd_cents,
        2_000,
        "nothing was charged"
    );

    // (e) Hot reload: edit the file, no restart.
    let pid = journey.pid();
    journey.write_config(&config(3_000, None));
    assert!(!is_block(&journey.prompt("e-3")), "allowed after the edit");
    assert_eq!(journey.pid(), pid, "the same daemon process served it");
    assert_eq!(journey.status().reserved_usd_cents, 3_000);
    let human =
        String::from_utf8_lossy(&journey.cli(&["shared-budget", "status"]).stdout).into_owned();
    assert!(
        human.contains("file edited since the daemon started"),
        "{human}"
    );
    assert_eq!(journey.status_json()["config"]["changed_since_start"], true);
    // The cap is a ceiling again.
    assert!(is_block(&journey.prompt("e-4")));
    journey.stop();
}

// =====================================================================
// (f) Restart and SIGKILL: nothing is lost, holds survive; the ledger's
//     expiry primitive (driven directly) then reclaims them.
// =====================================================================

fn charge_700(journey: &Journey) -> SharedBudgetOutcome {
    journey.rpc(SharedBudgetRequest::ChargeAgent {
        session_id: "f-1".into(),
        agent_id: Some("sub".into()),
        amount_usd_cents: 700,
        idempotency_key: "f-charge".into(),
        charge_source: ChargeSource::SubagentTranscript,
        event_time: None,
        provider: None,
    })
}

#[test]
fn a_restarted_and_a_sigkilled_daemon_recover_state_and_the_ledger_reclaims_dead_holds() {
    let mut journey = Journey::start(&config(10_000, None));
    let h1 = match journey.admit("f-1") {
        SharedBudgetOutcome::Allowed { reservation_id, .. } => reservation_id,
        other => panic!("{other:?}"),
    };
    let h2 = match journey.admit("f-2") {
        SharedBudgetOutcome::Allowed { reservation_id, .. } => reservation_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        charge_700(&journey),
        SharedBudgetOutcome::Charged {
            already_charged: false,
            ..
        }
    ));
    let before = journey.status();
    assert_eq!(
        (before.used_usd_cents, before.reserved_usd_cents),
        (700, 2_000)
    );

    // Graceful restart.
    let first_pid = journey.pid();
    journey.stop();
    journey.spawn_daemon();
    assert_ne!(journey.pid(), first_pid, "a new daemon serves");
    let after = journey.status();
    assert_eq!(rows(&after), rows(&before), "executor table survives");
    assert_eq!(after.remaining_usd_cents, before.remaining_usd_cents);
    assert!(matches!(
        charge_700(&journey),
        SharedBudgetOutcome::Charged {
            already_charged: true,
            ..
        }
    ));
    assert_eq!(
        journey.status().used_usd_cents,
        700,
        "the replay is not new spend"
    );
    assert!(matches!(
        journey.admit("f-1"),
        SharedBudgetOutcome::Allowed {
            reservation_id,
            hold_reused: true,
            ..
        } if reservation_id == h1
    ));

    // SIGKILL: no shutdown path runs; the holds still count afterwards.
    let second_pid = journey.pid();
    journey.kill9();
    journey.spawn_daemon();
    assert_ne!(journey.pid(), second_pid);
    let crashed = journey.status();
    assert_eq!(
        (crashed.used_usd_cents, crashed.reserved_usd_cents),
        (700, 2_000),
        "the crash lost nothing and freed nothing early"
    );

    // Holds survive the SIGKILL. Reclaiming them is exercised through the
    // ledger's expiry primitive, called directly while the daemon is down:
    // the hold TTL is not configurable, so a daemon-driven expiry cannot be
    // reached without waiting it out. Daemon-driven expiry with an injected
    // clock is covered in-process by `an_abandoned_hold_expires_and_frees_its_capacity`
    // in crates/daemon/tests/shared_budget_core.rs.
    journey.kill9();
    let mut store = LedgerStore::open(journey.path().join("ledger.sqlite3")).unwrap();
    let latest = [h1, h2]
        .iter()
        .map(|id| {
            store
                .get_shared_reservation(*id)
                .unwrap()
                .unwrap()
                .expires_at
        })
        .max()
        .unwrap();
    assert_eq!(
        store
            .expire_stale_shared_pool_reservations(latest + time::Duration::seconds(1))
            .unwrap()
            .len(),
        2
    );
    drop(store);
    journey.spawn_daemon();
    let reclaimed = journey.status();
    assert_eq!(reclaimed.reserved_usd_cents, 0, "dead holds are reclaimed");
    assert_eq!(reclaimed.used_usd_cents, 700, "settled spend is never lost");
    assert_eq!(reclaimed.remaining_usd_cents, Some(9_300));
    assert_reconciled(&journey);
}

// =====================================================================
// (g) Period separation and retention (not a clock-driven rollover): the
//     current period is its own pool and an earlier one is retained.
// =====================================================================

/// `monthly-YYYY-MM` shifted back by `months`.
fn label_back(label: &str, months: i64) -> String {
    let year: i64 = label["monthly-".len()..][..4].parse().unwrap();
    let month: i64 = label["monthly-".len() + 5..][..2].parse().unwrap();
    let index = year * 12 + (month - 1) - months;
    format!(
        "monthly-{:04}-{:02}",
        index.div_euclid(12),
        index.rem_euclid(12) + 1
    )
}

/// Books `amount` cents to the (earlier) period `label` through the daemon.
fn charge_in_period(journey: &Journey, label: &str, key: &str, amount: u64) {
    match journey.rpc(SharedBudgetRequest::ChargeAgent {
        session_id: "history".into(),
        agent_id: Some(format!("agent-{key}")),
        amount_usd_cents: amount,
        idempotency_key: key.to_string(),
        charge_source: ChargeSource::SubagentTranscript,
        event_time: Some(format!("{}-15T12:00:00Z", &label["monthly-".len()..])),
    }) {
        SharedBudgetOutcome::Charged {
            event_time_clamped: false,
            already_charged: false,
            ..
        } => {}
        other => panic!("{other:?}"),
    }
}

fn current_label(journey: &Journey) -> String {
    journey.status_json()["period"]["label"]
        .as_str()
        .unwrap()
        .to_string()
}

fn export(journey: &Journey, label: &str) -> Value {
    let period = &label["monthly-".len()..];
    let out = journey.cli(&[
        "shared-budget",
        "export",
        "--period",
        period,
        "--raw-ids",
        "--overwrite",
    ]);
    assert!(
        out.status.success(),
        "export {period}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    let path = PathBuf::from(summary["written"].as_str().unwrap());
    assert!(
        path.starts_with(journey.path().join("exports")),
        "local only"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc["body"]["transport_policy"], "local_only");
    doc["body"].clone()
}

#[test]
fn a_charge_booked_to_an_earlier_period_stays_in_its_own_retained_pool() {
    let journey = Journey::start(&config(50_000, None));
    let now = current_label(&journey);
    let previous = label_back(&now, 1);

    charge_in_period(&journey, &previous, "old-1", 700);
    assert!(is_allowed(&journey.admit("g-1")));

    // The current period is its own pool: last month's spend is not in it.
    let status = journey.status();
    assert!(status.period.as_ref().unwrap().pool_id.ends_with(&now));
    assert_eq!(
        (status.used_usd_cents, status.reserved_usd_cents),
        (0, HOLD),
        "the previous period's 700 does not count against this month"
    );
    assert_eq!(status.remaining_usd_cents, Some(49_000));
    assert_eq!(status.executors.len(), 1);

    // ...and the old period is still there, intact.
    let old = export(&journey, &previous);
    assert_eq!(old["totals"]["used"], 700);
    assert_eq!(old["totals"]["reserved"], 0);
    let new = export(&journey, &now);
    assert_eq!(new["totals"]["used"], 0);
    assert_eq!(new["totals"]["reserved"], HOLD);
    assert_reconciled(&journey);
}

// =====================================================================
// (h) is asserted by assert_reconciled in every test above.
// (i) Retention: prune dry-run, then --apply of an ended period.
// =====================================================================

#[test]
fn prune_removes_only_an_ended_period_and_leaves_the_current_one_intact() {
    let journey = Journey::start(&config(50_000, None));
    let now = current_label(&journey);
    let recent = label_back(&now, 1);
    let old = label_back(&now, 3);

    charge_in_period(&journey, &old, "old", 700);
    charge_in_period(&journey, &recent, "recent", 500);
    assert!(is_allowed(&journey.admit("i-1")));
    let before = rows(&journey.status());

    // The operator exports the old period before pruning it.
    assert_eq!(export(&journey, &old)["totals"]["used"], 700);

    let prune = |apply: bool| {
        let mut args = vec![
            "shared-budget",
            "prune",
            "--keep-months",
            "1",
            "--time-zone",
            "UTC",
        ];
        if apply {
            args.push("--apply");
        }
        let out = journey.cli(&args);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };

    let dry = prune(false);
    assert_eq!(dry["report"]["dry_run"], true);
    assert_eq!(dry["report"]["pools_selected"], 1, "{dry}");
    assert_eq!(dry["report"]["pools_pruned"], 0);
    assert_eq!(dry["report"]["rows_deleted"], 0);
    assert_eq!(
        export(&journey, &old)["totals"]["used"],
        700,
        "a dry run deletes nothing"
    );

    let applied = prune(true);
    assert_eq!(applied["report"]["dry_run"], false);
    assert_eq!(applied["report"]["pools_pruned"], 1, "{applied}");
    assert!(applied["report"]["rows_deleted"].as_u64().unwrap() >= 1);

    // The ended period is gone; the kept one and the current one are not.
    let gone = journey.cli(&[
        "shared-budget",
        "export",
        "--period",
        &old["monthly-".len()..],
    ]);
    assert!(!gone.status.success(), "a pruned period cannot be exported");
    assert_eq!(export(&journey, &recent)["totals"]["used"], 500);
    let current = export(&journey, &now);
    assert_eq!(current["totals"]["reserved"], HOLD);
    assert_eq!(rows(&journey.status()), before, "the live period is intact");
    assert!(is_allowed(&journey.admit("i-2")), "admission still works");
    assert_reconciled(&journey);
}

// =====================================================================
// `doctor` against the live daemon.
// =====================================================================

#[test]
fn doctor_reports_the_live_budget_and_never_prints_the_host_traffic_value() {
    const SENTINEL: &str = "SENTINEL-VALUE-must-never-be-printed";
    let journey = Journey::start(&config(50_000, None));
    assert!(is_allowed(&journey.admit("d-1")));

    let out = journey
        .command(&["doctor", "--json"])
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", SENTINEL)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!text.contains(SENTINEL), "the value must never be printed");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains(SENTINEL),
        "nor on stderr"
    );
    // The isolated HOME has no settings.json, so with a budget file present
    // the admission and settlement hooks are missing: doctor fails.
    assert_eq!(out.status.code(), Some(1), "{text}");

    let doc: Value = serde_json::from_str(&text).unwrap();
    let finding = |id: &str| -> Value {
        doc["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .unwrap_or_else(|| panic!("no {id} finding in {text}"))
            .clone()
    };
    assert_eq!(finding("shared_budget_config")["severity"], "ok");
    assert_eq!(finding("shared_budget_daemon")["severity"], "ok");
    assert_eq!(finding("shared_budget_clock")["severity"], "ok");
    assert_eq!(finding("shared_budget_settlements")["severity"], "ok");
    let hooks = finding("shared_budget_hooks");
    assert_eq!(hooks["severity"], "error");
    assert!(hooks["message"]
        .as_str()
        .unwrap()
        .contains("nothing is admitted or charged"));
    assert!(hooks["message"].as_str().unwrap().contains("SessionEnd"));
    let limits = finding("shared_budget_limits")["message"]
        .as_str()
        .unwrap()
        .to_string();
    for needle in [
        "admission-bounded (cooperative)",
        "in-turn overshoot",
        "Codex route has no budget gate",
        "no gateway hard cap",
        "accounting boundary",
        "/compact",
        "partly flushed",
        "opens the next month",
        "never a provider bill or quota",
    ] {
        assert!(limits.contains(needle), "{needle}: {limits}");
    }
    let traffic = finding("host_traffic_setting")["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(traffic.contains("process environment has it"), "{traffic}");
    assert!(!traffic.contains("process environment does not have it"));

    // Without the variable it says so.
    let plain = journey.cli(&["doctor", "--json"]);
    let plain_text = String::from_utf8_lossy(&plain.stdout).into_owned();
    assert!(plain_text.contains("process environment does not have it"));
}

#[test]
fn doctor_flags_an_exhausted_pool_while_status_still_exits_zero() {
    let journey = Journey::start(&config(2_000, None));
    assert!(!is_block(&journey.prompt("x-1")));
    assert!(!is_block(&journey.prompt("x-2")));

    let out = journey.cli(&["doctor", "--json"]);
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let exhausted = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "shared_budget_exhausted")
        .unwrap_or_else(|| panic!("no exhausted finding in {doc}"));
    assert_eq!(exhausted["severity"], "warn");
    assert!(exhausted["message"]
        .as_str()
        .unwrap()
        .contains("every new prompt is denied"));
    // The operator view agrees and still exits 0 (doctor is the failing check).
    let status = journey.cli(&["shared-budget", "status"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&status.stdout).contains("Remaining: $0.00"));
}

/// A daemon that answers the legacy `Status` but accepts and then never
/// answers a shared-budget request: the budget segment must be dropped within
/// its bound, not stretch the statusline.
#[test]
fn a_stalled_shared_budget_answer_drops_the_segment_within_its_bound() {
    use libra_governor_protocol::{StatusResult, StatusScope};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let state = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = state.path().join("shared-budget.json");
    std::fs::write(&path, config(50_000, None)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let listener = UnixListener::bind(state.path().join("daemon.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let halt = stop.clone();
    let server = std::thread::spawn(move || {
        let mut held: Vec<UnixStream> = Vec::new();
        while !halt.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let Ok(envelope) = wire::read_message::<_, RequestEnvelope>(BufReader::new(
                stream.try_clone().unwrap(),
            )) else {
                continue;
            };
            match envelope.request {
                Request::Status => {
                    let _ = wire::write_message(
                        &stream,
                        &ResponseEnvelope {
                            protocol_version: PROTOCOL_VERSION,
                            response: Response::Status(Box::new(StatusResult {
                                scope: StatusScope::HostLatestObservation,
                                current_task: None,
                                task_budget: None,
                                task_budget_amounts: None,
                                configured_budget: None,
                                task_budget_freshness: None,
                            })),
                        },
                    );
                }
                _ => held.push(stream), // accept, read, never answer
            }
        }
    });

    let start = Instant::now();
    let out = Command::new(bin())
        .arg("statusline")
        .env("LIBRA_GOVERNOR_STATE_DIR", state.path())
        .env("HOME", home.path())
        .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();

    assert!(out.status.success());
    let line = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    assert_eq!(
        line, "libra: idle",
        "the segment is dropped, the line intact"
    );
    // 200 ms bound plus process start; far below the 5 s request timeout.
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}
