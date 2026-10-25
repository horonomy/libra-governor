//! HORO-2215 (S6): the Claude Code hook runtime of the local monthly shared
//! budget (ADR-0026), driven through the real compiled `libra-governor`
//! binary.
//!
//! Evidence class: **TestOwnedLive** -- real hook processes against either a
//! real `libra-governor daemon run` or, where an exact daemon answer or a
//! stall must be forced, an in-test daemon speaking the real wire protocol.
//! No real Claude Code host is involved.
//!
//! The behaviour matrix (see `agent/budget_gate.rs`):
//!
//! | hook | budget file | daemon | result |
//! |---|---|---|---|
//! | UserPromptSubmit | absent | any | unchanged (no extra connection) |
//! | UserPromptSubmit | present | allowed / not_configured | unchanged (+ warning line) |
//! | UserPromptSubmit | present | denied / refused / unreachable | `{"decision":"block","reason":..}` |
//! | PreToolUse | absent | any | empty, daemon never contacted |
//! | PreToolUse | present | exhausted / denied | PreToolUse deny JSON |
//! | PreToolUse | present | else, stall, down | empty (allow) |

use std::collections::BTreeSet;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libra_governor_domain::{ExecutorKind, SharedPoolReservationId};
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, SharedBudgetCheckState,
    SharedBudgetDenyReason, SharedBudgetOutcome, SharedBudgetRefusal, SharedBudgetRequest,
    SoftLimitScope, SoftLimitWarning, PROTOCOL_VERSION,
};

mod support;

const LABEL: &str = "Libra-accounted USD, admission-bounded";
const HOLD: u64 = 1_000;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

// ---------------------------------------------------------------------
// Running one hook process.
// ---------------------------------------------------------------------

struct Ran {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    elapsed: Duration,
}

impl Ran {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{:?}", self.stdout))
    }
}

fn run_hook(sub: &[&str], state: &Path, payload: &str, env: &[(&str, &str)]) -> Ran {
    let mut command = Command::new(bin());
    command
        .args(sub)
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .env_remove("LIBRA_GOVERNOR_NO_AUTOSPAWN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    let start = Instant::now();
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Ran {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
        elapsed: start.elapsed(),
    }
}

fn prompt_payload(session: &str, cwd: &Path) -> String {
    serde_json::json!({"session_id": session, "cwd": cwd, "prompt": "SECRET PROMPT TEXT"})
        .to_string()
}

fn prompt(state: &Path, session: &str, cwd: &Path, env: &[(&str, &str)]) -> Ran {
    run_hook(
        &["hook", "user-prompt-submit"],
        state,
        &prompt_payload(session, cwd),
        env,
    )
}

fn pre_tool(state: &Path, payload: serde_json::Value) -> Ran {
    run_hook(&["hook", "pre-tool-use"], state, &payload.to_string(), &[])
}

fn keys(value: &serde_json::Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn write_budget(state: &Path, json: &str) {
    std::fs::write(state.join("shared-budget.json"), json).unwrap();
}

fn config(cap: u64, extra: &str) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"UTC","turn_hold_usd_cents":{HOLD}{extra}}}"#
    )
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    repo
}

/// Asserts the prompt hook's block output: exactly `{decision, reason}`.
fn assert_block(ran: &Ran) -> String {
    assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
    let v = ran.json();
    assert_eq!(keys(&v), set(&["decision", "reason"]), "{v}");
    assert_eq!(v["decision"], "block");
    let reason = v["reason"].as_str().unwrap().to_string();
    assert!(reason.contains(LABEL), "{reason}");
    assert!(!reason.contains("SECRET PROMPT TEXT"));
    reason
}

