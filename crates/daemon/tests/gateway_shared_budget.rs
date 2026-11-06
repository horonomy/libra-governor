//! HORO-2274 (S11e): the gateway enforcing the local monthly cap per request,
//! against a REAL ledger, the real `LedgerSpendAuthority` and
//! `LedgerSharedAuthority`, and a fake upstream. A manual clock decides every
//! time-dependent result, and prices are chosen so every figure is exact:
//! the test model costs $100/Mtok in, $1000/Mtok out, so a request with a
//! ~120-byte body and `max_tokens` 8000 reserves 812 cents.
//!
//! Property throughout: the granted worst case never exceeds the cap, a
//! request the cap refuses never reaches the provider, and every outcome
//! (including the awkward ones) leaves a row booked at an amount that is
//! never below what the provider could have billed.

mod common;
#[path = "../../gateway/tests/fake_upstream.rs"]
mod fake_upstream;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fake_upstream::*;
use hyper::StatusCode;
use libra_governor_daemon::gateway_authority::{
    open_gateway_ledger, LedgerRequestRecorder, LedgerSharedAuthority, LedgerSpendAuthority,
};
use libra_governor_daemon::shared_budget::{
    gateway_orphans_at_start, ManualClock, SharedBudgetRuntime,
};
use libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;
use libra_governor_domain::{ReservationState, ResourceKind};
use libra_governor_gateway::authority::{
    AuthorityError, SharedBudgetAuthority, SharedDecision, SharedMode, SharedReserveRequest,
    SharedSettled, SharedSettlement, SpendAuthority,
};
use libra_governor_gateway::config::GatewayCredentialMode;
use libra_governor_gateway::proxy::RequestRecorder;
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{wire, Request, RequestEnvelope, ResponseEnvelope, PROTOCOL_VERSION};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const SESSION: &str = "x-claude-code-session-id";
const CANARY: &str = "CANARY-PROMPT-7f3a91";
/// 812 cents for the canary body at `max_tokens` 8000 (see the module docs).
const WORST: u64 = 812;
/// What `cut_mid_output` books: 120 input tokens plus the full 8000
/// `max_tokens` of output, at the test prices.
const TRUNCATED_BOOKING: u64 = 802;

fn file(enforcement: &str, cap: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"UTC",
        "turn_hold_usd_cents":10,"enforcement":"{enforcement}",
        "pricing_overrides":{{"test-model":{{"input_usd_per_mtok":100,
        "cache_creation_input_usd_per_mtok":100,"cache_read_input_usd_per_mtok":10,
        "output_usd_per_mtok":1000}}}}}}"#
    )
}

fn body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "model": "test-model", "max_tokens": 8000, "stream": true,
        "messages": [{"role": "user", "content": CANARY}],
    }))
    .unwrap()
}

struct SharedRow {
    key: String,
    state: String,
    amount: u64,
    settled: Option<u64>,
    usage_known: Option<bool>,
}

struct Fx {
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    rt: Arc<SharedBudgetRuntime>,
    ledger: Arc<Mutex<LedgerStore>>,
    upstream: FakeUpstream,
    tokio: tokio::runtime::Runtime,
}

