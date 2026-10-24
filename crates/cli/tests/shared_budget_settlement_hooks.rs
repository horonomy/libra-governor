//! HORO-2216 (S7): the `SubagentStop` and `SessionEnd` hooks of the local
//! monthly shared budget (ADR-0026), driven through the real compiled
//! `libra-governor` binary.
//!
//! Evidence class: **TestOwnedLive** -- real hook processes against a real
//! `libra-governor daemon run` (end to end) or an in-test daemon speaking the
//! real wire protocol (exact request shapes, stalls). The transcripts are
//! SYNTHETIC files mirroring the Claude Code 2.1.306 shape; no real Claude
//! Code host runs the hooks.
//!
//! | hook | budget file | daemon | result |
//! |---|---|---|---|
//! | SubagentStop / SessionEnd | absent | any | nothing sent, no connect |
//! | SubagentStop / SessionEnd | present | down / stalled / odd | silent, exit 0 |
//! | SubagentStop | present | up | `ChargeAgentTranscript` |
//! | SessionEnd | present | up | `SettleTail` |

use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_domain::ExecutorKind;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, SharedBudgetOutcome,
    SharedBudgetRequest, SharedBudgetStatus, PROTOCOL_VERSION,
};

mod support;

const HOLD: u64 = 1_000;
const CAP: u64 = 5_000;
/// 1,000,000 input tokens of this model = 300 cents in the pinned table.
const MODEL: &str = "claude-sonnet-4-5";

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

fn run_hook(sub: &[&str], state: &Path, payload: &str) -> Ran {
    run_hook_with(sub, state, payload, true)
}

/// `no_autospawn = false` removes the switch that stops a hook starting a
/// daemon, so a hook that wrongly tried would actually start one.
fn run_hook_with(sub: &[&str], state: &Path, payload: &str, no_autospawn: bool) -> Ran {
    run_hook_bytes(sub, state, payload.as_bytes(), no_autospawn)
}

fn run_hook_bytes(sub: &[&str], state: &Path, payload: &[u8], no_autospawn: bool) -> Ran {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(bin());
    command
        .args(sub)
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_autospawn {
        command.env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1");
    } else {
        command.env_remove("LIBRA_GOVERNOR_NO_AUTOSPAWN");
    }
    let mut child = command.spawn().unwrap();
    let start = Instant::now();
    // The hook may stop reading (oversize input): a closed pipe is fine.
    let _ = child.stdin.take().unwrap().write_all(payload);
    let out = child.wait_with_output().unwrap();
    Ran {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
        elapsed: start.elapsed(),
    }
}

fn write_budget(state: &Path) {
    write_budget_cap(state, CAP);
}