/// Asserts the PreToolUse deny output and returns its reason.
fn assert_deny(ran: &Ran) -> String {
    assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
    let v = ran.json();
    assert_eq!(keys(&v), set(&["hookSpecificOutput"]), "{v}");
    let inner = &v["hookSpecificOutput"];
    assert_eq!(
        keys(inner),
        set(&[
            "hookEventName",
            "permissionDecision",
            "permissionDecisionReason"
        ])
    );
    assert_eq!(inner["hookEventName"], "PreToolUse");
    assert_eq!(inner["permissionDecision"], "deny");
    let reason = inner["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(reason.contains(LABEL), "{reason}");
    reason
}

fn assert_allow(ran: &Ran) {
    assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
    assert_eq!(ran.stdout, "", "an allowed tool call must print nothing");
}

/// The outer `additionalContext` of a prompt hook's normal output.
fn outer_context(ran: &Ran) -> String {
    assert_eq!(ran.code, Some(0), "stderr: {}", ran.stderr);
    let v = ran.json();
    assert_eq!(keys(&v), set(&["hookSpecificOutput"]), "{v}");
    assert_eq!(
        keys(&v["hookSpecificOutput"]),
        set(&["hookEventName", "additionalContext"])
    );
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------
// In-test daemon: the real wire protocol, an exact scripted answer.
// ---------------------------------------------------------------------

/// What the scripted daemon does with one request.
enum Answer {
    Reply(Response),
    /// Accept, read the request, never answer (until the daemon is dropped).
    Stall,
    /// Send one byte every 150 ms for 3 s, never a full line.
    Trickle,
}

struct Fake {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    fn start(state: &Path, handler: impl Fn(&Request) -> Answer + Send + 'static) -> Self {
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (reqs, halt) = (requests.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            let mut held: Vec<UnixStream> = Vec::new();
            while !halt.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
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
                reqs.lock()
                    .unwrap()
                    .push(serde_json::to_value(&envelope.request).unwrap());
                match handler(&envelope.request) {
                    Answer::Reply(response) => {
                        let _ = wire::write_message(
                            &stream,
                            &ResponseEnvelope {
                                protocol_version: PROTOCOL_VERSION,
                                response,
                            },
                        );
                    }
                    Answer::Stall => held.push(stream),
                    Answer::Trickle => {
                        std::thread::spawn(move || {
                            for _ in 0..20 {
                                if (&stream).write_all(b" ").is_err() {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(150));
                            }
                        });
                    }
                }
            }
        });
        Self {
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }

    /// Kinds of recorded requests, e.g. `["shared_budget:admit", "preflight"]`.
    fn kinds(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| {
                let mut kind = r["type"]
                    .as_str()
                    .or_else(|| r["kind"].as_str())
                    .unwrap_or("?")
                    .to_string();
                if let Some(op) = r["shared_budget"]["op"].as_str() {
                    kind = format!("{kind}:{op}");
                }
                kind
            })
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn reply(outcome: SharedBudgetOutcome) -> Answer {
    Answer::Reply(Response::SharedBudget(Box::new(outcome)))
}

fn allowed(warnings: Vec<SoftLimitWarning>) -> SharedBudgetOutcome {
    SharedBudgetOutcome::Allowed {
        reservation_id: SharedPoolReservationId::new(),
        pool_id: "metered:local-budget/monthly-2027-01".into(),
        hold_usd_cents: HOLD,
        hold_reused: false,
        executor_kind: ExecutorKind::AgentAbsent,
        remaining_usd_cents: 4_000,
        period_end: "2027-02-01T00:00:00Z".into(),
        warnings,
        warnings_unavailable: false,
    }
}

fn denied(reason: SharedBudgetDenyReason) -> SharedBudgetOutcome {
    SharedBudgetOutcome::Denied {
        reason,
        remaining_usd_cents: Some(500),
        period_end: Some("2027-02-01T00:00:00Z".into()),
        detail: Some("time_zone is not an IANA zone".into()),
    }
}

fn err(message: &str) -> Answer {
    Answer::Reply(Response::Error {
        message: message.into(),
    })
}

// ---------------------------------------------------------------------
// A real daemon.
// ---------------------------------------------------------------------

struct Daemon {
    child: Option<Child>,
    _home: tempfile::TempDir,
}

impl Daemon {
    fn start(state: &Path) -> Self {
        let home = tempfile::tempdir().unwrap();
        let child = Command::new(bin())
            .args(["daemon", "run"])
            .env("LIBRA_GOVERNOR_STATE_DIR", state)
            .env("HOME", home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon run");
        let mut daemon = Self {
            child: Some(child),
            _home: home,
        };
        let sock = state.join("daemon.sock");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if status_ok(&sock) {
                return daemon;
            }
            let exited = daemon
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten());
            assert!(exited.is_none(), "daemon exited during startup: {exited:?}");
            assert!(Instant::now() < deadline, "daemon never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn status_ok(sock: &Path) -> bool {
    let Ok(stream) = UnixStream::connect(sock) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let sent = wire::write_message(
        &stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request: Request::SharedBudget {
                shared_budget: SharedBudgetRequest::Status {},
            },
        },
    );
    sent.is_ok() && wire::read_message::<_, ResponseEnvelope>(BufReader::new(stream)).is_ok()
}

// =====================================================================
// (a) No budget file: nothing changes, nothing is contacted.
// =====================================================================

/// The exact text of the existing golden (`agent_contract.rs`), with the
/// task id and the wall-clock timing normalised.
const GOLDEN_PREFLIGHT: &str = "[libra-governor] Preflight complete (task <ID>, confidence: high, \
     recon: <T>s).\n\
     Draft Completion Contract (revision 1):\n  \
     - [required] Matches the user's stated request\n  \
     - [required] Relevant tests pass (cargo test)\n  \
     - [optional] Changes are scoped to the likely affected area(s): src/login.rs\n\
     Cost/time estimate: cold start — insufficient local history: no ExecutionReceipt rows \
     recorded yet (confidence: low). This preflight is advisory only.";

fn normalise(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    // `recon: N.NNs` -> `recon: <T>s`
    while let Some(i) = rest.find("recon: ") {
        out.push_str(&rest[..i + 7]);
        rest = &rest[i + 7..];
        let n = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if n > 0 && rest[n..].starts_with('s') {
            out.push_str("<T>s");
            rest = &rest[n + 1..];
        }
    }
    out.push_str(rest);
    // UUID -> <ID>
    let b: Vec<char> = out.chars().collect();
    let mut res = String::new();
    let mut i = 0;
    while i < b.len() {
        let shape = [8usize, 4, 4, 4, 12];
        let mut j = i;
        let mut ok = true;
        for (gi, g) in shape.iter().enumerate() {
            if j + g > b.len() || !b[j..j + g].iter().all(|c| c.is_ascii_hexdigit()) {
                ok = false;
                break;
            }
            j += g;
            if gi < 4 {
                if b.get(j) != Some(&'-') {
                    ok = false;
                    break;
                }
                j += 1;
            }
        }
        if ok {
            res.push_str("<ID>");
            i = j;
        } else {
            res.push(b[i]);
            i += 1;
        }
    }
    res
}

#[test]
fn no_budget_file_prompt_output_matches_the_pre_s6_golden_exactly() {
    let state = support::DaemonState::new();
    let repo = repo();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/login.rs"), "// login").unwrap();
    assert!(!state.path().join("shared-budget.json").exists());

    let payload = serde_json::json!({
        "session_id": "golden-session", "cwd": repo.path(), "prompt": "fix the login bug",
    })
    .to_string();
    let ran = run_hook(&["hook", "user-prompt-submit"], state.path(), &payload, &[]);

    let outer = outer_context(&ran);
    let inner: serde_json::Value = serde_json::from_str(&outer).expect("double-encoded envelope");
    let text = inner["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(normalise(text), GOLDEN_PREFLIGHT);
    assert!(ran.stdout.ends_with('\n') && ran.stdout.matches('\n').count() == 1);
    assert!(
        !ran.stdout.contains("decision"),
        "no-file output must never block"
    );
}

#[test]
fn no_budget_file_never_sends_a_shared_budget_request() {
    let state = tempfile::tempdir().unwrap();
    let fake = Fake::start(state.path(), |_| err("scripted"));
    let ran = prompt(state.path(), "s-nofile", repo().path(), &[]);
    // Today's behaviour: one Preflight connection, a fail-open notice.
    assert_eq!(
        fake.kinds(),
        vec!["preflight".to_string()],
        "{:?}",
        fake.requests()
    );
    assert!(outer_context(&ran).contains("preflight unavailable"));
}

#[test]
fn a_state_dir_that_is_not_a_directory_has_no_budget_file_so_everything_fails_open() {
    let parent = tempfile::tempdir().unwrap();
    let blocked = parent.path().join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    let ran = prompt(&blocked, "s-enotdir", parent.path(), &[]);
    assert!(!ran.stdout.contains("decision"), "{}", ran.stdout);
    assert_allow(&pre_tool(&blocked, tool_payload("s", None)));
}

#[test]
fn no_budget_file_post_tool_use_and_stop_print_nothing_and_exit_zero() {
    let state = support::DaemonState::new();
    for (sub, payload) in [
        (
            ["hook", "post-tool-use"],
            serde_json::json!({"session_id": "s", "tool_name": "Bash"}),
        ),
        (["hook", "stop"], serde_json::json!({"session_id": "s"})),
    ] {
        let ran = run_hook(&sub, state.path(), &payload.to_string(), &[]);
        assert_eq!(ran.code, Some(0));
        assert_eq!(ran.stdout, "", "{sub:?}");
    }
}

#[test]
fn no_budget_file_pre_tool_use_is_empty_and_never_connects() {
    let state = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(state.path().join("daemon.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let ran = pre_tool(
        state.path(),
        serde_json::json!({"session_id": "s", "tool_name": "Bash", "tool_input": {}}),
    );
    assert_allow(&ran);
    // A connection, even one the client closed straight away, would still be
    // sitting in the backlog.
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "PreToolUse contacted the daemon although no budget file exists"
    );
}

// =====================================================================
// (b) Prompt gate against a scripted daemon: every outcome row.
// =====================================================================

#[test]
fn prompt_denials_block_without_calling_preflight() {
    let cases: Vec<(SharedBudgetOutcome, &str)> = vec![
        (
            denied(SharedBudgetDenyReason::CapExhausted),
            "$5.00 remaining",
        ),
        (denied(SharedBudgetDenyReason::ConfigInvalid), "invalid"),
        (
            denied(SharedBudgetDenyReason::ClockAnomaly),
            "clock anomaly",
        ),
    ];
    for (outcome, needle) in cases {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path(), &config(5_000, ""));
        let canned = outcome.clone();
        let fake = Fake::start(state.path(), move |_| reply(canned.clone()));
        let ran = prompt(state.path(), "s-denied", repo().path(), &[]);
        let reason = assert_block(&ran);
        assert!(reason.contains(needle), "{needle}: {reason}");
        assert_eq!(
            fake.kinds(),
            vec!["shared_budget:admit".to_string()],
            "a blocked prompt must not reach Preflight: {:?}",
            fake.requests()
        );
    }
}

#[test]
fn cap_exhausted_reason_names_remaining_and_period_end() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let _fake = Fake::start(state.path(), |_| {
        reply(denied(SharedBudgetDenyReason::CapExhausted))
    });
    let reason = assert_block(&prompt(state.path(), "s", repo().path(), &[]));
    assert!(reason.contains("$5.00 remaining"), "{reason}");
    // The fixture's zone is UTC, so the zone-local end reads +00:00 (UTC).
    assert!(
        reason.contains("2027-02-01T00:00:00+00:00 (UTC)"),
        "{reason}"
    );
    assert!(reason.contains("monthly_cap_usd_cents"), "{reason}");
    assert!(reason.contains("(cooperative)"), "{reason}");
}

#[test]
fn prompt_admit_carries_the_session_prompt_id_and_no_agent() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |_| reply(SharedBudgetOutcome::NotConfigured));
    let payload = serde_json::json!({
        "session_id": "s-ids", "cwd": repo().path(), "prompt": "x", "prompt_id": "pid-7",
        "agent_id": "ignored-for-admit",
    })
    .to_string();
    run_hook(&["hook", "user-prompt-submit"], state.path(), &payload, &[]);
    let admit = &fake.requests()[0]["shared_budget"];
    assert_eq!(admit["session_id"], "s-ids");
    assert_eq!(admit["prompt_id"], "pid-7");
    assert!(admit["agent_id"].is_null(), "{admit}");
}

