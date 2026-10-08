//! `libra-governor-domain` — the canonical, harness-agnostic domain types
//! Libra's daemon, ledger, and estimator are built on.
//!
//! # Privacy invariant
//!
//! None of the types in this crate carry a field for raw prompt text or
//! raw tool output content. [`ExecutionEventKind::ToolInvoked`] carries a
//! tool *name* only; [`ExecutionOutcome`] carries evidence *references*
//! (URLs, IDs, paths), never inlined content. This is a structural
//! guarantee, not a convention some caller could violate by populating an
//! optional field — the field simply does not exist on the type.
//!
//! [`TaskFeatures`] (HORO-1130) extends this same guarantee to
//! estimator bucketing: every one of its fields is an irreversible
//! derived scalar — a count, a boolean, a length, a small closed-set
//! enum, or a truncated SHA-256 digest of a canonicalized path — never
//! the prompt text, tool output, or file path it was computed from. A
//! reviewer does not need to trust a convention here either: there is no
//! `String`-typed field on [`TaskFeatures`] that could hold raw content
//! except `repo_key` (a hex digest, not a path) and `model` (a short
//! model identifier, never derived from prompt or file content).
//!
//! # The economic unit
//!
//! [`TaskIdentity`], not a session or a token, is the anchor every other
//! type in this crate attaches to. See
//! `docs/adr/0002-task-not-session-as-economic-unit.md` for why.
//!
//! # Economic attribution ("who owns this economic fact")
//!
//! [`EconomicAttribution`]/[`EconomicEvent`] (HORO-1666) are Libra's own
//! "who owns this economic fact" contract, built on top of — never
//! duplicating — the shared [`ExecutionIdentity`] envelope. See
//! `docs/adr/0007-economic-attribution-vs-execution-identity.md`.

mod agent;
mod business_context;
mod capability;
mod completion_contract;
mod confidence;
mod economic_attribution;
mod economic_event;
mod economic_ingest;
mod economic_rollup;
mod economic_truth;
mod estimate;
mod execution_event;
mod execution_identity;
mod execution_outcome;
mod execution_plan;
mod execution_receipt;
mod external_approval;
mod outcome_attestation;
pub mod pacing;
mod policy;
mod progressive;
mod quota_window;
mod regime;
mod replan;
mod replay;
mod reservation;
mod resource_account;
mod resource_amount;
mod shared_pool;
mod task_features;
mod task_identity;

