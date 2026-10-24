//! HORO-2197: on the legacy (non-budget) hook route a subagent's tool call
//! carries its parent's `session_id` plus an `agent_id`. These tests drive
//! the real `libra-governor` hook CLI against a real, auto-spawned daemon and
//! read the daemon's own ledger, so they prove where each call was booked:
//! to its `(session, agent)` executor, and never to the parent's counters.
//!
//! The counters are observed two independent ways: the `tool_call_counts`
//! rows themselves (so the per-executor totals can be summed against what was
//! sent), and each executor's execution receipt printed by `hook stop`.
//!
//! Scope: attribution of the legacy per-session counters/tasks only. This is
//! local, synthetic-host evidence -- it does not claim a real Claude Code or
//! Codex subagent was observed.

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use libra_governor_protocol::wire;
use serde_json::{json, Value};

mod support;

const PREFIX: &str = "legacy-executor-v1:";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn hook(state: &std::path::Path, subcommand: &str, payload: &Value) -> Output {
    let mut child = Command::new(bin())
        .args(["hook", subcommand])
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "hooks always exit 0");
    output
}

/// A lane the counters know: the session's main executor, or a derived key
/// decoded back into what it names.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Lane {
    Main(String),
    Agent { session: String, agent: String },
    Unattributed { session: String },
}

fn counters(state: &std::path::Path) -> BTreeMap<Lane, i64> {
    let path = state.join("ledger.sqlite3");
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return BTreeMap::new();
    };
    let Ok(mut statement) = conn.prepare("SELECT session_id, count FROM tool_call_counts") else {
        return BTreeMap::new();
    };
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap()
        .map(|row| {
            let (key, count) = row.unwrap();
            let lane = match key.strip_prefix(PREFIX) {
                None => Lane::Main(key),
                Some(encoded) => {
                    let parts: Vec<Value> = serde_json::from_str(encoded).unwrap();
                    let text = |i: usize| parts[i].as_str().unwrap().to_owned();
                    match parts[0].as_str().unwrap() {
                        // ["agent", provider, session, agent]
                        "agent" => Lane::Agent {
                            session: text(2),
                            agent: text(3),
                        },
                        // ["unattributed", provider, session]
                        "unattributed" => Lane::Unattributed { session: text(2) },
                        other => panic!("unknown lane kind {other}"),
                    }
                }
            };
            (lane, count)
        })
        .collect()
}

fn total(counters: &BTreeMap<Lane, i64>) -> i64 {
    counters.values().sum()
}

/// `hook post-tool-use` is fire-and-forget: the daemon books the call after
/// the hook has already exited, so wait for the ledger to show everything
/// sent before asking for a receipt.
fn wait_for_total(state: &std::path::Path, expected: i64) -> BTreeMap<Lane, i64> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let seen = counters(state);
        if total(&seen) == expected {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "daemon booked {} of {expected} tool calls: {seen:?}",
            total(&seen)
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn prompt(state: &std::path::Path, repo: &std::path::Path, session: &str, agent: Option<&str>) {
    let mut payload = json!({"session_id": session, "cwd": repo, "prompt": "do the work"});
    if let Some(agent) = agent {
        payload["agent_id"] = json!(agent);
    }
    let output = hook(state, "user-prompt-submit", &payload);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Preflight complete"), "{agent:?}: {stdout}");
}

fn tool(state: &std::path::Path, session: &str, agent: Option<&str>, times: usize) {
    for _ in 0..times {
        let mut payload = json!({"session_id": session, "tool_name": "Bash"});
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
        }
        hook(state, "post-tool-use", &payload);
    }
}

fn receipt_tool_calls(state: &std::path::Path, session: &str, agent: Option<&str>) -> String {
    let mut payload = json!({"session_id": session});
    if let Some(agent) = agent {
        payload["agent_id"] = json!(agent);
    }
    let output = hook(state, "stop", &payload);
    let summary = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        summary.contains("Execution Receipt"),
        "{agent:?}: {summary}"
    );
    summary
}

