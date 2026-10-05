//! Passive adapter metadata and explicit local-code trust, separate from host admission.

pub mod catalog;
pub mod contract;
pub mod identity;
mod parents;
pub mod state;

/// Whether registry/trust authority changed. Failed mutation preparation may
/// retain owned directories or the permanent reservation without changing authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryEffect {
    NoChange,
    AppliedVerified,
    EffectUnconfirmed,
}

/// Fixed local failures never retain adapter payloads, paths or parser diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryFailure {
    pub stage: &'static str,
    pub reason: &'static str,
    pub effect: RegistryEffect,
}

impl RegistryFailure {
    pub fn new(stage: &'static str, reason: &'static str) -> Self {
        Self {
            stage,
            reason,
            effect: RegistryEffect::NoChange,
        }
    }

    pub fn unconfirmed(stage: &'static str, reason: &'static str) -> Self {
        Self {
            stage,
            reason,
            effect: RegistryEffect::EffectUnconfirmed,
        }
    }
}

impl std::fmt::Display for RegistryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host adapter {}: {}", self.stage, self.reason)
    }
}

impl std::error::Error for RegistryFailure {}
