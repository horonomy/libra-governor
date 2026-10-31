//! Bounded wait for the host's transcript before a `Stop` settles
//! (HORO-2246, ADR-0026 *Flush lag*).
//!
//! Claude Code writes its transcript asynchronously. On 2.1.306 the file for
//! a turn (all of its lines, at once) appears about 70 ms after the `Stop`
//! hook is dispatched, so a `Finalize` sent immediately usually finds an
//! absent file (first turn of a session) or no new lines (a resumed session)
//! and the daemon books the flat turn hold instead of the measured spend.
//!
//! Before it sends `Finalize` the hook therefore polls the daemon's
//! read-only `TranscriptReady` probe, for at most [`DEFAULT_CAP`]. Properties
//! this module guarantees:
//!
//! - **No transcript access.** The daemon alone binds, windows and prices a
//!   transcript, so it alone says what "ready" means; the hook only asks.
//! - **Never denies, never fails the host.** Every outcome, including a
//!   deadline, an unreachable daemon, an old daemon or a refused probe, ends
//!   in the unchanged `Finalize`; a turn that never becomes ready settles at
//!   the flat hold exactly as it did before.
//! - **Never occupies the daemon.** The daemon is a single-threaded serial
//!   loop, so each probe is its own short connection that is closed before
//!   the hook sleeps. Waiting happens in this process only.
//! - **Off for everyone else.** No budget file means no probe and no connect
//!   (the unconfigured path is unchanged); Codex has no transcript path here
//!   and is skipped.
//! - **Bounded.** The cap is far below the host's hook timeout;
//!   `LIBRA_GOVERNOR_STOP_FLUSH_WAIT_MS` overrides it (`0` disables the wait,
//!   values above [`MAX_CAP`] are clamped, unparsable values use the default).

use std::path::Path;
use std::time::{Duration, Instant};

use libra_governor_protocol::{Request, Response, SharedBudgetOutcome, SharedBudgetRequest};

use super::{budget_gate, AgentKind};
use crate::client;

/// Total time a `Stop` may wait for the transcript. About 20x the measured
/// lag; a turn still unflushed after this settles at the flat hold.
pub const DEFAULT_CAP: Duration = Duration::from_millis(1_500);
/// Largest accepted override.
pub const MAX_CAP: Duration = Duration::from_millis(5_000);
/// Environment override, in milliseconds.
pub const CAP_ENV: &str = "LIBRA_GOVERNOR_STOP_FLUSH_WAIT_MS";
/// Budget for one probe's connect + write + read. A daemon that cannot
/// answer this quickly is busy; the hook stops waiting rather than queue.
const PROBE_BUDGET: Duration = Duration::from_millis(400);
/// First pause between probes; doubles up to [`MAX_STEP`], which keeps the
/// overshoot past the moment the file lands to a few tens of milliseconds.
const FIRST_STEP: Duration = Duration::from_millis(5);
const MAX_STEP: Duration = Duration::from_millis(25);

/// What one probe said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The transcript is not there yet and waiting may help.
    NotReady,
    /// Stop waiting: ready, or nothing a wait could change (no budget
    /// configured, daemon unreachable or too old, probe refused or slow).
    Done,
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The first probe already said done: nothing was waited for.
    Immediate,
    /// A later probe said done.
    AfterWaiting { probes: u32 },
    /// The cap passed first; the caller proceeds as if no wait existed.
    GaveUp { probes: u32 },
}

/// The cap for an override value (`None` = unset).
pub fn cap_from(value: Option<&str>) -> Duration {
    match value.map(str::trim).map(str::parse::<u64>) {
        Some(Ok(ms)) => Duration::from_millis(ms).min(MAX_CAP),
        _ => DEFAULT_CAP,
    }
}

/// Polls `probe` with a doubling pause until it says done or `elapsed()`
/// reaches `cap`. Pure over its three closures so the schedule is testable
/// with a virtual clock; production passes `thread::sleep` and an `Instant`.
pub fn wait_for_flush(
    cap: Duration,
    mut probe: impl FnMut() -> Probe,
    mut sleep: impl FnMut(Duration),
    mut elapsed: impl FnMut() -> Duration,
) -> Ended {
    let mut probes = 0_u32;
    let mut step = FIRST_STEP;
    loop {
        probes += 1;
        if probe() == Probe::Done {
            return if probes == 1 {
                Ended::Immediate
            } else {
                Ended::AfterWaiting { probes }
            };
        }
        let spent = elapsed();
        if spent >= cap {
            return Ended::GaveUp { probes };
        }
        sleep(step.min(cap - spent));
        step = (step * 2).min(MAX_STEP);
    }
}

