//! [`AttestationSource`]/[`OutcomeAttestation`] — the provenance-tagged
//! record of who claimed a task finished, and whether that claim is
//! authoritative (HORO-1174).
//!
//! # Only Provider/GovernorLocal sources are authoritative
//!
//! An [`AttestationSource::Agent`] attestation is recorded — it always
//! writes an `outcome_attestations` row — but never promotes
//! `receipts.outcome_json`. The model that just did the work is not a
//! trustworthy witness to whether it succeeded; an external Outcome
//! Provider (CI, a deployment system) or the Governor's own local
//! finalize logic are. See [`AttestationSource::is_authoritative`].

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{agent::AgentKind, execution_outcome::ExecutionOutcome};

/// Who is claiming this outcome, and how much that claim is worth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttestationSource {
    /// An external Outcome Provider pushed this attestation over the
    /// daemon's Unix socket (`Request::RecordOutcome`).
    Provider { provider_id: String },
    /// The Governor's own local finalize logic recorded this outcome
    /// (e.g. `handle_finalize`'s receipt-time outcome).
    GovernorLocal,
    /// The governed agent itself claimed this outcome (e.g. a message in
    /// its own transcript). Recorded for visibility; **never**
    /// authoritative — see [`Self::is_authoritative`].
    Agent { agent: AgentKind },
}

impl AttestationSource {
    /// `true` for [`Self::Provider`]/[`Self::GovernorLocal`], `false` for
    /// [`Self::Agent`] — the one-way valve this module exists to enforce.
    pub fn is_authoritative(&self) -> bool {
        !matches!(self, Self::Agent { .. })
    }
}

/// One recorded claim about a task's outcome, with its source and
/// idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeAttestation {
    pub source: AttestationSource,
    /// A free-text identifier for the specific attester (a CI run id, a
    /// deployment id) — distinct from `source`'s coarse kind tag. Used
    /// together with `idempotency_key` for the ledger's
    /// `UNIQUE(task_id, source_id, idempotency_key)` dedupe.
    pub source_id: String,
    pub outcome: ExecutionOutcome,
    pub idempotency_key: String,
    pub attested_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_and_governor_local_are_authoritative() {
        assert!(AttestationSource::Provider {
            provider_id: "example-provider".to_string()
        }
        .is_authoritative());
        assert!(AttestationSource::GovernorLocal.is_authoritative());
    }

    #[test]
    fn agent_is_never_authoritative() {
        for agent in AgentKind::ALL {
            assert!(!AttestationSource::Agent { agent }.is_authoritative());
        }
    }

    #[test]
    fn attestation_round_trips_through_json() {
        let attestation = OutcomeAttestation {
            source: AttestationSource::Provider {
                provider_id: "example-provider".to_string(),
            },
            source_id: "ci-run-42".to_string(),
            outcome: ExecutionOutcome::Completed {
                evidence: vec!["https://ci.example.com/runs/42".to_string()],
            },
            idempotency_key: "ci-run-42-final".to_string(),
            attested_at: OffsetDateTime::UNIX_EPOCH,
        };
        let json = serde_json::to_string(&attestation).unwrap();
        let round_tripped: OutcomeAttestation = serde_json::from_str(&json).unwrap();
        assert_eq!(attestation, round_tripped);
    }
}