impl Fx {
    /// A ledger with one preflighted task per session, and `file` as the
    /// shared-budget file (`None` for no file).
    fn new(file: Option<String>, sessions: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(json) = file {
            std::fs::write(dir.path().join(SHARED_BUDGET_FILE_NAME), json).unwrap();
        }
        let clock = Arc::new(ManualClock::new(
            OffsetDateTime::parse("2027-01-15T12:00:00Z", &Rfc3339).unwrap(),
        ));
        let rt = Arc::new(SharedBudgetRuntime::load(dir.path()).with_clock(clock.clone()));
        let config = common::daemon_config(dir.path(), common::generous_policy(), rt.clone());
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
        for session in sessions {
            let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
            wire::write_message(
                &client,
                &RequestEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request: Request::Preflight {
                        agent_id: None,
                        provider: None,
                        task_hint: "add a login endpoint".to_string(),
                        cwd: common::fixture_repo(),
                        session_id: session.to_string(),
                    },
                },
            )
            .unwrap();
            libra_governor_daemon::handle_connection(server, &mut ledger, &mut None, &config)
                .unwrap();
            let _: ResponseEnvelope = wire::read_message(std::io::BufReader::new(client)).unwrap();
        }
        drop(ledger);
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        Self {
            upstream: start_fake_upstream(&tokio),
            ledger: open_gateway_ledger(&config.ledger_path).unwrap(),
            dir,
            clock,
            rt,
            tokio,
        }
    }

    /// Replaces the shared-budget file, as an operator (or a bad write) would.
    fn write_file(&self, contents: &str) {
        std::fs::write(self.dir.path().join(SHARED_BUDGET_FILE_NAME), contents).unwrap();
    }

    fn remove_file(&self) {
        std::fs::remove_file(self.dir.path().join(SHARED_BUDGET_FILE_NAME)).unwrap();
    }

    fn ledger_path(&self) -> PathBuf {
        self.dir.path().join("ledger.sqlite3")
    }

    fn authority(&self, boot_id: &str, governor_held: bool) -> Arc<LedgerSharedAuthority> {
        Arc::new(LedgerSharedAuthority::new(
            Arc::clone(&self.ledger),
            Arc::clone(&self.rt),
            boot_id.to_string(),
            governor_held,
        ))
    }

    /// A real gateway over the real ledger, with `shared` as its shared budget.
    fn gateway(
        &self,
        shared: Arc<dyn SharedBudgetAuthority>,
        credential_mode: GatewayCredentialMode,
    ) -> RunningGateway {
        start_gateway_for(
            &self.upstream,
            GatewayShape {
                resource_kind: ResourceKind::Tokens,
                credential_mode,
                max_request_bytes: libra_governor_gateway::config::DEFAULT_MAX_REQUEST_BYTES,
            },
            Arc::new(LedgerSpendAuthority::new(Arc::clone(&self.ledger)))
                as Arc<dyn SpendAuthority>,
            Arc::new(LedgerRequestRecorder::new(
                Arc::clone(&self.ledger),
                self.dir.path().join("daemon.log"),
            )) as Arc<dyn RequestRecorder>,
            Some(shared),
        )
    }

    fn held_gateway(&self, boot_id: &str) -> RunningGateway {
        self.gateway(
            self.authority(boot_id, true),
            governor_held_fake_credential(),
        )
    }

    fn post(&self, gw: &RunningGateway, session: Option<&str>, scenario: &str) -> TestResponse {
        let mut headers = vec![
            ("x-api-key", gw.token.as_str()),
            (SCENARIO_HEADER, scenario),
        ];
        if let Some(session) = session {
            headers.push((SESSION, session));
        }
        send(
            &self.tokio,
            gw.addr,
            "POST",
            "/v1/messages",
            &headers,
            &body(),
        )
    }

    fn shared_rows(&self) -> Vec<SharedRow> {
        let conn = rusqlite::Connection::open_with_flags(
            self.ledger_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT state, amount, settled_amount, usage_known, idempotency_key
                 FROM shared_pool_reservations ORDER BY created_at, id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(SharedRow {
                state: r.get(0)?,
                amount: r.get(1)?,
                settled: r.get(2)?,
                usage_known: r.get(3)?,
                key: r.get(4)?,
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// The states of every task reservation the gateway took.
    fn task_states(&self, session: &str) -> Vec<ReservationState> {
        let ledger = self.ledger.lock().unwrap();
        let task = ledger.task_id_for_session(session).unwrap().unwrap();
        ledger
            .reservations_for_task(task)
            .unwrap()
            .into_iter()
            .filter(|r| r.idempotency_key.starts_with("gw:"))
            .map(|r| r.state)
            .collect()
    }

    fn remaining(&self, cap: u64) -> i64 {
        let spent: u64 = self
            .shared_rows()
            .iter()
            .map(|r| match r.state.as_str() {
                "settled" => r.settled.unwrap_or(r.amount),
                "active" => r.amount,
                _ => 0,
            })
            .sum();
        cap as i64 - spent as i64
    }
}

/// Starts a request on a background thread and does not wait for it: a
/// stream the test controls. The thread's `send` panics when the gateway is
/// later stopped under it; that is the point, and is swallowed here.
fn start_in_background(gw: &RunningGateway, session: &'static str, scenario: &'static str) {
    let (addr, token) = (gw.addr, gw.token.clone());
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let headers = [
                ("x-api-key", token.as_str()),
                (SESSION, session),
                (SCENARIO_HEADER, scenario),
            ];
            send(&runtime, addr, "POST", "/v1/messages", &headers, &body());
        });
    });
}