fn wire_request(state: &std::path::Path, request: Value) -> Value {
    let stream = UnixStream::connect(state.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    wire::write_message(
        &stream,
        &json!({"protocol_version": libra_governor_protocol::PROTOCOL_VERSION, "request": request}),
    )
    .unwrap();
    let response: Value = wire::read_message(BufReader::new(&stream)).unwrap();
    response["response"].clone()
}

#[test]
fn each_executor_is_counted_apart_and_the_parent_only_counts_itself() {
    let state = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let s = "parent-session";

    // Parent, two named subagents and one whose agent id is blank all share
    // the parent's session id.
    prompt(state.path(), repo.path(), s, None);
    prompt(state.path(), repo.path(), s, Some("agent-1"));
    prompt(state.path(), repo.path(), s, Some("agent-2"));
    prompt(state.path(), repo.path(), s, Some("  "));

    tool(state.path(), s, None, 2);
    tool(state.path(), s, Some("agent-1"), 3);
    tool(state.path(), s, Some("agent-2"), 5);
    tool(state.path(), s, Some("  "), 7);
    tool(state.path(), s, Some(""), 4);

    // Sum consistency: every call booked exactly once -- 2+3+5+7+4 = 21.
    let seen = wait_for_total(state.path(), 21);
    let expected: BTreeMap<Lane, i64> = [
        (Lane::Main(s.into()), 2),
        (
            Lane::Agent {
                session: s.into(),
                agent: "agent-1".into(),
            },
            3,
        ),
        (
            Lane::Agent {
                session: s.into(),
                agent: "agent-2".into(),
            },
            5,
        ),
        // A blank agent id is an executor we cannot name: one shared
        // "unattributed" lane for the session ("  " and "" alike), never
        // the parent and never a named agent.
        (Lane::Unattributed { session: s.into() }, 11),
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, expected);

    // The receipts agree with the counters, executor by executor.
    let parent = receipt_tool_calls(state.path(), s, None);
    assert!(parent.contains("Tool calls: 2"), "parent: {parent}");
    let one = receipt_tool_calls(state.path(), s, Some("agent-1"));
    assert!(one.contains("Tool calls: 3"), "agent-1: {one}");
    let two = receipt_tool_calls(state.path(), s, Some("agent-2"));
    assert!(two.contains("Tool calls: 5"), "agent-2: {two}");
    let blank = receipt_tool_calls(state.path(), s, Some("  "));
    assert!(blank.contains("Tool calls: 11"), "unattributed: {blank}");
}

#[test]
fn an_old_client_without_agent_id_behaves_exactly_as_before() {
    let state = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let s = "old-client-session";

    prompt(state.path(), repo.path(), s, None);
    // A v21-shaped request: no `agent_id` / `provider` keys at all.
    for _ in 0..3 {
        let ack = wire_request(
            state.path(),
            json!({"kind": "tool_invoked", "session_id": s, "tool_name": "Bash"}),
        );
        assert_eq!(ack["kind"], "ack", "{ack}");
    }
    let seen = counters(state.path());
    assert_eq!(
        seen,
        [(Lane::Main(s.into()), 3)].into_iter().collect(),
        "pre-v22 traffic is booked under the raw session id, byte for byte"
    );
    let finalize = wire_request(
        state.path(),
        json!({"kind": "finalize", "session_id": s, "model": null, "provider": "claude-code",
               "transcript_path": null}),
    );
    assert_eq!(finalize["kind"], "finalize", "{finalize}");
    assert_eq!(finalize["state"], "finalized", "{finalize}");
    assert_eq!(finalize["receipt"]["tool_call_count"], 3, "{finalize}");
}

#[test]
fn a_client_cannot_name_a_derived_lane_as_its_session() {
    let state = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    prompt(state.path(), repo.path(), "any", None);
    for request in [
        json!({"kind": "tool_invoked", "session_id": format!("{PREFIX}[\"agent\",null,\"s\",\"a\"]"),
               "tool_name": "Bash"}),
        json!({"kind": "preflight", "task_hint": "h", "cwd": "/tmp",
               "session_id": format!("{PREFIX}x")}),
        json!({"kind": "finalize", "session_id": format!("{PREFIX}x"), "model": null,
               "provider": null, "transcript_path": null}),
    ] {
        let response = wire_request(state.path(), request.clone());
        assert_eq!(response["kind"], "error", "{request}: {response}");
    }
    assert!(counters(state.path()).is_empty());
}

#[test]
fn a_hook_with_no_usable_session_books_nothing_anywhere() {
    let state = support::DaemonState::new();
    let repo = tempfile::tempdir().unwrap();
    // Start the daemon so a mis-routed event would have somewhere to land.
    prompt(state.path(), repo.path(), "present", None);
    for payload in [
        json!({"tool_name": "Bash", "agent_id": "a-1"}),
        json!({"session_id": "", "tool_name": "Bash"}),
        json!({"session_id": "   ", "tool_name": "Bash", "agent_id": "a-1"}),
        json!({"session_id": 123, "tool_name": "Bash"}),
    ] {
        hook(state.path(), "post-tool-use", &payload);
    }
    // One control call proves the daemon was live and the loop above was
    // not simply too early to observe anything.
    tool(state.path(), "present", None, 1);
    let seen = wait_for_total(state.path(), 1);
    assert_eq!(
        seen,
        [(Lane::Main("present".into()), 1)].into_iter().collect()
    );
}
