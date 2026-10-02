//! [`RegimeKey`] — the execution-regime identity a calibration sample was
//! produced under (HORO-1671).
//!
//! # Why this is a separate type from [`crate::EconomicAttribution`]
//!
//! `EconomicAttribution`/`UnknownReason` (HORO-1666) answer *who owns an
//! economic fact* and are pinned at `ECONOMIC_ATTRIBUTION_CONTRACT_VERSION
//! = 1`. This type answers a different question — *was this sample
//! produced under comparable conditions to the current one* — and needs
//! its own `MixedWithinTask` cause that has no analogue there. Widening
//! `UnknownReason` to carry it would widen that pinned contract's wire
//! vocabulary for an unrelated concern. The two types are deliberately
//! not unified.
//!
//! # Comparability vs. bucketing vs. reported-only
//!
//! Three distinct roles, easy to conflate:
//!
//! - **Comparability** (on [`RegimeKey`]): what the execution *runs
//!   with* — model, harness, pricing, enforcement tier, schema versions.
//!   Two samples that differ here are not safely poolable for confidence.
//! - **Bucketing** (on [`crate::TaskFeatures`], unchanged by this
//!   ticket): what the task *is* — repo, topology. Already has its own
//!   hierarchical backoff ladder; comparing on it here too would
//!   double-penalize the same fact.
//! - **Reported-only** (on [`RegimeProvenance`], not [`RegimeKey`]):
//!   `topology` (the bucket ladder's own dimension, see above) and
//!   `cache_class` (a per-task session-shape property — a long
//!   conversation vs. a fresh one — not a property of the execution
//!   regime; comparing on it would cry drift between two consecutive
//!   tasks run under an identical model/pricing/tier).
//!
//! # The positive-evidence rule
//!
//! [`RegimeKey::comparison`] treats a dimension as evidence of change
//! **only** when both sides are [`DimensionValue::Known`] and differ.
//! [`DimensionValue::Unavailable`] on either side — for *any* reason,
//! including [`DimensionUnavailable::MixedWithinTask`] — is absence of
//! evidence, never evidence of absence. This is the single rule that
//! keeps a latent dimension (today: `model`, `harness`, `harness_version`,
//! `effort` are all `Unavailable` on both the current regime and every
//! historical receipt) from manufacturing false drift merely because
//! Libra does not yet observe it.
//!
//! Comparability is **not transitive**: A (`model: Unknown`) can be
//! comparable to both B (`model: X`) and C (`model: Y`) while B and C are
//! not comparable to each other. Cohort *identity* therefore uses exact
//! [`Eq`] (reflexive and transitive — a deterministic partition);
//! comparability is used only to decide whether a cohort's evidence
//! counts toward the *active* regime's confidence. The two must never be
//! conflated.
//!
//! # Privacy
//!
//! Every [`DimensionValue::Known`] token is a short, closed-vocabulary
//! identifier (a model name, a pricing version, a schema tag) —
//! constructed only through [`RegimeKey::builder`]/the typed `From`
//! conversions in this module, never written ad hoc by a caller. There is
//! no field here for prompt text, tool output, or a raw file path.

use serde::{Deserialize, Serialize};

use crate::capability::EnforcementTier;

/// Schema tag for the [`RegimeKey`]/[`RegimeProvenance`] wire shape
/// itself. Bump when a field is added/removed/renamed on these types.
pub const REGIME_SCHEMA_VERSION: &str = "regime-v1";

/// Coarse, deliberately slow-moving estimator-regime schema token —
/// **not** the same as [`crate::ESTIMATOR_VERSION`]. Bump
/// `ESTIMATOR_VERSION` for any traceability-worthy estimator change;
/// bump this constant *only* when the quantile/bucketing logic that
/// shapes the output *distribution* changes. Keeping the two separate is
/// what stops this ticket's own `ESTIMATOR_VERSION` bump
/// (`v3-tiered-confidence` -> `v4-regime-aware`) from reading as a
/// regime change and self-invalidating every pre-upgrade calibration
/// sample on the very release that introduces regime awareness.
pub const ESTIMATOR_REGIME_SCHEMA: &str = "esr-v1";