/// Fails every settlement, as a ledger write error would.
struct FailingSettle(Arc<LedgerSharedAuthority>);

impl SharedBudgetAuthority for FailingSettle {
    fn mode(&self) -> SharedMode {
        self.0.mode()
    }
    fn reserve(&self, req: SharedReserveRequest<'_>) -> Result<SharedDecision, AuthorityError> {
        self.0.reserve(req)
    }
    fn settle(&self, _: SharedSettlement<'_>) -> Result<SharedSettled, AuthorityError> {
        Err(AuthorityError("injected settle failure".to_string()))
    }
    fn release(
        &self,
        id: libra_governor_domain::SharedPoolReservationId,
    ) -> Result<(), AuthorityError> {
        self.0.release(id)
    }
    fn renew(
        &self,
        id: libra_governor_domain::SharedPoolReservationId,
        ttl_secs: u64,
    ) -> Result<(), AuthorityError> {
        self.0.renew(id, ttl_secs)
    }
}

// ---- 1, 8: the cap refuses before the provider, and the task row goes back ----

#[test]
fn an_exhausted_cap_refuses_before_the_provider_and_releases_the_task_reservation() {
    let fx = Fx::new(Some(file("gateway", WORST - 1)), &["s1"]);
    let gw = fx.held_gateway("boot1");

    let response = fx.post(&gw, Some("s1"), "sse");

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("shared_budget_exceeded"));
    assert!(fx.upstream.received().is_empty(), "the provider was called");
    assert!(fx.shared_rows().is_empty(), "a refusal must book nothing");
    wait_until(
        || fx.task_states("s1") == vec![ReservationState::Released],
        "the compensating release of the task reservation",
    );
}

// ---- 2: concurrency across sessions ----

#[test]
fn concurrent_requests_never_hold_more_than_the_cap() {
    let sessions = ["s1", "s2", "s3", "s4"];
    let cap = 5_000;
    let fx = Fx::new(Some(file("gateway", cap)), &sessions);
    let gw = fx.held_gateway("boot1");

    let responses: Vec<TestResponse> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let (fx, gw) = (&fx, &gw);
                scope.spawn(move || fx.post(gw, Some(sessions[i % 4]), "slow_stream"))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let granted = responses
        .iter()
        .filter(|r| r.status == StatusCode::OK)
        .count();
    let refused: Vec<_> = responses
        .iter()
        .filter(|r| r.decision() == Some("shared_budget_exceeded"))
        .collect();
    assert_eq!(granted + refused.len(), 16);
    assert!(
        !refused.is_empty(),
        "the cap was sized so some requests cannot fit"
    );
    assert!(granted > 0);

    let rows = fx.shared_rows();
    assert_eq!(
        rows.len(),
        granted,
        "one row per grant and none for a refusal"
    );
    assert!(rows.iter().map(|r| r.amount).sum::<u64>() <= cap);
    wait_until(
        || fx.shared_rows().iter().all(|r| r.state == "settled"),
        "every granted request to settle",
    );
    assert!(
        fx.remaining(cap) >= 0,
        "settled spend stayed within the cap"
    );
    for session in sessions {
        wait_until(
            || {
                fx.task_states(session)
                    .iter()
                    .all(|s| *s != ReservationState::Active)
            },
            "no task reservation left active",
        );
    }
}

