//! The one production construction site for the authoritative
//! `AttestationSource::Provider` variant (HORO-1727 PR 5b / ADR-0017).
//!
//! # Why this file exists as a single, narrow module
//!
//! `crates/daemon/tests/outcome_authority_not_wired_live.rs` scans
//! `daemon/src` and `cli/src` for any other construction of
//! `AttestationSource::Provider` and fails the build if it finds one —
//! this module is the single allowed exception, named explicitly in that
//! guard's own allowlist. Keeping the construction in one small,
//! easily-read file (rather than inline in `server.rs`'s much larger
//! `handle_record_outcome`) is what makes that allowlist meaningful: a
//! reviewer — or the guard's own doc comment — can point at exactly this
//! file as "the only place this is allowed to happen, and here is all the
//! logic that gates it."
//!
//! # This still does not satisfy Decision 1 by itself
//!
//! Per ADR-0017: a verified HMAC signature proves the push was signed
//! with a key `DaemonConfig::outcome_authority` was configured with — it
//! does NOT prove that key is inaccessible to the governed agent being
//! assessed, which is Decision 1's actual requirement on a single-OS-user
//! deployment. `DaemonConfig::outcome_authority` stays `None` in every
//! real deployment today (see that field's own docs); this module exists
//! so the verification logic is exercised and tested now, ready for a
//! genuinely separate-principal deployment to activate later — not as a
//! present-day claim that Decision 1 is satisfied.

use libra_governor_domain::AttestationSource;
use libra_governor_extension::{OutcomeAuthorityConfig, OutcomeClaimContent, SignedOutcomeClaim};
use libra_governor_protocol::SignedOutcomeClaimWire;

use crate::server::DaemonConfig;

/// Attempts to verify `wire` as a trusted-provider claim over
/// `(task_id, plan_id, outcome_kind, evidence_digest)` and, only on
/// success, returns the authoritative [`AttestationSource::Provider`]
/// carrying the CONFIGURED provider's own `source_id` — never anything
/// `wire`/the caller supplied (HORO-1727 Decision 1's core rule, already
/// enforced inside `libra_governor_extension::verify`; this function just
/// wires that primitive to the daemon's config and the request's own
/// already-present fields).
///
/// Returns `None` — never a fallback identity, never a best-effort
/// partial match — when `config.outcome_authority` is unset, `wire` is
/// absent, or verification fails for any reason. The caller
/// (`server::handle_record_outcome`) always has a non-authoritative
/// `Unverified` fallback ready, exactly as it does today.
///
/// Returns the constructed source alongside the plain `provider_id`
/// string (rather than making the caller pattern-match the variant back
/// out) so that `server.rs` never needs to spell the authoritative
/// variant's name at all — keeping this module the single place that
/// name appears in non-test `daemon/src` code, which is what makes the
/// static guard's allowlist meaningful.
/// What a `RecordOutcome` push is actually about, derived server-side
/// from that same request's own fields — see [`verify_claim`]'s own docs
/// for why this is never caller-asserted separately from the real
/// content being persisted.
pub struct ClaimSubject<'a> {
    pub task_id: libra_governor_domain::TaskId,
    pub plan_id: Option<libra_governor_domain::PlanId>,
    pub outcome_kind: &'a str,
    pub evidence_digest: &'a str,
    pub idempotency_key: &'a str,
}