pub use agent::{
    AgentCapabilities, AgentKind, Capability, CapabilityGap, NormalizedEventKind,
    AGENT_ADAPTER_CONTRACT_VERSION,
};
pub use business_context::{
    apply_business_context, try_apply_business_context, BusinessContextSummary, NarrowingError,
    Priority,
};
pub use capability::{
    CredentialCustody, EnforcementCapabilities, EnforcementTier, MonetaryEnforcement,
    NoMonetaryCap, UsageAccounting, CAPABILITY_SCHEMA_VERSION,
};
pub use completion_contract::{CompletionContract, CompletionCriterion};
pub use confidence::{Confidence, MIN_CLASS_SAMPLES};
pub use economic_attribution::{
    Attributed, DimensionKey, DimensionSource, EconomicAttribution, EconomicDimension,
    GatewayRequestId, ModelRequest, OrganizationId, PrincipalId, ProvenParent, UnknownReason,
    ECONOMIC_ATTRIBUTION_CONTRACT_VERSION,
};
pub use economic_event::{
    EconomicEvent, EconomicEventError, EconomicEventId, EconomicScope, FactRole, ResourceBasis,
    ResourceFact, TruthStrength, EXECUTION_CHAIN,
};
pub use economic_ingest::{
    deltas_from_snapshots, events_from_gateway_request, CounterKind, GatewayRequestObservation,
    HostCounterSnapshot,
};
pub use economic_rollup::{
    exclusive_spend, inclusive_spend, project, AgentLineage, Projection, RollupError, SpendTotals,
    Subtotal,
};
pub use economic_truth::{
    Allocation, AmountScope, Availability, CheckId, CheckOutcome, CompletionReservePosture,
    CustodyNode, CustodyTree, EconomicCategories, EconomicTruth, Forecast, LeaseHolds,
    NoEconomicBasis, NodeLineage, PartialBucket, Reconciliation, ReconciliationCheck,
    ReleasedCapacity, ScopeFilter, ScopeResolution, ScopedAmount, SelectorEcho, SettledSpend,
    Totals, TreeBudget, Truncation, TruthProvenance, TruthSource, Unattributed, UnattributedReason,
    ECONOMIC_TRUTH_SCHEMA_VERSION,
};
pub use estimate::{Estimate, ESTIMATOR_VERSION};
pub use execution_event::{ExecutionEvent, ExecutionEventKind};
pub use execution_identity::{
    is_valid_tool_provider, ExecutionIdentity, ExecutionIdentityBuilder, ExecutionIdentityError,
    LineageStatus, Scope, ScopeIdentityMissing, EXECUTION_IDENTITY_ENVELOPE_VERSION,
};
pub use execution_outcome::ExecutionOutcome;
pub use execution_plan::{ExecutionPlan, PlanId};
pub use execution_receipt::{ExecutionReceipt, ReservationEvidence};
pub use external_approval::{apply_external_approval, ExternalApproval, ExternalVerdict};
pub use outcome_attestation::{AttestationSource, OutcomeAttestation};
pub use policy::{
    preset_by_name, Admission, ApprovalRequest, AutonomyBoundary, ConstraintMode,
    ConstraintOutcome, DenyReason, Policy, PolicyDecision, PolicyEvaluationError,
    PolicyPresetInputs, PolicyValidationError, PresetError, ResourceBound, TimeBound,
    NAMED_PRESETS, POLICY_SCHEMA_VERSION,
};
pub use progressive::{
    propose_runtime_decision, replan_cost_benefit_from_remaining, Feasibility, FeasibilityBound,
    MissingEvidence, NoSpendBasis, ProgressEvidence, ProposedAction, RemainingDuration,
    RemainingResource, RemainingWorkEstimate, ReplanCostInputs, ReplanEconomicsInsufficient,
    RuntimeDecision, RuntimeDecisionProposal, Shadow, ShadowDecisionSummary, SpendScope,
    SpendSoFar, StopReason, MIN_CONDITIONAL_SAMPLES, REMAINING_WORK_SCHEMA_VERSION,
    RUNTIME_DECISION_SCHEMA_VERSION,
};
pub use quota_window::{
    decode_provider_snapshot, decode_quota_window, AlignedPeriod, BlockingStatus, BucketState,
    CreditNamespace, DecodedProviderSnapshot, DecodedQuotaWindow, EntitlementSource,
    GaugeFreshness, GaugeReading, GaugeState, IanaTimeZone, IndeterminateReason, OutstandingHold,
    PeriodState, PoolId, ProviderAccount, ProviderSnapshot, QuotaAmount, QuotaEvidence, QuotaScope,
    QuotaSubject, QuotaUnit, QuotaUsage, QuotaWindow, QuotaWindowError, QuotaWindowId, Relief,
    ResetWeekday, StaleReason, WallClockTime, WindowEvaluation, WindowKind, WindowState,
    WorkingHours, WorkingHoursError, QUOTA_WINDOW_SCHEMA_VERSION,
};
pub use regime::{
    CacheClass, DimensionUnavailable, DimensionValue, RegimeBasis, RegimeComparison,
    RegimeDimension, RegimeKey, RegimeKeyBuilder, RegimeProvenance, ESTIMATOR_REGIME_SCHEMA,
    REGIME_SCHEMA_VERSION,
};
pub use replan::{
    evaluate_hysteresis, evaluate_replan_cost_against_policy, possible_tool_loop, should_replan,
    tool_call_count_is_material, HysteresisOutcome, RemainingEstimate, ReplanAssumptions,
    ReplanCostBenefit, ReplanDecisionError, ReplanHysteresisConfig, ReplanHysteresisState,
    ReplanId, ReplanReason, ReplanRecord, ReplanTier, ReplanTriggerKind,
    ABSOLUTE_TOOL_CALL_COUNT_FALLBACK, DEFAULT_LOOP_STREAK_THRESHOLD,
    DETERMINISTIC_WIDENING_FACTOR, REPLAN_SCHEMA_VERSION, TOOL_CALL_COUNT_MATERIAL_MULTIPLIER,
};
pub use replay::{
    aggregate_regret, replay_point, replay_trajectory, AggregateRegret, AggregationError,
    AlternateOutcomeEffect, CounterfactualDecision, DecisionPoint, DimensionVerdict, Disagreement,
    DisagreementCounts, FirstRefusal, PersistedPins, PinComparison, PinDimension, PinDrift,
    PolicyComparison, PostHocRegret, ReplayDimension, ReplayEligibility, ReplayNoBasis, ReplayPins,
    ReplayedAdmission, ResourceNoBasis, ResourceProjection, TimeProjection, TrajectoryRegret,
    UnpinnedReason, MIN_REPLAY_SAMPLES, REPLAY_PINS_SCHEMA_VERSION,
};
pub use reservation::{
    completion_reserve_for, BudgetSnapshot, CompletionReserveBasis, CompletionReserveEstimate,
    LeaseKind, Reservation, ReservationClass, ReservationId, ReservationState, TaskBudget,
    COMPLETION_RESERVE_BASE_FRACTION, COMPLETION_RESERVE_MAX_FRACTION,
    COMPLETION_RESERVE_PER_REQUIRED_CRITERION, RESERVATION_SCHEMA_VERSION,
};
pub use resource_account::{
    AccountCapacity, AccountId, AccountLevel, AccountProvenance, AccountState, AllocationAuthority,
    EnforcementScope, ResourceAccount, RESOURCE_ACCOUNT_SCHEMA_VERSION,
};
pub use resource_amount::{Headroom, ResourceAmount, ResourceKind};
pub use shared_pool::{
    PoolAdmission, PoolProviderSnapshot, QuotaPool, SharedPoolReservation, SharedPoolReservationId,
    SHARED_POOL_RESERVATION_SCHEMA_VERSION,
};
pub use task_features::{BucketTier, BuildTopology, TaskFeatures, FEATURE_SCHEMA_VERSION};
pub use task_identity::{ExternalRef, TaskId, TaskIdentity};
pub mod execution_association;
pub use execution_association::{
    AssociationUnavailable, ExecutionPosition, ExecutionTarget, EXECUTION_ASSOCIATION_VERSION,
};
