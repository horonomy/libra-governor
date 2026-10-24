//! HORO-2246: the `Stop` hook waits (bounded, never denying) for the host's
//! transcript before it asks the daemon to settle the turn.
//!
//! Evidence class: **TestOwnedLive**. The hook is the real compiled binary.
//! The sequencing tests talk to a scripted daemon on the real wire protocol
//! (so the exact order and number of requests is asserted, with no timing
//! race); the end-to-end tests use a real `daemon run` and SYNTHETIC
//! transcripts. No real Claude Code host runs here; the measured lag
//! (~70 ms on 2.1.306) is in ADR-0026, not asserted.
//!
//! No test relies on a sleep for correctness: a hook that must still be
//! waiting is waiting because the file does not exist and its cap is far
//! longer than the test; every wait in the test body is a bounded poll.

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

use libra_governor_protocol::{
    wire, FinalizeOutcome, Request, RequestEnvelope, Response, ResponseEnvelope,
    SharedBudgetOutcome, SharedBudgetRequest, SharedBudgetStatus, PROTOCOL_VERSION,
};

mod support;

const HOLD: u64 = 1_000;
const MODEL: &str = "claude-sonnet-4-5";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn write_budget(state: &Path) {
    let path = state.join("shared-budget.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"schema_version":1,"monthly_cap_usd_cents":100000,"time_zone":"UTC","turn_hold_usd_cents":{HOLD}}}"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn stop_command(state: &Path, home: &Path, cap_ms: Option<&str>) -> Command {
    let mut c = Command::new(bin());
    c.args(["hook", "stop"])
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .env("HOME", home)
        .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
        .env_remove("LIBRA_GOVERNOR_STOP_FLUSH_WAIT_MS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(ms) = cap_ms {
        c.env("LIBRA_GOVERNOR_STOP_FLUSH_WAIT_MS", ms);
    }
    c
}

fn payload(session: &str, transcript: &Path) -> String {
    serde_json::json!({
        "session_id": session,
        "hook_event_name": "Stop",
        "transcript_path": transcript,
        "last_assistant_message": "done",
    })
    .to_string()
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    elapsed: Duration,
}

fn spawn_stop(mut command: Command, body: &str) -> Child {
    let mut child = command.spawn().unwrap();
    let _ = child.stdin.take().unwrap().write_all(body.as_bytes());
    child
}

fn finish(child: Child, started: Instant) -> Ran {
    let out = child.wait_with_output().unwrap();
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        elapsed: started.elapsed(),
    }
}

// ---------------------------------------------------------------------
// A scripted daemon on the real wire protocol.
// ---------------------------------------------------------------------

/// What the scripted daemon answers a `TranscriptReady` probe with.
#[derive(Clone, Copy)]
enum Probe {
    Ready(bool),
    /// A daemon that does not know the request (older protocol).
    Error,
}

struct Scripted {
    ops: Arc<Mutex<Vec<&'static str>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Scripted {
    /// `script[i]` answers the i-th probe; the last entry repeats.
    fn start(state: &Path, script: Vec<Probe>) -> Self {
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ops = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, halt) = (ops.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            let mut probes = 0_usize;
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
                let response = match envelope.request {
                    Request::SharedBudget {
                        shared_budget: SharedBudgetRequest::TranscriptReady { .. },
                    } => {
                        log.lock().unwrap().push("probe");
                        let answer = script[probes.min(script.len() - 1)];
                        probes += 1;
                        match answer {
                            Probe::Ready(ready) => Response::SharedBudget(Box::new(
                                SharedBudgetOutcome::TranscriptReadiness { ready },
                            )),
                            Probe::Error => Response::Error {
                                message: "unknown request".into(),
                            },
                        }
                    }
                    Request::Finalize { .. } => {
                        log.lock().unwrap().push("finalize");
                        Response::Finalize(FinalizeOutcome::NoActiveTask)
                    }
                    _ => {
                        log.lock().unwrap().push("other");
                        Response::Ack
                    }
                };
                let _ = wire::write_message(
                    &stream,
                    &ResponseEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        response,
                    },
                );
            }
        });
        Self {
            ops,
            stop,
            handle: Some(handle),
        }
    }

    fn ops(&self) -> Vec<&'static str> {
        self.ops.lock().unwrap().clone()
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_against(script: Vec<Probe>, cap_ms: Option<&str>, budget: bool) -> (Vec<&'static str>, Ran) {
    let state = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    if budget {
        write_budget(state.path());
    }
    let daemon = Scripted::start(state.path(), script);
    let transcript = state.path().join("never.jsonl");
    let started = Instant::now();
    let child = spawn_stop(
        stop_command(state.path(), home.path(), cap_ms),
        &payload("s1", &transcript),
    );
    let ran = finish(child, started);
    (daemon.ops(), ran)
}

