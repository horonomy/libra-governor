//! HORO-2281 (C1): the provider-namespaced shared-budget lane key.
//! `budget_lane` is pure; these tests need no daemon and no ledger.

use libra_governor_daemon::shared_budget::{budget_lane, PROVIDER_LANE_PREFIX};
use libra_governor_protocol::SharedBudgetRefusal;

#[test]
fn a_claude_lane_keeps_the_raw_session_id_byte_for_byte() {
    assert_eq!(budget_lane("sess-1", None).unwrap(), "sess-1");
    assert_eq!(
        budget_lane("sess-1", Some("claude-code")).unwrap(),
        "sess-1"
    );
}
