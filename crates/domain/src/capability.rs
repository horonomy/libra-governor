//! [`EnforcementCapabilities`] — what Libra can honestly claim to enforce
//! for a given integration mode (HORO-1144).
//!
//! # Why a typed tier rather than a boolean
//!
//! "Supported: yes/no" is a lie in both directions. Claude Code talking to
//! the Anthropic API with a BYOK key routed through the gateway gets a
//! genuine pre-spend monetary boundary. Claude Code on a Max subscription
//! routed through the same gateway gets real *token* observation and no
//! monetary cap at all, because the provider does not expose that
//! subscription's own accounting to us. Claude Code with hooks and no
//! gateway gets neither — it gets advice. Collapsing those three into one
//! boolean either overclaims for two of them or underclaims for one.
//!
//! So the capability model is a small closed set of types, and the code
//! that would overclaim is refused rather than documented away: the
//! gateway's own configuration validation will not start a
//! [`EnforcementTier::GatewayObservedQuota`] deployment against a
//! [`crate::ResourceKind::Usd`] admission policy, because that pairing
//! would advertise a hard monetary cap nothing here can honor. See
//! `docs/adr/0003-gateway-enforcement-boundary.md`.
//!
//! # The tier is configured, never negotiated
//!
//! Nothing in this module sniffs, probes, or infers a tier at runtime. A
//! tier is a consequence of how the operator configured credential
//! custody, and [`EnforcementCapabilities::for_tier`] is a pure function
//! from that configuration to what may be claimed. A runtime guess about
//! which tier applies would be a worse lie than a boolean, because it
//! would look authoritative.

use serde::{Deserialize, Serialize};

/// Traceability tag every produced [`EnforcementCapabilities`] carries,
/// following the same convention as [`crate::POLICY_SCHEMA_VERSION`] and
/// [`crate::RESERVATION_SCHEMA_VERSION`].
pub const CAPABILITY_SCHEMA_VERSION: &str = "capability-v1";

/// Which enforcement boundary is actually in place for an integration
/// (HORO-1144).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementTier {
    /// The gateway holds an API/BYOK provider credential the agent never
    /// sees, prices each request against a pinned pricing table, and
    /// refuses a call that would breach a hard budget before any provider
    /// spend occurs. The only tier that may claim monetary enforcement.
    GatewayMetered,
    /// The gateway forwards the caller's own subscription credential
    /// unchanged. Token usage reported by the provider is still
    /// observable and settled exactly, but the subscription's own quota
    /// accounting is opaque to us, so no monetary cap can be claimed.
    GatewayObservedQuota,
    /// No gateway at all — preflight and lifecycle hooks only. Advisory:
    /// there is no point in the request path at which spend can be
    /// refused.
    HooksOnly,
}

/// Where a tier's usage figures come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAccounting {
    /// Exact token counts read from the provider's own response or
    /// stream accounting.
    ProviderReported,
    /// No usage figure was reported, so settlement conservatively falls
    /// back to the reserved amount — the HORO-1141 behaviour recorded as
    /// [`crate::Reservation::usage_known`] `Some(false)`. Nothing is
    /// refunded that cannot be proven unspent.
    ReservedAmountFallback,
    /// No usage information exists at this tier at all.
    None,
}

/// Why a tier cannot enforce a monetary cap (HORO-1144). Every variant
/// names a concrete missing input, never a vague "unsupported".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoMonetaryCap {
    /// The provider meters a subscription against a quota it does not
    /// expose per-request, so no per-request price exists to enforce.
    OpaqueProviderQuota,
    /// The pinned pricing table carries no entry for this model, so any
    /// monetary figure would be invented.
    NoPinnedPrice { model: String },
    /// There is no point in the request path where a call could be
    /// refused before spend — the [`EnforcementTier::HooksOnly`] case.
    NoEnforcementPoint,
}

/// Whether a monetary (currency-denominated) hard cap can actually be
/// enforced at this tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonetaryEnforcement {
    /// A hard monetary boundary is enforced, priced against the named
    /// pinned pricing table version. That version is recorded on every
    /// reservation, settlement, and provenance row so a spend figure can
    /// always be traced to the prices that produced it.
    Enforced { pricing_version: String },
    /// No monetary cap. The reason is a value, not a footnote.
    NotAvailable { reason: NoMonetaryCap },
}

/// Who holds the upstream provider credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialCustody {
    /// The Governor resolves the credential from a user-configured
    /// credential command and holds it in-process. The agent never sees
    /// it — it authenticates to the local gateway with an opaque local
    /// capability token instead.
    GovernorHeld,
    /// The agent supplies its own credential, which the gateway forwards
    /// unchanged. The Governor observes usage but takes no custody.
    AgentHeld,
    /// No credential passes through Libra at all — the
    /// [`EnforcementTier::HooksOnly`] case.
    NotApplicable,
}

/// The full, honest capability statement for one integration mode
/// (HORO-1144). Produced only by [`Self::for_tier`], so a caller cannot
/// assemble a combination the tier does not actually support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnforcementCapabilities {
    pub tier: EnforcementTier,
    pub usage_accounting: UsageAccounting,
    pub monetary_enforcement: MonetaryEnforcement,
    pub credential_custody: CredentialCustody,
    /// `true` only when a request can be refused *before* provider spend
    /// occurs. The literal meaning of "hard boundary".
    pub pre_spend_refusal: bool,
    pub capability_schema_version: String,
}