#[test]
fn not_configured_and_allowed_continue_to_preflight() {
    for outcome in [SharedBudgetOutcome::NotConfigured, allowed(vec![])] {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path(), &config(5_000, ""));
        let fake = Fake::start(state.path(), move |req| match req {
            Request::SharedBudget { .. } => reply(outcome.clone()),
            _ => err("scripted"),
        });
        let ran = prompt(state.path(), "s-ok", repo().path(), &[]);
        assert_eq!(
            fake.kinds(),
            vec!["shared_budget:admit".to_string(), "preflight".to_string()]
        );
        let ctx = outer_context(&ran);
        assert!(!ran.stdout.contains("decision"));
        assert!(!ctx.contains("soft limit"), "{ctx}");
    }
}

#[test]
fn allowed_with_soft_limit_warns_in_one_short_line_and_never_blocks() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let warning = SoftLimitWarning {
        scope: SoftLimitScope::PerSession,
        session_id: "s-warn".into(),
        agent_id: None,
        used_plus_reserved_usd_cents: 1_000,
        threshold_usd_cents: 500,
    };
    let _fake = Fake::start(state.path(), move |req| match req {
        Request::SharedBudget { .. } => reply(allowed(vec![warning.clone()])),
        _ => err("scripted"),
    });
    let ran = prompt(state.path(), "s-warn", repo().path(), &[]);
    let ctx = outer_context(&ran);
    let last = ctx.lines().last().unwrap();
    assert_eq!(
        last,
        "[libra-governor] budget soft limit: per-session $10.00 of $5.00 (warn only; \
         Libra-accounted USD, admission-bounded)."
    );
    assert!(!ctx.contains("s-warn") && !ctx.contains("SECRET PROMPT TEXT"));
}