/// One execution-regime dimension's value: either observed, or honestly
/// absent with a named reason. There is no silent "zero"/"default" — see
/// module docs on the positive-evidence rule this enables.
///
/// Adjacently tagged (`tag`/`content`), not internally tagged: an
/// internally-tagged representation cannot serialize a newtype variant
/// whose payload is a bare `String` (serde has no map to merge the tag
/// field into) — `Known(String)` would fail exactly that way under
/// `#[serde(tag = "state")]` alone. Caught in integration testing against
/// the real daemon, not by any unit test working with in-memory values
/// only — see the `serde_json::to_string` round-trip test added here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum DimensionValue {
    Known(String),
    Unavailable(DimensionUnavailable),
}

impl DimensionValue {
    pub fn known(token: impl Into<String>) -> Self {
        DimensionValue::Known(token.into())
    }

    pub fn is_known(&self) -> bool {
        matches!(self, DimensionValue::Known(_))
    }
}

/// Why a [`DimensionValue`] is [`DimensionValue::Unavailable`] — named at
/// the type level so "we don't know" is never confused with "it doesn't
/// apply" or "it's a historical row from before this contract existed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionUnavailable {
    /// The host/harness exposes no primitive for this dimension at all
    /// (e.g. reasoning/effort level — no source exists anywhere today).
    HostExposesNoPrimitive,
    /// The harness may expose this, but Libra does not yet capture it on
    /// the path that would need to (e.g. harness identity on
    /// `Request::Finalize`).
    NotWiredByLibra,
    /// No gateway is configured, so there is no pricing/enforcement-tier
    /// fact to report — never substitute the bare build-time constant,
    /// which would assert a price nothing enforced.
    NoGatewayConfigured,
    /// The task's gateway requests spanned more than one distinct value
    /// for this dimension (e.g. a pricing-version rollover mid-task).
    MixedWithinTask { distinct: u32 },
    /// This row predates HORO-1671 — never backfilled, never guessed.
    PreRegimeRecord,
}

/// The execution-regime identity a calibration sample was produced
/// under — exactly the dimensions that participate in comparability and
/// cohort identity. See module docs for what is deliberately *not* here
/// (`topology`, `cache_class` — see [`RegimeProvenance`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RegimeKey {
    pub model: DimensionValue,
    pub harness: DimensionValue,
    pub harness_version: DimensionValue,
    pub effort: DimensionValue,
    pub pricing_version: DimensionValue,
    pub enforcement_tier: DimensionValue,
    pub estimator_regime_schema: DimensionValue,
    pub feature_schema: DimensionValue,
}

/// Builder — the only place a [`RegimeKey`]'s token strings are written.
/// Accepts typed inputs and canonicalizes them, so no caller writes a raw
/// token by hand.
#[derive(Debug, Clone, Default)]
pub struct RegimeKeyBuilder {
    model: Option<DimensionValue>,
    harness: Option<DimensionValue>,
    harness_version: Option<DimensionValue>,
    effort: Option<DimensionValue>,
    pricing_version: Option<DimensionValue>,
    enforcement_tier: Option<DimensionValue>,
    feature_schema: Option<DimensionValue>,
}

impl RegimeKey {
    pub fn builder() -> RegimeKeyBuilder {
        RegimeKeyBuilder::default()
    }

    /// A key representing a pre-HORO-1671 row: every dimension
    /// [`DimensionUnavailable::PreRegimeRecord`]. Never backfilled with a
    /// guess.
    pub fn pre_regime_record() -> Self {
        let unavailable = DimensionValue::Unavailable(DimensionUnavailable::PreRegimeRecord);
        RegimeKey {
            model: unavailable.clone(),
            harness: unavailable.clone(),
            harness_version: unavailable.clone(),
            effort: unavailable.clone(),
            pricing_version: unavailable.clone(),
            enforcement_tier: unavailable.clone(),
            estimator_regime_schema: unavailable.clone(),
            feature_schema: unavailable,
        }
    }

