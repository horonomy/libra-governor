//! Hook-side runtime of shared-budget settlement (HORO-2216, ADR-0026 S7):
//! `SubagentStop` -> `ChargeAgentTranscript`, `SessionEnd` -> `SettleTail`.
//!
//! Like [`super::budget_gate`] this holds NO policy. The hook only parses the
//! host payload, forwards the transcript path verbatim and lets the daemon
//! read, bind (session and agent), price, de-duplicate and charge. It never
//! opens a transcript, never computes an amount and never blocks: every
//! failure here (no budget file, daemon down, timeout, malformed payload,
//! missing path) is silent and exits 0. Only prompt admission denies.
//!
//! | Budget file | Payload | Result |
//! |---|---|---|
//! | absent | any | nothing sent, no connect |
//! | present | unusable session / missing path / malformed | nothing sent |
//! | present | usable, daemon unreachable or slow | nothing printed |
//! | present | usable | one request, outcome logged only |
//!
//! Identity contract: a blank id is absent, an agent id is never invented,
//! and an agent id outside `[A-Za-z0-9_-]` is not forwarded as an id (the
//! charge is then booked unattributed, keyed by its path, rather than
//! dropped or attributed to a guess).

use std::path::Path;
use std::time::Duration;

use libra_governor_protocol::{
    Request, Response, SharedBudgetOutcome, SharedBudgetRefusal, SharedBudgetRequest,
};
use serde_json::Value;

use super::budget_gate;
use super::AgentKind;
use crate::client;

/// Wall-clock budget for the whole connect + write + read round trip. The
/// installed hook entries ask Claude Code for a 10 s window; this stays well
/// inside it. A slow daemon is abandoned, never waited on past this.
pub const SETTLE_BUDGET: Duration = Duration::from_secs(6);

/// A `SubagentStop` payload reduced to what the charge needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStop {
    pub session_id: String,
    /// `None` when the host sent none, a blank one, or one outside
    /// `[A-Za-z0-9_-]` (see module docs).
    pub agent_id: Option<String>,
    /// Relayed verbatim; the daemon decides whether it is acceptable.
    pub agent_transcript_path: Option<String>,
    /// The host sent an agent id that was dropped for its charset.
    pub agent_id_rejected: bool,
}

/// A `SessionEnd` payload reduced to what the tail settlement needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    pub session_id: String,
    pub transcript_path: Option<String>,
}

/// Why a payload produced nothing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Not JSON, not an object, or no usable `session_id`.
    Malformed(String),
    /// A different `hook_event_name` than this entry point handles.
    OtherEvent(String),
}

fn object(raw: &str, expected_event: &str) -> Result<serde_json::Map<String, Value>, Skip> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| Skip::Malformed(format!("not JSON: {e}")))?;
    let Value::Object(map) = value else {
        return Err(Skip::Malformed("payload is not a JSON object".into()));
    };
    if let Some(name) = map.get("hook_event_name").and_then(Value::as_str) {
        if name != expected_event {
            return Err(Skip::OtherEvent(name.to_string()));
        }
    }
    Ok(map)
}

/// A string field that is present, a JSON string and not blank. Anything
/// else is absent: never an error, never a default.
fn non_blank(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

fn session_id(map: &serde_json::Map<String, Value>) -> Result<String, Skip> {
    non_blank(map, "session_id")
        .ok_or_else(|| Skip::Malformed("missing or blank session_id".into()))
}

/// The charset the daemon's own sweep accepts for an agent id.
pub fn agent_id_charset_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn parse_subagent_stop(raw: &str) -> Result<SubagentStop, Skip> {
    let map = object(raw, "SubagentStop")?;
    let session_id = session_id(&map)?;
    let (agent_id, agent_id_rejected) = match non_blank(&map, "agent_id") {
        Some(id) if agent_id_charset_ok(&id) => (Some(id), false),
        Some(_) => (None, true),
        None => (None, false),
    };
    Ok(SubagentStop {
        session_id,
        agent_id,
        agent_transcript_path: non_blank(&map, "agent_transcript_path"),
        agent_id_rejected,
    })
}

pub fn parse_session_end(raw: &str) -> Result<SessionEnd, Skip> {
    let map = object(raw, "SessionEnd")?;
    Ok(SessionEnd {
        session_id: session_id(&map)?,
        transcript_path: non_blank(&map, "transcript_path"),
    })
}

/// The request for a parsed `SubagentStop`, or why none is sent.
pub fn charge_request(stop: &SubagentStop) -> Result<SharedBudgetRequest, &'static str> {
    if budget_gate::session_id_unusable(&stop.session_id) {
        return Err("unusable session id");
    }
    let Some(path) = stop.agent_transcript_path.clone() else {
        // The next Stop's subagent sweep still finds the file.
        return Err("no agent_transcript_path in payload");
    };
    Ok(SharedBudgetRequest::ChargeAgentTranscript {
        session_id: stop.session_id.clone(),
        agent_id: stop.agent_id.clone(),
        agent_transcript_path: path,
    })
}