#[test]
fn an_unusable_daemon_answer_with_a_budget_file_blocks() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |_| {
        reply(SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Available,
            remaining_usd_cents: 1,
            period_end: "x".into(),
        })
    });
    let reason = assert_block(&prompt(state.path(), "s", repo().path(), &[]));
    assert!(
        reason.contains("budget enforcement unavailable"),
        "{reason}"
    );
    assert_eq!(fake.kinds(), vec!["shared_budget:admit".to_string()]);
}

// =====================================================================
// (b) Daemon unreachable with / without a budget file.
// =====================================================================

#[test]
fn unreachable_daemon_with_a_budget_file_blocks_with_and_without_no_autospawn() {
    // NO_AUTOSPAWN: immediate.
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let ran = prompt(
        state.path(),
        "s-down",
        repo().path(),
        &[("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")],
    );
    let reason = assert_block(&ran);
    assert!(
        reason.contains("budget enforcement unavailable: daemon unreachable"),
        "{reason}"
    );
    assert!(ran.elapsed < Duration::from_secs(2), "{:?}", ran.elapsed);

    // Autospawn enabled, but the daemon cannot come up (its socket path is
    // occupied by a directory): the existing spawn attempt fails, then block.
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    std::fs::create_dir(state.path().join("daemon.sock")).unwrap();
    let ran = prompt(state.path(), "s-down2", repo().path(), &[]);
    let reason = assert_block(&ran);
    assert!(
        reason.contains("budget enforcement unavailable: daemon unreachable"),
        "{reason}"
    );
}