// ---- 3: upstream failures release both rows ----

#[test]
fn upstream_5xx_and_529_release_both_rows() {
    for scenario in ["server_error", "overloaded_529"] {
        let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
        let gw = fx.held_gateway("boot1");
        fx.post(&gw, Some("s1"), scenario);
        wait_until(
            || {
                fx.shared_rows().iter().all(|r| r.state == "released")
                    && !fx.shared_rows().is_empty()
            },
            "the shared release",
        );
        wait_until(
            || fx.task_states("s1") == vec![ReservationState::Released],
            "the task release",
        );
        assert_eq!(fx.remaining(5_000), 5_000, "{scenario}: nothing stays held");
    }
}

// ---- 4: a cut-off stream books the S11b amount, marked unknown ----

#[test]
fn a_stream_cut_off_mid_output_books_the_s11b_amount() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    fx.post(&gw, Some("s1"), "cut_mid_output");
    wait_until(
        || fx.shared_rows().iter().all(|r| r.state == "settled") && !fx.shared_rows().is_empty(),
        "the settlement",
    );
    let row = &fx.shared_rows()[0];
    assert_eq!(row.amount, WORST);
    assert_eq!(row.settled, Some(TRUNCATED_BOOKING));
    assert_eq!(row.usage_known, Some(false));
}

#[test]
fn a_clean_stream_books_the_measured_amount_marked_known() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    fx.post(&gw, Some("s1"), "sse");
    wait_until(
        || fx.shared_rows().iter().all(|r| r.state == "settled") && !fx.shared_rows().is_empty(),
        "the settlement",
    );
    let row = &fx.shared_rows()[0];
    // 120 input at $100/Mtok and 30 output at $1000/Mtok: 1.2 + 3.0 cents, rounded up.
    assert_eq!((row.settled, row.usage_known), (Some(5), Some(true)));
}

// ---- 5: a settle that fails leaves the row to the expiry sweep ----

#[test]
fn a_failed_settle_leaves_the_row_active_until_the_sweep_books_it() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.gateway(
        Arc::new(FailingSettle(fx.authority("boot1", true))),
        governor_held_fake_credential(),
    );
    fx.post(&gw, Some("s1"), "sse");
    wait_until(
        || fx.task_states("s1") == vec![ReservationState::Settled],
        "the task settlement that follows the failed shared one",
    );
    assert_eq!(fx.shared_rows()[0].state, "active");

    fx.clock
        .set(OffsetDateTime::parse("2027-01-15T12:15:00Z", &Rfc3339).unwrap());
    fx.ledger
        .lock()
        .unwrap()
        .expire_stale_shared_pool_reservations(fx.rt.now())
        .unwrap();
    let row = &fx.shared_rows()[0];
    assert_eq!(
        (row.state.as_str(), row.settled, row.usage_known),
        ("settled", Some(WORST), Some(false))
    );
}

// ---- 6: a restart books what the dead process held, once ----

