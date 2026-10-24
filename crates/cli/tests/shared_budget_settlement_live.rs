//! HORO-2196 (S5): shared-budget SETTLEMENT against a REAL
//! `libra-governor daemon run` process over its real Unix socket:
//! Admit -> Finalize with a synthetic transcript -> Status, the subagent
//! transcript charge, and settlement across a real SIGKILL/restart.
//!
//! Evidence class: **TestOwnedLive**. The transcripts are SYNTHETIC files
//! that mirror the documented Claude Code JSONL shape; their per-line
//! session field (`sessionId`) is UNCONFIRMED against a real host. No real
//! Claude Code host and no hook runtime (slices S6/S7) is involved.
//!
//! Every child is owned by a guard that kills and reaps it on drop, and
//! every wait is a bounded loop.

use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_domain::ExecutorKind;
use libra_governor_protocol::{
    wire, FinalizeOutcome, Request, RequestEnvelope, Response, ResponseEnvelope,
    SharedBudgetOutcome, SharedBudgetRequest, SharedBudgetStatus, PROTOCOL_VERSION,
};

mod support;

const HOLD: u64 = 1_000;
const CAP: u64 = 5_000;
/// 1,000,000 input tokens of this model = 300 cents in the pinned table.
const MODEL: &str = "claude-sonnet-4-5";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