#[test]
fn the_hook_polls_until_ready_then_finalizes_exactly_once() {
    let (ops, ran) = run_against(
        vec![
            Probe::Ready(false),
            Probe::Ready(false),
            Probe::Ready(false),
            Probe::Ready(true),
        ],
        Some("5000"),
        true,
    );
    assert_eq!(
        ops,
        ["probe", "probe", "probe", "probe", "finalize"],
        "four probes, then one Finalize, in that order"
    );
    assert_eq!(ran.code, Some(0));
    assert_eq!(ran.stdout, "", "a Stop hook never prints to stdout");
}

#[test]
fn an_already_flushed_transcript_costs_one_probe_and_no_wait() {
    let (ops, ran) = run_against(vec![Probe::Ready(true)], Some("5000"), true);
    assert_eq!(ops, ["probe", "finalize"]);
    assert_eq!(ran.code, Some(0));
}

#[test]
fn a_transcript_that_never_arrives_still_finalizes_once_after_the_cap_and_never_denies() {
    let (ops, ran) = run_against(vec![Probe::Ready(false)], Some("250"), true);
    assert_eq!(ran.code, Some(0));
    assert_eq!(ran.stdout, "", "no deny, no block output");
    assert_eq!(ops.iter().filter(|o| **o == "finalize").count(), 1);
    assert_eq!(
        ops.last(),
        Some(&"finalize"),
        "Finalize is the last request"
    );
    assert!(
        ops.iter().filter(|o| **o == "probe").count() >= 3,
        "it kept polling until the cap: {ops:?}"
    );
    assert!(ran.elapsed >= Duration::from_millis(250), "waited the cap");
    assert!(ran.elapsed < Duration::from_secs(4), "and not much more");
}

#[test]
fn a_daemon_that_cannot_answer_the_probe_ends_the_wait_at_once() {
    let (ops, ran) = run_against(vec![Probe::Error], Some("5000"), true);
    assert_eq!(
        ops,
        ["probe", "finalize"],
        "an old daemon: one probe, no waiting"
    );
    assert_eq!(ran.code, Some(0));
    assert!(ran.elapsed < Duration::from_secs(3));
}

#[test]
fn zero_disables_the_wait_and_the_probe() {
    let (ops, _) = run_against(vec![Probe::Ready(false)], Some("0"), true);
    assert_eq!(ops, ["finalize"]);
}

#[test]
fn without_a_budget_file_the_stop_hook_never_probes() {
    let (ops, ran) = run_against(vec![Probe::Ready(false)], Some("5000"), false);
    assert_eq!(ops, ["finalize"], "the unconfigured path is unchanged");
    assert_eq!(ran.code, Some(0));
    assert!(ran.elapsed < Duration::from_secs(3));
}

#[test]
fn an_unreachable_daemon_is_not_waited_on() {
    let state = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_budget(state.path());
    let started = Instant::now();
    let child = spawn_stop(
        stop_command(state.path(), home.path(), Some("5000")),
        &payload("s1", &state.path().join("never.jsonl")),
    );
    let ran = finish(child, started);
    assert_eq!(ran.code, Some(0));
    assert!(ran.elapsed < Duration::from_secs(3), "{:?}", ran.elapsed);
}

// ---------------------------------------------------------------------
// Against a real daemon.
// ---------------------------------------------------------------------

