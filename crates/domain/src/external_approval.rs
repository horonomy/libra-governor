//! [`ExternalApproval`]/[`apply_external_approval`] — the other one-way
//! valve an external Policy Webhook provider has on admission (HORO-1174).
//!
//! # Only `ApprovalRequired` is affected
//!
//! [`apply_external_approval`] only ever changes an
//! [`crate::Admission::ApprovalRequired`] decision. `Admit` and `Deny`
//! pass through verbatim — no input from this surface can widen a
//! hard-ceiling `Deny` into an `Admit`, and no input can downgrade a
//! clean `Admit` into something requiring approval. `protected_criteria`
//! on the containing [`crate::PolicyDecision`] is untouched by this
//! function; only the daemon's caller ever reads it, and it is always
//! copied, never recomputed, from the original decision.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::policy::{Admission, DenyReason, PolicyDecision};

/// A Policy Webhook provider's verdict on one `ApprovalRequired`
/// admission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ExternalVerdict {
    Approve,
    Reject {
        reason: String,
    },
    /// The provider declined to answer (non-200, timeout, malformed
    /// response, or an unknown verdict string) — treated identically to
    /// "did not answer": the admission is left unchanged.
    Abstain,
}

/// One resolved verdict from an external Policy Webhook provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalApproval {
    pub provider_id: String,
    pub verdict: ExternalVerdict,
    pub decided_at: OffsetDateTime,
}