impl EnforcementCapabilities {
    /// The capability statement implied by `tier`.
    ///
    /// `pricing_version` is consumed only by
    /// [`EnforcementTier::GatewayMetered`] — the one tier that may claim
    /// monetary enforcement — and is ignored by the other two. It is
    /// taken by value for every tier rather than being threaded through a
    /// tier-specific constructor so that a caller cannot construct a
    /// `GatewayMetered` statement without having a concrete pricing
    /// version in hand.
    pub fn for_tier(tier: EnforcementTier, pricing_version: impl Into<String>) -> Self {
        let (usage_accounting, monetary_enforcement, credential_custody, pre_spend_refusal) =
            match tier {
                EnforcementTier::GatewayMetered => (
                    UsageAccounting::ProviderReported,
                    MonetaryEnforcement::Enforced {
                        pricing_version: pricing_version.into(),
                    },
                    CredentialCustody::GovernorHeld,
                    true,
                ),
                EnforcementTier::GatewayObservedQuota => (
                    UsageAccounting::ProviderReported,
                    MonetaryEnforcement::NotAvailable {
                        reason: NoMonetaryCap::OpaqueProviderQuota,
                    },
                    CredentialCustody::AgentHeld,
                    // A token-denominated budget is still refusable at
                    // this tier: the gateway meters tokens exactly even
                    // though it cannot price them.
                    true,
                ),
                EnforcementTier::HooksOnly => (
                    UsageAccounting::None,
                    MonetaryEnforcement::NotAvailable {
                        reason: NoMonetaryCap::NoEnforcementPoint,
                    },
                    CredentialCustody::NotApplicable,
                    false,
                ),
            };
        Self {
            tier,
            usage_accounting,
            monetary_enforcement,
            credential_custody,
            pre_spend_refusal,
            capability_schema_version: CAPABILITY_SCHEMA_VERSION.to_string(),
        }
    }

    /// `true` iff this capability statement may claim a currency-
    /// denominated hard cap. Callers deciding whether to *advertise* a
    /// monetary limit must ask this rather than matching on
    /// [`Self::tier`], so a future tier cannot silently inherit a claim
    /// it does not support.
    pub fn claims_monetary_cap(&self) -> bool {
        matches!(
            self.monetary_enforcement,
            MonetaryEnforcement::Enforced { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_metered_is_the_only_tier_claiming_a_monetary_cap() {
        let metered = EnforcementCapabilities::for_tier(EnforcementTier::GatewayMetered, "p-v1");
        let observed =
            EnforcementCapabilities::for_tier(EnforcementTier::GatewayObservedQuota, "p-v1");
        let hooks = EnforcementCapabilities::for_tier(EnforcementTier::HooksOnly, "p-v1");

        assert!(metered.claims_monetary_cap());
        assert!(!observed.claims_monetary_cap());
        assert!(!hooks.claims_monetary_cap());
    }

    #[test]
    fn a_claimed_monetary_cap_always_names_its_pricing_version() {
        let metered =
            EnforcementCapabilities::for_tier(EnforcementTier::GatewayMetered, "pricing-2026-09");
        assert_eq!(
            metered.monetary_enforcement,
            MonetaryEnforcement::Enforced {
                pricing_version: "pricing-2026-09".to_string()
            },
            "a monetary claim with no traceable price list would be unauditable"
        );
    }

    #[test]
    fn subscription_tier_names_the_concrete_reason_it_cannot_price() {
        let observed =
            EnforcementCapabilities::for_tier(EnforcementTier::GatewayObservedQuota, "p-v1");
        assert_eq!(
            observed.monetary_enforcement,
            MonetaryEnforcement::NotAvailable {
                reason: NoMonetaryCap::OpaqueProviderQuota
            }
        );
        assert_eq!(observed.usage_accounting, UsageAccounting::ProviderReported);
        assert_eq!(observed.credential_custody, CredentialCustody::AgentHeld);
    }

    #[test]
    fn only_gateway_tiers_can_refuse_before_spend() {
        assert!(
            EnforcementCapabilities::for_tier(EnforcementTier::GatewayMetered, "p").pre_spend_refusal
        );
        assert!(
            EnforcementCapabilities::for_tier(EnforcementTier::GatewayObservedQuota, "p")
                .pre_spend_refusal
        );
        assert!(
            !EnforcementCapabilities::for_tier(EnforcementTier::HooksOnly, "p").pre_spend_refusal,
            "hooks fire after the request is already in flight — there is nothing to refuse"
        );
    }

    #[test]
    fn hooks_only_holds_no_credential_and_observes_no_usage() {
        let hooks = EnforcementCapabilities::for_tier(EnforcementTier::HooksOnly, "p");
        assert_eq!(hooks.credential_custody, CredentialCustody::NotApplicable);
        assert_eq!(hooks.usage_accounting, UsageAccounting::None);
    }

    #[test]
    fn capabilities_round_trip_through_json() {
        let original =
            EnforcementCapabilities::for_tier(EnforcementTier::GatewayMetered, "pricing-x");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EnforcementCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn unpriced_model_reason_carries_the_model_it_could_not_price() {
        let reason = NoMonetaryCap::NoPinnedPrice {
            model: "some-unlisted-model".to_string(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        assert!(json.contains("some-unlisted-model"));
    }
}
