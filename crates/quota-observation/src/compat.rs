//! Read-only protocol-version compatibility reporting (HORO-1870):
//! given a parsed [`BootstrapResponse`], report whether this host's own
//! [`PROTOCOL_VERSION`] appears in the extension's declared
//! `supported_versions`.
//!
//! This does **not** address gap G9 (`tests/README.md`'s
//! compatibility-matrix row 7, which is about `BootstrapResult`'s
//! `capabilities` never being cross-checked against what a `snapshot`
//! call requires) — this module never reads `capabilities` at all.
//! `protocol_version`/`bootstrap_version` negotiation is its own,
//! separately-unticketed unenforced gap this module makes observable to
//! an operator; G9 itself remains completely open.
//!
//! This is display-only for an operator CLI (a `describe`/`doctor`
//! command, HORO-1870 PR2) — it does **not** enforce or reject
//! anything. `crate::runner` is unchanged by this module's existence,
//! and no new rejection behavior is added anywhere in this crate.

use crate::wire::{BootstrapResponse, PROTOCOL_VERSION};

/// The result of comparing this host's [`PROTOCOL_VERSION`] against an
/// extension's declared `supported_versions`. Purely informational —
/// nothing in this crate consults this type to decide whether to proceed
/// with a `snapshot` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescribeCompatibility {
    /// The extension's `supported_versions` names this host's
    /// [`PROTOCOL_VERSION`].
    Supported,
    /// The extension's `supported_versions` does not name this host's
    /// [`PROTOCOL_VERSION`] — a later `snapshot` call is expected to
    /// fail, but this type does not act on that by itself.
    Unsupported,
}

/// Reports whether `response`'s declared `supported_versions` names this
/// host's own [`PROTOCOL_VERSION`].
///
/// Returns `None` when `response` carries no `result` at all — bootstrap
/// itself failed (`response.error` is set instead), so there is nothing
/// to compare; the caller already has `response.error` to report that
/// case.
pub fn check_describe(response: &BootstrapResponse) -> Option<DescribeCompatibility> {
    let result = response.result.as_ref()?;
    if result
        .supported_versions
        .iter()
        .any(|version| version == PROTOCOL_VERSION)
    {
        Some(DescribeCompatibility::Supported)
    } else {
        Some(DescribeCompatibility::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::BootstrapResult;

    fn response_with(result: Option<BootstrapResult>) -> BootstrapResponse {
        BootstrapResponse {
            bootstrap_version: crate::wire::BOOTSTRAP_VERSION.into(),
            request_id: "r1".into(),
            result,
            error: None,
        }
    }

    #[test]
    fn supported_when_this_hosts_protocol_version_is_listed() {
        let response = response_with(Some(BootstrapResult {
            extension_id: "ext".into(),
            extension_version: "1.0.0".into(),
            supported_versions: vec![PROTOCOL_VERSION.to_string(), "some.other/9".into()],
            capabilities: vec![],
        }));
        assert_eq!(
            check_describe(&response),
            Some(DescribeCompatibility::Supported)
        );
    }

    #[test]
    fn unsupported_when_this_hosts_protocol_version_is_absent() {
        let response = response_with(Some(BootstrapResult {
            extension_id: "ext".into(),
            extension_version: "1.0.0".into(),
            supported_versions: vec!["some.other/9".into()],
            capabilities: vec![],
        }));
        assert_eq!(
            check_describe(&response),
            Some(DescribeCompatibility::Unsupported)
        );
    }

    #[test]
    fn none_when_bootstrap_response_has_no_result() {
        let response = response_with(None);
        assert_eq!(check_describe(&response), None);
    }
}
