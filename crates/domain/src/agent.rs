//! [`AgentCapabilities`] — the honest, per-agent capability statement
//! this integration may claim for a governed coding agent host
//! (HORO-1157).
//!
//! # Why this exists, and why it is not a trait
//!
//! `crates/protocol`'s wire format and `crates/daemon`'s dispatch logic
//! (`handle_preflight`/`handle_tool_invoked`/`handle_finalize`) carry no
//! Claude-specific field or tool-name vocabulary — verified during
//! HORO-1157's design work. Codex's real hook payloads (verified against
//! `openai/codex`'s generated JSON schemas) name the same fields Claude
//! Code's do, and its `hookSpecificOutput.hookEventName`/
//! `.additionalContext` stdout contract is the same shape. Two hosts
//! that already agree on wire shape do not need a trait or a plugin
//! abstraction to be "supported" — that would be new machinery solving
//! a problem the data does not have. See
//! `docs/adr/0004-agent-adapter-contract.md` for the full "no trait
//! until real divergence appears" decision.
//!
//! What genuinely differs between hosts is what each one's hooks *can*
//! honestly be claimed to do — mirroring the discipline
//! [`crate::EnforcementCapabilities`] (HORO-1144) already established
//! for gateway tiers: a small closed set of types, produced only by a
//! pure function from configuration/host identity, never sniffed or
//! probed at runtime. [`AgentCapabilities::for_agent`] is that function
//! for "which agent host".
//!
//! # The structural invariant this module enforces
//!
//! A capability statement must never claim
//! [`AgentCapabilities::claims_pre_spend_refusal`] while also reporting
//! `model_gateway: Unavailable` — a hard budget cannot be pre-spend
//! enforced through a gateway that is not itself available for this
//! host. See the `hard_budget_enforcement_never_outruns_model_gateway`
//! test below; it inspects every entry in [`AgentKind::ALL`], and
//! [`AgentKind::all_variants_is_exhaustive`]'s compile-time-only match is
//! a tripwire that fails the build if a new [`AgentKind`] variant is
//! added without also adding it to [`AgentKind::ALL`] — so a future
//! agent variant cannot silently slip past this invariant unnoticed.

use serde::{Deserialize, Serialize};

/// Traceability tag every produced [`AgentCapabilities`] carries,
/// following the same convention as [`crate::CAPABILITY_SCHEMA_VERSION`]
/// and [`crate::POLICY_SCHEMA_VERSION`].
pub const AGENT_ADAPTER_CONTRACT_VERSION: &str = "agent-adapter-v1";

/// Which governed coding agent host this capability statement describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    /// Every [`AgentKind`] variant that exists today. The sole shared
    /// source of "all agents" for both this module's invariant test and
    /// `agents_cmd.rs`'s human/JSON rendering — do not duplicate this
    /// list elsewhere.
    ///
    /// [`Self::all_variants_is_exhaustive`] below is a compile-time
    /// tripwire: adding a new [`AgentKind`] variant makes that function's
    /// `match` non-exhaustive, which fails the build until this constant
    /// (and every other exhaustive match on [`AgentKind`]) is updated. Without
    /// that guard, a new variant could silently miss this list while
    /// everything still compiled.
    pub const ALL: [AgentKind; 2] = [AgentKind::ClaudeCode, AgentKind::Codex];

    /// Compile-time-only exhaustiveness check for [`Self::ALL`] — never
    /// called at runtime. See [`Self::ALL`]'s doc comment.
    #[allow(dead_code)]
    fn all_variants_is_exhaustive(agent: AgentKind) {
        match agent {
            AgentKind::ClaudeCode | AgentKind::Codex => {}
        }
    }
}