struct Live {
    state: support::DaemonState,
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Live {
    fn start() -> Self {
        let live = Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            daemon: None,
        };
        write_budget(live.state.path());
        let mut live = live;
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
        while live.try_rpc(SharedBudgetRequest::Status {}).is_none() {
            assert!(Instant::now() < deadline, "daemon never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
        live
    }

    fn sock(&self) -> PathBuf {
        self.state.path().join("daemon.sock")
    }

    fn try_rpc(&self, request: SharedBudgetRequest) -> Option<SharedBudgetOutcome> {
        let stream = UnixStream::connect(self.sock()).ok()?;
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

    fn admit(&self, session: &str) -> SharedBudgetOutcome {
        self.try_rpc(SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
        })
        .expect("daemon answers")
    }

    fn status(&self) -> SharedBudgetStatus {
        match self.try_rpc(SharedBudgetRequest::Status {}) {
            Some(SharedBudgetOutcome::Status(s)) => *s,
            other => panic!("expected Status, got {other:?}"),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.state.stop();
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// An assistant line stamped now, with 1,000,000 input tokens (300 cents).
fn assistant_line(session: &str, id: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{}","sessionId":"{session}","isSidechain":false,"message":{{"id":"{id}","model":"{MODEL}","usage":{{"input_tokens":1000000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0}}}}}}"#,
        OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
    )
}

/// A pass-through between the hook and the real daemon that counts the
/// connections (one per probe or request), so a test can wait until the hook
/// has provably probed before it acts.
struct Proxy {
    connections: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    fn start(listen: &Path, target: PathBuf) -> Self {
        let listener = UnixListener::bind(listen).unwrap();
        listener.set_nonblocking(true).unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, halt) = (connections.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let Ok((client, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                client.set_nonblocking(false).unwrap();
                let Ok(daemon) = UnixStream::connect(&target) else {
                    continue;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let (mut c_in, mut c_out) = (client.try_clone().unwrap(), client);
                let (mut d_in, mut d_out) = (daemon.try_clone().unwrap(), daemon);
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut c_in, &mut d_out);
                });
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut d_in, &mut c_out);
                });
            }
        });
        Self {
            connections,
            stop,
            handle: Some(handle),
        }
    }

    fn seen(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[test]
fn a_real_daemon_settles_a_late_flushed_turn_at_the_measured_amount_while_other_sessions_proceed() {
    let live = Live::start();
    assert!(matches!(
        live.admit("s1"),
        SharedBudgetOutcome::Allowed { .. }
    ));
    // The hook talks to the daemon through a counting pass-through in its own
    // state dir (which holds the budget file that switches the wait on).
    let hook_state = tempfile::tempdir().unwrap();
    write_budget(hook_state.path());
    let proxy = Proxy::start(&hook_state.path().join("daemon.sock"), live.sock());
    let transcript = live.state.path().join("s1.jsonl");
    let started = Instant::now();
    let mut child = spawn_stop(
        stop_command(hook_state.path(), live.home.path(), Some("5000")),
        &payload("s1", &transcript),
    );

    // Wait (bounded poll) until the hook has demonstrably probed at least
    // once, so the next Admit happens while it is polling and sleeping.
    let deadline = Instant::now() + Duration::from_secs(30);
    while proxy.seen() == 0 {
        assert!(Instant::now() < deadline, "the hook never probed");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        live.admit("s2"),
        SharedBudgetOutcome::Allowed { .. }
    ));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the hook is still waiting for a transcript that does not exist"
    );
    let st = live.status();
    assert_eq!(
        (st.used_usd_cents, st.reserved_usd_cents),
        (0, 2 * HOLD),
        "nothing settled yet; both holds are intact"
    );

    // The host flushes; the hook sees it, settles once, and exits.
    std::fs::write(&transcript, assistant_line("s1", "m1") + "\n").unwrap();
    let ran = finish(child, started);
    assert_eq!(ran.code, Some(0));
    let st = live.status();
    assert_eq!(
        (st.used_usd_cents, st.reserved_usd_cents),
        (300, HOLD),
        "s1 settled at its measured 300, not the 1000 hold; s2's hold is untouched"
    );
    assert_eq!(st.unmeasured_settlements, 0);
}

#[test]
fn a_real_daemon_books_the_flat_hold_when_the_cap_passes_and_a_late_stop_replay_adds_nothing() {
    let live = Live::start();
    live.admit("s1");
    let transcript = live.state.path().join("s1.jsonl");
    let started = Instant::now();
    let child = spawn_stop(
        stop_command(live.state.path(), live.home.path(), Some("200")),
        &payload("s1", &transcript),
    );
    let ran = finish(child, started);
    assert_eq!(ran.code, Some(0));
    let st = live.status();
    assert_eq!((st.used_usd_cents, st.reserved_usd_cents), (HOLD, 0));
    assert_eq!(st.unmeasured_settlements, 1);

    // The line lands after the fact; replaying the Stop books nothing more.
    std::fs::write(&transcript, assistant_line("s1", "m1") + "\n").unwrap();
    let replay = finish(
        spawn_stop(
            stop_command(live.state.path(), live.home.path(), Some("200")),
            &payload("s1", &transcript),
        ),
        Instant::now(),
    );
    assert_eq!(replay.code, Some(0));
    let st = live.status();
    assert_eq!((st.used_usd_cents, st.reserved_usd_cents), (HOLD, 0));
}
