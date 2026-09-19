//! `libra-governor agents [--json]` (HORO-1157) — the honest,
//! machine-readable-or-human-readable capability matrix for every
//! governed agent this integration knows about.
//!
//! Purely a rendering of [`libra_governor_domain::AgentCapabilities::for_agent`]
//! for every [`AgentKind`] — no probing, no daemon round trip, nothing
//! computed here. `integrations/README.md`'s human-readable matrix
//! points at `--json` as the source of truth precisely because this
//! command derives directly from the same pure function the domain
//! crate's own tests hold to its structural invariant.

use libra_governor_domain::{AgentCapabilities, AgentKind};

pub fn run(json: bool) {
    let matrix: Vec<AgentCapabilities> = AgentKind::ALL
        .iter()
        .map(|agent| AgentCapabilities::for_agent(*agent))
        .collect();

    if json {
        println!("{}", render_json(&matrix));
    } else {
        println!("{}", render_human(&matrix));
    }
}

fn agent_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::ClaudeCode => "Claude Code",
        AgentKind::Codex => "Codex",
    }
}

fn capability_label(cap: &libra_governor_domain::Capability) -> String {
    use libra_governor_domain::{Capability, CapabilityGap};
    match cap {
        Capability::Available => "available".to_string(),
        Capability::Unavailable { gap } => {
            let reason = match gap {
                CapabilityGap::HostExposesNoPrimitive { detail } => detail.clone(),
                CapabilityGap::NotWiredByLibra { detail, follow_up } => match follow_up {
                    Some(ticket) => format!("{detail} (follow-up: {ticket})"),
                    None => detail.clone(),
                },
                CapabilityGap::Incompatible {
                    agent_side,
                    libra_side,
                } => {
                    format!("incompatible: agent side is {agent_side}, Libra side is {libra_side}")
                }
            };
            format!("unavailable — {reason}")
        }
    }
}

fn render_human(matrix: &[AgentCapabilities]) -> String {
    let mut out = String::new();
    out.push_str("libra-governor agents\n");
    out.push_str("======================\n\n");
    for caps in matrix {
        out.push_str(&format!(
            "{} (contract {})\n",
            agent_label(caps.agent),
            caps.contract_version
        ));
        let rows: [(&str, &libra_governor_domain::Capability); 13] = [
            ("preflight_gate", &caps.preflight_gate),
            ("tool_observation", &caps.tool_observation),
            ("tool_gate", &caps.tool_gate),
            ("completion_receipt", &caps.completion_receipt),
            ("model_event_observation", &caps.model_event_observation),
            ("model_gateway", &caps.model_gateway),
            ("hard_budget_enforcement", &caps.hard_budget_enforcement),
            ("persistent_status_surface", &caps.persistent_status_surface),
            (
                "inline_explanation_channel",
                &caps.inline_explanation_channel,
            ),
            ("session_lifecycle", &caps.session_lifecycle),
            ("subagent_lifecycle", &caps.subagent_lifecycle),
            ("interruption_signal", &caps.interruption_signal),
            ("mcp_explain_surface", &caps.mcp_explain_surface),
        ];
        for (name, cap) in rows {
            out.push_str(&format!("  {name}: {}\n", capability_label(cap)));
        }
        out.push('\n');
    }
    out.push_str("Source of truth: `libra-governor agents --json` (this command). See ADR 0004.\n");
    out
}

fn render_json(matrix: &[AgentCapabilities]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "agents": matrix }))
        .unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_render_lists_every_agent_and_every_row() {
        let matrix: Vec<AgentCapabilities> = AgentKind::ALL
            .iter()
            .map(|a| AgentCapabilities::for_agent(*a))
            .collect();
        let rendered = render_human(&matrix);
        assert!(rendered.contains("Claude Code"));
        assert!(rendered.contains("Codex"));
        assert!(rendered.contains("model_gateway"));
        assert!(rendered.contains("hard_budget_enforcement"));
        assert!(rendered.contains("subagent_lifecycle"));
        assert!(rendered.contains("mcp_explain_surface"));
    }

    #[test]
    fn codex_row_names_the_wire_format_incompatibility_in_human_output() {
        let caps = AgentCapabilities::for_agent(AgentKind::Codex);
        let rendered = render_human(std::slice::from_ref(&caps));
        assert!(rendered.contains("incompatible"));
        assert!(rendered.contains("responses"));
    }

    #[test]
    fn json_render_round_trips_and_matches_for_agent() {
        let matrix: Vec<AgentCapabilities> = AgentKind::ALL
            .iter()
            .map(|a| AgentCapabilities::for_agent(*a))
            .collect();
        let rendered = render_json(&matrix);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let agents = value["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
        let parsed: Vec<AgentCapabilities> =
            serde_json::from_value(value["agents"].clone()).unwrap();
        assert_eq!(parsed, matrix);
    }
}
