//! [`BusinessContextSummary`] and [`apply_business_context`] — the
//! narrowing-only integration point between an external Business Context
//! Provider and [`crate::Policy`] evaluation (HORO-1174).
//!
//! # The trust boundary (R1/R2 — see `docs/adr/0005-local-extension-points.md`)
//!
//! [`BusinessContextSummary`] is a **separate type** from
//! [`crate::TaskFeatures`] and never merges into it — different modules,
//! different ledger tables, no conversion function anywhere between the
//! two. See the `business_context_field_set_contains_no_task_feature_or_policy_field`
//! test below.
//!
//! [`BusinessContextSummary::advisory_criteria`] never enters
//! [`crate::Policy::quality_floor`]. This is the sharpest rule in the
//! whole extension surface: a provider-supplied "please also verify X"
//! must never become a criterion [`crate::Policy::evaluate`] treats as
//! required. The advisory criteria are recorded (on this summary, and in
//! the `business_context` ledger table) and reported on a separate,
//! provenance-tagged field — never merged into
//! [`crate::PolicyDecision::protected_criteria`], which stays purely
//! policy-derived. This is enforced by a tested value invariant (every
//! `apply_business_context` output has `derived.quality_floor ==
//! base.quality_floor`, unconditionally — see the property tests below)
//! plus the simple fact that no function anywhere in this crate converts
//! a `Vec<String>` of advisory criteria into a `Vec<CompletionCriterion>`
//! — not a type-level impossibility (nothing stops a *future* caller from
//! writing one), but an absence that is easy to grep for and a value
//! invariant that is tested on every commit.
//!
//! The one real, tested lever business context has on admission is
//! **deadline narrowing** — see [`apply_business_context`]. Priority,
//! cost center, and advisory criteria are recorded metadata only, never
//! decision inputs. One real, tested lever beats three decorative ones.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    policy::{Policy, PolicyValidationError, TimeBound},
    task_identity::ExternalRef,
};

/// A provider's stated priority for a task. Recorded metadata only — never
/// a decision input (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

/// One fetched business-context response, summarized for the ledger and
/// for [`crate::PreflightResult`] (via `libra-governor-protocol`).
///
/// `advisory_criteria`/`priority`/`cost_center`/`external_refs` are
/// recorded metadata only — see module docs for why they never influence
/// [`crate::Policy::evaluate`]. `deadline` is the one field
/// [`apply_business_context`] actually reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusinessContextSummary {
    pub provider_id: String,
    pub schema_version: String,
    pub priority: Option<Priority>,
    pub cost_center: Option<String>,
    pub deadline: Option<OffsetDateTime>,
    /// Advisory completion criteria a provider would like verified.
    /// **Never** merged into [`crate::Policy::quality_floor`] — see module
    /// docs. Capped at 16 entries of 200 chars by
    /// `libra-governor-extension`'s wire layer before this summary is ever
    /// constructed; this type itself does not re-enforce that cap (it is
    /// a wire-layer concern, not a domain invariant).
    pub advisory_criteria: Vec<String>,
    pub external_refs: Vec<ExternalRef>,
    /// Whether [`apply_business_context`] actually narrowed the policy
    /// from this context's `deadline`. `false` when `deadline` is `None`,
    /// when the deadline was already in the past (fail-open), or when the
    /// narrowed policy failed to validate (fail-open).
    pub applied: bool,
    pub received_at: OffsetDateTime,
}

