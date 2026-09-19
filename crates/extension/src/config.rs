//! Configuration and validation for the three outbound extension surfaces
//! (HORO-1174).
//!
//! # Loopback-only, no TLS — by design
//!
//! Every configured URL's scheme must be the literal `http` and its host
//! must be the literal `127.0.0.1` or `::1` — **not** `localhost` (no DNS
//! dependency at all, unlike `crates/gateway`'s equivalent check, which
//! does accept `localhost`). SSRF is structurally impossible here: the
//! destination comes only from `config.json`, never from anything
//! request-derived, and it must resolve to nothing (a literal loopback
//! address needs no resolution at all). That is also why there is no TLS
//! dependency in this crate: a loopback-only destination has no
//! meaningful hostname to validate a certificate against, and pulling in
//! `hyper-rustls` to encrypt a connection that never leaves the loopback
//! interface would be security theater, not security. See
//! `docs/adr/0005-local-extension-points.md`.
//!
//! # Latency budget — the numbers are read from the real code, not assumed
//!
//! `crates/cli/src/client.rs::REQUEST_TIMEOUT` is 5s.
//! `crates/daemon/src/recon.rs::ReconBudget::default().max_duration` is
//! 3s. The ticket's original design assumed 1000ms hard caps for both the
//! business-context and policy-webhook surfaces, which sums to
//! `3s + 1s + 1s == 5s` — **zero** slack against the client's own
//! timeout, before accounting for anything else `handle_preflight` does
//! (estimator bucketing, three ledger writes, `reconcile_stale_reservations`).
//! [`REQUIRED_SLACK`] and the caps below are deliberately set so
//! `RECON_MAX_DURATION + BUSINESS_CONTEXT_TIMEOUT_CAP +
//! POLICY_WEBHOOK_TIMEOUT_CAP + REQUIRED_SLACK < REQUEST_TIMEOUT` holds
//! with real margin (3.0 + 0.7 + 0.7 + 0.5 == 4.9, strictly < 5.0 — see
//! the `latency_budget` test module, which asserts `<` strictly). This is
//! a deviation from the ticket's stated 500/1000ms and 750/1000ms
//! default/cap pairing (which sums to exactly 5.0s with zero slack — see
//! the deviation note in `docs/adr/0005-local-extension-points.md`), made
//! because the stated caps could never satisfy their own required
//! assertion against the real, verified `REQUEST_TIMEOUT`/`ReconBudget`
//! values.

use std::time::Duration;

use crate::secret::{SecretError, WebhookSecret, WebhookSecretCommand};

/// `crates/cli/src/client.rs::REQUEST_TIMEOUT`, copied here as a named
/// constant so the latency-budget assertion in this module can check
/// against it without a dependency on the `cli` crate. Kept in sync by
/// the `latency_budget` test module's own doc comment — if `client.rs`'s
/// real value ever changes, this constant (and the assertion) must be
/// updated in the same commit.
pub const CLI_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// `crates/daemon/src/recon.rs::ReconBudget::default().max_duration`,
/// copied here for the same reason as [`CLI_REQUEST_TIMEOUT`].
pub const RECON_MAX_DURATION: Duration = Duration::from_secs(3);

/// Minimum slack this crate insists on leaving between
/// `RECON_MAX_DURATION + business_context_cap + policy_webhook_cap` and
/// [`CLI_REQUEST_TIMEOUT`], for everything else `handle_preflight` does
/// between those calls (estimator bucketing, ledger writes,
/// `reconcile_stale_reservations`) — see module docs.
pub const REQUIRED_SLACK: Duration = Duration::from_millis(500);

pub const BUSINESS_CONTEXT_TIMEOUT_DEFAULT_MS: u64 = 500;
pub const BUSINESS_CONTEXT_TIMEOUT_CAP_MS: u64 = 700;
pub const POLICY_WEBHOOK_TIMEOUT_DEFAULT_MS: u64 = 700;
pub const POLICY_WEBHOOK_TIMEOUT_CAP_MS: u64 = 700;
pub const EVENTS_TIMEOUT_DEFAULT_MS: u64 = 3_000;
pub const EVENTS_MAX_ATTEMPTS_DEFAULT: u32 = 5;

/// Cap on a response body read from any of the three surfaces.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Cap on the number of `advisory_criteria` entries kept from a Business
/// Context response.
pub const MAX_ADVISORY_CRITERIA: usize = 16;
/// Cap on the character length of one `advisory_criteria` entry, or a
/// Policy Webhook `reason`.
pub const MAX_ADVISORY_CRITERION_CHARS: usize = 200;