struct Live {
    state: support::DaemonState,
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Live {
    fn new() -> Self {
        let live = Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            daemon: None,
        };
        let path = live.state.path().join("shared-budget.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":1,"monthly_cap_usd_cents":{CAP},"time_zone":"UTC","turn_hold_usd_cents":{HOLD}}}"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        live
    }

    fn sock(&self) -> PathBuf {
        self.state.path().join("daemon.sock")
    }

    fn start(&mut self) {
        assert!(self.daemon.is_none(), "daemon already started");
        self.daemon = Some(
            Command::new(bin())
                .args(["daemon", "run"])
                .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
                .env("HOME", self.home.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn daemon run"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if try_call(
                &self.sock(),
                Request::SharedBudget {
                    shared_budget: SharedBudgetRequest::Status {},
                },
            )
            .is_some()
            {
                return;
            }
            let exited = self
                .daemon
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten());
            assert!(exited.is_none(), "daemon exited during startup: {exited:?}");
            assert!(Instant::now() < deadline, "daemon never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL: no shutdown path runs.
    fn kill9(&mut self) {
        let mut child = self.daemon.take().expect("daemon running");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn call(&self, request: Request) -> Response {
        try_call(&self.sock(), request).expect("daemon answers")
    }

    fn sb(&self, request: SharedBudgetRequest) -> SharedBudgetOutcome {
        match self.call(Request::SharedBudget {
            shared_budget: request,
        }) {
            Response::SharedBudget(outcome) => *outcome,
            other => panic!("expected a SharedBudget response, got {other:?}"),
        }
    }

    fn admit(&self, session: &str) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
        })
    }

    fn finalize(&self, session: &str, transcript: &Path) {
        match self.call(Request::Finalize {
            session_id: session.to_string(),
            model: None,
            provider: Some("claude-code".to_string()),
            transcript_path: Some(transcript.display().to_string()),
        }) {
            Response::Finalize(FinalizeOutcome::NoActiveTask) => {}
            other => panic!("expected Finalize(NoActiveTask), got {other:?}"),
        }
    }

    fn status(&self) -> SharedBudgetStatus {
        match self.sb(SharedBudgetRequest::Status {}) {
            SharedBudgetOutcome::Status(s) => *s,
            other => panic!("expected Status, got {other:?}"),
        }
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
/// real clock, after the hold was admitted.
fn rec(secs_from_now: i64, id: &str, session: &str, input_tokens: u64, sidechain: bool) -> String {
    let ts = (OffsetDateTime::now_utc() + time::Duration::seconds(secs_from_now))
        .format(&Rfc3339)
        .unwrap();
    let side = if sidechain {
        r#""isSidechain":true,"#
    } else {
        ""
    };
    format!(
        r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{session}",{side}"message":{{"id":"{id}","model":"{MODEL}","usage":{{"input_tokens":{input_tokens},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn admit_finalize_status_and_subagent_charge_account_exactly_against_a_real_daemon() {
    let mut live = Live::new();
    live.start();

    assert!(matches!(
        live.admit("s1"),
        SharedBudgetOutcome::Allowed { .. }
    ));
    let st = live.status();
    assert_eq!(
        (
            st.used_usd_cents,
            st.reserved_usd_cents,
            st.remaining_usd_cents
        ),
        (0, HOLD, Some(4_000))
    );

    // The turn: two million input tokens = 600 cents, plus a sidechain
    // record that belongs to the subagent and must not be counted here.
    let parent = live.state.path().join("parent.jsonl");
    write_lines(
        &parent,
        &[
            rec(1, "m1", "s1", 2_000_000, false),
            rec(2, "sub1", "s1", 1_000_000, true),
        ],
    );
    live.finalize("s1", &parent);
    let st = live.status();
    assert_eq!(
        (
            st.used_usd_cents,
            st.reserved_usd_cents,
            st.remaining_usd_cents
        ),
        (600, 0, Some(4_400)),
        "hold released, 600 cents settled, sidechain excluded"
    );

    // A retried Stop changes nothing.
    live.finalize("s1", &parent);
    assert_eq!(live.status().used_usd_cents, 600);

    // The subagent's own transcript: 300 cents, charged once.
    let sub = live.state.path().join("agent-x.jsonl");
    write_lines(&sub, &[rec(2, "sub1", "s1", 1_000_000, true)]);
    let charge = |live: &Live| {
        live.sb(SharedBudgetRequest::ChargeAgentTranscript {
            session_id: "s1".to_string(),
            agent_id: Some("x".to_string()),
            agent_transcript_path: sub.display().to_string(),
        })
    };
    match charge(&live) {
        SharedBudgetOutcome::TranscriptCharged {
            amount_charged_usd_cents,
            cumulative_usd_cents,
            executor_kind,
            usage_known,
            ..
        } => {
            assert_eq!((amount_charged_usd_cents, cumulative_usd_cents), (300, 300));
            assert_eq!(executor_kind, ExecutorKind::Agent);
            assert!(usage_known);
        }
        other => panic!("expected TranscriptCharged, got {other:?}"),
    }
    match charge(&live) {
        SharedBudgetOutcome::TranscriptCharged {
            amount_charged_usd_cents,
            already_charged,
            ..
        } => assert_eq!((amount_charged_usd_cents, already_charged), (0, true)),
        other => panic!("expected TranscriptCharged, got {other:?}"),
    }
    let st = live.status();
    assert_eq!(
        (
            st.used_usd_cents,
            st.reserved_usd_cents,
            st.remaining_usd_cents
        ),
        (900, 0, Some(4_100))
    );
    let sum: u64 = st.executors.iter().map(|e| e.used_usd_cents).sum();
    assert_eq!(sum, st.used_usd_cents, "executors sum to the pool");
}

#[test]
fn a_hold_admitted_before_a_sigkill_is_settled_by_the_stop_after_the_restart() {
    let mut live = Live::new();
    live.start();
    assert!(matches!(
        live.admit("s1"),
        SharedBudgetOutcome::Allowed { .. }
    ));
    let tx = live.state.path().join("s1.jsonl");
    write_lines(&tx, &[rec(1, "m1", "s1", 1_000_000, false)]);

    live.kill9();
    live.start();
    // The reservation survived in the ledger: still held after restart.
    let st = live.status();
    assert_eq!((st.used_usd_cents, st.reserved_usd_cents), (0, HOLD));

    live.finalize("s1", &tx);
    let st = live.status();
    assert_eq!((st.used_usd_cents, st.reserved_usd_cents), (300, 0));
    // The telemetry counters are process memory and start at zero again.
    assert_eq!(st.transcript_binding_mismatches, 0);
}