    fn field(&self, dim: RegimeDimension) -> &DimensionValue {
        match dim {
            RegimeDimension::Model => &self.model,
            RegimeDimension::Harness => &self.harness,
            RegimeDimension::HarnessVersion => &self.harness_version,
            RegimeDimension::Effort => &self.effort,
            RegimeDimension::PricingVersion => &self.pricing_version,
            RegimeDimension::EnforcementTier => &self.enforcement_tier,
            RegimeDimension::EstimatorRegimeSchema => &self.estimator_regime_schema,
            RegimeDimension::FeatureSchema => &self.feature_schema,
        }
    }

    /// The positive-evidence comparison rule — see module docs.
    pub fn comparison(&self, other: &RegimeKey) -> RegimeComparison {
        let mut differing = Vec::new();
        for dim in RegimeDimension::ALL {
            if let (DimensionValue::Known(a), DimensionValue::Known(b)) =
                (self.field(dim), other.field(dim))
            {
                if a != b {
                    differing.push(dim);
                }
            }
        }
        if differing.is_empty() {
            RegimeComparison::Comparable
        } else {
            RegimeComparison::Incompatible { differing }
        }
    }
}

impl RegimeKeyBuilder {
    pub fn model(mut self, model: Option<&str>) -> Self {
        self.model = Some(match model {
            Some(m) => DimensionValue::known(m),
            None => DimensionValue::Unavailable(DimensionUnavailable::HostExposesNoPrimitive),
        });
        self
    }

    pub fn harness(mut self, harness: Option<crate::AgentKind>) -> Self {
        self.harness = Some(match harness {
            Some(h) => DimensionValue::known(h.as_tool_provider()),
            None => DimensionValue::Unavailable(DimensionUnavailable::NotWiredByLibra),
        });
        self
    }

    pub fn harness_version(mut self, version: Option<&str>) -> Self {
        self.harness_version = Some(match version {
            Some(v) => DimensionValue::known(v),
            None => DimensionValue::Unavailable(DimensionUnavailable::HostExposesNoPrimitive),
        });
        self
    }

    pub fn effort(mut self, effort: Option<&str>) -> Self {
        self.effort = Some(match effort {
            Some(e) => DimensionValue::known(e),
            None => DimensionValue::Unavailable(DimensionUnavailable::HostExposesNoPrimitive),
        });
        self
    }

    /// `pricing_version`/`tier` share one gateway-configuration gate:
    /// pass `None` for both when no gateway is configured, rather than
    /// asserting the build-time pricing constant against nothing.
    pub fn gateway_pricing(mut self, pricing_version: Option<&str>) -> Self {
        self.pricing_version = Some(match pricing_version {
            Some(v) => DimensionValue::known(v),
            None => DimensionValue::Unavailable(DimensionUnavailable::NoGatewayConfigured),
        });
        self
    }

    /// Multiple distinct pricing versions observed within one task.
    pub fn gateway_pricing_mixed(mut self, distinct: u32) -> Self {
        self.pricing_version = Some(DimensionValue::Unavailable(
            DimensionUnavailable::MixedWithinTask { distinct },
        ));
        self
    }

    pub fn enforcement_tier(mut self, tier: Option<EnforcementTier>) -> Self {
        self.enforcement_tier = Some(match tier {
            Some(t) => DimensionValue::known(enforcement_tier_token(t)),
            None => DimensionValue::Unavailable(DimensionUnavailable::NoGatewayConfigured),
        });
        self
    }

    pub fn enforcement_tier_mixed(mut self, distinct: u32) -> Self {
        self.enforcement_tier = Some(DimensionValue::Unavailable(
            DimensionUnavailable::MixedWithinTask { distinct },
        ));
        self
    }

    pub fn feature_schema(mut self, schema: impl Into<String>) -> Self {
        self.feature_schema = Some(DimensionValue::known(schema));
        self
    }

