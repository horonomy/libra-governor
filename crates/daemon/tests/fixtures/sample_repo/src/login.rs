// Fixture file for crates/daemon/tests/preflight_integration.rs.
// Exists only so bounded reconnaissance has a real "login"-named path to
// match against a prompt like "fix the login bug".

pub fn login(_username: &str, _password: &str) -> bool {
    false
}