/// The wire schema version this build speaks — the `x-libra-schema-version`
/// header value and every wire request/event's `schema_version` field.
pub const WIRE_SCHEMA_VERSION: &str = "libra.extension.v1";

/// One outbound-URL surface's raw configuration (business-context
/// provider or policy webhook).
#[derive(Debug, Clone)]
pub struct SurfaceConfig {
    pub url: String,
    pub timeout_ms: u64,
    pub secret_command: String,
    pub secret_args: Vec<String>,
}

/// The events-delivery surface's raw configuration.
#[derive(Debug, Clone)]
pub struct EventsConfig {
    pub url: String,
    pub timeout_ms: u64,
    pub max_attempts: u32,
    /// Which event kinds to deliver at all — an empty list disables
    /// delivery without disabling the whole `[extensions]` block's other
    /// surfaces.
    pub kinds: Vec<String>,
    pub secret_command: String,
    pub secret_args: Vec<String>,
}

/// Raw, unvalidated `[extensions]` configuration as an operator writes
/// it. Absent (`ExtensionConfig::default()`-equivalent `None` fields)
/// means every surface is off — see crate docs.
#[derive(Debug, Clone, Default)]
pub struct ExtensionConfig {
    pub business_context_provider: Option<SurfaceConfig>,
    pub policy_webhook: Option<SurfaceConfig>,
    pub events: Option<EventsConfig>,
}