/// Applies `approval`'s verdict to `decision`'s admission, in place of the
/// containing `PolicyDecision`'s own `admission` field.
///
/// - `Admission::Admit` / `Admission::Deny` pass through **verbatim** —
///   see module docs.
/// - `Admission::ApprovalRequired` + `ExternalVerdict::Approve` ->
///   `Admission::Admit`.
/// - `Admission::ApprovalRequired` + `ExternalVerdict::Reject { reason }`
///   -> `Admission::Deny(vec![DenyReason::ExternalPolicyRejected {
///   provider_id, reason }])` — replacing, not appending to, the
///   `ApprovalRequired` request list, since a reject is a new decision,
///   not an accumulation of reasons.
/// - `Admission::ApprovalRequired` + `ExternalVerdict::Abstain` ->
///   unchanged (`ApprovalRequired`, same requests).
///
/// `resource_outcome`/`time_outcome`/`confidence_ok`/`protected_criteria`
/// and `policy_schema_version` are all copied from `decision` unchanged —
/// this function only ever replaces the `admission` field.
pub fn apply_external_approval(
    decision: PolicyDecision,
    approval: &ExternalApproval,
) -> PolicyDecision {
    let admission = match (&decision.admission, &approval.verdict) {
        (Admission::ApprovalRequired(_), ExternalVerdict::Approve) => Admission::Admit,
        (Admission::ApprovalRequired(_), ExternalVerdict::Reject { reason }) => {
            Admission::Deny(vec![DenyReason::ExternalPolicyRejected {
                provider_id: approval.provider_id.clone(),
                reason: reason.clone(),
            }])
        }
        // ApprovalRequired + Abstain, or Admit/Deny with any verdict:
        // pass through verbatim.
        (other, _) => other.clone(),
    };

    PolicyDecision {
        admission,
        ..decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion_contract::{CompletionContract, CompletionCriterion},
        policy::{ApprovalRequest, ConstraintOutcome},
        resource_amount::ResourceAmount,
    };

    fn base_decision(admission: Admission) -> PolicyDecision {
        PolicyDecision {
            policy_schema_version: "policy-v1".to_string(),
            admission,
            resource_outcome: ConstraintOutcome::Admit,
            time_outcome: ConstraintOutcome::Admit,
            confidence_ok: true,
            protected_criteria: CompletionContract::first(vec![CompletionCriterion::required(
                "tests pass",
            )])
            .required_criteria()
            .cloned()
            .collect(),
        }
    }

    fn approval(verdict: ExternalVerdict) -> ExternalApproval {
        ExternalApproval {
            provider_id: "example-provider".to_string(),
            verdict,
            decided_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest::Resource {
            projected: ResourceAmount::Tokens(1500),
            target: ResourceAmount::Tokens(1000),
            elastic_ceiling: Some(ResourceAmount::Tokens(1200)),
            hard_ceiling: ResourceAmount::Tokens(2000),
        }
    }

    #[test]
    fn admit_passes_through_every_verdict_verbatim() {
        for verdict in [
            ExternalVerdict::Approve,
            ExternalVerdict::Reject {
                reason: "no".to_string(),
            },
            ExternalVerdict::Abstain,
        ] {
            let decision = base_decision(Admission::Admit);
            let result = apply_external_approval(decision.clone(), &approval(verdict));
            assert_eq!(result.admission, Admission::Admit);
            assert_eq!(result.protected_criteria, decision.protected_criteria);
        }
    }

    #[test]
    fn deny_passes_through_every_verdict_verbatim_no_widening() {
        let deny = Admission::Deny(vec![DenyReason::ResourceExceedsHardCeiling {
            projected: ResourceAmount::Tokens(5000),
            hard_ceiling: ResourceAmount::Tokens(2000),
        }]);
        for verdict in [
            ExternalVerdict::Approve,
            ExternalVerdict::Reject {
                reason: "no".to_string(),
            },
            ExternalVerdict::Abstain,
        ] {
            let decision = base_decision(deny.clone());
            let result = apply_external_approval(decision, &approval(verdict));
            assert_eq!(
                result.admission, deny,
                "no external verdict may widen a hard-ceiling Deny"
            );
        }
    }

    #[test]
    fn approval_required_plus_approve_becomes_admit() {
        let decision = base_decision(Admission::ApprovalRequired(vec![approval_request()]));
        let result = apply_external_approval(decision, &approval(ExternalVerdict::Approve));
        assert_eq!(result.admission, Admission::Admit);
    }

    #[test]
    fn approval_required_plus_reject_becomes_deny_with_the_reason() {
        let decision = base_decision(Admission::ApprovalRequired(vec![approval_request()]));
        let result = apply_external_approval(
            decision,
            &approval(ExternalVerdict::Reject {
                reason: "over cost-center cap".to_string(),
            }),
        );
        assert_eq!(
            result.admission,
            Admission::Deny(vec![DenyReason::ExternalPolicyRejected {
                provider_id: "example-provider".to_string(),
                reason: "over cost-center cap".to_string(),
            }])
        );
    }

    #[test]
    fn approval_required_plus_abstain_is_unchanged() {
        let decision = base_decision(Admission::ApprovalRequired(vec![approval_request()]));
        let before = decision.admission.clone();
        let result = apply_external_approval(decision, &approval(ExternalVerdict::Abstain));
        assert_eq!(result.admission, before);
    }

    #[test]
    fn protected_criteria_is_always_copied_never_recomputed() {
        let decision = base_decision(Admission::ApprovalRequired(vec![approval_request()]));
        let expected = decision.protected_criteria.clone();
        let result = apply_external_approval(decision, &approval(ExternalVerdict::Approve));
        assert_eq!(result.protected_criteria, expected);
    }

    /// Old serialized `admission_json` rows carrying a pre-HORO-1174
    /// `DenyReason` must still deserialize — `ExternalPolicyRejected` is
    /// purely additive.
    #[test]
    fn deny_reason_deserializes_the_old_variants_after_the_additive_change() {
        let old = r#"{"ResourceExceedsHardCeiling":{"projected":{"kind":"tokens","amount":5000},"hard_ceiling":{"kind":"tokens","amount":2000}}}"#;
        let parsed: DenyReason = serde_json::from_str(old).unwrap();
        assert_eq!(
            parsed,
            DenyReason::ResourceExceedsHardCeiling {
                projected: ResourceAmount::Tokens(5000),
                hard_ceiling: ResourceAmount::Tokens(2000),
            }
        );
    }
}