    pub fn build(self) -> RegimeKey {
        let unset = || DimensionValue::Unavailable(DimensionUnavailable::HostExposesNoPrimitive);
        RegimeKey {
            model: self.model.unwrap_or_else(unset),
            harness: self.harness.unwrap_or_else(unset),
            harness_version: self.harness_version.unwrap_or_else(unset),
            effort: self.effort.unwrap_or_else(unset),
            pricing_version: self.pricing_version.unwrap_or(DimensionValue::Unavailable(
                DimensionUnavailable::NoGatewayConfigured,
            )),
            enforcement_tier: self.enforcement_tier.unwrap_or(DimensionValue::Unavailable(
                DimensionUnavailable::NoGatewayConfigured,
            )),
            estimator_regime_schema: DimensionValue::known(ESTIMATOR_REGIME_SCHEMA),
            feature_schema: self.feature_schema.unwrap_or_else(unset),
        }
    }
}

fn enforcement_tier_token(tier: EnforcementTier) -> &'static str {
    match tier {
        EnforcementTier::GatewayMetered => "gateway_metered",
        EnforcementTier::GatewayObservedQuota => "gateway_observed_quota",
        EnforcementTier::HooksOnly => "hooks_only",
    }
}

/// Every [`RegimeKey`] dimension that participates in comparison. The
/// sole shared source of "all dimensions" for [`RegimeKey::comparison`]
/// and this module's exhaustiveness tripwire — see
/// [`Self::all_variants_is_exhaustive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegimeDimension {
    Model,
    Harness,
    HarnessVersion,
    Effort,
    PricingVersion,
    EnforcementTier,
    EstimatorRegimeSchema,
    FeatureSchema,
}

impl RegimeDimension {
    pub const ALL: [RegimeDimension; 8] = [
        RegimeDimension::Model,
        RegimeDimension::Harness,
        RegimeDimension::HarnessVersion,
        RegimeDimension::Effort,
        RegimeDimension::PricingVersion,
        RegimeDimension::EnforcementTier,
        RegimeDimension::EstimatorRegimeSchema,
        RegimeDimension::FeatureSchema,
    ];

    /// Compile-time-only exhaustiveness check — never called at runtime.
    /// Adding a variant without adding it to [`Self::ALL`] (and to
    /// [`RegimeKey::field`]/[`RegimeKey`] itself) fails this match.
    #[allow(dead_code)]
    fn all_variants_is_exhaustive(dim: RegimeDimension) {
        match dim {
            RegimeDimension::Model
            | RegimeDimension::Harness
            | RegimeDimension::HarnessVersion
            | RegimeDimension::Effort
            | RegimeDimension::PricingVersion
            | RegimeDimension::EnforcementTier
            | RegimeDimension::EstimatorRegimeSchema
            | RegimeDimension::FeatureSchema => {}
        }
    }
}

/// The result of comparing two [`RegimeKey`]s. See module docs — this is
/// NOT a total order and NOT transitive; use only to decide applicability
/// to the current regime, never to derive cohort identity (use exact
/// [`Eq`] for that).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegimeComparison {
    Comparable,
    Incompatible { differing: Vec<RegimeDimension> },
}

impl RegimeComparison {
    pub fn is_comparable(&self) -> bool {
        matches!(self, RegimeComparison::Comparable)
    }
}

/// A per-task cache-usage shape, derived from summed gateway-request
/// token counters. Reported-only (see module docs) — never part of
/// [`RegimeKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheClass {
    NoCacheObserved,
    CacheReadDominant,
    CacheWritingDominant,
}

impl CacheClass {
    /// Ties (including `0, 0`) resolve to [`Self::NoCacheObserved`] —
    /// the honest "nothing to classify" answer, not a guess toward
    /// either dominant class.
    pub fn from_counts(creation: u64, read: u64) -> Self {
        if creation == 0 && read == 0 {
            CacheClass::NoCacheObserved
        } else if read > creation {
            CacheClass::CacheReadDominant
        } else if creation > read {
            CacheClass::CacheWritingDominant
        } else {
            CacheClass::NoCacheObserved
        }
    }

    fn token(self) -> &'static str {
        match self {
            CacheClass::NoCacheObserved => "no_cache_observed",
            CacheClass::CacheReadDominant => "cache_read_dominant",
            CacheClass::CacheWritingDominant => "cache_writing_dominant",
        }
    }
}