#[test]
fn a_new_boot_books_the_previous_boots_active_hold_exactly_once() {
    let cap = 5_000;
    let fx = Fx::new(Some(file("gateway", cap)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    start_in_background(&gw, "s1", "stall_after_start");
    wait_until(
        || fx.shared_rows().iter().any(|r| r.state == "active"),
        "the hold of the in-flight stream",
    );
    assert_eq!(fx.remaining(cap), cap as i64 - WORST as i64);

    drop(gw);
    let booked = gateway_orphans_at_start(&mut fx.ledger.lock().unwrap(), &fx.rt, "boot2").unwrap();
    assert_eq!(booked.len(), 1);
    let row = &fx.shared_rows()[0];
    assert_eq!(
        (row.state.as_str(), row.settled, row.usage_known),
        ("settled", Some(WORST), Some(false))
    );
    assert!(
        fx.remaining(cap) < cap as i64,
        "never above the pre-request value"
    );
    assert!(
        gateway_orphans_at_start(&mut fx.ledger.lock().unwrap(), &fx.rt, "boot2")
            .unwrap()
            .is_empty()
    );

    // The new process works, and its own hold is not swept.
    let gw2 = fx.held_gateway("boot2");
    assert_eq!(fx.post(&gw2, Some("s1"), "sse").status, StatusCode::OK);
}

// ---- 7: no session, no reservation ----

#[test]
fn a_missing_session_header_reserves_nothing() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    let response = fx.post(&gw, None, "sse");
    assert_eq!(response.decision(), Some("task_unbound"));
    assert!(fx.shared_rows().is_empty());
    assert!(fx.upstream.received().is_empty());
}

// ---- 9: nothing from the request is stored ----

#[test]
fn no_prompt_token_or_credential_reaches_the_ledger() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    fx.post(&gw, Some("s1"), "sse");
    wait_until(
        || fx.shared_rows().iter().all(|r| r.state == "settled") && !fx.shared_rows().is_empty(),
        "the settlement",
    );
    let everything = common::ledger_fingerprint(&fx.ledger_path());
    // Failure messages name the leaked value's kind, never the value itself.
    for (kind, secret) in [
        ("prompt canary", CANARY),
        ("gateway token", gw.token.as_str()),
        ("upstream key", FAKE_UPSTREAM_KEY),
    ] {
        assert!(!everything.contains(secret), "{kind} reached the ledger");
    }
}

// ---- dark: the mode is opt-in ----

#[test]
fn without_enforcement_gateway_the_gateway_books_nothing_shared() {
    for file in [None, Some(file("hooks", 5_000))] {
        let fx = Fx::new(file, &["s1"]);
        let gw = fx.held_gateway("boot1");
        assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
        assert!(fx.shared_rows().is_empty());
    }
}

#[test]
fn a_pass_through_gateway_cannot_honour_enforcement_gateway_and_refuses() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.gateway(
        fx.authority("boot1", false),
        GatewayCredentialMode::PassThroughSubscription,
    );
    let response = fx.post(&gw, Some("s1"), "sse");
    assert_eq!(response.decision(), Some("shared_budget_unavailable"));
    assert!(fx.upstream.received().is_empty());
    assert!(fx.shared_rows().is_empty());
    assert!(
        fx.task_states("s1").is_empty(),
        "no task reservation either"
    );
}

// ---- an unreadable file never silently lifts a cap that was in force ----

const CORRUPT: &str = "{ truncated";

fn assert_refused_unavailable(fx: &Fx, gw: &RunningGateway, upstream_calls: usize) {
    let response = fx.post(gw, Some("s1"), "sse");
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("shared_budget_unavailable"));
    assert_eq!(
        fx.upstream.received().len(),
        upstream_calls,
        "a refused request reached the provider"
    );
}

#[test]
fn a_file_corrupted_while_gateway_enforcement_is_in_force_refuses_until_restored() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
    assert_eq!(fx.upstream.received().len(), 1);

    fx.write_file(CORRUPT);
    assert_refused_unavailable(&fx, &gw, 1);
    assert_eq!(fx.shared_rows().len(), 1, "the refusal booked nothing");

    fx.write_file(&file("gateway", 5_000));
    assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
    assert_eq!(fx.shared_rows().len(), 2, "enforcement resumed");
}

#[test]
fn a_file_corrupted_before_any_request_still_refuses_when_it_started_gateway_enforced() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    fx.write_file(CORRUPT);
    assert_refused_unavailable(&fx, &gw, 0);
}

#[test]
fn an_unreadable_file_refuses_a_governor_held_gateway_even_if_never_gateway_enforced() {
    let fx = Fx::new(Some(CORRUPT.to_string()), &["s1"]);
    let gw = fx.held_gateway("boot1");
    assert_refused_unavailable(&fx, &gw, 0);
    assert!(fx.shared_rows().is_empty());

    // A hooks-mode file stands the gateway down; breaking it refuses again.
    fx.write_file(&file("hooks", 5_000));
    assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
    fx.write_file(CORRUPT);
    assert_refused_unavailable(&fx, &gw, 1);
    assert!(fx.shared_rows().is_empty());
}

