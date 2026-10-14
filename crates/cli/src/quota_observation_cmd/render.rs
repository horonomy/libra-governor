//! Human-readable rendering for the quota-observation CLI's read/probe
//! commands. Every function here formats only parsed, typed fields —
//! never a raw response frame, raw stderr, or anything that could carry
//! through a credential-shaped string from an extension's output
//! verbatim (N43/N51 in `crates/quota-observation/tests/README.md`).

use libra_governor_quota_observation::compat::DescribeCompatibility;
use libra_governor_quota_observation::freshness::{self, BoundSnapshotResult};
use libra_governor_quota_observation::registration::Registration;
use libra_governor_quota_observation::wire::{
    BootstrapResponse, Coverage, Metric, Reading, Scope, Unit, WireError,
};
use time::OffsetDateTime;

/// `scope`'s one-line operator-facing form — `<kind>:<handle>`, never the
/// `{kind: "account", handle: "acct"}`-shaped `Debug` output a future
/// reader would otherwise copy-paste into something expecting a plain
/// string.
fn format_scope(scope: &Scope) -> String {
    let (kind, handle) = match scope {
        Scope::Principal { handle } => ("principal", handle),
        Scope::Account { handle } => ("account", handle),
        Scope::Team { handle } => ("team", handle),
        Scope::Organization { handle } => ("organization", handle),
        Scope::Project { handle } => ("project", handle),
        Scope::SharedPool { handle } => ("shared_pool", handle),
        Scope::Provider { handle } => ("provider", handle),
    };
    format!("{kind}:{handle}")
}

fn format_metric(metric: Metric) -> &'static str {
    match metric {
        Metric::TotalTokens => "total_tokens",
        Metric::InputTokens => "input_tokens",
        Metric::OutputTokens => "output_tokens",
        Metric::Requests => "requests",
        Metric::Spend => "spend",
        Metric::CreditUsage => "credit_usage",
        Metric::Utilization => "utilization",
    }
}

fn format_unit(unit: &Unit) -> String {
    match unit {
        Unit::Tokens => "tokens".to_string(),
        Unit::Requests => "requests".to_string(),
        Unit::UsdCents => "usd_cents".to_string(),
        Unit::PercentBasisPoints => "percent_basis_points".to_string(),
        Unit::OpaqueCredit { namespace } => format!("opaque_credit:{namespace}"),
    }
}

/// `reading`'s one-line operator-facing form, using [`Quantity`]'s own
/// `Display` (the exact canonical decimal string, never `Debug`'s
/// `Quantity { canonical: "600000", value: 600000 }` struct dump).
fn format_reading(reading: &Reading) -> String {
    match reading {
        Reading::Used { value, limit } => format!("used {value} of {limit}"),
        Reading::Remaining { value, limit } => format!("{value} remaining of {limit}"),
        Reading::Utilization { used_basis_points } => {
            format!("{used_basis_points} basis points used")
        }
        Reading::Undisclosed => "undisclosed".to_string(),
    }
}

/// `doctor`'s one passive-mode line per registration — no live call, just
/// what is already on disk.
pub fn doctor_passive_line(registration: &Registration) -> String {
    format!(
        "{} — extension {} {} — enabled={} approved={}",
        registration.registration_id,
        registration.extension_id,
        registration.extension_version,
        registration.enabled,
        registration.approval.approved,
    )
}

/// `doctor --probe`'s one live-call summary: the extension's own declared
/// identity/capabilities and whether this host's protocol version is
/// supported. Never reports quota availability — that field does not
/// exist on a `BootstrapResponse` at all.
pub fn describe_probe_summary(
    response: &BootstrapResponse,
    compatibility: Option<DescribeCompatibility>,
) -> String {
    if let Some(error) = &response.error {
        return format!(
            "  --probe: extension reported an error: {}",
            format_wire_error(error)
        );
    }
    let Some(result) = &response.result else {
        return "  --probe: response carried neither a result nor an error".to_string();
    };
    let compat = match compatibility {
        Some(DescribeCompatibility::Supported) => "supported",
        Some(DescribeCompatibility::Unsupported) => "NOT supported by this extension",
        None => "unknown (no result to compare)",
    };
    format!(
        "  --probe: extension_id={} extension_version={} supported_versions={:?} \
         capabilities={:?} — this host's protocol version is {compat}",
        result.extension_id,
        result.extension_version,
        result.supported_versions,
        result.capabilities,
    )
}

fn format_wire_error(error: &WireError) -> String {
    match error.retry_after_seconds {
        Some(seconds) => format!(
            "{:?} (retry: {:?} after {seconds}s)",
            error.code, error.retry
        ),
        None => format!("{:?} (retry: {:?})", error.code, error.retry),
    }
}

/// `inspect`'s error-response summary — reached when the extension itself
/// returned `{"error": ...}` rather than a snapshot result.
pub fn snapshot_error_summary(error: &WireError) -> String {
    format!("extension reported an error: {}", format_wire_error(error))
}

/// `inspect`'s live-result summary: the real `coverage`, each constraint's
/// reading (with a freshness tag computed against `received_at`/`now`),
/// and every `missing[]` entry with its reason. This is the only
/// rendering path in the whole CLI surface that can say
/// `coverage=partial` or tag a reading `stale`.
pub fn snapshot_result_summary(
    registration: &Registration,
    bound: &BoundSnapshotResult,
    received_at: OffsetDateTime,
    now: OffsetDateTime,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "coverage={:?} context.selector={} authority={}",
        bound.coverage, bound.context.selector, bound.authority
    ));
    for constraint in &bound.constraints {
        let max_staleness = registration
            .expected
            .iter()
            .find(|e| e.id == constraint.id)
            .map(|e| e.max_staleness_secs);
        let freshness_tag = match max_staleness {
            Some(max_staleness) => {
                match freshness::is_fresh(
                    constraint,
                    max_staleness,
                    bound.context_valid_until.as_deref(),
                    received_at,
                    now,
                ) {
                    Ok(true) => "fresh",
                    Ok(false) => "stale",
                    Err(_) => "unknown (invalid timestamp)",
                }
            }
            None => "unknown (no matching expected[] entry)",
        };
        lines.push(format!(
            "  constraint {} scope={} metric={} unit={} reading={} \
             upstream_enforcement={:?} observed_at={} [{freshness_tag}]",
            constraint.id,
            format_scope(&constraint.scope),
            format_metric(constraint.metric),
            format_unit(&constraint.unit),
            format_reading(&constraint.reading),
            constraint.upstream_enforcement,
            constraint.observed_at,
        ));
    }
    for missing in &bound.missing {
        lines.push(format!(
            "  missing {} reason={:?}",
            missing.id, missing.reason
        ));
    }
    if matches!(bound.coverage, Coverage::Partial) && bound.missing.is_empty() {
        // Unreachable in practice (apply_account_binding/validate_wire_coverage
        // already enforce this), kept only so a future refactor that broke
        // that invariant would still render something honest rather than
        // silently claiming partial coverage with no visible reason.
        lines.push("  (partial coverage with no listed reason)".to_string());
    }
    lines.join("\n")
}
