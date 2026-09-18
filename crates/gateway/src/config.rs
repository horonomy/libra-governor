//! Gateway configuration and the validation that refuses to start it
//! (HORO-1144).
//!
//! # Validation is a refusal, not a warning
//!
//! Every check in [`validate`] returns a [`ConfigError`] that leaves the
//! gateway **off**. The daemon logs it and carries on serving hooks and
//! the statusline normally — a mistyped upstream host must not take away
//! preflight, estimation, and the ledger — but nothing starts listening.
//! A gateway that started in a configuration it could not enforce would
//! be worse than no gateway, because the user would believe they had one.
//!
//! # The two refusals worth reading the code for
//!
//! **The upstream is configuration, never input.** The host must be on an
//! allowlist, the scheme must be `https`, and the URL must carry no
//! userinfo, path, query, or fragment — anything extra is a sign the
//! value came from somewhere it should not have. Combined with
//! [`crate::route`]'s closed table, there is no inbound value anywhere
//! that can influence where bytes go.
//!
//! **An honest capability tier is enforced here.** Starting in
//! [`GatewayCredentialMode::PassThroughSubscription`] against a
//! [`ResourceKind::Usd`] admission policy is refused
//! ([`ConfigError::SubscriptionModeCannotEnforceUsd`]). That pairing
//! would advertise a hard monetary cap the system cannot honor, because a
//! subscription's own quota accounting is not exposed to us. This is the
//! mechanism behind HORO-1144's "subscription modes are labeled
//! honestly" — a validation failure rather than a caveat in a README.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use libra_governor_domain::{EnforcementTier, ResourceKind};

use crate::credential::CredentialCommand;
use crate::pricing::{ModelPricing, PricingTable};

/// Default cap on an inbound request body. Anthropic's own limit is well
/// under this; the cap exists so a local process cannot make the daemon
/// buffer unbounded memory by claiming to send a message.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Default cap on requests in flight at once.
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 64;
/// Default lifetime of a per-request reservation. Shorter than the
/// plan-level TTL on purpose: an in-flight request whose process died
/// should return its capacity in minutes, not a quarter of an hour.
pub const DEFAULT_GATEWAY_RESERVATION_TTL_SECS: u64 = 600;
/// Default upstream connect timeout. There is deliberately no *response*
/// timeout — a streaming completion is long-lived by design, and cutting
/// one off at an arbitrary deadline would break the product to enforce a
/// number nobody chose.
pub const DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 30;
/// The only upstream host proxied unless the operator extends the list.
pub const DEFAULT_UPSTREAM_HOST: &str = "api.anthropic.com";

/// Who holds the upstream credential — the configured fact the
/// enforcement tier follows from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayCredentialMode {
    /// The Governor resolves and holds an API/BYOK credential the agent
    /// never sees. [`EnforcementTier::GatewayMetered`].
    GovernorHeld { credential: CredentialCommand },
    /// The caller's own subscription credential is forwarded unchanged.
    /// Token usage is still observed and settled exactly; no monetary cap
    /// is claimed. [`EnforcementTier::GatewayObservedQuota`].
    PassThroughSubscription,
}

impl GatewayCredentialMode {
    /// The tier this credential mode implies. A pure mapping — see
    /// `libra_governor_domain::capability`'s "configured, never
    /// negotiated".
    pub fn tier(&self) -> EnforcementTier {
        match self {
            GatewayCredentialMode::GovernorHeld { .. } => EnforcementTier::GatewayMetered,
            GatewayCredentialMode::PassThroughSubscription => EnforcementTier::GatewayObservedQuota,
        }
    }
}

/// Raw, unvalidated gateway configuration as an operator writes it.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Where the gateway listens. Must be a loopback address.
    pub bind_addr: SocketAddr,
    /// The upstream base URL — scheme, host, and optional port only.
    pub upstream: String,
    /// Hosts the upstream may be. Extending this is a deliberate act.
    pub upstream_host_allowlist: Vec<String>,
    /// Permits an upstream port other than 443.
    pub allow_nonstandard_port: bool,
    /// Permits an `http://` upstream **only** when its host is a loopback
    /// literal. See ADR 0003's "Deviations": this exists so the crate's
    /// own fake-upstream tests can exist, and the loopback host check —
    /// not the flag — is what keeps it from being an exfiltration path.
    pub allow_plaintext_loopback_upstream: bool,
    pub credential_mode: GatewayCredentialMode,
    /// Where the local capability token is persisted.
    pub token_path: PathBuf,
    pub reservation_ttl_secs: u64,
    pub max_request_bytes: usize,
    pub max_concurrent_requests: usize,
    pub upstream_connect_timeout_secs: u64,
    /// Operator corrections/extensions to the pinned pricing table.
    pub pricing_overrides: BTreeMap<String, ModelPricing>,
}

