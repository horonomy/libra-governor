//! Human-readable prose for a [`BucketTier`] (HORO-1132), shared by the
//! `UserPromptSubmit` (`hook.rs`) and `Stop` (`hook_stop.rs`) hook
//! renderers so a preflight and the receipt that later finalizes it
//! describe the estimate's evidentiary basis identically. `n` is the
//! estimate's `sample_count` at the time it was computed.

use libra_governor_domain::BucketTier;

pub(crate) fn describe_bucket_tier(tier: BucketTier, n: usize) -> String {
    match tier {
        BucketTier::ColdStart => "basis: no local history yet".to_string(),
        BucketTier::Global => format!("basis: global history (mixed task classes), n={n}"),
        BucketTier::RepoTopologyModel => {
            format!("basis: this repo + build topology + model, n={n}")
        }
        BucketTier::RepoTopology => format!("basis: this repo + build topology, n={n}"),
        BucketTier::Repo => format!("basis: this repo, n={n}"),
        BucketTier::Topology => format!("basis: build topology (cross-repo), n={n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_has_no_sample_count_in_its_prose() {
        let prose = describe_bucket_tier(BucketTier::ColdStart, 0);
        assert_eq!(prose, "basis: no local history yet");
        assert!(!prose.contains("n="));
    }

    #[test]
    fn every_non_cold_start_tier_reports_its_sample_count() {
        for tier in [
            BucketTier::Global,
            BucketTier::RepoTopologyModel,
            BucketTier::RepoTopology,
            BucketTier::Repo,
            BucketTier::Topology,
        ] {
            let prose = describe_bucket_tier(tier, 7);
            assert!(
                prose.contains("n=7"),
                "{tier:?} prose {prose:?} must report its sample count"
            );
        }
    }
}
