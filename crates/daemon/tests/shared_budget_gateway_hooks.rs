//! HORO-2274 (S11f): the hook side of `enforcement = "gateway"` (replace
//! mode) -- `Admit`, `Stop`, `SubagentStop`, `SessionEnd`, the subagent sweep
//! and the readiness probe -- driven through the real request dispatch
//! (`handle_connection` over a socket pair) with an injected clock.
//!
//! In replace mode the gateway is the only writer of spend, so every hook path
//! must book nothing. "Nothing" is asserted as a whole-ledger diff, not as a
//! counter. A hold admitted before a flip to gateway mode still settles by
//! transcript (settlement follows the row, not the mode).
//!
//! Evidence class: **TestOwnedLive** (synthetic clients and SYNTHETIC
//! transcripts that mirror the documented Claude Code JSONL shape).

mod common;

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_daemon::recon::ReconBudget;
use libra_governor_daemon::shared_budget::{
    gateway_key, gateway_orphans_at_start, gateway_reserve, GatewayReserveOutcome,
    GatewayReserveRequest, ManualClock, SharedBudgetRuntime,
};
use libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;
use libra_governor_daemon::DaemonConfig;
use libra_governor_protocol::{
    wire, FinalizeOutcome, Request, RequestEnvelope, Response, ResponseEnvelope,
    SharedBudgetCheckState, SharedBudgetDenyReason, SharedBudgetOutcome, SharedBudgetRefusal,
    SharedBudgetRequest, SharedBudgetStatus, PROTOCOL_VERSION, SHARED_BUDGET_GUARANTEE,
    SHARED_BUDGET_GUARANTEE_GATEWAY,
};

const BASE: &str = "2027-01-15T12:00:00Z";
const SONNET: &str = "claude-sonnet-4-5";
const CAP: u64 = 100_000;
const HOLD: u64 = 1_000;

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

fn at(secs: i64) -> OffsetDateTime {
    t(BASE) + time::Duration::seconds(secs)
}

/// A valid file for the given mode, with one flat override so a gateway
/// reservation has an exact worst case (1000 tool-prompt tokens at 10 cents
/// plus 10 output tokens at 5 cents = 10_050).
fn file(enforcement: &str) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{CAP},"time_zone":"UTC",
        "turn_hold_usd_cents":{HOLD},"enforcement":"{enforcement}",
        "pricing_overrides":{{"flat-model":{{
        "input_usd_per_mtok":100000,"cache_creation_input_usd_per_mtok":100000,
        "cache_read_input_usd_per_mtok":10000,"output_usd_per_mtok":50000}}}}}}"#
    )
}

fn daemon_config(dir: &Path, shared_budget: SharedBudgetRuntime) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        lifetime_scope: libra_governor_domain::LifetimeScope::LifetimeBounded,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority: None,
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        launch_gate: None,
        shared_budget: Arc::new(shared_budget),
    }
}

struct Fx {
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    config: DaemonConfig,
    ledger: libra_governor_ledger::LedgerStore,
}