#[test]
fn a_restart_over_a_corrupt_file_refuses_rather_than_forwarding_uncapped() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    fx.write_file(CORRUPT);
    // A new daemon process: a fresh runtime and authority that never saw the
    // file valid.
    let restarted = Arc::new(SharedBudgetRuntime::load(fx.dir.path()).with_clock(fx.clock.clone()));
    let authority = Arc::new(LedgerSharedAuthority::new(
        Arc::clone(&fx.ledger),
        restarted,
        "boot2".to_string(),
        true,
    ));
    let gw = fx.gateway(authority, governor_held_fake_credential());
    assert_refused_unavailable(&fx, &gw, 0);
}

#[test]
fn a_pass_through_gateway_is_not_stopped_by_a_file_that_was_never_gateway_enforced() {
    let fx = Fx::new(Some(CORRUPT.to_string()), &["s1"]);
    let gw = fx.gateway(
        fx.authority("boot1", false),
        GatewayCredentialMode::PassThroughSubscription,
    );
    assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
}

#[test]
fn removing_the_file_removes_the_cap() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let gw = fx.held_gateway("boot1");
    assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
    fx.remove_file();
    assert_eq!(fx.post(&gw, Some("s1"), "sse").status, StatusCode::OK);
    assert_eq!(fx.shared_rows().len(), 1, "no new row without a file");
}

// ---- the daemon's start sequence ----

#[test]
fn daemon_start_books_the_previous_boots_hold_before_the_gateway_takes_requests() {
    let cap = 5_000;
    let fx = Fx::new(Some(file("gateway", cap)), &["s1"]);
    // A hold left by a process that died.
    let key = libra_governor_daemon::shared_budget::gateway_key("deadboot", "r1").unwrap();
    libra_governor_daemon::shared_budget::gateway_reserve(
        &mut fx.ledger.lock().unwrap(),
        &fx.rt,
        &libra_governor_daemon::shared_budget::GatewayReserveRequest {
            session_id: "s1",
            model: "test-model",
            body_bytes: 100,
            max_tokens: Some(8000),
            image_count: 0,
            fast_requested: false,
            key: &key,
            ttl_secs: 600,
        },
    )
    .unwrap();
    assert_eq!(fx.shared_rows()[0].state, "active");

    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let token_path = fx.dir.path().join("gateway.token");
    let mut config = common::daemon_config(fx.dir.path(), common::generous_policy(), fx.rt.clone());
    config.gateway = Some(harness_gateway_config(
        addr,
        token_path.clone(),
        fx.upstream.addr.port(),
        governor_held_fake_credential(),
        libra_governor_gateway::config::DEFAULT_MAX_REQUEST_BYTES,
    ));
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();

    let started = libra_governor_daemon::begin_gateway(&mut ledger, &config);

    // Booked by the time the start returns, i.e. before any request.
    let rows = fx.shared_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].state.as_str(), rows[0].settled, rows[0].usage_known),
        ("settled", Some(rows[0].amount), Some(false))
    );
    assert_eq!(started.boot_id.len(), 32);
    assert!(started.boot_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(started.handle.is_some(), "the gateway started");

    // The running gateway keys its holds with that same boot id.
    let token =
        libra_governor_gateway::credential::LocalCapabilityToken::load_or_create(&token_path)
            .unwrap()
            .expose_for_agent()
            .to_string();
    wait_until(
        || {
            let headers = [
                ("x-api-key", token.as_str()),
                (SESSION, "s1"),
                (SCENARIO_HEADER, "sse"),
            ];
            let runtime = &fx.tokio;
            // The listener may not be up yet; retry until the gateway answers.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                send(runtime, addr, "POST", "/v1/messages", &headers, &body()).status
                    == StatusCode::OK
            }))
            .unwrap_or(false)
        },
        "the started gateway to serve a request",
    );
    let prefix = format!("gw:{}:", started.boot_id);
    assert!(
        fx.shared_rows().iter().any(|r| r.key.starts_with(&prefix)),
        "the new hold is keyed with the daemon's boot id"
    );
}