#[test]
fn a_daemon_that_never_answers_admit_is_unreachable_and_blocks_within_the_client_timeout() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let _fake = Fake::start(state.path(), |_| Answer::Stall);
    let ran = prompt(state.path(), "s-stall", repo().path(), &[]);
    let reason = assert_block(&ran);
    assert!(reason.contains("daemon unreachable"), "{reason}");
    assert!(
        ran.elapsed >= Duration::from_secs(4) && ran.elapsed < Duration::from_secs(12),
        "bounded by the existing 5 s client timeout, took {:?}",
        ran.elapsed
    );
}

#[test]
fn unreachable_daemon_without_a_budget_file_is_todays_fail_open() {
    let state = tempfile::tempdir().unwrap();
    let ran = prompt(
        state.path(),
        "s-open",
        repo().path(),
        &[("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")],
    );
    assert_eq!(
        outer_context(&ran),
        "[libra-governor] preflight unavailable: daemon unreachable. Proceeding without \
         governance."
    );
    assert!(!ran.stdout.contains("decision"));
}

// =====================================================================
// (b) Real daemon, real budget file.
// =====================================================================

#[test]
fn real_daemon_allows_then_exhausts_and_blocks_a_second_session() {
    let state = support::DaemonState::new();
    write_budget(state.path(), &config(1_500, ""));
    let _daemon = Daemon::start(state.path());
    let repo = repo();

    let first = prompt(state.path(), "real-A", repo.path(), &[]);
    let ctx = outer_context(&first);
    assert!(ctx.contains("Preflight complete"), "{ctx}");

    let second = prompt(state.path(), "real-B", repo.path(), &[]);
    let reason = assert_block(&second);
    assert!(reason.contains("$5.00 remaining"), "{reason}");
}