impl Fx {
    fn new(enforcement: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SHARED_BUDGET_FILE_NAME), file(enforcement)).unwrap();
        let clock = Arc::new(ManualClock::new(t(BASE)));
        let runtime = SharedBudgetRuntime::load(dir.path()).with_clock(clock.clone());
        let config = daemon_config(dir.path(), runtime);
        let ledger = libra_governor_ledger::LedgerStore::open(&config.ledger_path).unwrap();
        Self {
            dir,
            clock,
            config,
            ledger,
        }
    }

    fn gateway() -> Self {
        Self::new("gateway")
    }

    fn call_versioned(&mut self, version: u32, request: Request) -> Response {
        let (client, server) = UnixStream::pair().unwrap();
        wire::write_message(
            &client,
            &RequestEnvelope {
                protocol_version: version,
                request,
            },
        )
        .unwrap();
        let mut current_task = None;
        libra_governor_daemon::handle_connection(
            server,
            &mut self.ledger,
            &mut current_task,
            &self.config,
        )
        .unwrap();
        let envelope: ResponseEnvelope = wire::read_message(BufReader::new(client)).unwrap();
        envelope.response
    }

    fn call(&mut self, request: Request) -> Response {
        self.call_versioned(PROTOCOL_VERSION, request)
    }

    fn sb(&mut self, request: SharedBudgetRequest) -> SharedBudgetOutcome {
        match self.call(Request::SharedBudget {
            shared_budget: request,
        }) {
            Response::SharedBudget(outcome) => *outcome,
            other => panic!("expected a SharedBudget response, got {other:?}"),
        }
    }

    fn admit(&mut self, session: &str) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
            provider: None,
        })
    }

    fn status(&mut self) -> SharedBudgetStatus {
        match self.sb(SharedBudgetRequest::Status {}) {
            SharedBudgetOutcome::Status(s) => *s,
            other => panic!("expected Status, got {other:?}"),
        }
    }

    fn finalize(&mut self, session: &str, transcript: Option<&str>) {
        match self.call(Request::Finalize {
            agent_id: None,
            prompt_id: None,
            session_id: session.to_string(),
            model: None,
            provider: Some("claude-code".to_string()),
            transcript_path: transcript.map(String::from),
        }) {
            Response::Finalize(FinalizeOutcome::NoActiveTask) => {}
            other => panic!("expected Finalize(NoActiveTask), got {other:?}"),
        }
    }

    fn fingerprint(&self) -> String {
        common::ledger_fingerprint(&self.config.ledger_path)
    }

    /// Reserves and returns one gateway hold for `session` (left active).
    fn gateway_hold(&mut self, session: &str, request_id: &str) {
        let key = gateway_key("boot-1", request_id).unwrap();
        let req = GatewayReserveRequest {
            session_id: session,
            model: "flat-model",
            body_bytes: 0,
            max_tokens: Some(10),
            image_count: 0,
            key: &key,
            ttl_secs: 600,
        };
        match gateway_reserve(&mut self.ledger, &self.config.shared_budget, &req).unwrap() {
            GatewayReserveOutcome::Granted(_) => {}
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    fn set_mode(&self, enforcement: &str) {
        std::fs::write(
            self.dir.path().join(SHARED_BUDGET_FILE_NAME),
            file(enforcement),
        )
        .unwrap();
    }
}

fn rec(secs: i64, id: &str, session: &str, agent: Option<&str>, input_millions: u64) -> String {
    use libra_governor_daemon::usage::priced::TRANSCRIPT_SESSION_FIELD;
    let agent = agent
        .map(|a| format!(r#""agentId":"{a}","#))
        .unwrap_or_default();
    format!(
        r#"{{"type":"assistant","timestamp":"{}","{TRANSCRIPT_SESSION_FIELD}":"{session}",{agent}"message":{{"id":"{id}","model":"{SONNET}","usage":{{"input_tokens":{},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#,
        at(secs).format(&Rfc3339).unwrap(),
        input_millions * 1_000_000,
    )
}

/// A main transcript with one million input tokens (300 cents) and, next to
/// it, a subagent file with another million.
fn transcripts(dir: &Path, session: &str) -> (String, String) {
    let main = dir.join(format!("{session}.jsonl"));
    std::fs::write(&main, rec(5, "m1", session, None, 1) + "\n").unwrap();
    let subdir = dir.join(session).join("subagents");
    std::fs::create_dir_all(&subdir).unwrap();
    let sub = subdir.join("agent-a1.jsonl");
    std::fs::write(&sub, rec(6, "s1", session, Some("a1"), 1) + "\n").unwrap();
    (
        main.to_str().unwrap().to_string(),
        sub.to_str().unwrap().to_string(),
    )
}

// ---- Admit ----

#[test]
fn admit_in_gateway_mode_reserves_nothing_and_writes_nothing() {
    let mut fx = Fx::gateway();
    let before = fx.fingerprint();
    for _ in 0..2 {
        match fx.admit("sess-1") {
            SharedBudgetOutcome::GatewayMetered {
                remaining_usd_cents,
                period_end,
            } => {
                assert_eq!(remaining_usd_cents, CAP as i64);
                assert!(period_end.starts_with("2027-02-01"));
            }
            other => panic!("expected GatewayMetered, got {other:?}"),
        }
    }
    assert_eq!(fx.fingerprint(), before, "an Admit wrote to the ledger");
    assert!(
        fx.ledger
            .shared_pool_ids_with_prefix("metered:")
            .unwrap()
            .is_empty(),
        "an Admit must not even create the period's pool"
    );
}

#[test]
fn admit_in_gateway_mode_denies_when_remaining_is_below_the_hold() {
    let mut fx = Fx::gateway();
    fx.gateway_hold("routed", "r1");
    // Book spend so that less than one hold remains: 100000 - 10050 - 89000.
    let booked = fx.sb(SharedBudgetRequest::ChargeAgent {
        session_id: "routed".into(),
        agent_id: None,
        amount_usd_cents: 89_000,
        idempotency_key: "spend".into(),
        charge_source: libra_governor_domain::ChargeSource::SubagentTranscript,
        event_time: None,
        provider: None,
    });
    assert!(matches!(booked, SharedBudgetOutcome::Charged { .. }));
    let before = fx.fingerprint();
    match fx.admit("sess-2") {
        SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents,
            period_end,
            ..
        } => {
            assert_eq!(remaining_usd_cents, Some(950));
            assert!(period_end.is_some());
        }
        other => panic!("expected Denied cap_exhausted, got {other:?}"),
    }
    assert_eq!(fx.fingerprint(), before, "a denial wrote to the ledger");
}

#[test]
fn check_in_gateway_mode_never_reports_a_lane_hold() {
    let mut fx = Fx::gateway();
    match fx.sb(SharedBudgetRequest::Check {
        session_id: "sess-1".into(),
        agent_id: None,
        provider: None,
    }) {
        SharedBudgetOutcome::Checked { state, .. } => {
            assert_eq!(state, SharedBudgetCheckState::Available)
        }
        other => panic!("expected Checked, got {other:?}"),
    }
}

// ---- the charge paths book zero ----

#[test]
fn stop_subagent_stop_session_end_and_the_sweep_book_zero() {
    let mut fx = Fx::gateway();
    // A routed session gives the ledger real content to diff.
    fx.gateway_hold("sess-1", "r1");
    let (main, sub) = transcripts(fx.dir.path(), "sess-1");
    let before = fx.fingerprint();

    // Stop: settle_turn plus the subagent sweep over sess-1/subagents.
    fx.finalize("sess-1", Some(&main));
    assert_eq!(fx.fingerprint(), before, "Stop booked something");

    // SubagentStop.
    let out = fx.sb(SharedBudgetRequest::ChargeAgentTranscript {
        session_id: "sess-1".into(),
        agent_id: Some("a1".into()),
        agent_transcript_path: sub,
    });
    assert_eq!(
        out,
        SharedBudgetOutcome::Refused {
            reason: SharedBudgetRefusal::GatewayMetered
        }
    );
    assert_eq!(fx.fingerprint(), before, "SubagentStop booked something");

    // SessionEnd.
    let out = fx.sb(SharedBudgetRequest::SettleTail {
        session_id: "sess-1".into(),
        transcript_path: main.clone(),
        provider: None,
    });
    assert_eq!(
        out,
        SharedBudgetOutcome::Refused {
            reason: SharedBudgetRefusal::GatewayMetered
        }
    );
    assert_eq!(fx.fingerprint(), before, "SessionEnd booked something");

    // Again: idempotent, still nothing.
    fx.finalize("sess-1", Some(&main));
    assert_eq!(fx.fingerprint(), before);
    let st = fx.status();
    assert_eq!(st.used_usd_cents, 0);
}

#[test]
fn in_hooks_mode_the_same_paths_still_book() {
    let mut fx = Fx::new("hooks");
    let granted = fx.admit("sess-1");
    assert!(matches!(granted, SharedBudgetOutcome::Allowed { .. }));
    let (main, sub) = transcripts(fx.dir.path(), "sess-1");
    fx.finalize("sess-1", Some(&main));
    let st = fx.status();
    assert!(
        st.used_usd_cents >= 300,
        "the Stop and the sweep must still book in hooks mode"
    );
    let _ = sub;
}

// ---- mode flips ----

#[test]
fn a_hold_admitted_before_the_flip_still_settles_by_transcript() {
    let mut fx = Fx::new("hooks");
    let granted = fx.admit("sess-1");
    assert!(matches!(
        granted,
        SharedBudgetOutcome::Allowed {
            hold_usd_cents: HOLD,
            ..
        }
    ));
    let (main, _) = transcripts(fx.dir.path(), "sess-1");
    fx.set_mode("gateway");
    fx.clock.set(at(10));

    fx.finalize("sess-1", Some(&main));
    let st = fx.status();
    assert_eq!(st.enforcement, "gateway");
    assert_eq!(
        (st.used_usd_cents, st.reserved_usd_cents),
        (300, 0),
        "the pre-flip hold settled at the measured transcript spend"
    );

    // A second Stop adds nothing: the hold is final.
    fx.finalize("sess-1", Some(&main));
    assert_eq!(fx.status().used_usd_cents, 300);
}

// ---- unrouted sessions ----

#[test]
fn an_unrouted_session_is_counted_once_and_a_routed_one_is_not() {
    let mut fx = Fx::gateway();
    fx.gateway_hold("routed", "r1");
    let (routed, _) = transcripts(fx.dir.path(), "routed");
    let (bypass, _) = transcripts(fx.dir.path(), "bypass");

    fx.finalize("routed", Some(&routed));
    assert_eq!(fx.status().unrouted_sessions, 0);

    fx.finalize("bypass", Some(&bypass));
    fx.finalize("bypass", Some(&bypass));
    let st = fx.status();
    assert_eq!(st.unrouted_sessions, 1, "distinct sessions, not Stops");
    assert_eq!(st.used_usd_cents, 0, "detection never charges");
}

#[test]
fn hooks_mode_never_counts_unrouted_sessions() {
    let mut fx = Fx::new("hooks");
    let (main, _) = transcripts(fx.dir.path(), "bypass");
    fx.admit("bypass");
    fx.finalize("bypass", Some(&main));
    assert_eq!(fx.status().unrouted_sessions, 0);
}

// ---- readiness ----

#[test]
fn a_subagent_file_is_ready_at_once_in_gateway_mode_only() {
    let mut fx = Fx::gateway();
    let missing = fx
        .dir
        .path()
        .join("s/subagents/agent-a1.jsonl")
        .to_str()
        .unwrap()
        .to_string();
    let probe = |fx: &mut Fx| {
        fx.sb(SharedBudgetRequest::TranscriptReady {
            session_id: "s".into(),
            transcript_path: missing.clone(),
            provider: None,
        })
    };
    assert_eq!(
        probe(&mut fx),
        SharedBudgetOutcome::TranscriptReadiness { ready: true }
    );
    fx.set_mode("hooks");
    fx.clock.set(at(1));
    assert_eq!(
        probe(&mut fx),
        SharedBudgetOutcome::TranscriptReadiness { ready: false },
        "control: hooks mode still waits for a file that does not exist yet"
    );
}

// ---- Status ----

#[test]
fn status_reports_the_mode_its_label_and_the_gateway_counters() {
    let mut fx = Fx::gateway();
    fx.config.gateway_stats.record_truncated_settle();
    fx.config.gateway_stats.record_truncated_settle();
    fx.config.gateway_stats.record_input_bound_violation();
    fx.gateway_hold("routed", "r1");
    fx.gateway_hold("routed", "r2");
    let st = fx.status();
    assert_eq!(st.enforcement, "gateway");
    assert_eq!(st.guarantee, SHARED_BUDGET_GUARANTEE_GATEWAY);
    assert_eq!(st.gateway_in_flight, 2);
    assert_eq!(st.truncated_settles, 2);
    assert_eq!(st.input_bound_violations, 1);

    let mut hooks = Fx::new("hooks");
    let st = hooks.status();
    assert_eq!(st.enforcement, "hooks");
    assert_eq!(st.guarantee, SHARED_BUDGET_GUARANTEE);
    assert_eq!(
        (
            st.gateway_in_flight,
            st.truncated_settles,
            st.input_bound_violations
        ),
        (0, 0, 0)
    );
}

#[test]
fn status_counts_the_orphans_the_startup_sweep_booked() {
    let mut fx = Fx::gateway();
    fx.gateway_hold("routed", "r1");
    // A new process (a new boot id) books the previous process's hold.
    let booked =
        gateway_orphans_at_start(&mut fx.ledger, &fx.config.shared_budget, "boot-2").unwrap();
    assert_eq!(booked.len(), 1);
    let st = fx.status();
    assert_eq!(st.orphans_booked, 1);
    assert_eq!(st.gateway_in_flight, 0);
}

// ---- version skew ----

#[test]
fn a_v24_hook_is_refused_before_it_can_be_admitted_in_gateway_mode() {
    let mut fx = Fx::gateway();
    let before = fx.fingerprint();
    let response = fx.call_versioned(
        PROTOCOL_VERSION - 1,
        Request::SharedBudget {
            shared_budget: SharedBudgetRequest::Admit {
                session_id: "sess-1".into(),
                agent_id: None,
                prompt_id: None,
                provider: None,
            },
        },
    );
    match response {
        Response::Error { message } => {
            assert!(
                message.starts_with("protocol version mismatch"),
                "{message}"
            )
        }
        other => panic!("expected an error, got {other:?}"),
    }
    assert_eq!(fx.fingerprint(), before);
}
