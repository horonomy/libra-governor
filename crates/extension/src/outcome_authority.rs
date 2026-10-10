//! Verified-provider outcome claim signature verification (HORO-1727 PR
//! 5a).
//!
//! # Production-unreachable by construction
//!
//! Nothing in this module is wired into `daemon::server`'s `RecordOutcome`
//! handling, and nothing here constructs the authoritative `Provider`
//! variant of `AttestationSource` itself. This is a standalone
//! verification primitive: given a configured set of trusted provider
//! keys and a signed claim, it either returns the verified provider's own
//! configured `source_id` or a fail-closed error. Per ADR-0017, this
//! proves verification logic, not principal separation — a config
//! entry's secret is still readable by the same OS user running the
//! governed agent on a single-workstation deployment, so wiring this into
//! the daemon does not by itself satisfy HORO-1727 Decision 1.
//!
//! # Decision 1's core rule, enforced here
//!
//! [`verify`] always returns the `source_id` from the CONFIG ENTRY whose
//! secret verified the signature — never any field the caller supplied.
//! A claimed `source_id` in the request body is not proof of identity
//! (HORO-1727 Decision 1); only a signature that verifies against a
//! configured secret is.
//!
//! # Reuses `crate::sign`'s canonical format, does not invent a new one
//!
//! The signature is computed with [`crate::sign::sign`] exactly as event
//! delivery already does: `issued_at` as its `timestamp`, `idempotency_key`
//! as its `marker`, and [`canonical_bytes`] as its `body`. Binding
//! `plan_id` into the signed content binds the contract revision too,
//! because a plan's revision is fixed at creation — a claim signed for
//! revision N's plan cannot be replayed to authorize revision N+1.

use libra_governor_domain::{PlanId, TaskId};

use crate::secret::WebhookSecret;

/// One configured trusted provider: the `source_id` this deployment
/// trusts THAT KEY to assert, and the secret used to verify its
/// signatures. The `source_id` here is operator-configured, out-of-band
/// of any request — never read from a claim.
pub struct TrustedProvider {
    pub source_id: String,
    pub secret: WebhookSecret,
}

/// The full set of trusted providers for this deployment, plus the
/// maximum clock skew a claim's `issued_at` may have from "now" before
/// it is refused as expired.
pub struct OutcomeAuthorityConfig {
    pub providers: Vec<TrustedProvider>,
    pub max_skew_secs: u64,
}

/// The unsigned content of one outcome claim — everything
/// [`canonical_bytes`] covers (`issued_at` and `idempotency_key` are
/// covered separately, by `crate::sign`'s own payload wrapping — see
/// module docs).
#[derive(Debug, Clone)]
pub struct OutcomeClaimContent {
    pub task_id: TaskId,
    /// `None` is never authoritative — see [`verify`]'s explicit
    /// [`OutcomeClaimVerificationError::UnboundClaim`] refusal. An
    /// unbound claim cannot promote a receipt at any revision (see
    /// `libra_governor_ledger::LedgerStore::record_outcome_attestation`),
    /// so accepting one here would only produce a verified-but-inert
    /// claim — refused explicitly instead, so the caller gets a clear
    /// reason rather than a silent no-op.
    pub plan_id: Option<PlanId>,
    pub outcome_kind: String,
    /// Hex-encoded SHA-256 digest of the evidence payload, not the raw
    /// evidence itself — keeps the signed payload a bounded, fixed shape
    /// regardless of how much evidence a provider attaches.
    pub evidence_digest: String,
    pub idempotency_key: String,
    pub issued_at: u64,
}

/// A claim plus its signature, exactly as received.
#[derive(Debug, Clone)]
pub struct SignedOutcomeClaim {
    pub content: OutcomeClaimContent,
    /// `v1=<hex>` — the same wire shape `crate::sign::sign` produces.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutcomeClaimVerificationError {
    #[error("claim has no bound plan_id — an unbound claim can never be authoritative")]
    UnboundClaim,
    #[error("claim's issued_at is outside the allowed clock skew")]
    Expired,
    #[error("signature does not verify against any configured provider")]
    UnknownOrInvalidSignature,
}