impl GatewayConfig {
    /// A configuration with the shipped defaults for everything except
    /// the two things that have no sensible default.
    pub fn new(
        bind_addr: SocketAddr,
        token_path: PathBuf,
        credential_mode: GatewayCredentialMode,
    ) -> Self {
        Self {
            bind_addr,
            upstream: format!("https://{DEFAULT_UPSTREAM_HOST}"),
            upstream_host_allowlist: vec![DEFAULT_UPSTREAM_HOST.to_string()],
            allow_nonstandard_port: false,
            allow_plaintext_loopback_upstream: false,
            credential_mode,
            token_path,
            reservation_ttl_secs: DEFAULT_GATEWAY_RESERVATION_TTL_SECS,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            upstream_connect_timeout_secs: DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS,
            pricing_overrides: BTreeMap::new(),
        }
    }
}

/// Why a gateway configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("upstream `{upstream}` is not a valid absolute URL")]
    UpstreamNotAUrl { upstream: String },
    #[error("upstream scheme `{scheme}` is not https (and is not a permitted plaintext loopback)")]
    UpstreamSchemeNotHttps { scheme: String },
    #[error("upstream host `{host}` is not in the configured allowlist")]
    UpstreamHostNotAllowed { host: String },
    #[error("upstream URL carries userinfo, which would smuggle a credential into a config field")]
    UpstreamHasUserinfo,
    #[error(
        "upstream URL carries a path, query, or fragment; only a scheme/host/port base is accepted"
    )]
    UpstreamHasPathOrQuery,
    #[error("upstream port {port} is not 443 and allow_nonstandard_port is not set")]
    UpstreamNonStandardPort { port: u16 },
    #[error("bind address {addr} is not a loopback address")]
    BindAddrNotLoopback { addr: SocketAddr },
    #[error(
        "pass-through subscription mode cannot enforce a USD budget: the provider does not expose \
         that subscription's own accounting, so a monetary hard cap would be a false claim"
    )]
    SubscriptionModeCannotEnforceUsd,
    #[error("{field} must be greater than zero")]
    NonPositive { field: &'static str },
}

/// A configuration that has passed every check in [`validate`]. The proxy
/// takes one of these, never a raw [`GatewayConfig`], so an unvalidated
/// value cannot reach the request path at all.
#[derive(Debug, Clone)]
pub struct ValidatedGatewayConfig {
    pub bind_addr: SocketAddr,
    /// `https` in every real deployment; `http` only for a permitted
    /// loopback upstream.
    pub upstream_scheme: String,
    pub upstream_host: String,
    pub upstream_port: u16,
    /// Every `Host` header value this listener accepts. A request whose
    /// `Host` is not one of these is refused before routing.
    pub accepted_host_authorities: Vec<String>,
    pub credential_mode: GatewayCredentialMode,
    pub tier: EnforcementTier,
    pub token_path: PathBuf,
    pub reservation_ttl_secs: u64,
    pub max_request_bytes: usize,
    pub max_concurrent_requests: usize,
    pub upstream_connect_timeout: Duration,
    pub pricing: PricingTable,
}

impl ValidatedGatewayConfig {
    /// `scheme://host[:port]` — the base every outbound URL is built
    /// from. Combined with a [`crate::route::Route`]'s own constant path,
    /// this is the entire outbound URL construction.
    pub fn upstream_origin(&self) -> String {
        let default_port = if self.upstream_scheme == "https" {
            443
        } else {
            80
        };
        if self.upstream_port == default_port {
            format!("{}://{}", self.upstream_scheme, self.upstream_host)
        } else {
            format!(
                "{}://{}:{}",
                self.upstream_scheme, self.upstream_host, self.upstream_port
            )
        }
    }

    /// `true` if `host_header` is one this listener answers to.
    pub fn accepts_host(&self, host_header: &str) -> bool {
        self.accepted_host_authorities
            .iter()
            .any(|accepted| accepted.eq_ignore_ascii_case(host_header))
    }
}