/// Everything recorded about the execution regime a sample was produced
/// under: the comparable [`RegimeKey`] plus two reported-only dimensions
/// that must never affect comparison or cohort identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegimeProvenance {
    pub schema_version: String,
    pub key: RegimeKey,
    pub topology: DimensionValue,
    pub cache_class: DimensionValue,
}

impl RegimeProvenance {
    pub fn new(
        key: RegimeKey,
        topology: Option<crate::BuildTopology>,
        cache_class: CacheClass,
    ) -> Self {
        RegimeProvenance {
            schema_version: REGIME_SCHEMA_VERSION.to_string(),
            key,
            topology: match topology {
                Some(t) => DimensionValue::known(format!("{t:?}")),
                None => DimensionValue::Unavailable(DimensionUnavailable::HostExposesNoPrimitive),
            },
            cache_class: DimensionValue::known(cache_class.token()),
        }
    }

    /// A provenance representing a pre-HORO-1671 row — every dimension
    /// honestly `PreRegimeRecord`/absent.
    pub fn pre_regime_record() -> Self {
        RegimeProvenance {
            schema_version: REGIME_SCHEMA_VERSION.to_string(),
            key: RegimeKey::pre_regime_record(),
            topology: DimensionValue::Unavailable(DimensionUnavailable::PreRegimeRecord),
            cache_class: DimensionValue::Unavailable(DimensionUnavailable::PreRegimeRecord),
        }
    }
}

/// What [`crate::Estimate`] carries forward about the regime it was
/// computed under, plus the sample counts that fed its confidence —
/// HORO-1669's "confidence/calibration provenance" AC is this field,
/// unchanged, flowing through [`crate::RemainingEstimate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegimeBasis {
    pub provenance: RegimeProvenance,
    pub in_regime_sample_count: usize,
    pub out_of_regime_sample_count: usize,
}