/// The canonical bytes a claim's signature covers, beyond what
/// `crate::sign::sign`'s own `(timestamp, marker, body)` wrapping already
/// binds (`issued_at`, `idempotency_key`).
pub fn canonical_bytes(content: &OutcomeClaimContent) -> Vec<u8> {
    format!(
        "{}.{}.{}.{}",
        content.task_id,
        content
            .plan_id
            .map(|p| p.0.to_string())
            .unwrap_or_else(|| "-".to_string()),
        content.outcome_kind,
        content.evidence_digest,
    )
    .into_bytes()
}

/// Constant-time byte comparison — a verification path must not leak
/// timing information about how many leading bytes of a guessed
/// signature matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verifies `claim` against every configured provider's secret in turn
/// and returns the MATCHING PROVIDER'S OWN configured `source_id` — never
/// the claim's own content (HORO-1727 Decision 1's core rule).
///
/// Fails closed: an unbound claim, an `issued_at` outside
/// `config.max_skew_secs` of `now`, or a signature that does not match
/// any configured provider are all refused the same way — `Err`, never a
/// default `source_id` or a best-effort partial match. Checked in that
/// order so a claim already refused as unbound or expired never pays for
/// (or leaks timing about) a signature comparison.
pub fn verify(
    config: &OutcomeAuthorityConfig,
    claim: &SignedOutcomeClaim,
    now: u64,
) -> Result<String, OutcomeClaimVerificationError> {
    if claim.content.plan_id.is_none() {
        return Err(OutcomeClaimVerificationError::UnboundClaim);
    }
    let skew = now.abs_diff(claim.content.issued_at);
    if skew > config.max_skew_secs {
        return Err(OutcomeClaimVerificationError::Expired);
    }

    let body = canonical_bytes(&claim.content);
    for provider in &config.providers {
        let expected = crate::sign::sign(
            &provider.secret,
            claim.content.issued_at,
            &claim.content.idempotency_key,
            &body,
        );
        if constant_time_eq(expected.as_bytes(), claim.signature.as_bytes()) {
            return Ok(provider.source_id.clone());
        }
    }
    Err(OutcomeClaimVerificationError::UnknownOrInvalidSignature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::WebhookSecretCommand;
    use libra_governor_domain::TaskId;

    fn fake_secret(value: &str) -> WebhookSecret {
        WebhookSecretCommand::new("/bin/sh", vec!["-c".to_string(), format!("printf {value}")])
            .resolve()
            .unwrap()
    }

    fn signed(secret: &WebhookSecret, content: OutcomeClaimContent) -> SignedOutcomeClaim {
        let body = canonical_bytes(&content);
        let signature =
            crate::sign::sign(secret, content.issued_at, &content.idempotency_key, &body);
        SignedOutcomeClaim { content, signature }
    }

    fn base_content(task_id: TaskId) -> OutcomeClaimContent {
        OutcomeClaimContent {
            task_id,
            plan_id: Some(PlanId::new()),
            outcome_kind: "completed".to_string(),
            evidence_digest: "deadbeef".repeat(8),
            idempotency_key: "claim-1".to_string(),
            issued_at: 1_700_000_000,
        }
    }

    fn config(providers: Vec<TrustedProvider>) -> OutcomeAuthorityConfig {
        OutcomeAuthorityConfig {
            providers,
            max_skew_secs: 300,
        }
    }

    #[test]
    fn a_validly_signed_claim_verifies_and_returns_the_configured_source_id_not_the_claims_own() {
        let secret = fake_secret("sk-fake-provider-a");
        let task_id = TaskId::new();
        let claim = signed(&secret, base_content(task_id));
        let cfg = config(vec![TrustedProvider {
            source_id: "configured-ci-system".to_string(),
            secret,
        }]);

        let result = verify(&cfg, &claim, claim.content.issued_at).unwrap();
        assert_eq!(result, "configured-ci-system");
    }

    #[test]
    fn a_claim_with_no_plan_id_is_refused_as_unbound_even_with_a_valid_signature() {
        let secret = fake_secret("sk-fake-provider-a");
        let mut content = base_content(TaskId::new());
        content.plan_id = None;
        let claim = signed(&secret, content);
        let cfg = config(vec![TrustedProvider {
            source_id: "configured-ci-system".to_string(),
            secret,
        }]);

        let err = verify(&cfg, &claim, claim.content.issued_at).unwrap_err();
        assert_eq!(err, OutcomeClaimVerificationError::UnboundClaim);
    }

    #[test]
    fn a_claim_signed_with_an_unconfigured_key_is_refused() {
        let signing_secret = fake_secret("sk-fake-attacker-key");
        let claim = signed(&signing_secret, base_content(TaskId::new()));
        let cfg = config(vec![TrustedProvider {
            source_id: "configured-ci-system".to_string(),
            secret: fake_secret("sk-fake-provider-a"),
        }]);

        let err = verify(&cfg, &claim, claim.content.issued_at).unwrap_err();
        assert_eq!(
            err,
            OutcomeClaimVerificationError::UnknownOrInvalidSignature
        );
    }

    #[test]
    fn a_claim_whose_content_was_tampered_after_signing_is_refused() {
        let secret = fake_secret("sk-fake-provider-a");
        let mut claim = signed(&secret, base_content(TaskId::new()));
        claim.content.outcome_kind = "failed".to_string();
        let cfg = config(vec![TrustedProvider {
            source_id: "configured-ci-system".to_string(),
            secret,
        }]);

        let err = verify(&cfg, &claim, claim.content.issued_at).unwrap_err();
        assert_eq!(
            err,
            OutcomeClaimVerificationError::UnknownOrInvalidSignature
        );
    }

    #[test]
    fn a_claim_issued_outside_the_allowed_clock_skew_is_refused_as_expired() {
        let secret = fake_secret("sk-fake-provider-a");
        let claim = signed(&secret, base_content(TaskId::new()));
        let cfg = config(vec![TrustedProvider {
            source_id: "configured-ci-system".to_string(),
            secret,
        }]);

        let far_future = claim.content.issued_at + cfg.max_skew_secs + 1;
        let err = verify(&cfg, &claim, far_future).unwrap_err();
        assert_eq!(err, OutcomeClaimVerificationError::Expired);

        let far_past = claim.content.issued_at - cfg.max_skew_secs - 1;
        let err = verify(&cfg, &claim, far_past).unwrap_err();
        assert_eq!(err, OutcomeClaimVerificationError::Expired);
    }

    #[test]
    fn a_claim_at_exactly_the_skew_boundary_still_verifies() {
        let secret = fake_secret("sk-fake-provider-a");
        let claim = signed(&secret, base_content(TaskId::new()));
        let cfg = config(vec![TrustedProvider {
            source_id: "configured-ci-system".to_string(),
            secret,
        }]);

        let at_boundary = claim.content.issued_at + cfg.max_skew_secs;
        assert!(verify(&cfg, &claim, at_boundary).is_ok());
    }

    #[test]
    fn verification_tries_every_configured_provider_and_matches_the_right_one() {
        let secret_a = fake_secret("sk-fake-provider-a");
        let secret_b = fake_secret("sk-fake-provider-b");
        let claim = signed(&secret_b, base_content(TaskId::new()));
        let cfg = config(vec![
            TrustedProvider {
                source_id: "provider-a".to_string(),
                secret: secret_a,
            },
            TrustedProvider {
                source_id: "provider-b".to_string(),
                secret: secret_b,
            },
        ]);

        let result = verify(&cfg, &claim, claim.content.issued_at).unwrap();
        assert_eq!(result, "provider-b");
    }

    #[test]
    fn two_different_plan_ids_produce_different_signatures_for_otherwise_identical_content() {
        let secret = fake_secret("sk-fake-provider-a");
        let task_id = TaskId::new();
        let a = signed(&secret, base_content(task_id));
        let mut content_b = base_content(task_id);
        content_b.plan_id = Some(PlanId::new());
        let b = signed(&secret, content_b);

        assert_ne!(
            a.signature, b.signature,
            "binding plan_id into the signed content must make a claim for one plan/revision \
             unusable for another"
        );
    }
}
