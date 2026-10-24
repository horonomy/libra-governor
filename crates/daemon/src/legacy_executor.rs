//! Which executor a legacy-route hook event belongs to (HORO-2197).
//!
//! The legacy route (`Preflight` / `ToolInvoked` / `Finalize`) keeps its
//! per-session task binding, tool-call counter, same-tool streak, replan
//! hysteresis and shadow decisions in tables keyed by one string. A
//! subagent's tool call carries its *parent's* `session_id` plus an
//! `agent_id`, so keying on `session_id` alone books the subagent's activity
//! to the parent. The lane key below gives every `(session, agent)` executor
//! its own row without a schema change.
//!
//! | Wire shape | Executor | Lane key |
//! |---|---|---|
//! | no `agent_id` | the session's main executor (`agent_absent`) | the raw `session_id`, byte for byte, so old clients and pre-existing rows are unchanged |
//! | non-blank `agent_id` | that agent | reserved prefix + JSON `["agent", provider, session, agent]` |
//! | blank `agent_id` | present but unnameable (`unattributed`) | reserved prefix + JSON `["unattributed", provider, session]` |
//!
//! A missing agent never means root, and a *present-but-blank* agent never
//! means "missing": it is an executor we cannot name, so it must not be
//! folded into the parent. The JSON encoding is injective, and the prefix is
//! reserved against native session ids by [`is_reserved_session`], so no
//! native session can collide with a derived lane.
//!
//! Only the *ledger-state* key is derived. Anything that identifies the
//! session to the outside (shared-budget holds and settlement, extension
//! payloads) keeps the real `session_id`.

/// Prefix of every derived lane key. Legacy requests whose own `session_id`
/// starts with it are refused, like the execution-owner namespace.
pub const RESERVED_LANE_PREFIX: &str = "legacy-executor-v1:";

/// Whether a native `session_id` lies in a namespace the daemon derives
/// lane keys in, and must therefore not be accepted from a client.
pub fn is_reserved_session(session_id: &str) -> bool {
    session_id.starts_with(RESERVED_LANE_PREFIX)
}

/// The ledger key for one legacy-route event's executor.
pub fn lane_key(session_id: &str, agent_id: Option<&str>, provider: Option<&str>) -> String {
    let encoded = match agent_id {
        None => return session_id.to_owned(),
        Some(agent) if agent.trim().is_empty() => {
            serde_json::to_string(&("unattributed", provider, session_id))
        }
        Some(agent) => serde_json::to_string(&("agent", provider, session_id, agent)),
    }
    .expect("a tuple of strings always serializes");
    format!("{RESERVED_LANE_PREFIX}{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_agent_keeps_the_raw_session_key() {
        assert_eq!(lane_key("s-1", None, Some("claude-code")), "s-1");
        assert_eq!(lane_key("  s 1  ", None, None), "  s 1  ");
    }

    #[test]
    fn a_named_agent_gets_its_own_lane_apart_from_the_parent() {
        let parent = lane_key("s-1", None, Some("claude-code"));
        let a = lane_key("s-1", Some("a-1"), Some("claude-code"));
        let b = lane_key("s-1", Some("a-2"), Some("claude-code"));
        assert_ne!(a, parent);
        assert_ne!(a, b);
        assert!(a.starts_with(RESERVED_LANE_PREFIX));
    }

    #[test]
    fn a_blank_agent_is_unattributed_never_the_parent_and_never_a_named_agent() {
        let parent = lane_key("s-1", None, Some("claude-code"));
        let named = lane_key("s-1", Some("unattributed"), Some("claude-code"));
        for blank in ["", " ", "\t\n"] {
            let lane = lane_key("s-1", Some(blank), Some("claude-code"));
            assert_ne!(
                lane, parent,
                "blank {blank:?} must not fold into the parent"
            );
            assert_ne!(lane, named);
        }
        assert_ne!(
            lane_key("s-1", Some(""), None),
            lane_key("s-2", Some(""), None)
        );
    }

    #[test]
    fn lane_keys_are_injective_across_session_agent_and_provider() {
        // Separator-shaped ids cannot make two executors share a key.
        let x = lane_key("a", Some("b\",\"c"), None);
        let y = lane_key("a\",\"b", Some("c"), None);
        assert_ne!(x, y);
        assert_ne!(
            lane_key("s", Some("a"), Some("claude-code")),
            lane_key("s", Some("a"), Some("codex"))
        );
        assert_ne!(
            lane_key("s", Some("a"), None),
            lane_key("s", Some("a"), Some(""))
        );
    }

    #[test]
    fn a_derived_lane_is_a_reserved_session_so_a_client_cannot_name_it() {
        assert!(is_reserved_session(&lane_key("s", Some("a"), None)));
        assert!(!is_reserved_session("s"));
    }
}