#[test]
fn real_daemon_over_a_soft_limit_appends_the_warning_and_still_allows() {
    let state = support::DaemonState::new();
    write_budget(
        state.path(),
        &config(50_000, r#","soft_limits":{"per_session_usd_cents":500}"#),
    );
    let _daemon = Daemon::start(state.path());
    let ran = prompt(state.path(), "real-soft", repo().path(), &[]);
    let ctx = outer_context(&ran);
    assert!(ctx.contains("Preflight complete"), "{ctx}");
    assert!(
        ctx.lines()
            .last()
            .unwrap()
            .starts_with("[libra-governor] budget soft limit: per-session $10.00 of $5.00"),
        "{ctx}"
    );
}

#[test]
fn real_daemon_with_an_invalid_file_blocks_and_fixing_it_unblocks() {
    let state = support::DaemonState::new();
    write_budget(state.path(), "{ this is not json");
    let _daemon = Daemon::start(state.path());
    let reason = assert_block(&prompt(state.path(), "real-bad", repo().path(), &[]));
    assert!(reason.contains("invalid"), "{reason}");

    write_budget(state.path(), &config(50_000, ""));
    let ctx = outer_context(&prompt(state.path(), "real-bad", repo().path(), &[]));
    assert!(ctx.contains("Preflight complete"), "{ctx}");
}

#[test]
fn real_daemon_without_a_file_matches_the_no_file_golden_shape() {
    let state = support::DaemonState::new();
    let _daemon = Daemon::start(state.path());
    let ran = prompt(state.path(), "real-nofile", repo().path(), &[]);
    let outer = outer_context(&ran);
    let inner: serde_json::Value = serde_json::from_str(&outer).unwrap();
    assert!(inner["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .starts_with("[libra-governor] Preflight complete"));
}

// =====================================================================
// (c) PreToolUse matrix.
// =====================================================================

fn tool_payload(session: &str, agent: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "session_id": session, "hook_event_name": "PreToolUse",
        "tool_name": "Bash", "tool_input": {"command": "echo SECRET"},
    });
    if let Some(a) = agent {
        v["agent_id"] = a.into();
    }
    v
}

#[test]
fn pre_tool_use_denies_only_exhausted_and_denied_answers() {
    let deny_cases: Vec<SharedBudgetOutcome> = vec![
        SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Exhausted,
            remaining_usd_cents: 500,
            period_end: "2027-02-01T00:00:00Z".into(),
        },
        denied(SharedBudgetDenyReason::CapExhausted),
        denied(SharedBudgetDenyReason::ConfigInvalid),
        denied(SharedBudgetDenyReason::ClockAnomaly),
    ];
    for outcome in deny_cases {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path(), &config(5_000, ""));
        let fake = Fake::start(state.path(), move |_| reply(outcome.clone()));
        let reason = assert_deny(&pre_tool(state.path(), tool_payload("s", None)));
        assert!(!reason.contains("SECRET"));
        assert_eq!(fake.kinds(), vec!["shared_budget:check".to_string()]);
    }

    let allow_cases: Vec<Answer> = vec![
        reply(SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Available,
            remaining_usd_cents: 5_000,
            period_end: "x".into(),
        }),
        reply(SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Admitted,
            remaining_usd_cents: 5_000,
            period_end: "x".into(),
        }),
        reply(SharedBudgetOutcome::NotConfigured),
        reply(SharedBudgetOutcome::Refused {
            reason: SharedBudgetRefusal::EmptySessionId,
        }),
        reply(allowed(vec![])),
        err("scripted daemon error"),
    ];
    for answer in allow_cases {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path(), &config(5_000, ""));
        let answer = Mutex::new(Some(answer));
        let _fake = Fake::start(state.path(), move |_| {
            answer.lock().unwrap().take().unwrap_or(Answer::Stall)
        });
        assert_allow(&pre_tool(state.path(), tool_payload("s", None)));
    }
}