/// The normalized hook-lifecycle event vocabulary this integration
/// recognizes across agent hosts (HORO-1157). Mirrors
/// `crates/cli/src/agent::event::NormalizedEvent`'s variant set at the
/// level of "which kinds of event exist", independent of any one host's
/// raw payload shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedEventKind {
    SessionStarted,
    SessionResumed,
    SessionEnded,
    PromptSubmitted,
    ToolCompleted,
    TurnCompleted,
    SubagentStarted,
    SubagentStopped,
    Interrupted,
}

/// Why a capability is unavailable for a given agent — every variant
/// names a concrete cause, never a vague "unsupported", mirroring
/// [`crate::NoMonetaryCap`]'s discipline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityGap {
    /// The host's own hook/config surface exposes no primitive this
    /// capability could be built on at all (e.g. no statusline key in
    /// the host's config schema, no per-request token/cost field on any
    /// hook payload).
    HostExposesNoPrimitive { detail: String },
    /// The host exposes a usable primitive, but this integration has
    /// deliberately not wired it to a daemon action yet — a scoping
    /// decision, not a host limitation. `follow_up` names a concrete
    /// ticket only when one genuinely already exists; never invented.
    NotWiredByLibra {
        detail: String,
        follow_up: Option<String>,
    },
    /// The host's own interface for this capability is structurally
    /// incompatible with Libra's side of it (e.g. an API wire format
    /// mismatch), not merely unimplemented.
    Incompatible {
        agent_side: String,
        libra_side: String,
    },
}

/// Whether a capability is available for a given agent, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Available,
    Unavailable { gap: CapabilityGap },
}

impl Capability {
    fn is_available(&self) -> bool {
        matches!(self, Capability::Available)
    }
}

/// The full, honest capability statement for one agent host
/// (HORO-1157). Produced only by [`Self::for_agent`], a pure function of
/// `agent` alone — nothing here sniffs, probes, or infers a capability
/// at runtime. See module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub agent: AgentKind,
    pub contract_version: String,
    /// A `UserPromptSubmit`-shaped hook can run bounded reconnaissance
    /// and surface a draft Completion Contract before work starts.
    pub preflight_gate: Capability,
    /// A `PostToolUse`-shaped hook can observe which tool ran.
    pub tool_observation: Capability,
    /// A tool call can be hard-refused before it runs (not merely
    /// observed after). See ADR 0003 §8: MVP 1.0 hooks are advisory for
    /// every agent — the gateway is the only hard-enforcement point.
    pub tool_gate: Capability,
    /// A `Stop`-shaped hook can finalize the task into an
    /// `ExecutionReceipt`.
    pub completion_receipt: Capability,
    /// A hook payload exposes per-request token count, cost, or
    /// provider id.
    pub model_event_observation: Capability,
    /// The provider gateway (ADR 0003) can sit in front of this host's
    /// model calls at all — a wire-format precondition, independent of
    /// whether a hard budget is actually configured.
    pub model_gateway: Capability,
    /// A hard, pre-spend-refusing monetary/resource budget can actually
    /// be enforced for this host. See the structural invariant in module
    /// docs: this must never be `Available` while `model_gateway` is
    /// `Unavailable`.
    pub hard_budget_enforcement: Capability,
    /// A persistent one-line status surface (Claude Code's
    /// `statusLine`) exists in this host's config schema.
    pub persistent_status_surface: Capability,
    /// The host can inject text into its own context window from a hook
    /// (`hookSpecificOutput.additionalContext` or equivalent).
    pub inline_explanation_channel: Capability,
    /// `SessionStart`/`SessionEnd`-shaped events are wired to a daemon
    /// action.
    pub session_lifecycle: Capability,
    /// `SubagentStart`/`SubagentStop`-shaped events are wired to a
    /// daemon action.
    pub subagent_lifecycle: Capability,
    /// An interrupt/cancel-shaped event is wired to a daemon action.
    pub interruption_signal: Capability,
    /// An MCP-based explain/query surface is wired for this host (never
    /// an enforcement boundary — see `ARCHITECTURE.md`).
    pub mcp_explain_surface: Capability,
    /// Which [`NormalizedEventKind`]s this integration actually emits
    /// today for this agent.
    pub emitted_events: Vec<NormalizedEventKind>,
}

