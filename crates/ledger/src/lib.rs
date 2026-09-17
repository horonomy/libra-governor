//! Placeholder crate for `libra-governor-ledger`.
//!
//! This crate is part of the Libra Governor bootstrap scaffold (HORO-1118).
//! Real domain logic lands in follow-up tickets.

/// Returns the crate name, confirming the crate builds and links.
pub fn placeholder() -> &'static str {
    "ledger"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "ledger");
    }
}
