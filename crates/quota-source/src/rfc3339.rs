//! A small RFC 3339 UTC timestamp parser, matching the exact leniency
//! `libra_governor_domain::quota_window`'s own (private,
//! `pub(crate)`-only, hence unreusable from this crate) wire modules use:
//! accept a trailing `Z`, normalize it to `+00:00` before handing off to
//! `time`'s parser. Kept deliberately tiny and local rather than adding a
//! new workspace dependency for one parsing concern.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid RFC 3339 timestamp {raw:?}: {source}")]
pub struct Rfc3339Error {
    raw: String,
    #[source]
    source: time::error::Parse,
}

pub fn parse(raw: &str) -> Result<OffsetDateTime, Rfc3339Error> {
    let normalized = raw
        .strip_suffix('Z')
        .map(|s| format!("{s}+00:00"))
        .unwrap_or_else(|| raw.to_string());
    OffsetDateTime::parse(&normalized, &Rfc3339).map_err(|source| Rfc3339Error {
        raw: raw.to_string(),
        source,
    })
}