#[test]
fn pre_tool_use_fails_open_on_a_stalled_daemon_within_the_500ms_budget() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let _fake = Fake::start(state.path(), |_| Answer::Stall);
    let ran = pre_tool(state.path(), tool_payload("s", None));
    assert_allow(&ran);
    assert!(
        ran.elapsed >= Duration::from_millis(400) && ran.elapsed < Duration::from_millis(2_500),
        "expected the hard ~500 ms bound, took {:?}",
        ran.elapsed
    );
}

#[test]
fn pre_tool_use_with_a_budget_file_and_no_daemon_allows_and_spawns_nothing() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let ran = pre_tool(state.path(), tool_payload("s", None));
    assert_allow(&ran);
    assert!(ran.elapsed < Duration::from_secs(2), "{:?}", ran.elapsed);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !state.path().join("daemon.sock").exists(),
        "a daemon was spawned"
    );
    assert!(
        !state.path().join("daemon.pid").exists(),
        "a daemon was spawned"
    );
}

#[test]
fn pre_tool_use_malformed_or_unrelated_payloads_allow_silently() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |_| {
        reply(SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Exhausted,
            remaining_usd_cents: 0,
            period_end: "x".into(),
        })
    });
    for payload in [
        "not json",
        "[\"s\"]",
        r#"{"tool_name":"Bash"}"#,
        r#"{"session_id":"s","hook_event_name":"PostToolUse"}"#,
    ] {
        let ran = run_hook(&["hook", "pre-tool-use"], state.path(), payload, &[]);
        assert_allow(&ran);
    }
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[test]
fn pre_tool_use_subagent_events_carry_their_own_agent_id_and_never_an_invented_one() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |_| {
        reply(SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Available,
            remaining_usd_cents: 1,
            period_end: "x".into(),
        })
    });
    pre_tool(state.path(), tool_payload("sess-1", Some("sub-9")));
    pre_tool(state.path(), tool_payload("sess-1", None));
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0]["shared_budget"]["op"], "check");
    assert_eq!(reqs[0]["shared_budget"]["session_id"], "sess-1");
    assert_eq!(reqs[0]["shared_budget"]["agent_id"], "sub-9");
    assert!(
        reqs[1]["shared_budget"]["agent_id"].is_null(),
        "{}",
        reqs[1]
    );
}

// =====================================================================
// (d) Two concurrent sessions stay separate, end to end.
// =====================================================================

#[test]
fn two_concurrent_sessions_with_different_outcomes_stay_separate_end_to_end() {
    let state = support::DaemonState::new();
    // Fits exactly one 1_000-cent hold: one session is admitted, one refused.
    write_budget(state.path(), &config(1_500, ""));
    let _daemon = Daemon::start(state.path());
    let repo = repo();
    let path: PathBuf = state.path().to_path_buf();
    let cwd: PathBuf = repo.path().to_path_buf();

    let handles: Vec<_> = ["conc-A", "conc-B"]
        .into_iter()
        .map(|session| {
            let (p, c) = (path.clone(), cwd.clone());
            std::thread::spawn(move || (session, prompt(&p, session, &c, &[])))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let blocked: Vec<_> = results
        .iter()
        .filter(|(_, r)| r.json().get("decision").is_some())
        .collect();
    let admitted: Vec<_> = results
        .iter()
        .filter(|(_, r)| r.json().get("decision").is_none())
        .collect();
    assert_eq!(blocked.len(), 1, "exactly one session must be refused");
    assert_eq!(admitted.len(), 1, "exactly one session must be admitted");
    assert_block(&blocked[0].1);
    assert!(outer_context(&admitted[0].1).contains("Preflight complete"));

    // Their tool calls follow their own session's outcome, not each other's.
    assert_allow(&pre_tool(state.path(), tool_payload(admitted[0].0, None)));
    assert_deny(&pre_tool(state.path(), tool_payload(blocked[0].0, None)));
}

// =====================================================================
// (e) Codex has no gate: its route is untouched.
// =====================================================================

#[test]
fn codex_prompt_hook_never_asks_the_shared_budget() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |req| match req {
        Request::SharedBudget { .. } => reply(denied(SharedBudgetDenyReason::CapExhausted)),
        _ => err("scripted"),
    });
    let ran = run_hook(
        &["codex-hook", "user-prompt-submit"],
        state.path(),
        &prompt_payload("codex-s", repo().path()),
        &[],
    );
    assert!(!ran.stdout.contains("decision"), "{}", ran.stdout);
    assert_eq!(fake.kinds(), vec!["preflight".to_string()]);
}