pub fn verify_claim(
    config: &DaemonConfig,
    wire: Option<&SignedOutcomeClaimWire>,
    subject: ClaimSubject<'_>,
    now: time::OffsetDateTime,
) -> Option<(AttestationSource, String)> {
    let authority_config: &OutcomeAuthorityConfig = config.outcome_authority.as_ref()?;
    let wire = wire?;

    let claim = SignedOutcomeClaim {
        content: OutcomeClaimContent {
            task_id: subject.task_id,
            plan_id: subject.plan_id,
            outcome_kind: subject.outcome_kind.to_string(),
            evidence_digest: subject.evidence_digest.to_string(),
            idempotency_key: subject.idempotency_key.to_string(),
            issued_at: wire.issued_at,
        },
        signature: wire.signature.clone(),
    };

    let now_secs = now.unix_timestamp().max(0) as u64;
    let provider_id = libra_governor_extension::verify(authority_config, &claim, now_secs).ok()?;
    let source = AttestationSource::Provider {
        provider_id: provider_id.clone(),
    };
    Some((source, provider_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{PlanId, TaskId};
    use libra_governor_extension::{
        canonical_bytes, OutcomeAuthorityConfig, OutcomeClaimContent, TrustedProvider,
        WebhookSecretCommand,
    };

    fn fixed_now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn fake_secret(value: &str) -> libra_governor_extension::WebhookSecret {
        WebhookSecretCommand::new("/bin/sh", vec!["-c".to_string(), format!("printf {value}")])
            .resolve()
            .unwrap()
    }

    fn test_daemon_config_with_authority(
        dir: &std::path::Path,
        authority: Option<OutcomeAuthorityConfig>,
    ) -> DaemonConfig {
        DaemonConfig {
            socket_path: dir.join("d.sock"),
            ledger_path: dir.join("ledger.sqlite3"),
            log_path: dir.join("daemon.log"),
            recon_budget: crate::recon::ReconBudget::default(),
            replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
            policy: crate::default_admission_policy(),
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
            extensions: None,
            extension_runtime: std::sync::OnceLock::new(),
            outcome_authority: authority,
            progressive_interval_secs: crate::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        }
    }

    #[test]
    fn no_config_means_no_verification_is_even_attempted() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_daemon_config_with_authority(dir.path(), None);
        let wire = SignedOutcomeClaimWire {
            issued_at: 1_700_000_000,
            signature: "v1=whatever".to_string(),
        };

        let result = verify_claim(
            &config,
            Some(&wire),
            ClaimSubject {
                task_id: TaskId::new(),
                plan_id: Some(PlanId::new()),
                outcome_kind: "completed",
                evidence_digest: "deadbeef",
                idempotency_key: "key-1",
            },
            fixed_now(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn no_wire_claim_means_no_verification_is_even_attempted() {
        let dir = tempfile::tempdir().unwrap();
        let secret = fake_secret("sk-fake-wiring-test");
        let config = test_daemon_config_with_authority(
            dir.path(),
            Some(OutcomeAuthorityConfig {
                providers: vec![TrustedProvider {
                    source_id: "configured-ci".to_string(),
                    secret,
                }],
                max_skew_secs: 300,
            }),
        );

        let result = verify_claim(
            &config,
            None,
            ClaimSubject {
                task_id: TaskId::new(),
                plan_id: Some(PlanId::new()),
                outcome_kind: "completed",
                evidence_digest: "deadbeef",
                idempotency_key: "key-1",
            },
            fixed_now(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn a_validly_signed_claim_returns_an_authoritative_provider_source_with_the_configured_id() {
        let dir = tempfile::tempdir().unwrap();
        let secret = fake_secret("sk-fake-wiring-test");
        let task_id = TaskId::new();
        let plan_id = Some(PlanId::new());
        let content = OutcomeClaimContent {
            task_id,
            plan_id,
            outcome_kind: "completed".to_string(),
            evidence_digest: "deadbeef".to_string(),
            idempotency_key: "key-1".to_string(),
            issued_at: 1_700_000_000,
        };
        let signature = libra_governor_extension::sign(
            &secret,
            content.issued_at,
            &content.idempotency_key,
            &canonical_bytes(&content),
        );
        let config = test_daemon_config_with_authority(
            dir.path(),
            Some(OutcomeAuthorityConfig {
                providers: vec![TrustedProvider {
                    source_id: "configured-ci".to_string(),
                    secret,
                }],
                max_skew_secs: 300,
            }),
        );
        let wire = SignedOutcomeClaimWire {
            issued_at: content.issued_at,
            signature,
        };

        let (source, provider_id) = verify_claim(
            &config,
            Some(&wire),
            ClaimSubject {
                task_id,
                plan_id,
                outcome_kind: "completed",
                evidence_digest: "deadbeef",
                idempotency_key: "key-1",
            },
            fixed_now(),
        )
        .unwrap();
        assert!(source.is_authoritative());
        assert_eq!(provider_id, "configured-ci");
    }

    #[test]
    fn a_claim_whose_idempotency_key_does_not_match_this_push_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let secret = fake_secret("sk-fake-wiring-test");
        let task_id = TaskId::new();
        let plan_id = Some(PlanId::new());
        let content = OutcomeClaimContent {
            task_id,
            plan_id,
            outcome_kind: "completed".to_string(),
            evidence_digest: "deadbeef".to_string(),
            idempotency_key: "key-1".to_string(),
            issued_at: 1_700_000_000,
        };
        let signature = libra_governor_extension::sign(
            &secret,
            content.issued_at,
            &content.idempotency_key,
            &canonical_bytes(&content),
        );
        let config = test_daemon_config_with_authority(
            dir.path(),
            Some(OutcomeAuthorityConfig {
                providers: vec![TrustedProvider {
                    source_id: "configured-ci".to_string(),
                    secret,
                }],
                max_skew_secs: 300,
            }),
        );
        let wire = SignedOutcomeClaimWire {
            issued_at: content.issued_at,
            signature,
        };

        // Same signature, but verified against a DIFFERENT idempotency
        // key than the one it was actually signed with.
        let result = verify_claim(
            &config,
            Some(&wire),
            ClaimSubject {
                task_id,
                plan_id,
                outcome_kind: "completed",
                evidence_digest: "deadbeef",
                idempotency_key: "a-different-key",
            },
            fixed_now(),
        );
        assert!(result.is_none());
    }
}