/// Whether `host` is a loopback literal. Deliberately a *literal* check
/// and not a DNS resolution: resolution is exactly the step an attacker
/// controls, and a name that resolves to loopback today can resolve
/// elsewhere on the next lookup.
fn is_loopback_host(host: &str) -> bool {
    let stripped = host.trim_start_matches('[').trim_end_matches(']');
    if stripped.eq_ignore_ascii_case("localhost") {
        return true;
    }
    stripped
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Validates `raw` against `policy_resource_kind` — the [`ResourceKind`]
/// the daemon's admission policy is denominated in.
///
/// The policy's kind is an input because one of the checks is about the
/// *combination*: see [`ConfigError::SubscriptionModeCannotEnforceUsd`].
pub fn validate(
    raw: GatewayConfig,
    policy_resource_kind: ResourceKind,
) -> Result<ValidatedGatewayConfig, ConfigError> {
    if raw.credential_mode == GatewayCredentialMode::PassThroughSubscription
        && policy_resource_kind == ResourceKind::Usd
    {
        return Err(ConfigError::SubscriptionModeCannotEnforceUsd);
    }

    if !raw.bind_addr.ip().is_loopback() {
        return Err(ConfigError::BindAddrNotLoopback {
            addr: raw.bind_addr,
        });
    }

    let uri: hyper::Uri = raw
        .upstream
        .parse()
        .map_err(|_| ConfigError::UpstreamNotAUrl {
            upstream: raw.upstream.clone(),
        })?;

    let scheme = uri
        .scheme_str()
        .ok_or_else(|| ConfigError::UpstreamNotAUrl {
            upstream: raw.upstream.clone(),
        })?
        .to_ascii_lowercase();
    let authority = uri
        .authority()
        .ok_or_else(|| ConfigError::UpstreamNotAUrl {
            upstream: raw.upstream.clone(),
        })?;

    // `Authority::host()` strips userinfo, so the presence of `@` in the
    // full authority string is how userinfo is detected at all.
    if authority.as_str().contains('@') {
        return Err(ConfigError::UpstreamHasUserinfo);
    }

    // `Uri` normalises a bare origin's path to "/", so both an empty
    // path and "/" mean "no path". Anything else is extra.
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("");
    if !(path_and_query.is_empty() || path_and_query == "/") {
        return Err(ConfigError::UpstreamHasPathOrQuery);
    }
    if raw.upstream.contains('#') {
        return Err(ConfigError::UpstreamHasPathOrQuery);
    }

    let host = authority.host().to_ascii_lowercase();
    if host.is_empty() {
        return Err(ConfigError::UpstreamNotAUrl {
            upstream: raw.upstream.clone(),
        });
    }

    match scheme.as_str() {
        "https" => {}
        "http" if raw.allow_plaintext_loopback_upstream && is_loopback_host(&host) => {}
        _ => return Err(ConfigError::UpstreamSchemeNotHttps { scheme }),
    }

    if !raw
        .upstream_host_allowlist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&host))
    {
        return Err(ConfigError::UpstreamHostNotAllowed { host });
    }

    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = authority.port_u16().unwrap_or(default_port);
    if scheme == "https" && port != 443 && !raw.allow_nonstandard_port {
        return Err(ConfigError::UpstreamNonStandardPort { port });
    }

    for (field, value) in [
        ("reservation_ttl_secs", raw.reservation_ttl_secs as usize),
        ("max_request_bytes", raw.max_request_bytes),
        ("max_concurrent_requests", raw.max_concurrent_requests),
        (
            "upstream_connect_timeout_secs",
            raw.upstream_connect_timeout_secs as usize,
        ),
    ] {
        if value == 0 {
            return Err(ConfigError::NonPositive { field });
        }
    }

    let tier = raw.credential_mode.tier();
    let accepted_host_authorities = accepted_hosts_for(raw.bind_addr);
    let pricing = if raw.pricing_overrides.is_empty() {
        PricingTable::pinned()
    } else {
        PricingTable::with_overrides(raw.pricing_overrides.clone())
    };

    Ok(ValidatedGatewayConfig {
        bind_addr: raw.bind_addr,
        upstream_scheme: scheme,
        upstream_host: host,
        upstream_port: port,
        accepted_host_authorities,
        credential_mode: raw.credential_mode,
        tier,
        token_path: raw.token_path,
        reservation_ttl_secs: raw.reservation_ttl_secs,
        max_request_bytes: raw.max_request_bytes,
        max_concurrent_requests: raw.max_concurrent_requests,
        upstream_connect_timeout: Duration::from_secs(raw.upstream_connect_timeout_secs),
        pricing,
    })
}

