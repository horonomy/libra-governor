//! The one piece of this crate `libra-governor doctor` surfaces: which
//! quota-source mode is active. See
//! `crates/cli/src/doctor_cmd.rs`'s `collect_findings` for where this is
//! wired in as an additive, local-only finding — never a new `DoctorResult`
//! field, never a daemon/protocol change (this ticket stays out of
//! HORO-1763's daemon/ledger lane).

use std::fmt;

/// Which quota-source capability this build is actually running with.
/// `Unavailable` is this ticket's honest, permanent answer for this
/// build — there is no live adapter and none is wired in — see the
/// crate's own module docs for why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaSourceMode {
    /// No authorized live quota source exists in this build. The only
    /// path available is [`crate::fixture::import_fixture`].
    Unavailable,
    /// Reserved for a future, separately-reviewed adapter crate that
    /// implements [`crate::QuotaSource`] against a real,
    /// [`crate::LiveEndpointAuthorization`]-gated endpoint. Never
    /// returned by [`describe_active_mode`] in this build — named here so
    /// `doctor`'s rendering code already has a stable place to grow into
    /// once one exists, without another protocol/daemon change.
    LiveAdapter,
}

impl fmt::Display for QuotaSourceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuotaSourceMode::Unavailable => write!(
                f,
                "unavailable — no authorized live quota source; fixture import only"
            ),
            QuotaSourceMode::LiveAdapter => write!(f, "live adapter"),
        }
    }
}

/// This build's actual mode. A plain function (not a trait method, not
/// daemon state) because this ticket ships no live adapter to select
/// between — there is exactly one honest answer today.
pub fn describe_active_mode() -> QuotaSourceMode {
    QuotaSourceMode::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_active_mode_is_unavailable_in_this_build() {
        assert_eq!(describe_active_mode(), QuotaSourceMode::Unavailable);
    }

    #[test]
    fn unavailable_renders_the_honest_doctor_message() {
        assert_eq!(
            QuotaSourceMode::Unavailable.to_string(),
            "unavailable — no authorized live quota source; fixture import only"
        );
    }
}