/// The request for a parsed `SessionEnd`, or why none is sent.
pub fn tail_request(end: &SessionEnd) -> Result<SharedBudgetRequest, &'static str> {
    if budget_gate::session_id_unusable(&end.session_id) {
        return Err("unusable session id");
    }
    let Some(path) = end.transcript_path.clone() else {
        return Err("no transcript_path in payload");
    };
    Ok(SharedBudgetRequest::SettleTail {
        session_id: end.session_id.clone(),
        transcript_path: path,
    })
}

/// One short, id-free line describing what the daemon answered.
fn outcome_line(what: &str, outcome: &SharedBudgetOutcome) -> String {
    match outcome {
        SharedBudgetOutcome::TranscriptCharged {
            amount_charged_usd_cents,
            already_charged,
            unmeasured,
            binding_mismatch,
            ..
        } => format!(
            "{what}: charged {amount_charged_usd_cents} cents (already_charged={already_charged}, \
             unmeasured={unmeasured}, binding_mismatch={binding_mismatch})"
        ),
        SharedBudgetOutcome::TailSettled {
            amount_charged_usd_cents,
            already_charged,
            unmeasured,
            binding_mismatch,
            ..
        } => format!(
            "{what}: tail charged {amount_charged_usd_cents} cents \
             (already_charged={already_charged}, unmeasured={unmeasured}, \
             binding_mismatch={binding_mismatch})"
        ),
        SharedBudgetOutcome::NotConfigured => format!("{what}: shared budget not configured"),
        SharedBudgetOutcome::Refused {
            reason: SharedBudgetRefusal::NoSettledLane,
        } => format!("{what}: no settled lane, nothing to settle"),
        SharedBudgetOutcome::Refused { reason } => format!("{what}: refused ({reason:?})"),
        _ => format!("{what}: unexpected daemon answer"),
    }
}