impl AgentCapabilities {
    /// The capability statement for `agent`. See module docs on why this
    /// is a pure function, never a runtime probe.
    pub fn for_agent(agent: AgentKind) -> Self {
        let emitted_events = vec![
            NormalizedEventKind::PromptSubmitted,
            NormalizedEventKind::ToolCompleted,
            NormalizedEventKind::TurnCompleted,
        ];

        let tool_gate = Capability::Unavailable {
            gap: CapabilityGap::NotWiredByLibra {
                detail: "PreToolUse permissionDecision:deny exists on the host; MVP 1.0 hooks \
                          are advisory only — hard enforcement is the gateway (ADR 0003)"
                    .to_string(),
                follow_up: None,
            },
        };
        let model_event_observation = Capability::Unavailable {
            gap: CapabilityGap::HostExposesNoPrimitive {
                detail: "hook payloads expose no token count, cost, or provider id".to_string(),
            },
        };
        let session_lifecycle = Capability::Unavailable {
            gap: CapabilityGap::NotWiredByLibra {
                detail: "SessionStart/SessionEnd-shaped events exist on the host but map only \
                          to RecognizedUnwired in MVP 1.0 — no protocol variant or ledger \
                          semantics exist for them yet"
                    .to_string(),
                follow_up: None,
            },
        };
        let subagent_lifecycle = Capability::Unavailable {
            gap: CapabilityGap::NotWiredByLibra {
                detail: "SubagentStart/SubagentStop-shaped events exist on the host but map \
                          only to RecognizedUnwired in MVP 1.0"
                    .to_string(),
                follow_up: None,
            },
        };
        let mcp_explain_surface = |detail: &str| Capability::Unavailable {
            gap: CapabilityGap::NotWiredByLibra {
                detail: detail.to_string(),
                follow_up: None,
            },
        };

        let (
            model_gateway,
            hard_budget_enforcement,
            persistent_status_surface,
            interruption_signal,
            mcp_explain,
        ) = match agent {
            AgentKind::ClaudeCode => (
                Capability::Available,
                Capability::Available,
                Capability::Available,
                Capability::Unavailable {
                    gap: CapabilityGap::HostExposesNoPrimitive {
                        detail: "no Interrupt/cancel hook event exists on this host".to_string(),
                    },
                },
                mcp_explain_surface(
                    "MCP client support exists on the host but no explain/query \
                                      surface is wired by Libra yet",
                ),
            ),
            AgentKind::Codex => {
                let incompatible = || CapabilityGap::Incompatible {
                    agent_side: "responses (POST /v1/responses)".to_string(),
                    libra_side: "anthropic messages (POST /v1/messages)".to_string(),
                };
                (
                    Capability::Unavailable {
                        gap: incompatible(),
                    },
                    Capability::Unavailable {
                        gap: incompatible(),
                    },
                    Capability::Unavailable {
                        gap: CapabilityGap::HostExposesNoPrimitive {
                            detail: "no statusline key exists in the Codex config schema at any \
                                      level"
                                .to_string(),
                        },
                    },
                    Capability::Unavailable {
                        gap: CapabilityGap::NotWiredByLibra {
                            detail: "Codex exposes an Interrupt hook event (1s default / 3s max \
                                      budget) but it is not wired to a daemon action in MVP 1.0"
                                .to_string(),
                            follow_up: None,
                        },
                    },
                    mcp_explain_surface(
                        "Codex supports MCP as a client (mcp_servers stdio config) but no \
                         explain/query surface is wired by Libra yet",
                    ),
                )
            }
        };

        Self {
            agent,
            contract_version: AGENT_ADAPTER_CONTRACT_VERSION.to_string(),
            preflight_gate: Capability::Available,
            tool_observation: Capability::Available,
            tool_gate,
            completion_receipt: Capability::Available,
            model_event_observation,
            model_gateway,
            hard_budget_enforcement,
            persistent_status_surface,
            inline_explanation_channel: Capability::Available,
            session_lifecycle,
            subagent_lifecycle,
            interruption_signal,
            mcp_explain_surface: mcp_explain,
            emitted_events,
        }
    }