impl Default for RegimeBasis {
    /// The honest default for a historical `Estimate` blob that predates
    /// this field (`#[serde(default)]`) — never guessed as "comparable to
    /// everything" or "comparable to nothing", just explicitly
    /// pre-regime.
    fn default() -> Self {
        RegimeBasis {
            provenance: RegimeProvenance::pre_regime_record(),
            in_regime_sample_count: 0,
            out_of_regime_sample_count: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentKind;

    fn known_key(model: &str) -> RegimeKey {
        RegimeKey::builder()
            .model(Some(model))
            .harness(Some(AgentKind::ClaudeCode))
            .gateway_pricing(Some("pricing-2026-09-static-v1"))
            .enforcement_tier(Some(EnforcementTier::GatewayMetered))
            .feature_schema("fs-v1")
            .build()
    }

    #[test]
    fn unknown_dimension_is_never_evidence_of_a_regime_change_either_direction() {
        let unavailable = RegimeKey::builder().feature_schema("fs-v1").build();
        let known = known_key("claude-sonnet-5");
        assert_eq!(unavailable.comparison(&known), RegimeComparison::Comparable);
        assert_eq!(known.comparison(&unavailable), RegimeComparison::Comparable);
        assert_eq!(
            unavailable.comparison(&unavailable),
            RegimeComparison::Comparable
        );
    }

    #[test]
    fn mixed_within_task_is_not_evidence_of_a_regime_change() {
        let mixed = RegimeKey::builder()
            .gateway_pricing_mixed(2)
            .feature_schema("fs-v1")
            .build();
        let known = known_key("claude-sonnet-5");
        assert!(mixed.comparison(&known).is_comparable());
    }

    #[test]
    fn differing_known_dimension_is_incompatible_and_names_it() {
        let a = known_key("claude-sonnet-5");
        let b = known_key("claude-opus-5");
        match a.comparison(&b) {
            RegimeComparison::Incompatible { differing } => {
                assert_eq!(differing, vec![RegimeDimension::Model]);
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn comparability_is_not_transitive() {
        let unavailable = RegimeKey::builder().feature_schema("fs-v1").build();
        let a = known_key("model-a");
        let b = known_key("model-b");
        assert!(unavailable.comparison(&a).is_comparable());
        assert!(unavailable.comparison(&b).is_comparable());
        assert!(!a.comparison(&b).is_comparable());
    }

    #[test]
    fn pre_regime_record_is_comparable_to_a_fully_known_regime() {
        let pre = RegimeKey::pre_regime_record();
        let known = known_key("claude-sonnet-5");
        assert!(pre.comparison(&known).is_comparable());
    }

    #[test]
    fn cache_class_ties_resolve_to_no_cache_observed() {
        assert_eq!(CacheClass::from_counts(0, 0), CacheClass::NoCacheObserved);
        assert_eq!(CacheClass::from_counts(5, 5), CacheClass::NoCacheObserved);
        assert_eq!(
            CacheClass::from_counts(2, 10),
            CacheClass::CacheReadDominant
        );
        assert_eq!(
            CacheClass::from_counts(10, 2),
            CacheClass::CacheWritingDominant
        );
    }

    #[test]
    fn regime_key_field_set_contains_no_raw_content() {
        let value = serde_json::to_value(known_key("claude-sonnet-5")).unwrap();
        assert_no_forbidden_substrings(&value);
    }

    fn assert_no_forbidden_substrings(value: &serde_json::Value) {
        const FORBIDDEN: [&str; 6] = ["prompt", "path", "source", "text", "content", "body"];
        match value {
            serde_json::Value::Object(map) => {
                for (key, v) in map {
                    let lower = key.to_lowercase();
                    for forbidden in FORBIDDEN {
                        assert!(
                            !lower.contains(forbidden),
                            "field {key:?} looks content-shaped (contains {forbidden:?})"
                        );
                    }
                    assert_no_forbidden_substrings(v);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_forbidden_substrings(item);
                }
            }
            serde_json::Value::String(s) => {
                let lower = s.to_lowercase();
                for forbidden in ["prompt", "source_text"] {
                    assert!(
                        !lower.contains(forbidden),
                        "value {s:?} looks content-shaped"
                    );
                }
            }
            _ => {}
        }
    }

    /// Pins the real integration failure this ticket hit against the
    /// live daemon: an internally-tagged `DimensionValue` could not
    /// serialize `Known(String)` at all (`serde_json::to_string` failed
    /// with "cannot serialize tagged newtype variant ... containing a
    /// string"), which no in-memory-only unit test caught until a real
    /// `hook stop` round-trip through the ledger surfaced it.
    #[test]
    fn dimension_value_known_serializes_and_round_trips() {
        let value = DimensionValue::known("claude-sonnet-5");
        let json = serde_json::to_string(&value).unwrap();
        let back: DimensionValue = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);

        let unavailable = DimensionValue::Unavailable(DimensionUnavailable::NoGatewayConfigured);
        let json = serde_json::to_string(&unavailable).unwrap();
        let back: DimensionValue = serde_json::from_str(&json).unwrap();
        assert_eq!(unavailable, back);
    }

    #[test]
    fn serde_round_trip_for_regime_key_and_provenance() {
        let key = known_key("claude-sonnet-5");
        let json = serde_json::to_string(&key).unwrap();
        let back: RegimeKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, back);

        let provenance = RegimeProvenance::new(
            key,
            Some(crate::BuildTopology::Cargo),
            CacheClass::CacheReadDominant,
        );
        let json = serde_json::to_string(&provenance).unwrap();
        let back: RegimeProvenance = serde_json::from_str(&json).unwrap();
        assert_eq!(provenance, back);
    }

    #[test]
    fn regime_basis_default_is_honestly_pre_regime() {
        let basis = RegimeBasis::default();
        assert_eq!(basis.provenance.key, RegimeKey::pre_regime_record());
        assert_eq!(basis.in_regime_sample_count, 0);
    }

    #[test]
    fn historical_estimate_json_without_the_regime_field_deserializes_via_default() {
        // Simulates a pre-HORO-1671 `plans.estimate_json` blob: no
        // `regime` key at all.
        #[derive(Serialize, Deserialize)]
        struct Probe {
            #[serde(default)]
            regime: RegimeBasis,
        }
        let probe: Probe = serde_json::from_str("{}").unwrap();
        assert_eq!(probe.regime, RegimeBasis::default());
    }
}
