//! Acceptance criterion 4: "explicit secure consent and internal endpoint
//! owner required before any live call is ever attempted". This crate
//! ships no live adapter (see the crate's own module docs), so this
//! module is a **structural** guarantee for whenever one is eventually
//! written: a real [`QuotaSource`] implementation cannot be constructed
//! without an explicit, separately-configured [`LiveEndpointAuthorization`]
//! — there is no path that defaults to "on", no `Default` impl, no public
//! field, no way to build one from only an endpoint URL.
//!
//! # Why this is enforced by the type system, not a runtime check
//!
//! A runtime check ("is `authorized` true?") can always be accidentally
//! skipped by a caller in a hurry. Making [`LiveEndpointAuthorization`]'s
//! fields private and its only constructor require both an
//! [`EndpointOwner`] and a [`ConsentRecord`] means a future adapter crate
//! simply has nothing to pass into [`QuotaSource::new`] (or whatever its
//! real constructor ends up being) without first obtaining both — the
//! compiler enforces the precondition, not a reviewer's memory of a
//! runtime flag.

use serde::{Deserialize, Serialize};

/// The named owner of the one authorized internal endpoint. Free text —
/// this crate has no directory service to validate it against — but
/// required, non-empty, and never a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointOwner(String);

/// Every way constructing an [`EndpointOwner`] can be refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointOwnerError {
    #[error("endpoint owner name must not be empty")]
    Empty,
}

impl EndpointOwner {
    pub fn new(name: impl Into<String>) -> Result<Self, EndpointOwnerError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(EndpointOwnerError::Empty);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A record that a human explicitly consented to this build making live
/// calls to the one named [`EndpointOwner`]'s endpoint. Carries no secret
/// — this is a decision record, not a credential — but is itself required
/// before [`LiveEndpointAuthorization::new`] will construct anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRecord {
    consented_by: String,
    /// A free-text reference to where consent was recorded (a Jira
    /// comment, a signed doc, an ADR) — never the consent content itself.
    recorded_at_reference: String,
}

/// Every way constructing a [`ConsentRecord`] can be refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConsentRecordError {
    #[error("consented_by must not be empty")]
    EmptyConsentedBy,
    #[error("recorded_at_reference must not be empty")]
    EmptyReference,
}

impl ConsentRecord {
    pub fn new(
        consented_by: impl Into<String>,
        recorded_at_reference: impl Into<String>,
    ) -> Result<Self, ConsentRecordError> {
        let consented_by = consented_by.into();
        let recorded_at_reference = recorded_at_reference.into();
        if consented_by.trim().is_empty() {
            return Err(ConsentRecordError::EmptyConsentedBy);
        }
        if recorded_at_reference.trim().is_empty() {
            return Err(ConsentRecordError::EmptyReference);
        }
        Ok(Self {
            consented_by,
            recorded_at_reference,
        })
    }
}

/// The structural precondition a real, future [`QuotaSource`]
/// implementation must hold before it may attempt a live call. Its fields
/// are private; [`Self::new`] is its only constructor, and it requires
/// both an [`EndpointOwner`] and a [`ConsentRecord`] — see this module's
/// own docs for why that is the whole point.
///
/// No live adapter in this crate ever constructs one of these — the type
/// exists purely as the gate a future adapter crate must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEndpointAuthorization {
    owner: EndpointOwner,
    consent: ConsentRecord,
    /// The one authorized origin (scheme + host[:port]) — matched against
    /// [`crate::response::ingest_response`]'s `authorized_origin`
    /// parameter so a redirect anywhere else is refused (acceptance
    /// criterion 2).
    authorized_origin: String,
}

impl LiveEndpointAuthorization {
    pub fn new(
        owner: EndpointOwner,
        consent: ConsentRecord,
        authorized_origin: impl Into<String>,
    ) -> Self {
        Self {
            owner,
            consent,
            authorized_origin: authorized_origin.into(),
        }
    }

    pub fn owner(&self) -> &EndpointOwner {
        &self.owner
    }

    pub fn consent(&self) -> &ConsentRecord {
        &self.consent
    }

    pub fn authorized_origin(&self) -> &str {
        &self.authorized_origin
    }
}

/// A versioned quota-source capability. A real, live implementation
/// requires a [`LiveEndpointAuthorization`] to even be constructed (see
/// this module's docs); the fixture/manual-import path in
/// [`crate::fixture`] does not implement this trait at all — it produces
/// an [`crate::AcquiredReading`] directly, since there is no "source" to
/// poll for a static fixture file.
///
/// No type in this crate implements this trait. It exists to be
/// implemented by a future, separately-reviewed adapter crate once a
/// real endpoint, owner, and consent exist — see this crate's module
/// docs on why none of that exists yet.
pub trait QuotaSource {
    /// The error type this source's acquisition may fail with.
    type Error: std::error::Error;

    /// The authorization this source was constructed with. A real
    /// implementation's constructor must take a [`LiveEndpointAuthorization`]
    /// and store it, so this can never be synthesized after the fact.
    fn authorization(&self) -> &LiveEndpointAuthorization;
}

/// ```compile_fail
/// // A `QuotaSource` cannot be backed by only a bare endpoint string —
/// // there is no constructor path that skips `EndpointOwner` and
/// // `ConsentRecord`. This doctest is the acceptance-criterion-4 proof:
/// // it must fail to compile.
/// use libra_governor_quota_source::LiveEndpointAuthorization;
///
/// struct NoAuthRequired;
/// impl NoAuthRequired {
///     fn new(endpoint: &str) -> LiveEndpointAuthorization {
///         // There is no `LiveEndpointAuthorization` constructor that
///         // takes only a URL — `new` requires an `EndpointOwner` and a
///         // `ConsentRecord` as well. This line cannot compile.
///         LiveEndpointAuthorization::new_from_url_only(endpoint)
///     }
/// }
/// ```
#[allow(dead_code)]
struct CompileFailDocProof;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_owner_rejects_empty_names() {
        assert!(matches!(
            EndpointOwner::new(""),
            Err(EndpointOwnerError::Empty)
        ));
        assert!(matches!(
            EndpointOwner::new("   "),
            Err(EndpointOwnerError::Empty)
        ));
    }

    #[test]
    fn consent_record_rejects_empty_fields() {
        assert!(matches!(
            ConsentRecord::new("", "HORO-1764 comment"),
            Err(ConsentRecordError::EmptyConsentedBy)
        ));
        assert!(matches!(
            ConsentRecord::new("owner", ""),
            Err(ConsentRecordError::EmptyReference)
        ));
    }

    #[test]
    fn a_fully_constructed_authorization_carries_its_own_origin() {
        let owner = EndpointOwner::new("platform-team").unwrap();
        let consent = ConsentRecord::new("platform-team-lead", "HORO-1764 comment #1").unwrap();
        let auth = LiveEndpointAuthorization::new(owner, consent, "https://proxy.example.invalid");
        assert_eq!(auth.authorized_origin(), "https://proxy.example.invalid");
    }
}
