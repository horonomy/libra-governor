//! The frozen `schema_version: 1` DogFood evidence event (ADR-0012 §3).
//!
//! Every field here mirrors the ADR's table exactly — this module makes
//! no product-local addition to the schema. Field-level rationale for
//! *values* this adapter chooses (as opposed to the schema shape itself)
//! lives in `crate::adapter`, next to the code that actually derives
//! them from ledger data.

use serde::{Deserialize, Serialize};

/// ADR-0012 §3 `product`. Only `LibraGovernor` is ever constructed by
/// this crate; the other variants exist so this type documents the full
/// frozen enum rather than inventing a Libra-only subset of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Circinus,
    Eltanin,
    Eridanus,
    Horologium,
    Ophiuchus,
    LibraGovernor,
    Aasm,
}

/// ADR-0012 §3 `profile` / `origin_profile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Personal,
    Corporate,
}

/// ADR-0012 §3 `decision_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    Observe,
    Enforce,
}

/// ADR-0012 §3 `actual_action` / `would_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
    Warn,
    NoOp,
    Error,
}

/// ADR-0012 §3 `coverage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Full,
    Partial,
    Gap,
}

/// ADR-0012 §3 `gap_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    BufferOverflow,
    DiskCap,
    AdapterUnsupported,
    SourceUnavailable,
    RedactionFailed,
    Unknown,
}

/// ADR-0012 §4 `payload_classification`. This adapter only ever
/// constructs `MetadataOnly` — see `crate::adapter` docs for why that is
/// a hard invariant, not merely today's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadClassification {
    MetadataOnly,
    RedactedSummary,
    ContentOptIn,
}

/// ADR-0012 §5 `transport_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Pending,
    Inflight,
    Acknowledged,
    Expired,
    Poison,
}

/// ADR-0012 §7 `eligibility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    ReplayableEvidence,
    NonReplayableOperation,
}

/// ADR-0012 §6 integrity envelope. `signature` is always `None` — no
/// signing key material exists in this adapter (optional in v1; see
/// ADR-0012 §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Integrity {
    pub canonicalization: String,
    pub content_hash: ContentHash,
    pub prev_event_hash: Option<String>,
    pub signature: Option<Signature>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentHash {
    pub alg: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    pub alg: String,
    pub key_id: String,
    pub value: String,
}

/// One frozen `schema_version: 1` DogFood evidence event (ADR-0012 §3).
///
/// `integrity` is populated by [`crate::canon::seal`] after every other
/// field is final — hashing must happen over everything else, per
/// `dogfood-evidence-canonicalization-v1.md` step 1 ("omit `integrity`
/// entirely before hashing").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DogfoodEvent {
    pub event_id: String,
    pub schema_version: u32,
    pub product: Product,
    pub product_version: String,
    pub adapter_version: String,
    pub occurred_at: String,
    pub ingested_at: String,
    pub profile: Profile,
    pub origin_profile: Profile,
    pub decision_mode: DecisionMode,
    pub scope_id: Option<String>,
    pub actual_action: Action,
    pub would_action: Option<Action>,
    pub coverage: Coverage,
    pub gap_reason: Option<GapReason>,
    pub dropped_count: u64,
    pub payload_classification: PayloadClassification,
    pub integrity: Integrity,
    pub destination: Option<String>,
    pub tenant_id: Option<String>,
    pub transport_state: TransportState,
    pub eligibility: Eligibility,
    pub permanently_ineligible: bool,
    pub imported: bool,
}

/// A schema invariant violation from ADR-0012 §3 that would make an
/// event malformed by construction. Checked by [`DogfoodEvent::validate`]
/// so a bug in `crate::adapter`'s derivation is caught before the event
/// is ever written or hashed, rather than trusted silently.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SchemaViolation {
    #[error("decision_mode=observe with actual_action=deny is malformed (ADR-0012 §3)")]
    ObserveWithDeniedActualAction,
    #[error("decision_mode=enforce requires would_action=null (ADR-0012 §3)")]
    EnforceWithNonNullWouldAction,
    #[error("decision_mode=enforce requires a non-null scope_id (ADR-0012 §3)")]
    EnforceWithoutScopeId,
    #[error("coverage != full requires a non-null gap_reason (ADR-0012 §3)")]
    NonFullCoverageWithoutGapReason,
}

impl DogfoodEvent {
    /// Checks the ADR-0012 §3 structural invariants that make a record
    /// "malformed by construction" — DFC-MODE-10, and the observe/deny
    /// combination the schema table calls out explicitly.
    pub fn validate(&self) -> Result<(), SchemaViolation> {
        match self.decision_mode {
            DecisionMode::Observe => {
                if self.actual_action == Action::Deny {
                    return Err(SchemaViolation::ObserveWithDeniedActualAction);
                }
            }
            DecisionMode::Enforce => {
                if self.would_action.is_some() {
                    return Err(SchemaViolation::EnforceWithNonNullWouldAction);
                }
                if self.scope_id.is_none() {
                    return Err(SchemaViolation::EnforceWithoutScopeId);
                }
            }
        }
        if self.coverage != Coverage::Full && self.gap_reason.is_none() {
            return Err(SchemaViolation::NonFullCoverageWithoutGapReason);
        }
        Ok(())
    }
}