fn probe_daemon(socket: &Path, session_id: &str, transcript_path: &str) -> Probe {
    let request = Request::SharedBudget {
        shared_budget: SharedBudgetRequest::TranscriptReady {
            session_id: session_id.to_string(),
            transcript_path: transcript_path.to_string(),
            provider: None,
        },
    };
    // One connection per probe, dropped before the caller sleeps.
    let answer = client::connect_only_with_budget(socket, PROBE_BUDGET)
        .and_then(|stream| client::roundtrip(&stream, request));
    probe_from(answer)
}

/// Maps a probe's round trip to what the hook does next. Only an explicit
/// "not ready" keeps waiting; an error, a timeout, an old daemon's error
/// answer, a refusal, "not configured" or any other answer ends the wait.
fn probe_from(answer: Result<Response, client::ClientError>) -> Probe {
    match answer {
        Ok(Response::SharedBudget(outcome)) => match *outcome {
            SharedBudgetOutcome::TranscriptReadiness { ready: false } => Probe::NotReady,
            _ => Probe::Done,
        },
        _ => Probe::Done,
    }
}

/// Called by `hook stop` before it sends `Finalize`. Returns when the
/// transcript is ready, the cap has passed, or waiting cannot help. Silent
/// apart from one content-free log line when it actually waited.
pub fn await_stop_flush(agent: AgentKind, session_id: &str, transcript_path: Option<&Path>) {
    if agent != AgentKind::ClaudeCode {
        return;
    }
    let Some(transcript_path) = transcript_path else {
        return;
    };
    if budget_gate::session_id_unusable(session_id) || !budget_gate::budget_file_present() {
        return;
    }
    let cap = cap_from(std::env::var(CAP_ENV).ok().as_deref());
    if cap.is_zero() {
        return;
    }
    let Ok(socket) = libra_governor_daemon::paths::socket_path() else {
        return;
    };
    let path = transcript_path.to_string_lossy().into_owned();
    let start = Instant::now();
    let ended = wait_for_flush(
        cap,
        || probe_daemon(&socket, session_id, &path),
        std::thread::sleep,
        || start.elapsed(),
    );
    let waited_ms = start.elapsed().as_millis();
    let line = match ended {
        Ended::Immediate => return,
        Ended::AfterWaiting { probes } => {
            format!("stop: transcript flushed after {waited_ms} ms ({probes} probes)")
        }
        Ended::GaveUp { probes } => format!(
            "stop: transcript not flushed after {waited_ms} ms ({probes} probes); \
             settling without it"
        ),
    };
    if let Ok(log_path) = libra_governor_daemon::paths::log_path() {
        libra_governor_daemon::log::append_line(&log_path, &format!("[claude-code] {line}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A virtual clock: `sleep` advances it, nothing reads wall time.
    struct Virtual {
        now: Cell<Duration>,
        naps: RefCell<Vec<Duration>>,
    }

    impl Virtual {
        fn new() -> Self {
            Self {
                now: Cell::new(Duration::ZERO),
                naps: RefCell::new(Vec::new()),
            }
        }
        fn sleep(&self, d: Duration) {
            self.now.set(self.now.get() + d);
            self.naps.borrow_mut().push(d);
        }
        fn elapsed(&self) -> Duration {
            self.now.get()
        }
    }

    fn run(cap: Duration, answers: &[Probe]) -> (Ended, Virtual, usize) {
        let clock = Virtual::new();
        let calls = Cell::new(0_usize);
        let ended = wait_for_flush(
            cap,
            || {
                let i = calls.get();
                calls.set(i + 1);
                *answers.get(i).unwrap_or(answers.last().unwrap())
            },
            |d| clock.sleep(d),
            || clock.elapsed(),
        );
        (ended, clock, calls.get())
    }

    #[test]
    fn a_transcript_already_there_costs_one_probe_and_no_sleep() {
        let (ended, clock, calls) = run(DEFAULT_CAP, &[Probe::Done]);
        assert_eq!(ended, Ended::Immediate);
        assert_eq!(calls, 1);
        assert!(clock.naps.borrow().is_empty());
    }

    #[test]
    fn a_late_flush_is_caught_by_the_probe_after_it_lands() {
        let (ended, clock, calls) = run(
            DEFAULT_CAP,
            &[
                Probe::NotReady,
                Probe::NotReady,
                Probe::NotReady,
                Probe::Done,
            ],
        );
        assert_eq!(ended, Ended::AfterWaiting { probes: 4 });
        assert_eq!(calls, 4);
        let naps: Vec<u64> = clock
            .naps
            .borrow()
            .iter()
            .map(|d| d.as_millis() as u64)
            .collect();
        assert_eq!(naps, [5, 10, 20], "the pause doubles");
        let (_, long, _) = run(Duration::from_secs(1), &[Probe::NotReady]);
        assert_eq!(
            long.naps.borrow().iter().max().copied(),
            Some(MAX_STEP),
            "and is capped"
        );
    }

    #[test]
    fn a_transcript_that_never_arrives_gives_up_at_the_cap_and_not_a_probe_later() {
        let (ended, clock, _) = run(Duration::from_millis(300), &[Probe::NotReady]);
        let Ended::GaveUp { probes } = ended else {
            panic!("expected GaveUp, got {ended:?}");
        };
        assert!(probes > 3);
        assert_eq!(
            clock.elapsed(),
            Duration::from_millis(300),
            "slept up to the cap exactly"
        );
        assert!(clock.naps.borrow().iter().all(|d| *d <= MAX_STEP));
    }

    #[test]
    fn a_probe_that_takes_the_whole_cap_ends_the_wait_without_another_sleep() {
        let clock = Virtual::new();
        let ended = wait_for_flush(
            Duration::from_millis(100),
            || {
                clock.now.set(Duration::from_millis(100));
                Probe::NotReady
            },
            |d| clock.sleep(d),
            || clock.elapsed(),
        );
        assert_eq!(ended, Ended::GaveUp { probes: 1 });
        assert!(clock.naps.borrow().is_empty());
    }

    #[test]
    fn only_an_explicit_not_ready_keeps_the_hook_waiting() {
        use libra_governor_protocol::SharedBudgetRefusal;
        let sb = |o: SharedBudgetOutcome| Ok(Response::SharedBudget(Box::new(o)));
        assert_eq!(
            probe_from(sb(SharedBudgetOutcome::TranscriptReadiness {
                ready: false
            })),
            Probe::NotReady
        );
        assert_eq!(
            probe_from(sb(SharedBudgetOutcome::TranscriptReadiness { ready: true })),
            Probe::Done
        );
        assert_eq!(
            probe_from(sb(SharedBudgetOutcome::NotConfigured)),
            Probe::Done
        );
        assert_eq!(
            probe_from(sb(SharedBudgetOutcome::Refused {
                reason: SharedBudgetRefusal::EmptySessionId
            })),
            Probe::Done
        );
        // An older daemon answers an unknown request with an error.
        assert_eq!(
            probe_from(Ok(Response::Error {
                message: "unknown".into()
            })),
            Probe::Done
        );
        assert_eq!(probe_from(Ok(Response::Ack)), Probe::Done);
        // Unreachable or slow (a connect/read timeout) is a client error.
        let down = client::ClientError::DaemonUnavailable(
            std::path::PathBuf::from("/x"),
            "timed out".into(),
        );
        assert_eq!(probe_from(Err(down)), Probe::Done);
        assert_eq!(
            probe_from(Err(client::ClientError::ProtocolMismatch(23, 22))),
            Probe::Done,
            "a daemon on another protocol version"
        );
        let slow = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert_eq!(probe_from(Err(slow.into())), Probe::Done);
    }

    #[test]
    fn the_cap_override_is_clamped_and_zero_disables_it() {
        assert_eq!(cap_from(None), DEFAULT_CAP);
        assert_eq!(cap_from(Some("250")), Duration::from_millis(250));
        assert_eq!(cap_from(Some(" 0 ")), Duration::ZERO);
        assert_eq!(cap_from(Some("999999")), MAX_CAP);
        assert_eq!(cap_from(Some("soon")), DEFAULT_CAP);
        assert_eq!(cap_from(Some("-5")), DEFAULT_CAP);
    }
}