/// The `Host` header values a listener on `bind_addr` answers to.
///
/// All of them denote this same loopback listener under a different
/// spelling — a client may write `localhost`, `127.0.0.1`, or `[::1]` for
/// the address it actually dialled. What is *not* here is any other host:
/// a request arriving with `Host: api.anthropic.com` is somebody using
/// this listener as a proxy for a destination it did not dial, and is
/// refused.
fn accepted_hosts_for(bind_addr: SocketAddr) -> Vec<String> {
    let port = bind_addr.port();
    vec![
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
        bind_addr.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> GatewayConfig {
        GatewayConfig::new(
            "127.0.0.1:8787".parse().unwrap(),
            PathBuf::from("/tmp/libra-test/gateway.token"),
            GatewayCredentialMode::GovernorHeld {
                credential: CredentialCommand::new("/bin/true", vec![]),
            },
        )
    }

    #[test]
    fn the_shipped_default_configuration_validates() {
        let validated = validate(base_config(), ResourceKind::Tokens).unwrap();
        assert_eq!(validated.upstream_scheme, "https");
        assert_eq!(validated.upstream_host, DEFAULT_UPSTREAM_HOST);
        assert_eq!(validated.upstream_port, 443);
        assert_eq!(validated.upstream_origin(), "https://api.anthropic.com");
        assert_eq!(validated.tier, EnforcementTier::GatewayMetered);
    }

    #[test]
    fn a_plaintext_upstream_is_refused() {
        let mut config = base_config();
        config.upstream = "http://api.anthropic.com".to_string();
        assert_eq!(
            validate(config, ResourceKind::Tokens).unwrap_err(),
            ConfigError::UpstreamSchemeNotHttps {
                scheme: "http".to_string()
            }
        );
    }

    #[test]
    fn a_plaintext_upstream_is_permitted_only_for_a_loopback_host_with_the_flag() {
        let mut config = base_config();
        config.upstream = "http://127.0.0.1:9911".to_string();
        config.upstream_host_allowlist = vec!["127.0.0.1".to_string()];

        // The flag alone is not the safeguard; without it, refused.
        assert!(validate(config.clone(), ResourceKind::Tokens).is_err());

        config.allow_plaintext_loopback_upstream = true;
        let validated = validate(config.clone(), ResourceKind::Tokens).unwrap();
        assert_eq!(validated.upstream_origin(), "http://127.0.0.1:9911");

        // And the host check, not the flag, is what refuses a real host.
        config.upstream = "http://api.anthropic.com".to_string();
        config.upstream_host_allowlist = vec![DEFAULT_UPSTREAM_HOST.to_string()];
        assert!(
            validate(config, ResourceKind::Tokens).is_err(),
            "the plaintext flag must never permit a non-loopback upstream"
        );
    }

    #[test]
    fn a_host_outside_the_allowlist_is_refused() {
        let mut config = base_config();
        config.upstream = "https://evil.example.com".to_string();
        assert_eq!(
            validate(config, ResourceKind::Tokens).unwrap_err(),
            ConfigError::UpstreamHostNotAllowed {
                host: "evil.example.com".to_string()
            }
        );
    }

    #[test]
    fn an_extended_allowlist_admits_the_host_it_names() {
        let mut config = base_config();
        config.upstream = "https://proxy.internal.example".to_string();
        config.upstream_host_allowlist = vec!["proxy.internal.example".to_string()];
        let validated = validate(config, ResourceKind::Tokens).unwrap();
        assert_eq!(validated.upstream_host, "proxy.internal.example");
    }

    #[test]
    fn userinfo_in_the_upstream_url_is_refused() {
        let mut config = base_config();
        config.upstream = "https://user:sk-fake-secret@api.anthropic.com".to_string();
        assert_eq!(
            validate(config, ResourceKind::Tokens).unwrap_err(),
            ConfigError::UpstreamHasUserinfo
        );
    }

    #[test]
    fn a_path_query_or_fragment_on_the_upstream_is_refused() {
        for upstream in [
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/?x=1",
            "https://api.anthropic.com/#frag",
        ] {
            let mut config = base_config();
            config.upstream = upstream.to_string();
            assert_eq!(
                validate(config, ResourceKind::Tokens).unwrap_err(),
                ConfigError::UpstreamHasPathOrQuery,
                "{upstream} must not be accepted as a bare origin"
            );
        }
    }

    #[test]
    fn a_bare_origin_with_a_trailing_slash_is_accepted() {
        let mut config = base_config();
        config.upstream = "https://api.anthropic.com/".to_string();
        assert!(validate(config, ResourceKind::Tokens).is_ok());
    }

    #[test]
    fn a_non_standard_https_port_needs_an_explicit_opt_in() {
        let mut config = base_config();
        config.upstream = "https://api.anthropic.com:8443".to_string();
        assert_eq!(
            validate(config.clone(), ResourceKind::Tokens).unwrap_err(),
            ConfigError::UpstreamNonStandardPort { port: 8443 }
        );

        config.allow_nonstandard_port = true;
        let validated = validate(config, ResourceKind::Tokens).unwrap();
        assert_eq!(
            validated.upstream_origin(),
            "https://api.anthropic.com:8443"
        );
    }

    #[test]
    fn a_non_loopback_bind_address_is_refused() {
        let mut config = base_config();
        config.bind_addr = "0.0.0.0:8787".parse().unwrap();
        assert_eq!(
            validate(config, ResourceKind::Tokens).unwrap_err(),
            ConfigError::BindAddrNotLoopback {
                addr: "0.0.0.0:8787".parse().unwrap()
            }
        );
    }

    #[test]
    fn subscription_mode_against_a_usd_budget_is_refused_by_code_not_documentation() {
        let mut config = base_config();
        config.credential_mode = GatewayCredentialMode::PassThroughSubscription;
        assert_eq!(
            validate(config.clone(), ResourceKind::Usd).unwrap_err(),
            ConfigError::SubscriptionModeCannotEnforceUsd
        );

        // The same mode against a token budget is a perfectly honest
        // deployment — it just cannot claim a monetary cap.
        let validated = validate(config, ResourceKind::Tokens).unwrap();
        assert_eq!(validated.tier, EnforcementTier::GatewayObservedQuota);
    }

    #[test]
    fn governor_held_mode_may_enforce_a_usd_budget() {
        assert!(validate(base_config(), ResourceKind::Usd).is_ok());
    }

    #[test]
    fn zero_valued_limits_are_refused() {
        for mutate in [
            (|c: &mut GatewayConfig| c.reservation_ttl_secs = 0) as fn(&mut GatewayConfig),
            |c: &mut GatewayConfig| c.max_request_bytes = 0,
            |c: &mut GatewayConfig| c.max_concurrent_requests = 0,
            |c: &mut GatewayConfig| c.upstream_connect_timeout_secs = 0,
        ] {
            let mut config = base_config();
            mutate(&mut config);
            assert!(matches!(
                validate(config, ResourceKind::Tokens).unwrap_err(),
                ConfigError::NonPositive { .. }
            ));
        }
    }

    #[test]
    fn a_garbage_upstream_is_refused_rather_than_partially_parsed() {
        for upstream in ["not a url", "", "api.anthropic.com", "https://"] {
            let mut config = base_config();
            config.upstream = upstream.to_string();
            assert!(
                validate(config, ResourceKind::Tokens).is_err(),
                "{upstream:?} must not validate"
            );
        }
    }

    #[test]
    fn the_host_header_check_accepts_only_this_listener_s_own_spellings() {
        let validated = validate(base_config(), ResourceKind::Tokens).unwrap();
        assert!(validated.accepts_host("127.0.0.1:8787"));
        assert!(validated.accepts_host("localhost:8787"));
        assert!(validated.accepts_host("LOCALHOST:8787"));
        assert!(validated.accepts_host("[::1]:8787"));

        assert!(
            !validated.accepts_host("api.anthropic.com"),
            "a request naming a destination it did not dial is proxy abuse"
        );
        assert!(!validated.accepts_host("127.0.0.1:9999"));
        assert!(!validated.accepts_host("evil.example.com:8787"));
        assert!(!validated.accepts_host(""));
    }

    #[test]
    fn pricing_overrides_reach_the_validated_table() {
        let mut config = base_config();
        config.pricing_overrides.insert(
            "new-model".to_string(),
            ModelPricing {
                input_usd_per_mtok: 1.0,
                cache_creation_input_usd_per_mtok: 1.0,
                cache_read_input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 1.0,
            },
        );
        let validated = validate(config, ResourceKind::Tokens).unwrap();
        assert!(validated.pricing.lookup("new-model").is_some());
        assert_eq!(validated.pricing.override_count(), 1);
    }

    #[test]
    fn loopback_host_detection_is_literal_and_never_resolves() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.1.2.3"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("api.anthropic.com"));
        assert!(
            !is_loopback_host("localhost.evil.example.com"),
            "a name that merely starts with localhost is not loopback"
        );
        assert!(!is_loopback_host("0.0.0.0"));
    }
}
