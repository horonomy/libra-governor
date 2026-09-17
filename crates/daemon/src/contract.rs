//! Deterministic Completion Contract drafting from a bounded
//! reconnaissance result.
//!
//! Intentionally simple heuristics, not NLP: a baseline criterion plus at
//! most one criterion each for "tests were detected" and "an affected
//! area was matched." The draft is shown to the user, who can correct it
//! — correction UX is out of scope for this ticket (HORO-1125); only
//! producing the draft is in scope.

use libra_governor_domain::{CompletionContract, CompletionCriterion};

use crate::recon::ReconOutput;

/// Drafts the criteria for a *new* contract revision from a
/// reconnaissance result. Callers decide whether this becomes revision 1
/// ([`CompletionContract::first`]) or the next revision of an existing
/// contract ([`CompletionContract::next_revision`]) — this function only
/// produces the criteria list.
pub fn draft_criteria(recon: &ReconOutput) -> Vec<CompletionCriterion> {
    let mut criteria = vec![CompletionCriterion::required(
        "Matches the user's stated request",
    )];

    if !recon.detected_test_commands.is_empty() {
        criteria.push(CompletionCriterion::required(format!(
            "Relevant tests pass ({})",
            recon.detected_test_commands.join(", ")
        )));
    }

    if !recon.likely_affected_paths.is_empty() {
        criteria.push(CompletionCriterion::optional(format!(
            "Changes are scoped to the likely affected area(s): {}",
            recon.likely_affected_paths.join(", ")
        )));
    }

    criteria
}

/// Drafts the next contract revision, given the prior revision (if any)
/// and a reconnaissance result. Revision 1 when `previous` is `None`.
pub fn draft_contract(
    previous: Option<&CompletionContract>,
    recon: &ReconOutput,
) -> CompletionContract {
    let criteria = draft_criteria(recon);
    match previous {
        Some(previous) => previous.next_revision(criteria),
        None => CompletionContract::first(criteria),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_protocol::Confidence;
    use std::time::Duration;

    fn recon(test_commands: Vec<String>, affected_paths: Vec<String>) -> ReconOutput {
        ReconOutput {
            files_scanned: 3,
            dirs_scanned: 1,
            likely_affected_paths: affected_paths,
            detected_test_commands: test_commands,
            truncated: false,
            confidence: Confidence::Medium,
            reason: None,
            elapsed: Duration::from_millis(1),
        }
    }

    #[test]
    fn draft_always_includes_baseline_criterion() {
        let criteria = draft_criteria(&recon(vec![], vec![]));
        assert_eq!(criteria.len(), 1);
        assert!(criteria[0].required);
        assert_eq!(criteria[0].description, "Matches the user's stated request");
    }

    #[test]
    fn draft_adds_required_test_criterion_when_tests_detected() {
        let criteria = draft_criteria(&recon(vec!["cargo test".to_string()], vec![]));
        assert_eq!(criteria.len(), 2);
        assert!(criteria[1].required);
        assert!(criteria[1].description.contains("cargo test"));
    }

    #[test]
    fn draft_adds_optional_scope_criterion_when_paths_matched() {
        let criteria = draft_criteria(&recon(vec![], vec!["src/login.rs".to_string()]));
        assert_eq!(criteria.len(), 2);
        assert!(!criteria[1].required);
        assert!(criteria[1].description.contains("src/login.rs"));
    }

    #[test]
    fn draft_contract_starts_at_revision_one_with_no_prior() {
        let contract = draft_contract(None, &recon(vec![], vec![]));
        assert_eq!(contract.revision, 1);
    }

    #[test]
    fn draft_contract_increments_revision_from_prior() {
        let v1 = draft_contract(None, &recon(vec![], vec![]));
        let v2 = draft_contract(Some(&v1), &recon(vec!["cargo test".to_string()], vec![]));
        assert_eq!(v2.revision, 2);
    }
}