fn write_budget_cap(state: &Path, cap: u64) {
    let path = state.join("shared-budget.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"UTC","turn_hold_usd_cents":{HOLD}}}"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn subagent_stop(state: &Path, payload: serde_json::Value) -> Ran {
    run_hook(&["hook", "subagent-stop"], state, &payload.to_string())
}

fn session_end(state: &Path, payload: serde_json::Value) -> Ran {
    run_hook(&["hook", "session-end"], state, &payload.to_string())
}

fn assert_silent_success(ran: &Ran) {
    assert_eq!(ran.code, Some(0), "{:?}", ran.stdout);
    assert_eq!(ran.stdout, "", "a settlement hook never prints");
    assert_eq!(ran.stderr, "", "a settlement hook never writes stderr");
}

// ---------------------------------------------------------------------
// A scripted daemon on the real wire protocol.
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Answer {
    NotConfigured,
    Stall,
}

struct Fake {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    fn start(state: &Path, answer: Answer) -> Self {
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (reqs, halt) = (requests.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            let mut held: Vec<UnixStream> = Vec::new();
            while !halt.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
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
                reqs.lock()
                    .unwrap()
                    .push(serde_json::to_value(&envelope.request).unwrap());
                match answer {
                    Answer::NotConfigured => {
                        let _ = wire::write_message(
                            &stream,
                            &ResponseEnvelope {
                                protocol_version: PROTOCOL_VERSION,
                                response: Response::SharedBudget(Box::new(
                                    SharedBudgetOutcome::NotConfigured,
                                )),
                            },
                        );
                    }
                    Answer::Stall => held.push(stream),
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

    /// The `shared_budget` object of the single recorded request.
    fn only_shared_budget(&self) -> serde_json::Value {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        requests[0]["shared_budget"].clone()
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

// =====================================================================
// (a) Legacy path: no budget file -> nothing happens.
// =====================================================================

#[test]
fn no_budget_file_means_both_hooks_are_silent_and_never_connect() {
    let state = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(state.path().join("daemon.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();

    let valid_stop = serde_json::json!({
        "session_id": "s", "hook_event_name": "SubagentStop", "agent_id": "a1",
        "agent_transcript_path": "/x/subagents/agent-a1.jsonl"
    });
    let valid_end = serde_json::json!({
        "session_id": "s", "hook_event_name": "SessionEnd", "transcript_path": "/x/s.jsonl"
    });
    assert_silent_success(&subagent_stop(state.path(), valid_stop));
    assert_silent_success(&session_end(state.path(), valid_end));
    // Garbage in the legacy state is equally a no-op.
    assert_silent_success(&run_hook(&["hook", "subagent-stop"], state.path(), "{{{"));
    assert_silent_success(&run_hook(&["hook", "session-end"], state.path(), ""));

    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "a settlement hook contacted the daemon although no budget file exists"
    );
    let mut files: Vec<_> = std::fs::read_dir(state.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        vec!["daemon.sock".to_string()],
        "the legacy path writes nothing (no log, no state)"
    );
}

#[test]
fn a_state_dir_that_is_not_a_directory_fails_open_for_both_hooks() {
    let parent = tempfile::tempdir().unwrap();
    let blocked = parent.path().join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    assert_silent_success(&subagent_stop(
        &blocked,
        serde_json::json!({"session_id": "s", "agent_transcript_path": "/p"}),
    ));
    assert_silent_success(&session_end(
        &blocked,
        serde_json::json!({"session_id": "s", "transcript_path": "/p"}),
    ));
}

// =====================================================================
// (b) Budget file present, scripted daemon: exact requests.
// =====================================================================

#[test]
fn subagent_stop_sends_one_charge_with_the_path_verbatim() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path());
    let fake = Fake::start(state.path(), Answer::NotConfigured);
    let ran = subagent_stop(
        state.path(),
        serde_json::json!({
            "session_id": "sess-1", "hook_event_name": "SubagentStop",
            "agent_id": "a0f3_B-9", "agent_type": "general",
            "agent_transcript_path": "/h/p/sess-1/subagents/agent-a0f3_B-9.jsonl",
            "transcript_path": "/h/p/sess-1.jsonl", "stop_hook_active": false
        }),
    );
    assert_silent_success(&ran);
    assert_eq!(
        fake.only_shared_budget(),
        serde_json::json!({
            "op": "charge_agent_transcript",
            "session_id": "sess-1",
            "agent_id": "a0f3_B-9",
            "agent_transcript_path": "/h/p/sess-1/subagents/agent-a0f3_B-9.jsonl"
        })
    );
}

#[test]
fn subagent_stop_never_invents_or_repairs_an_agent_id() {
    for (label, agent) in [
        ("blank", serde_json::json!("")),
        ("whitespace", serde_json::json!("   ")),
        ("null", serde_json::Value::Null),
        ("number", serde_json::json!(7)),
        ("slash", serde_json::json!("a/b")),
        ("traversal", serde_json::json!("../x")),
        ("padded", serde_json::json!(" a1 ")),
        ("dot", serde_json::json!("a.b")),
    ] {
        let state = tempfile::tempdir().unwrap();
        write_budget(state.path());
        let fake = Fake::start(state.path(), Answer::NotConfigured);
        let ran = subagent_stop(
            state.path(),
            serde_json::json!({
                "session_id": "s", "agent_id": agent, "agent_transcript_path": "/t/a.jsonl"
            }),
        );
        assert_silent_success(&ran);
        let sent = fake.only_shared_budget();
        assert!(
            sent["agent_id"].is_null(),
            "{label}: the id must be absent, got {sent}"
        );
        assert_eq!(sent["agent_transcript_path"], "/t/a.jsonl", "{label}");
    }

    // A missing agent_id key is the same as a blank one.
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path());
    let fake = Fake::start(state.path(), Answer::NotConfigured);
    subagent_stop(
        state.path(),
        serde_json::json!({"session_id": "s", "agent_transcript_path": "/t/a.jsonl"}),
    );
    assert!(fake.only_shared_budget()["agent_id"].is_null());
}

#[test]
fn session_end_sends_one_settle_tail_with_the_path_verbatim() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path());
    let fake = Fake::start(state.path(), Answer::NotConfigured);
    let ran = session_end(
        state.path(),
        serde_json::json!({
            "session_id": "sess-2", "hook_event_name": "SessionEnd", "reason": "clear",
            "transcript_path": "/h/p/sess-2.jsonl", "cwd": "/w"
        }),
    );
    assert_silent_success(&ran);
    assert_eq!(
        fake.only_shared_budget(),
        serde_json::json!({
            "op": "settle_tail",
            "session_id": "sess-2",
            "transcript_path": "/h/p/sess-2.jsonl"
        })
    );
}

#[test]
fn malformed_or_incomplete_payloads_send_nothing_and_exit_zero() {
    let cases: Vec<(&str, String)> = vec![
        ("empty", String::new()),
        ("not json", "not json".into()),
        ("array", r#"["s"]"#.into()),
        ("no session", r#"{"agent_transcript_path":"/p"}"#.into()),
        ("blank session", r#"{"session_id":" ","agent_transcript_path":"/p","transcript_path":"/p"}"#.into()),
        ("no path", r#"{"session_id":"s","agent_id":"a"}"#.into()),
        ("blank path", r#"{"session_id":"s","agent_transcript_path":"","transcript_path":" "}"#.into()),
        (
            "reserved session",
            r#"{"session_id":"execution-owner-v1:x","agent_transcript_path":"/p","transcript_path":"/p"}"#.into(),
        ),
        (
            "other event",
            r#"{"session_id":"s","hook_event_name":"Stop","agent_transcript_path":"/p","transcript_path":"/p"}"#.into(),
        ),
    ];
    for sub in [["hook", "subagent-stop"], ["hook", "session-end"]] {
        for (label, payload) in &cases {
            let state = tempfile::tempdir().unwrap();
            write_budget(state.path());
            let fake = Fake::start(state.path(), Answer::NotConfigured);
            let ran = run_hook(&sub, state.path(), payload);
            assert_silent_success(&ran);
            assert!(
                fake.requests().is_empty(),
                "{sub:?} {label}: sent {:?}",
                fake.requests()
            );
        }
    }
}

#[test]
fn the_daemon_being_down_is_silent_fast_and_starts_nothing() {
    // Autospawn is left ENABLED here: the hook must refuse to start a daemon
    // on its own account. `DaemonState` stops any daemon that did start.
    let state = support::DaemonState::new();
    write_budget(state.path());
    for (sub, payload) in [
        (
            ["hook", "subagent-stop"],
            serde_json::json!({"session_id": "s", "agent_id": "a", "agent_transcript_path": "/t"}),
        ),
        (
            ["hook", "session-end"],
            serde_json::json!({"session_id": "s", "transcript_path": "/t"}),
        ),
    ] {
        let ran = run_hook_with(&sub, state.path(), &payload.to_string(), false);
        assert_silent_success(&ran);
        assert!(ran.elapsed < Duration::from_secs(4), "{:?}", ran.elapsed);
    }
    let mut files: Vec<_> = std::fs::read_dir(state.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with("shared-budget.json"))
        .collect();
    files.sort();
    assert!(
        !files
            .iter()
            .any(|f| f.starts_with("daemon.sock") || f.contains("pid")),
        "a settlement hook must not spawn a daemon: {files:?}"
    );
}

#[test]
fn a_stalled_daemon_is_abandoned_within_the_budget_and_never_blocks() {
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path());
    let _fake = Fake::start(state.path(), Answer::Stall);
    let ran = session_end(
        state.path(),
        serde_json::json!({"session_id": "s", "transcript_path": "/t"}),
    );
    assert_silent_success(&ran);
    assert!(
        ran.elapsed < Duration::from_secs(7),
        "hook outlived its settle budget: {:?}",
        ran.elapsed
    );
}

#[test]
fn without_a_budget_file_stdin_is_never_read() {
    // The caller keeps stdin open and never writes: a hook that read it
    // before checking for the budget file would hang.
    let state = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for sub in [["hook", "subagent-stop"], ["hook", "session-end"]] {
        let mut child = Command::new(bin())
            .args(sub)
            .env("LIBRA_GOVERNOR_STATE_DIR", state.path())
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        assert!(
            status.is_some_and(|s| s.success()),
            "{sub:?} blocked on stdin with no budget file"
        );
    }
}

#[test]
fn oversize_or_non_utf8_stdin_is_ignored_silently_and_quickly() {
    let valid = |pad: &str| {
        format!(
            r#"{{"session_id":"s","agent_transcript_path":"/t/a.jsonl","transcript_path":"/t/s.jsonl","pad":"{pad}"}}"#
        )
    };
    let oversize = valid(&"x".repeat(2 * 1024 * 1024)).into_bytes();
    let mut non_utf8 = valid("").into_bytes();
    non_utf8.splice(5..5, [0xff, 0xfe]);
    for sub in [["hook", "subagent-stop"], ["hook", "session-end"]] {
        for (label, bytes) in [("oversize", &oversize), ("non-utf8", &non_utf8)] {
            let state = tempfile::tempdir().unwrap();
            write_budget(state.path());
            let fake = Fake::start(state.path(), Answer::NotConfigured);
            let ran = run_hook_bytes(&sub, state.path(), bytes, true);
            assert_silent_success(&ran);
            assert!(
                ran.elapsed < Duration::from_secs(3),
                "{sub:?} {label}: {:?}",
                ran.elapsed
            );
            assert!(
                fake.requests().is_empty(),
                "{sub:?} {label}: {:?}",
                fake.requests()
            );
        }
    }
    // The boundary: a payload just under the cap is still processed.
    let state = tempfile::tempdir().unwrap();
    write_budget(state.path());
    let fake = Fake::start(state.path(), Answer::NotConfigured);
    let body = valid(&"x".repeat(1024 * 1024 - 200));
    assert!(body.len() < 1024 * 1024);
    assert_silent_success(&run_hook_bytes(
        &["hook", "session-end"],
        state.path(),
        body.as_bytes(),
        true,
    ));
    assert_eq!(fake.requests().len(), 1);
}

// =====================================================================
// (c) End to end against a REAL daemon.
// =====================================================================

struct Live {
    state: support::DaemonState,
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Live {
    fn start() -> Self {
        Self::start_with_cap(CAP)
    }

    fn start_with_cap(cap: u64) -> Self {
        let mut live = Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            daemon: None,
        };
        write_budget_cap(live.state.path(), cap);
        live.daemon = Some(
            Command::new(bin())
                .args(["daemon", "run"])
                .env("LIBRA_GOVERNOR_STATE_DIR", live.state.path())
                .env("HOME", live.home.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn daemon run"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while try_call(
            &live.sock(),
            Request::SharedBudget {
                shared_budget: SharedBudgetRequest::Status {},
            },
        )
        .is_none()
        {
            let exited = live
                .daemon
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten());
            assert!(exited.is_none(), "daemon exited during startup: {exited:?}");
            assert!(Instant::now() < deadline, "daemon never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
        live
    }

    fn sock(&self) -> PathBuf {
        self.state.path().join("daemon.sock")
    }

    fn status(&self) -> SharedBudgetStatus {
        match try_call(
            &self.sock(),
            Request::SharedBudget {
                shared_budget: SharedBudgetRequest::Status {},
            },
        ) {
            Some(Response::SharedBudget(outcome)) => match *outcome {
                SharedBudgetOutcome::Status(s) => *s,
                other => panic!("expected Status, got {other:?}"),
            },
            other => panic!("expected a SharedBudget response, got {other:?}"),
        }
    }

    fn hook(&self, sub: &[&str], payload: serde_json::Value) -> Ran {
        run_hook(sub, self.state.path(), &payload.to_string())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn try_call(socket: &Path, request: Request) -> Option<Response> {
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
            request,
        },
    )
    .ok()?;
    let envelope: ResponseEnvelope = wire::read_message(BufReader::new(stream)).ok()?;
    Some(envelope.response)
}

/// One synthetic assistant line stamped `secs_from_now` seconds from the
/// real clock.
fn rec(
    secs_from_now: i64,
    id: &str,
    session: &str,
    input_tokens: u64,
    agent: Option<&str>,
) -> String {
    let ts = (OffsetDateTime::now_utc() + time::Duration::seconds(secs_from_now))
        .format(&Rfc3339)
        .unwrap();
    let side = match agent {
        Some(a) => format!(r#""isSidechain":true,"agentId":"{a}","#),
        None => String::new(),
    };
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{session}",{side}"message":{{"id":"{id}","model":"{MODEL}","usage":{{"input_tokens":{input_tokens},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn append_line(path: &Path, line: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(f, "{line}").unwrap();
}

fn pool(st: &SharedBudgetStatus) -> (u64, u64, Option<i64>) {
    (
        st.used_usd_cents,
        st.reserved_usd_cents,
        st.remaining_usd_cents,
    )
}

#[test]
fn subagent_stop_charges_the_subagent_lane_and_session_end_settles_the_tail() {
    let live = Live::start();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let session = "sess-live";
    let agent = "a1b2c3";

    // The session dir mirrors a real host: <project>/<session>.jsonl and
    // <project>/<session>/subagents/agent-<id>.jsonl.
    let project = live.state.path().join("project");
    std::fs::create_dir_all(project.join(session).join("subagents")).unwrap();
    let main = project.join(format!("{session}.jsonl"));
    let sub = project
        .join(session)
        .join("subagents")
        .join(format!("agent-{agent}.jsonl"));

    // 1. The prompt is admitted through the real prompt hook: a hold.
    let admit = live.hook(
        &["hook", "user-prompt-submit"],
        serde_json::json!({"session_id": session, "cwd": repo.path(), "prompt": "do it"}),
    );
    assert_eq!(admit.code, Some(0));
    assert!(!admit.stdout.contains("\"decision\""), "{}", admit.stdout);
    assert_eq!(pool(&live.status()), (0, HOLD, Some(4_000)));

    // 2. The subagent finishes first (600 cents of its own spend); the
    //    SubagentStop hook charges it to its lane.
    write_lines(&main, &[rec(1, "m1", session, 2_000_000, None)]);
    write_lines(&sub, &[rec(2, "s1", session, 2_000_000, Some(agent))]);
    let ran = live.hook(
        &["hook", "subagent-stop"],
        serde_json::json!({
            "session_id": session, "hook_event_name": "SubagentStop",
            "agent_id": agent, "agent_type": "general",
            "agent_transcript_path": sub, "transcript_path": main
        }),
    );
    assert_silent_success(&ran);
    let st = live.status();
    assert_eq!(pool(&st), (600, HOLD, Some(3_400)));
    let lane: Vec<_> = st
        .executors
        .iter()
        .filter(|e| e.executor_kind == ExecutorKind::Agent)
        .collect();
    assert_eq!(lane.len(), 1, "{:?}", st.executors);
    assert_eq!(lane[0].agent_id.as_deref(), Some(agent));
    assert_eq!(lane[0].used_usd_cents, 600);

    // A repeated SubagentStop books nothing.
    live.hook(
        &["hook", "subagent-stop"],
        serde_json::json!({
            "session_id": session, "agent_id": agent, "agent_transcript_path": sub
        }),
    );
    assert_eq!(live.status().used_usd_cents, 600);

    // 3. The turn's Stop settles the hold from the main transcript (600
    //    cents); the sweep sees the already-charged subagent file and adds
    //    nothing for it.
    let stop = live.hook(
        &["hook", "stop"],
        serde_json::json!({"session_id": session, "transcript_path": main}),
    );
    assert_eq!(stop.code, Some(0));
    assert_eq!(pool(&live.status()), (1_200, 0, Some(3_800)));

    // 4. The host flushes the final message after Stop; SessionEnd settles
    //    that tail (300 cents) exactly once.
    append_line(&main, &rec(3, "m2", session, 1_000_000, None));
    let end_payload = serde_json::json!({
        "session_id": session, "hook_event_name": "SessionEnd", "reason": "other",
        "transcript_path": main
    });
    assert_silent_success(&live.hook(&["hook", "session-end"], end_payload.clone()));
    assert_eq!(pool(&live.status()), (1_500, 0, Some(3_500)));
    assert_silent_success(&live.hook(&["hook", "session-end"], end_payload));
    let st = live.status();
    assert_eq!(
        pool(&st),
        (1_500, 0, Some(3_500)),
        "SessionEnd is idempotent"
    );

    // 5. Everything is attributed: executors sum to the pool.
    let sum: u64 = st.executors.iter().map(|e| e.used_usd_cents).sum();
    assert_eq!(sum, st.used_usd_cents, "{:?}", st.executors);
    assert_eq!(
        st.executors
            .iter()
            .filter(|e| e.executor_kind == ExecutorKind::Agent)
            .map(|e| e.used_usd_cents)
            .sum::<u64>(),
        600,
        "the subagent lane holds only the subagent's spend"
    );
}

fn agent_rows(st: &SharedBudgetStatus) -> Vec<(Option<String>, u64)> {
    let mut rows: Vec<_> = st
        .executors
        .iter()
        .filter(|e| e.executor_kind == ExecutorKind::Agent)
        .map(|e| (e.agent_id.clone(), e.used_usd_cents))
        .collect();
    rows.sort();
    rows
}

fn assert_executors_sum_to_pool(st: &SharedBudgetStatus) {
    let sum: u64 = st.executors.iter().map(|e| e.used_usd_cents).sum();
    assert_eq!(sum, st.used_usd_cents, "{:?}", st.executors);
}

fn admit_via_prompt_hook(live: &Live, session: &str) {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let ran = live.hook(
        &["hook", "user-prompt-submit"],
        serde_json::json!({"session_id": session, "cwd": repo.path(), "prompt": "go"}),
    );
    assert_eq!(ran.code, Some(0));
}

#[test]
fn an_id_less_subagent_stop_is_charged_now_under_the_file_name_family_and_the_sweep_adds_nothing() {
    let live = Live::start();
    let session = "sess-noid";
    let project = live.state.path().join("project");
    std::fs::create_dir_all(project.join(session).join("subagents")).unwrap();
    let main = project.join(format!("{session}.jsonl"));
    let sub = project
        .join(session)
        .join("subagents")
        .join("agent-a9z.jsonl");
    admit_via_prompt_hook(&live, session);
    write_lines(&main, &[rec(1, "m1", session, 1_000_000, None)]);
    write_lines(&sub, &[rec(2, "s1", session, 2_000_000, Some("a9z"))]);

    // The host sent no usable id (blank / missing / outside the charset).
    for agent in [
        serde_json::json!(""),
        serde_json::json!("a/z"),
        serde_json::Value::Null,
    ] {
        let ran = live.hook(
            &["hook", "subagent-stop"],
            serde_json::json!({
                "session_id": session, "agent_id": agent, "agent_transcript_path": sub
            }),
        );
        assert_silent_success(&ran);
    }
    let st = live.status();
    assert_eq!(
        st.used_usd_cents, 600,
        "charged at the hook, not left to a later Stop"
    );
    assert_eq!(agent_rows(&st), vec![(Some("a9z".to_string()), 600)]);

    // The turn's Stop sweeps the same file: same family, nothing added twice.
    live.hook(
        &["hook", "stop"],
        serde_json::json!({"session_id": session, "transcript_path": main}),
    );
    let st = live.status();
    assert_eq!(st.used_usd_cents, 900, "{:?}", st.executors);
    assert_eq!(agent_rows(&st), vec![(Some("a9z".to_string()), 600)]);
    assert_executors_sum_to_pool(&st);
}

#[test]
fn a_session_ending_with_no_later_stop_still_has_its_id_less_subagent_charged_once() {
    let live = Live::start();
    let session = "sess-nostop";
    let dir = live
        .state
        .path()
        .join("proj")
        .join(session)
        .join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let main = live
        .state
        .path()
        .join("proj")
        .join(format!("{session}.jsonl"));
    let sub = dir.join("agent-n1.jsonl");
    admit_via_prompt_hook(&live, session);
    write_lines(&main, &[rec(1, "m1", session, 1_000_000, None)]);
    write_lines(&sub, &[rec(2, "s1", session, 1_000_000, Some("n1"))]);

    live.hook(
        &["hook", "subagent-stop"],
        serde_json::json!({"session_id": session, "agent_transcript_path": sub}),
    );
    live.hook(
        &["hook", "session-end"],
        serde_json::json!({"session_id": session, "transcript_path": main}),
    );
    let st = live.status();
    assert_eq!(agent_rows(&st), vec![(Some("n1".to_string()), 300)]);
    assert!(st.used_usd_cents >= 300);
    assert_executors_sum_to_pool(&st);
}

#[test]
fn the_same_file_by_any_path_alias_symlink_or_directory_is_charged_exactly_once() {
    let live = Live::start();
    let session = "sess-alias";
    let root = live.state.path();
    let real_dir = root.join("foreign");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("agent-q1.jsonl");
    write_lines(&real, &[rec(1, "q", session, 1_000_000, Some("q1"))]);
    std::fs::create_dir_all(root.join("links")).unwrap();
    let file_link = root.join("links").join("agent-q1.jsonl");
    std::os::unix::fs::symlink(&real, &file_link).unwrap();
    let dir_alias = root.join("alias");
    std::os::unix::fs::symlink(&real_dir, &dir_alias).unwrap();

    let paths: Vec<PathBuf> = vec![
        real.clone(),
        root.join("foreign")
            .join("..")
            .join("foreign")
            .join("agent-q1.jsonl"),
        file_link,
        dir_alias.join("agent-q1.jsonl"),
        real,
    ];
    for path in paths {
        let ran = live.hook(
            &["hook", "subagent-stop"],
            serde_json::json!({
                "session_id": session, "agent_id": "q1", "agent_transcript_path": path
            }),
        );
        assert_silent_success(&ran);
        let st = live.status();
        assert_eq!(st.used_usd_cents, 300, "{path:?}: {:?}", st.executors);
        assert_eq!(
            agent_rows(&st),
            vec![(Some("q1".to_string()), 300)],
            "{path:?}"
        );
    }
}

#[test]
fn the_sixty_fifth_subagent_file_is_charged_by_its_hook_and_never_twice() {
    let live = Live::start_with_cap(1_000_000);
    let session = "sess-65";
    let project = live.state.path().join("project");
    let dir = project.join(session).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let main = project.join(format!("{session}.jsonl"));
    admit_via_prompt_hook(&live, session);
    write_lines(&main, &[rec(1, "m1", session, 1_000_000, None)]);
    let mut last = PathBuf::new();
    for i in 0..65 {
        let id = format!("a{i:02}");
        last = dir.join(format!("agent-{id}.jsonl"));
        write_lines(
            &last,
            &[rec(2, &format!("s{i}"), session, 1_000_000, Some(&id))],
        );
    }
    // Sorted last, so the sweep (first 64 files) never reaches it.
    live.hook(
        &["hook", "subagent-stop"],
        serde_json::json!({"session_id": session, "agent_transcript_path": last}),
    );
    assert_eq!(live.status().used_usd_cents, 300);
    live.hook(
        &["hook", "stop"],
        serde_json::json!({"session_id": session, "transcript_path": main}),
    );
    let st = live.status();
    assert_eq!(agent_rows(&st).len(), 65, "{:?}", st.executors);
    assert!(agent_rows(&st).iter().all(|(_, used)| *used == 300));
    assert_eq!(
        st.used_usd_cents,
        300 + 65 * 300,
        "main turn + 65 files, once each"
    );
    assert_executors_sum_to_pool(&st);
}

#[test]
fn a_payload_id_that_disagrees_with_the_file_name_never_opens_a_second_family() {
    let live = Live::start();
    let session = "sess-dis";
    let sub = live.state.path().join("agent-a9z.jsonl");
    write_lines(&sub, &[rec(1, "s1", session, 2_000_000, Some("a9z"))]);
    for agent in ["zzz", "a9z", "other"] {
        live.hook(
            &["hook", "subagent-stop"],
            serde_json::json!({
                "session_id": session, "agent_id": agent, "agent_transcript_path": sub
            }),
        );
    }
    let st = live.status();
    assert_eq!(st.used_usd_cents, 600);
    assert_eq!(agent_rows(&st), vec![(Some("a9z".to_string()), 600)]);
    assert!(
        st.executors
            .iter()
            .all(|e| e.executor_kind == ExecutorKind::Agent),
        "no unattributed or second-id row: {:?}",
        st.executors
    );
}

#[test]
fn an_id_less_subagent_stop_for_a_file_the_sweep_cannot_see_is_charged_unattributed() {
    let live = Live::start();
    let session = "sess-odd";
    let odd = live.state.path().join("elsewhere.jsonl");
    write_lines(&odd, &[rec(1, "s1", session, 1_000_000, Some("zz"))]);
    let payload = serde_json::json!({
        "session_id": session, "hook_event_name": "SubagentStop",
        "agent_transcript_path": odd
    });
    assert_silent_success(&live.hook(&["hook", "subagent-stop"], payload.clone()));
    let st = live.status();
    assert_eq!(st.used_usd_cents, 300);
    assert!(st
        .executors
        .iter()
        .any(|e| e.executor_kind == ExecutorKind::Unattributed && e.used_usd_cents == 300));
    // A repeat books nothing.
    live.hook(&["hook", "subagent-stop"], payload);
    assert_eq!(live.status().used_usd_cents, 300);
}

#[test]
fn session_end_for_a_session_never_admitted_bills_nothing() {
    let live = Live::start();
    let main = live.state.path().join("ghost.jsonl");
    write_lines(&main, &[rec(1, "m1", "ghost", 2_000_000, None)]);
    let ran = live.hook(
        &["hook", "session-end"],
        serde_json::json!({"session_id": "ghost", "transcript_path": main}),
    );
    assert_silent_success(&ran);
    assert_eq!(pool(&live.status()).0, 0, "no lane, no billing of history");
}