/// Narrows `base`'s time bound against `deadline`, never any other
/// dimension. Returns `(derived_policy, applied)`.
///
/// # Algorithm
///
/// ```text
/// remaining = max(0, (deadline - now).whole_seconds())
/// if remaining == 0 { fail open: return (base.clone(), false) }
/// target'   = min(time.target_secs, remaining)
/// hard'     = min(time.hard_ceiling_secs.unwrap_or(remaining), remaining)
/// elastic'  = time.elastic_ceiling_secs.map(|e| e.clamp(target', hard'))
/// deadline' = min(time.deadline.unwrap_or(deadline), deadline)
/// ```
///
/// then [`Policy::validated_at`] on the result; any validation error is
/// fail-open (log by the caller, return `(base.clone(), false)`) — a
/// narrowing is never partially applied.
///
/// `resource`/`quality_floor`/`min_confidence`/`autonomy` are copied from
/// `base` verbatim and never computed from `deadline` — see the
/// `business_context_never_touches_non_time_fields` property test.
///
/// This function itself never logs; the caller (the daemon) is
/// responsible for logging a fail-open outcome, since this crate has no
/// logging dependency (see crate docs on domain-modeling-only crates).
pub fn apply_business_context(
    base: &Policy,
    deadline: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> (Policy, bool) {
    let Some(deadline) = deadline else {
        return (base.clone(), false);
    };

    let remaining = (deadline - now).whole_seconds().max(0) as u64;
    if remaining == 0 {
        return (base.clone(), false);
    }

    let time = &base.time;
    let target_prime = time.target_secs.min(remaining);
    let hard_prime = time.hard_ceiling_secs.unwrap_or(remaining).min(remaining);
    let elastic_prime = time
        .elastic_ceiling_secs
        .map(|e| e.clamp(target_prime, hard_prime));
    let deadline_prime = match time.deadline {
        Some(existing) => existing.min(deadline),
        None => deadline,
    };

    let narrowed_time = TimeBound {
        mode: time.mode,
        target_secs: target_prime,
        elastic_ceiling_secs: elastic_prime,
        hard_ceiling_secs: Some(hard_prime),
        deadline: Some(deadline_prime),
    };

    match Policy::validated_at(
        base.name.clone(),
        base.resource.clone(),
        narrowed_time,
        base.quality_floor.clone(),
        base.min_confidence,
        base.autonomy,
        now,
    ) {
        Ok(derived) => (derived, true),
        Err(_) => (base.clone(), false),
    }
}

/// Why a candidate narrowing could not be constructed — surfaced only for
/// a caller (the daemon) that wants to log the specific reason rather
/// than the fail-open boolean alone. [`apply_business_context`] itself
/// swallows this and returns `(base.clone(), false)`; this type exists so
/// a caller that wants to log *why* can re-derive the same validation
/// error deterministically via [`try_apply_business_context`].
pub type NarrowingError = PolicyValidationError;

/// Non-swallowing variant of [`apply_business_context`], for a caller
/// (the daemon's `handle_preflight`) that wants to log the specific
/// [`PolicyValidationError`] on a fail-open outcome rather than a bare
/// boolean.
pub fn try_apply_business_context(
    base: &Policy,
    deadline: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Result<Policy, NarrowingError> {
    let Some(deadline) = deadline else {
        return Ok(base.clone());
    };
    let remaining = (deadline - now).whole_seconds().max(0) as u64;
    if remaining == 0 {
        return Ok(base.clone());
    }

    let time = &base.time;
    let target_prime = time.target_secs.min(remaining);
    let hard_prime = time.hard_ceiling_secs.unwrap_or(remaining).min(remaining);
    let elastic_prime = time
        .elastic_ceiling_secs
        .map(|e| e.clamp(target_prime, hard_prime));
    let deadline_prime = match time.deadline {
        Some(existing) => existing.min(deadline),
        None => deadline,
    };

    let narrowed_time = TimeBound {
        mode: time.mode,
        target_secs: target_prime,
        elastic_ceiling_secs: elastic_prime,
        hard_ceiling_secs: Some(hard_prime),
        deadline: Some(deadline_prime),
    };

    Policy::validated_at(
        base.name.clone(),
        base.resource.clone(),
        narrowed_time,
        base.quality_floor.clone(),
        base.min_confidence,
        base.autonomy,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion_contract::{CompletionContract, CompletionCriterion},
        confidence::Confidence,
        policy::{AutonomyBoundary, ConstraintMode, PolicyPresetInputs},
        resource_amount::ResourceAmount,
    };

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![CompletionCriterion::required(
            "required verification passes",
        )])
    }

    fn preset_inputs() -> PolicyPresetInputs {
        PolicyPresetInputs {
            resource_target: ResourceAmount::Tokens(100_000),
            time_target_secs: 3600,
            quality_floor: quality_floor(),
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1_000_000)
    }

    fn admission_rank(admission: &crate::policy::Admission) -> u8 {
        match admission {
            crate::policy::Admission::Admit => 0,
            crate::policy::Admission::ApprovalRequired(_) => 1,
            crate::policy::Admission::Deny(_) => 2,
        }
    }

    #[test]
    fn no_deadline_is_a_no_op() {
        let base = Policy::balanced(preset_inputs()).unwrap();
        let (derived, applied) = apply_business_context(&base, None, now());
        assert!(!applied);
        assert_eq!(derived, base);
    }

    #[test]
    fn a_past_deadline_fails_open() {
        let base = Policy::balanced(preset_inputs()).unwrap();
        let past = now() - time::Duration::seconds(10);
        let (derived, applied) = apply_business_context(&base, Some(past), now());
        assert!(!applied);
        assert_eq!(derived, base);
    }

    #[test]
    fn a_tight_deadline_narrows_the_time_bound() {
        // balanced: target 3600s, elastic 4500s, hard 5400s.
        let base = Policy::balanced(preset_inputs()).unwrap();
        let deadline = now() + time::Duration::seconds(1200);
        let (derived, applied) = apply_business_context(&base, Some(deadline), now());
        assert!(applied);
        assert_eq!(derived.time.target_secs, 1200);
        assert_eq!(derived.time.hard_ceiling_secs, Some(1200));
        assert_eq!(derived.time.elastic_ceiling_secs, Some(1200));
        assert_eq!(derived.time.deadline, Some(deadline));
    }

    #[test]
    fn a_far_future_deadline_never_widens_the_policy() {
        let base = Policy::balanced(preset_inputs()).unwrap();
        let far_future = now() + time::Duration::days(3650);
        let (derived, applied) = apply_business_context(&base, Some(far_future), now());
        assert!(applied);
        // The base's own ceilings are already tighter than a 10-year-out
        // deadline, so the narrowing must not raise them past the base.
        assert!(derived.time.target_secs <= base.time.target_secs);
        assert!(derived.time.hard_ceiling_secs.unwrap() <= base.time.hard_ceiling_secs.unwrap());
    }

    #[test]
    fn business_context_never_touches_non_time_fields() {
        let base = Policy::balanced(preset_inputs()).unwrap();
        let deadline = now() + time::Duration::seconds(1200);
        let (derived, applied) = apply_business_context(&base, Some(deadline), now());
        assert!(applied);
        assert_eq!(derived.resource, base.resource);
        assert_eq!(derived.quality_floor, base.quality_floor);
        assert_eq!(derived.min_confidence, base.min_confidence);
        assert_eq!(derived.autonomy, base.autonomy);
    }

    /// Property test 1: for every (policy, deadline) pair, the derived
    /// policy's target/elastic/hard ceilings are each <= the base's.
    #[test]
    fn property_derived_ceilings_never_exceed_base_ceilings() {
        let presets: Vec<Policy> = vec![
            Policy::balanced(preset_inputs()).unwrap(),
            Policy::deadline_first(preset_inputs()).unwrap(),
            Policy::cost_first(preset_inputs()).unwrap(),
            Policy::strict_budget(preset_inputs()).unwrap(),
        ];
        for base in &presets {
            for remaining_secs in [1u64, 10, 100, 600, 3600, 7200, 100_000] {
                let deadline = now() + time::Duration::seconds(remaining_secs as i64);
                let (derived, _) = apply_business_context(base, Some(deadline), now());
                assert!(derived.time.target_secs <= base.time.target_secs);
                if let (Some(d_hard), Some(b_hard)) =
                    (derived.time.hard_ceiling_secs, base.time.hard_ceiling_secs)
                {
                    assert!(d_hard <= b_hard);
                }
                if let (Some(d_elastic), Some(b_elastic)) = (
                    derived.time.elastic_ceiling_secs,
                    base.time.elastic_ceiling_secs,
                ) {
                    assert!(d_elastic <= b_elastic);
                }
            }
        }
    }

    /// Property test 2: the derived policy is never more permissive than
    /// the base on the Admit > ApprovalRequired > Deny lattice, for any
    /// given projected duration.
    #[test]
    fn property_derived_is_never_more_permissive_than_base() {
        let presets: Vec<Policy> = vec![
            Policy::balanced(preset_inputs()).unwrap(),
            Policy::deadline_first(preset_inputs()).unwrap(),
        ];
        for base in &presets {
            for remaining_secs in [10u64, 600, 3600, 10_000] {
                let deadline = now() + time::Duration::seconds(remaining_secs as i64);
                let (derived, applied) = apply_business_context(base, Some(deadline), now());
                if !applied {
                    continue;
                }
                for projected_secs in [0u64, 100, 600, 1800, 3600, 7200, 20_000] {
                    let projected_resource = base.resource.target;
                    let base_decision = base
                        .evaluate(projected_resource, projected_secs, Confidence::High)
                        .unwrap();
                    let derived_decision = derived
                        .evaluate(projected_resource, projected_secs, Confidence::High)
                        .unwrap();
                    assert!(
                        admission_rank(&derived_decision.admission)
                            >= admission_rank(&base_decision.admission),
                        "base={:?} derived={:?} at projected_secs={projected_secs} \
                         remaining_secs={remaining_secs}: derived must never be more \
                         permissive than base",
                        base_decision.admission,
                        derived_decision.admission,
                    );
                }
            }
        }
    }

    /// Property test 3: the derived output always passes
    /// `Policy::validated_at` (proven structurally by re-running
    /// validation here rather than trusting `apply_business_context`'s
    /// internal call).
    #[test]
    fn property_derived_always_validates() {
        let presets: Vec<Policy> = vec![
            Policy::balanced(preset_inputs()).unwrap(),
            Policy::deadline_first(preset_inputs()).unwrap(),
            Policy::cost_first(preset_inputs()).unwrap(),
            Policy::strict_budget(preset_inputs()).unwrap(),
        ];
        for base in &presets {
            for remaining_secs in [1u64, 5, 100, 3600, 500_000] {
                let deadline = now() + time::Duration::seconds(remaining_secs as i64);
                let (derived, applied) = apply_business_context(base, Some(deadline), now());
                if applied {
                    Policy::validated_at(
                        derived.name.clone(),
                        derived.resource.clone(),
                        derived.time,
                        derived.quality_floor.clone(),
                        derived.min_confidence,
                        derived.autonomy,
                        now(),
                    )
                    .expect("a derived policy must always independently re-validate");
                }
            }
        }
    }

    /// Property test 4: resource/quality_floor/min_confidence/autonomy are
    /// always equal to base's, unconditionally — the R2 trust-boundary
    /// invariant this module exists to enforce.
    #[test]
    fn property_r2_advisory_fields_never_reach_the_policy() {
        let base = Policy::deadline_first(preset_inputs()).unwrap();
        for remaining_secs in [1u64, 100, 100_000] {
            let deadline = now() + time::Duration::seconds(remaining_secs as i64);
            let (derived, _) = apply_business_context(&base, Some(deadline), now());
            assert_eq!(derived.resource, base.resource);
            assert_eq!(derived.quality_floor, base.quality_floor);
            assert_eq!(derived.min_confidence, base.min_confidence);
            assert_eq!(derived.autonomy, base.autonomy);
        }
    }

    #[test]
    fn approval_mode_with_no_prior_hard_ceiling_gains_one_from_narrowing() {
        // Approval mode may have `hard_ceiling_secs: None` (open-ended).
        // Narrowing must introduce a hard ceiling from `remaining` rather
        // than leaving the constraint unbounded — this only ever makes
        // the constraint tighter (Deny becomes reachable), never looser.
        let base = Policy::validated(
            "test-approval-open-ended",
            crate::policy::ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::Tokens(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(1000),
            },
            TimeBound {
                mode: ConstraintMode::Approval,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: None,
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap();

        let deadline = now() + time::Duration::seconds(300);
        let (derived, applied) = apply_business_context(&base, Some(deadline), now());
        assert!(applied);
        assert_eq!(derived.time.hard_ceiling_secs, Some(300));
    }

    /// Field-set pinning test (R1-style): `BusinessContextSummary` must
    /// never carry a field that maps onto `min_confidence`, `autonomy`,
    /// or any resource ceiling — the same discipline
    /// `task_features_field_set_contains_no_outcome_or_actual_fields`
    /// applies to `TaskFeatures`.
    #[test]
    fn business_context_field_set_contains_no_task_feature_or_policy_field() {
        let sample = BusinessContextSummary {
            provider_id: "example-provider".to_string(),
            schema_version: "libra.extension.v1".to_string(),
            priority: Some(Priority::High),
            cost_center: Some("eng-platform".to_string()),
            deadline: Some(now()),
            advisory_criteria: vec!["check the thing".to_string()],
            external_refs: vec![],
            applied: true,
            received_at: now(),
        };
        let json = serde_json::to_value(&sample).unwrap();
        let fields: std::collections::BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        for forbidden in [
            "min_confidence",
            "autonomy",
            "resource",
            "hard_ceiling",
            "elastic_ceiling",
            "quality_floor",
            "protected_criteria",
            "prompt",
            "cwd",
        ] {
            assert!(
                !fields.contains(forbidden),
                "BusinessContextSummary must never carry a field named {forbidden:?}"
            );
        }
    }
}
