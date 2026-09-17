//! Placeholder crate for `libra-governor-daemon`.
//!
//! The daemon is the source-of-truth state machine, ledger, and policy
//! engine for the Governor. This crate is currently a bootstrap scaffold
//! (HORO-1118) with no real logic.

/// Returns the crate name, confirming the crate builds and links.
pub fn placeholder() -> &'static str {
    "daemon"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "daemon");
    }
}