// ---- the hook side follows the gateway that is really running (S11f) ----

fn admit_outcome(
    ledger: &mut LedgerStore,
    rt: &libra_governor_daemon::shared_budget::SharedBudgetRuntime,
) -> libra_governor_protocol::SharedBudgetOutcome {
    libra_governor_daemon::shared_budget::admit(ledger, rt, "s1", None, None, None).unwrap()
}

fn is_gateway_unavailable(o: &libra_governor_protocol::SharedBudgetOutcome) -> bool {
    matches!(
        o,
        libra_governor_protocol::SharedBudgetOutcome::Denied {
            reason: libra_governor_protocol::SharedBudgetDenyReason::GatewayUnavailable,
            ..
        }
    )
}

fn lifecycle_config(
    fx: &Fx,
    addr: std::net::SocketAddr,
    credential: GatewayCredentialMode,
) -> libra_governor_daemon::DaemonConfig {
    let mut config = common::daemon_config(fx.dir.path(), common::generous_policy(), fx.rt.clone());
    config.gateway = Some(harness_gateway_config(
        addr,
        fx.dir.path().join("gateway.token"),
        fx.upstream.addr.port(),
        credential,
        libra_governor_gateway::config::DEFAULT_MAX_REQUEST_BYTES,
    ));
    config
}

fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[test]
fn a_gateway_that_failed_to_start_leaves_admit_denying() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let failing = GatewayCredentialMode::GovernorHeld {
        credential: libra_governor_gateway::credential::CredentialCommand::new(
            "/bin/sh",
            vec!["-c".to_string(), "exit 1".to_string()],
        ),
    };
    let config = lifecycle_config(&fx, free_addr(), failing);
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let started = libra_governor_daemon::begin_gateway(&mut ledger, &config);
    assert!(started.handle.is_none(), "the start failed");

    let before = common::ledger_fingerprint(&config.ledger_path);
    assert!(is_gateway_unavailable(&admit_outcome(&mut ledger, &fx.rt)));
    assert_eq!(common::ledger_fingerprint(&config.ledger_path), before);
}

#[test]
fn a_running_governor_held_gateway_lets_admit_through() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let config = lifecycle_config(&fx, free_addr(), governor_held_fake_credential());
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let started = libra_governor_daemon::begin_gateway(&mut ledger, &config);
    assert!(started.handle.is_some());
    assert!(matches!(
        admit_outcome(&mut ledger, &fx.rt),
        libra_governor_protocol::SharedBudgetOutcome::GatewayMetered { .. }
    ));
}

#[test]
fn a_gateway_that_cannot_bind_flips_admit_to_denying() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = lifecycle_config(
        &fx,
        occupied.local_addr().unwrap(),
        governor_held_fake_credential(),
    );
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let _started = libra_governor_daemon::begin_gateway(&mut ledger, &config);
    let ledger = std::cell::RefCell::new(ledger);
    wait_until(
        || is_gateway_unavailable(&admit_outcome(&mut ledger.borrow_mut(), &fx.rt)),
        "the failed bind to clear the running state",
    );
    drop(occupied);
}

#[test]
fn a_gateway_that_stops_flips_admit_to_denying() {
    let fx = Fx::new(Some(file("gateway", 5_000)), &["s1"]);
    let config = lifecycle_config(&fx, free_addr(), governor_held_fake_credential());
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let started = libra_governor_daemon::begin_gateway(&mut ledger, &config);
    assert!(matches!(
        admit_outcome(&mut ledger, &fx.rt),
        libra_governor_protocol::SharedBudgetOutcome::GatewayMetered { .. }
    ));
    drop(started);
    let ledger = std::cell::RefCell::new(ledger);
    wait_until(
        || is_gateway_unavailable(&admit_outcome(&mut ledger.borrow_mut(), &fx.rt)),
        "the stopped gateway to clear the running state",
    );
}