/// Drains a child's pipes so a failed assertion cannot leave it blocked.
#[allow(dead_code)]
fn drain(mut r: impl Read) -> String {
    let mut s = String::new();
    let _ = r.read_to_string(&mut s);
    s
}

// =====================================================================
// Review fixes: host-input refusals fail open, parent-path stat errors,
// retry only on stale-daemon errors, one overall PreToolUse deadline.
// =====================================================================

#[test]
fn unusable_session_ids_with_a_budget_file_never_block_and_send_no_admit() {
    for session in ["", "   ", "\t", "execution-owner-v1:forged"] {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path(), &config(5_000, ""));
        let fake = Fake::start(state.path(), |_| {
            reply(denied(SharedBudgetDenyReason::CapExhausted))
        });
        let ran = prompt(state.path(), session, repo().path(), &[]);
        assert_eq!(ran.code, Some(0));
        assert!(
            !ran.stdout.contains("decision"),
            "{session:?}: {}",
            ran.stdout
        );
        assert!(outer_context(&ran).contains("preflight skipped"));
        assert!(
            fake.requests().is_empty(),
            "{session:?}: {:?}",
            fake.requests()
        );
    }
}

#[test]
fn a_daemon_refusal_of_host_input_fails_open_and_continues_to_preflight() {
    for refusal in [
        SharedBudgetRefusal::EmptySessionId,
        SharedBudgetRefusal::ReservedSessionScope,
        SharedBudgetRefusal::InvalidAgentId,
    ] {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path(), &config(5_000, ""));
        let fake = Fake::start(state.path(), move |req| match req {
            Request::SharedBudget { .. } => reply(SharedBudgetOutcome::Refused { reason: refusal }),
            _ => err("scripted"),
        });
        let ran = prompt(state.path(), "s-refused", repo().path(), &[]);
        assert!(
            !ran.stdout.contains("decision"),
            "{refusal:?}: {}",
            ran.stdout
        );
        assert_eq!(
            fake.kinds(),
            vec!["shared_budget:admit".to_string(), "preflight".to_string()]
        );
    }
}

#[test]
fn an_unsearchable_state_dir_is_not_a_budget_file_and_fails_open() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc_geteuid() } == 0 {
        eprintln!("skipped: running as root ignores directory permissions");
        return;
    }
    // The state dir sits under a parent that cannot be searched (EACCES on a
    // parent path component). The hook itself would chmod the state dir back
    // to 0700, so the unsearchable component must be above it.
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    std::fs::create_dir(&state).unwrap();
    write_budget(&state, &config(5_000, ""));
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let ran = prompt(
        &state,
        "s-eacces",
        repo().path(),
        &[("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")],
    );
    let tool = pre_tool(&state, tool_payload("s", None));
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!ran.stdout.contains("decision"), "{}", ran.stdout);
    assert_allow(&tool);
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[test]
fn an_error_after_a_possible_commit_is_not_retried_so_no_second_hold() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |_| err("ledger busy after commit"));
    let reason = assert_block(&prompt(state.path(), "s-once", repo().path(), &[]));
    assert!(reason.contains("libra-governor daemon run"), "{reason}");
    assert_eq!(fake.kinds(), vec!["shared_budget:admit".to_string()]);
}

#[test]
fn a_stale_daemon_version_error_is_retried_once() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let fake = Fake::start(state.path(), |_| {
        err("protocol version mismatch: daemon speaks 1, client sent 20")
    });
    assert_block(&prompt(state.path(), "s-stale", repo().path(), &[]));
    assert_eq!(fake.kinds().len(), 2, "{:?}", fake.kinds());
}

#[test]
fn pre_tool_use_has_one_overall_deadline_even_against_a_trickling_daemon() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path(), &config(5_000, ""));
    let _fake = Fake::start(state.path(), |_| Answer::Trickle);
    let ran = pre_tool(state.path(), tool_payload("s", None));
    assert_allow(&ran);
    assert!(
        ran.elapsed < Duration::from_millis(1_500),
        "total deadline exceeded: {:?}",
        ran.elapsed
    );
}