/// Sends `request` to an already-running daemon (never spawns one) within
/// [`SETTLE_BUDGET`] and logs the outcome. Every failure is only a log line.
fn send(socket_path: &Path, what: &'static str, request: SharedBudgetRequest, log: &dyn Fn(&str)) {
    let socket = socket_path.to_path_buf();
    let request = Request::SharedBudget {
        shared_budget: request,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // One deadline around connect + write + read; the worker is abandoned on
    // timeout and the hook process exits right after.
    std::thread::spawn(move || {
        let answer = client::connect_only_with_budget(&socket, SETTLE_BUDGET)
            .and_then(|stream| client::roundtrip(&stream, request));
        let _ = tx.send(answer);
    });
    match rx.recv_timeout(SETTLE_BUDGET) {
        Ok(Ok(Response::SharedBudget(outcome))) => log(&outcome_line(what, &outcome)),
        Ok(Ok(_)) => log(&format!("{what}: unexpected response kind")),
        Ok(Err(e)) => log(&format!("{what}: daemon request failed, ignoring: {e}")),
        Err(_) => log(&format!("{what}: daemon did not answer in time, ignoring")),
    }
}

fn run_with<P>(
    agent: AgentKind,
    raw: &str,
    what: &'static str,
    parse: impl FnOnce(&str) -> Result<P, Skip>,
    request: impl FnOnce(&P) -> Result<SharedBudgetRequest, &'static str>,
    note: impl FnOnce(&P, &dyn Fn(&str)),
) {
    // Codex has no shared-budget settlement: its route is unchanged.
    if agent != AgentKind::ClaudeCode {
        return;
    }
    let log_path = libra_governor_daemon::paths::log_path();
    let log = |message: &str| {
        if let Ok(path) = &log_path {
            libra_governor_daemon::log::append_line(path, &format!("[claude-code] {message}"));
        }
    };

    // Cheap stat first: no budget file means nothing is parsed or sent.
    if !budget_gate::budget_file_present() {
        return;
    }
    let parsed = match parse(raw) {
        Ok(parsed) => parsed,
        Err(Skip::Malformed(why)) => {
            log(&format!("{what}: malformed hook payload: {why}"));
            return;
        }
        Err(Skip::OtherEvent(_)) => return,
    };
    note(&parsed, &log);
    let request = match request(&parsed) {
        Ok(request) => request,
        Err(why) => {
            log(&format!("{what}: not settled: {why}"));
            return;
        }
    };
    let Ok(socket_path) = libra_governor_daemon::paths::socket_path() else {
        return;
    };
    send(&socket_path, what, request, &log);
}

fn read_stdin() -> Option<String> {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    Some(raw)
}

/// `libra-governor hook subagent-stop`: reads stdin, never fails the host.
pub fn run_subagent_stop(agent: AgentKind) {
    if let Some(raw) = read_stdin() {
        run_subagent_stop_with_input(agent, &raw);
    }
}

/// `libra-governor hook session-end`: reads stdin, never fails the host.
pub fn run_session_end(agent: AgentKind) {
    if let Some(raw) = read_stdin() {
        run_session_end_with_input(agent, &raw);
    }
}

pub fn run_subagent_stop_with_input(agent: AgentKind, raw: &str) {
    run_with(
        agent,
        raw,
        "subagent-stop",
        parse_subagent_stop,
        charge_request,
        |stop, log| {
            if stop.agent_id_rejected {
                log("subagent-stop: agent_id outside [A-Za-z0-9_-]; charging unattributed");
            }
        },
    );
}

pub fn run_session_end_with_input(agent: AgentKind, raw: &str) {
    run_with(
        agent,
        raw,
        "session-end",
        parse_session_end,
        tail_request,
        |_, _| (),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_stop_reads_identity_and_path_verbatim() {
        let stop = parse_subagent_stop(
            r#"{"session_id":"s-1","hook_event_name":"SubagentStop","agent_id":"a0f3_B-9",
                "agent_type":"x","agent_transcript_path":"/h/p/s-1/subagents/agent-a0f3_B-9.jsonl"}"#,
        )
        .unwrap();
        assert_eq!(stop.session_id, "s-1");
        assert_eq!(stop.agent_id.as_deref(), Some("a0f3_B-9"));
        assert!(!stop.agent_id_rejected);
        assert_eq!(
            charge_request(&stop).unwrap(),
            SharedBudgetRequest::ChargeAgentTranscript {
                session_id: "s-1".into(),
                agent_id: Some("a0f3_B-9".into()),
                agent_transcript_path: "/h/p/s-1/subagents/agent-a0f3_B-9.jsonl".into(),
            }
        );
    }

    #[test]
    fn blank_wrong_typed_and_missing_agent_ids_are_absent_never_invented() {
        for agent in [
            r#""agent_id":"""#,
            r#""agent_id":"   ""#,
            r#""agent_id":null"#,
            r#""agent_id":7"#,
            r#""agent_id":["a"]"#,
            r#""other":1"#,
        ] {
            let raw =
                format!(r#"{{"session_id":"s",{agent},"agent_transcript_path":"/t/agent.jsonl"}}"#);
            let stop = parse_subagent_stop(&raw).unwrap();
            assert_eq!(stop.agent_id, None, "{agent}");
            assert!(!stop.agent_id_rejected, "{agent}");
            assert!(matches!(
                charge_request(&stop).unwrap(),
                SharedBudgetRequest::ChargeAgentTranscript { agent_id: None, .. }
            ));
        }
    }

    #[test]
    fn an_agent_id_outside_the_charset_is_dropped_not_forwarded_or_repaired() {
        for bad in ["a/b", "../x", "a b", " a", "a\\n", "a.b", "é", "a:b"] {
            let raw = serde_json::json!({
                "session_id": "s",
                "agent_id": bad,
                "agent_transcript_path": "/t/a.jsonl",
            })
            .to_string();
            let stop = parse_subagent_stop(&raw).unwrap();
            assert_eq!(stop.agent_id, None, "{bad:?}");
            assert!(stop.agent_id_rejected, "{bad:?}");
        }
    }

    #[test]
    fn subagent_stop_without_a_path_or_with_an_unusable_session_sends_nothing() {
        let no_path = parse_subagent_stop(r#"{"session_id":"s","agent_id":"a"}"#).unwrap();
        assert!(charge_request(&no_path).is_err());
        let blank_path =
            parse_subagent_stop(r#"{"session_id":"s","agent_transcript_path":"  "}"#).unwrap();
        assert!(charge_request(&blank_path).is_err());
        let reserved = parse_subagent_stop(
            r#"{"session_id":"execution-owner-v1:x","agent_transcript_path":"/t"}"#,
        )
        .unwrap();
        assert!(charge_request(&reserved).is_err());
    }

    #[test]
    fn malformed_payloads_and_missing_sessions_are_skips_not_panics() {
        for raw in [
            "",
            "not json",
            "[]",
            "[\"s\"]",
            "null",
            "{}",
            r#"{"session_id":""}"#,
            r#"{"session_id":"  "}"#,
            r#"{"session_id":5}"#,
        ] {
            assert!(
                matches!(parse_subagent_stop(raw), Err(Skip::Malformed(_))),
                "{raw}"
            );
            assert!(
                matches!(parse_session_end(raw), Err(Skip::Malformed(_))),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_payload_for_another_event_is_ignored() {
        assert_eq!(
            parse_subagent_stop(r#"{"session_id":"s","hook_event_name":"SessionEnd"}"#),
            Err(Skip::OtherEvent("SessionEnd".into()))
        );
        assert_eq!(
            parse_session_end(r#"{"session_id":"s","hook_event_name":"SubagentStop"}"#),
            Err(Skip::OtherEvent("SubagentStop".into()))
        );
    }

    #[test]
    fn session_end_forwards_the_transcript_path_for_the_daemon_to_validate() {
        let end = parse_session_end(
            r#"{"session_id":"s-2","hook_event_name":"SessionEnd","reason":"clear",
                "transcript_path":"/h/p/s-2.jsonl"}"#,
        )
        .unwrap();
        assert_eq!(
            tail_request(&end).unwrap(),
            SharedBudgetRequest::SettleTail {
                session_id: "s-2".into(),
                transcript_path: "/h/p/s-2.jsonl".into(),
            }
        );
        let no_path = parse_session_end(r#"{"session_id":"s-2"}"#).unwrap();
        assert!(tail_request(&no_path).is_err());
    }
}