    /// `true` iff this capability statement may claim a hard,
    /// pre-spend-refusing budget. Callers deciding whether to *advertise*
    /// budget enforcement must ask this rather than matching on
    /// [`Self::agent`], mirroring
    /// [`crate::EnforcementCapabilities::claims_monetary_cap`]'s
    /// discipline.
    pub fn claims_pre_spend_refusal(&self) -> bool {
        self.hard_budget_enforcement.is_available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_agents() -> [AgentKind; 2] {
        AgentKind::ALL
    }

    /// The structural invariant this module must never violate: a
    /// capability statement must never claim
    /// [`AgentCapabilities::claims_pre_spend_refusal`] while also
    /// reporting `model_gateway: Unavailable` — a hard budget cannot be
    /// pre-spend enforced through a gateway that is not itself available
    /// for this host. Checked over every [`AgentKind`] variant, not just
    /// the two defined today, so a future agent added to the enum cannot
    /// silently violate it.
    #[test]
    fn hard_budget_enforcement_never_outruns_model_gateway() {
        for agent in all_agents() {
            let caps = AgentCapabilities::for_agent(agent);
            if caps.hard_budget_enforcement.is_available() {
                assert!(
                    caps.model_gateway.is_available(),
                    "{agent:?} claims hard_budget_enforcement without a usable model_gateway"
                );
            }
        }
    }

    #[test]
    fn claude_code_claims_a_gateway_and_pre_spend_refusal() {
        let caps = AgentCapabilities::for_agent(AgentKind::ClaudeCode);
        assert!(caps.model_gateway.is_available());
        assert!(caps.claims_pre_spend_refusal());
    }

    #[test]
    fn codex_names_the_concrete_wire_format_incompatibility() {
        let caps = AgentCapabilities::for_agent(AgentKind::Codex);
        assert!(!caps.claims_pre_spend_refusal());
        assert_eq!(
            caps.model_gateway,
            Capability::Unavailable {
                gap: CapabilityGap::Incompatible {
                    agent_side: "responses (POST /v1/responses)".to_string(),
                    libra_side: "anthropic messages (POST /v1/messages)".to_string(),
                }
            }
        );
    }

    #[test]
    fn codex_has_no_persistent_status_surface() {
        let caps = AgentCapabilities::for_agent(AgentKind::Codex);
        assert!(!caps.persistent_status_surface.is_available());
    }

    #[test]
    fn no_agent_hard_gates_tool_calls_in_mvp_1() {
        for agent in all_agents() {
            let caps = AgentCapabilities::for_agent(agent);
            assert!(
                !caps.tool_gate.is_available(),
                "{agent:?} must not claim hard tool gating in MVP 1.0 — ADR 0003"
            );
        }
    }

    #[test]
    fn every_agent_emits_the_same_three_wired_event_kinds() {
        for agent in all_agents() {
            let caps = AgentCapabilities::for_agent(agent);
            assert_eq!(
                caps.emitted_events,
                vec![
                    NormalizedEventKind::PromptSubmitted,
                    NormalizedEventKind::ToolCompleted,
                    NormalizedEventKind::TurnCompleted,
                ]
            );
        }
    }

    #[test]
    fn capabilities_round_trip_through_json() {
        for agent in all_agents() {
            let original = AgentCapabilities::for_agent(agent);
            let json = serde_json::to_string(&original).unwrap();
            let parsed: AgentCapabilities = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, original);
        }
    }
}