/// Why an `[extensions]` configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{surface} url `{url}` is not a valid absolute URL")]
    UrlNotAUrl { surface: &'static str, url: String },
    #[error("{surface} url scheme `{scheme}` is not http (loopback-plaintext only)")]
    SchemeNotHttp {
        surface: &'static str,
        scheme: String,
    },
    #[error(
        "{surface} url host `{host}` is not a loopback literal (127.0.0.1 or ::1 — not \
         localhost, which would add a DNS dependency)"
    )]
    HostNotLoopbackLiteral { surface: &'static str, host: String },
    #[error(
        "{surface} url carries userinfo, which would smuggle a credential into a config field"
    )]
    UrlHasUserinfo { surface: &'static str },
    #[error("{surface} url carries a query or fragment; only scheme/host/port/path is accepted")]
    UrlHasQueryOrFragment { surface: &'static str },
    #[error("{surface} timeout_ms {timeout_ms} exceeds the hard cap of {cap_ms}ms")]
    TimeoutAboveCap {
        surface: &'static str,
        timeout_ms: u64,
        cap_ms: u64,
    },
    #[error("{surface} secret_command must not be empty")]
    EmptySecretCommand { surface: &'static str },
    #[error("events.max_attempts must be greater than zero")]
    EventsMaxAttemptsZero,
}

/// A validated, ready-to-dial surface. The extension client takes one of
/// these, never a raw [`SurfaceConfig`] — the same "unvalidated value
/// cannot reach the request path" discipline `crates/gateway::config`
/// establishes.
#[derive(Debug, Clone)]
pub struct ValidatedSurfaceConfig {
    pub url: hyper::Uri,
    pub timeout: Duration,
    pub secret: WebhookSecret,
}

#[derive(Debug, Clone)]
pub struct ValidatedEventsConfig {
    pub url: hyper::Uri,
    pub timeout: Duration,
    pub max_attempts: u32,
    pub kinds: Vec<crate::event::EventKind>,
    pub secret: WebhookSecret,
}

/// A fully validated `[extensions]` block. `None` fields mean that
/// surface is off.
#[derive(Debug, Clone)]
pub struct ValidatedExtensionConfig {
    pub business_context_provider: Option<ValidatedSurfaceConfig>,
    pub policy_webhook: Option<ValidatedSurfaceConfig>,
    pub events: Option<ValidatedEventsConfig>,
}

impl ValidatedExtensionConfig {
    pub fn is_empty(&self) -> bool {
        self.business_context_provider.is_none()
            && self.policy_webhook.is_none()
            && self.events.is_none()
    }
}

fn is_loopback_literal(host: &str) -> bool {
    let stripped = host.trim_start_matches('[').trim_end_matches(']');
    stripped == "127.0.0.1" || stripped == "::1"
}

fn validate_surface(
    surface: &'static str,
    raw: &SurfaceConfig,
    cap_ms: u64,
) -> Result<ValidatedSurfaceConfig, ConfigError> {
    if raw.timeout_ms == 0 || raw.timeout_ms > cap_ms {
        return Err(ConfigError::TimeoutAboveCap {
            surface,
            timeout_ms: raw.timeout_ms,
            cap_ms,
        });
    }
    if raw.secret_command.trim().is_empty() {
        return Err(ConfigError::EmptySecretCommand { surface });
    }

    let uri: hyper::Uri = raw.url.parse().map_err(|_| ConfigError::UrlNotAUrl {
        surface,
        url: raw.url.clone(),
    })?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| ConfigError::UrlNotAUrl {
            surface,
            url: raw.url.clone(),
        })?
        .to_ascii_lowercase();
    let authority = uri.authority().ok_or_else(|| ConfigError::UrlNotAUrl {
        surface,
        url: raw.url.clone(),
    })?;
    if authority.as_str().contains('@') {
        return Err(ConfigError::UrlHasUserinfo { surface });
    }
    if raw.url.contains('?') || raw.url.contains('#') {
        return Err(ConfigError::UrlHasQueryOrFragment { surface });
    }
    if scheme != "http" {
        return Err(ConfigError::SchemeNotHttp { surface, scheme });
    }
    let host = authority.host().to_ascii_lowercase();
    if !is_loopback_literal(&host) {
        return Err(ConfigError::HostNotLoopbackLiteral { surface, host });
    }

    let secret = WebhookSecretCommand::new(raw.secret_command.clone(), raw.secret_args.clone())
        .resolve()
        .map_err(|_: SecretError| ConfigError::EmptySecretCommand { surface })?;

    Ok(ValidatedSurfaceConfig {
        url: uri,
        timeout: Duration::from_millis(raw.timeout_ms),
        secret,
    })
}

/// Validates a raw `[extensions]` block. Every field absent (`None`
/// everywhere) is not an error — it means every extension surface is
/// off. A present-but-invalid surface IS an error, refusing that whole
/// `[extensions]` block (the caller, per `crates/daemon`'s established
/// fail-open-for-the-daemon doctrine, logs and disables extensions rather
/// than aborting startup).
pub fn validate(raw: ExtensionConfig) -> Result<ValidatedExtensionConfig, ConfigError> {
    let business_context_provider = raw
        .business_context_provider
        .as_ref()
        .map(|s| {
            validate_surface(
                "business_context_provider",
                s,
                BUSINESS_CONTEXT_TIMEOUT_CAP_MS,
            )
        })
        .transpose()?;
    let policy_webhook = raw
        .policy_webhook
        .as_ref()
        .map(|s| validate_surface("policy_webhook", s, POLICY_WEBHOOK_TIMEOUT_CAP_MS))
        .transpose()?;
    let events = raw
        .events
        .as_ref()
        .map(|e| -> Result<ValidatedEventsConfig, ConfigError> {
            if e.max_attempts == 0 {
                return Err(ConfigError::EventsMaxAttemptsZero);
            }
            let surface_cfg = SurfaceConfig {
                url: e.url.clone(),
                timeout_ms: e.timeout_ms,
                secret_command: e.secret_command.clone(),
                secret_args: e.secret_args.clone(),
            };
            // Events has no fixed cap tied to the preflight latency
            // budget (it runs on the dispatcher thread, off the
            // client-request path) — the cap is generous but still
            // bounded so a misconfigured huge timeout cannot wedge the
            // dispatcher indefinitely on one delivery.
            const EVENTS_TIMEOUT_CAP_MS: u64 = 30_000;
            let validated = validate_surface("events", &surface_cfg, EVENTS_TIMEOUT_CAP_MS)?;
            let kinds = e
                .kinds
                .iter()
                .filter_map(|k| crate::event::EventKind::parse(k))
                .collect();
            Ok(ValidatedEventsConfig {
                url: validated.url,
                timeout: validated.timeout,
                max_attempts: e.max_attempts,
                kinds,
                secret: validated.secret,
            })
        })
        .transpose()?;

    Ok(ValidatedExtensionConfig {
        business_context_provider,
        policy_webhook,
        events,
    })
}

#[cfg(test)]
mod latency_budget {
    use super::*;

    /// The assertion this whole module's doc comment promises: a future
    /// change to `CLI_REQUEST_TIMEOUT`, `RECON_MAX_DURATION`, or either
    /// cap that violates the required slack fails loudly here rather
    /// than silently blowing the client's read timeout in production.
    #[test]
    fn recon_plus_both_caps_plus_slack_stays_under_the_request_timeout() {
        let total = RECON_MAX_DURATION
            + Duration::from_millis(BUSINESS_CONTEXT_TIMEOUT_CAP_MS)
            + Duration::from_millis(POLICY_WEBHOOK_TIMEOUT_CAP_MS)
            + REQUIRED_SLACK;
        assert!(
            total < CLI_REQUEST_TIMEOUT,
            "recon ({RECON_MAX_DURATION:?}) + business-context cap \
             ({BUSINESS_CONTEXT_TIMEOUT_CAP_MS}ms) + policy-webhook cap \
             ({POLICY_WEBHOOK_TIMEOUT_CAP_MS}ms) + required slack ({REQUIRED_SLACK:?}) = \
             {total:?}, which is not strictly under the client's REQUEST_TIMEOUT \
             ({CLI_REQUEST_TIMEOUT:?})"
        );
    }

    #[test]
    fn defaults_never_exceed_their_own_caps() {
        const _: () =
            assert!(BUSINESS_CONTEXT_TIMEOUT_DEFAULT_MS <= BUSINESS_CONTEXT_TIMEOUT_CAP_MS);
        const _: () = assert!(POLICY_WEBHOOK_TIMEOUT_DEFAULT_MS <= POLICY_WEBHOOK_TIMEOUT_CAP_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_surface() -> SurfaceConfig {
        SurfaceConfig {
            url: "http://127.0.0.1:8787/libra/business-context".to_string(),
            timeout_ms: BUSINESS_CONTEXT_TIMEOUT_DEFAULT_MS,
            secret_command: "/bin/sh".to_string(),
            secret_args: vec!["-c".to_string(), "printf test-secret".to_string()],
        }
    }

    #[test]
    fn a_valid_loopback_surface_validates() {
        let raw = ExtensionConfig {
            business_context_provider: Some(base_surface()),
            ..Default::default()
        };
        let validated = validate(raw).unwrap();
        assert!(validated.business_context_provider.is_some());
        assert!(!validated.is_empty());
    }

    #[test]
    fn absent_extensions_validates_to_all_none() {
        let validated = validate(ExtensionConfig::default()).unwrap();
        assert!(validated.is_empty());
    }

    #[test]
    fn https_is_refused() {
        let mut surface = base_surface();
        surface.url = "https://127.0.0.1:8787/libra/business-context".to_string();
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::SchemeNotHttp { .. }
        ));
    }

    #[test]
    fn localhost_is_refused_even_though_it_is_loopback() {
        let mut surface = base_surface();
        surface.url = "http://localhost:8787/libra/business-context".to_string();
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::HostNotLoopbackLiteral { .. }
        ));
    }

    #[test]
    fn a_non_loopback_host_is_refused() {
        let mut surface = base_surface();
        surface.url = "http://10.0.0.5:8787/libra/business-context".to_string();
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::HostNotLoopbackLiteral { .. }
        ));
    }

    #[test]
    fn ipv6_loopback_literal_is_accepted() {
        let mut surface = base_surface();
        surface.url = "http://[::1]:8787/libra/business-context".to_string();
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(validate(raw).is_ok());
    }

    #[test]
    fn userinfo_is_refused() {
        let mut surface = base_surface();
        surface.url = "http://user:pass@127.0.0.1:8787/libra/business-context".to_string();
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::UrlHasUserinfo { .. }
        ));
    }

    #[test]
    fn query_and_fragment_are_refused() {
        for suffix in ["?x=1", "#frag"] {
            let mut surface = base_surface();
            surface.url = format!("http://127.0.0.1:8787/libra/business-context{suffix}");
            let raw = ExtensionConfig {
                business_context_provider: Some(surface),
                ..Default::default()
            };
            assert!(matches!(
                validate(raw).unwrap_err(),
                ConfigError::UrlHasQueryOrFragment { .. }
            ));
        }
    }

    #[test]
    fn an_over_cap_timeout_is_refused() {
        let mut surface = base_surface();
        surface.timeout_ms = BUSINESS_CONTEXT_TIMEOUT_CAP_MS + 1;
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::TimeoutAboveCap { .. }
        ));
    }

    #[test]
    fn an_empty_secret_command_is_refused() {
        let mut surface = base_surface();
        surface.secret_command = String::new();
        let raw = ExtensionConfig {
            business_context_provider: Some(surface),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::EmptySecretCommand { .. }
        ));
    }

    #[test]
    fn zero_max_attempts_is_refused() {
        let raw = ExtensionConfig {
            events: Some(EventsConfig {
                url: "http://127.0.0.1:8787/libra/events".to_string(),
                timeout_ms: EVENTS_TIMEOUT_DEFAULT_MS,
                max_attempts: 0,
                kinds: vec!["admission".to_string()],
                secret_command: "/bin/sh".to_string(),
                secret_args: vec!["-c".to_string(), "printf test-secret".to_string()],
            }),
            ..Default::default()
        };
        assert!(matches!(
            validate(raw).unwrap_err(),
            ConfigError::EventsMaxAttemptsZero
        ));
    }
}
