//! Input envelope for `libra-governor quota explain --replay <path>`
//! (HORO-1767). Deliberately strict: `deny_unknown_fields`, a hard file
//! size cap, and every cap/validation the domain layer itself does not
//! already enforce for a bare `Vec<T>` field (see module docs on each
//! check below).

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use time::OffsetDateTime;

use libra_governor_domain::pacing::{
    PacingEvent, PacingPreference, ScenarioError, SimTask, TaskSet, MAX_SCENARIO_EVENTS,
    MAX_SCENARIO_WINDOWS,
};
use libra_governor_domain::{
    CompletionContract, Policy, PolicyValidationError, ProviderSnapshot, QuotaWindow,
};

pub const SCHEMA_VERSION: &str = "quota-explain-input-v1";

/// Hard cap on the input file's byte size. Enforced by reading at most
/// `MAX_INPUT_BYTES + 1` bytes (`File::take`), never by trusting
/// `fs::metadata().len()` — a special file (pipe, device) reports size 0
/// regardless of how much is actually readable from it.
pub const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema_version: String,
    policy: Policy,
    preference: PacingPreference,
    #[serde(default)]
    contract: Option<CompletionContract>,
    tasks: Vec<SimTask>,
    #[serde(default)]
    windows: Vec<QuotaWindow>,
    #[serde(default)]
    snapshots: Vec<ProviderSnapshot>,
    events: Vec<PacingEvent>,
}

/// A fully validated `quota explain` input scenario — never constructed
/// except via [`load`], so every field has already passed the same
/// validation a real caller elsewhere in this crate would be required to
/// pass (see each check in [`load`]'s own docs).
#[derive(Debug)]
pub struct Input {
    pub policy: Policy,
    pub preference: PacingPreference,
    pub contract: Option<CompletionContract>,
    pub tasks: TaskSet,
    pub windows: Vec<QuotaWindow>,
    pub snapshots: Vec<ProviderSnapshot>,
    pub events: Vec<PacingEvent>,
}

#[derive(Debug)]
pub enum InputError {
    Io(String),
    TooLarge { bytes: u64, max: u64 },
    Json(String),
    UnsupportedSchemaVersion(String),
    NoEvents,
    TooManyEvents { found: usize, max: usize },
    TooManyWindows { found: usize, max: usize },
    DuplicateWindowId(String),
    Policy(PolicyValidationError),
    Scenario(ScenarioError),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::Io(e) => write!(f, "could not read input file: {e}"),
            InputError::TooLarge { bytes, max } => {
                write!(
                    f,
                    "input file is at least {bytes} bytes, exceeding the {max}-byte cap"
                )
            }
            InputError::Json(e) => write!(f, "could not parse input JSON: {e}"),
            InputError::UnsupportedSchemaVersion(v) => write!(
                f,
                "unsupported schema_version {v:?} (expected {SCHEMA_VERSION:?})"
            ),
            InputError::NoEvents => write!(f, "input has no events — nothing to simulate"),
            InputError::TooManyEvents { found, max } => {
                write!(f, "input has {found} events, exceeding the cap of {max}")
            }
            InputError::TooManyWindows { found, max } => {
                write!(f, "input has {found} windows, exceeding the cap of {max}")
            }
            InputError::DuplicateWindowId(id) => write!(f, "duplicate window id {id}"),
            InputError::Policy(e) => write!(f, "invalid policy: {e}"),
            InputError::Scenario(e) => write!(f, "invalid task scenario: {e}"),
        }
    }
}

/// Reads, parses, and fully validates a `quota explain --replay` input
/// file. Never opens the daemon's ledger or state directory — the
/// replay's only input is this one file plus `--as-of` (see
/// `crates/cli/src/quota_cmd/mod.rs`'s own module docs and its
/// `no_daemon_or_state_access` test).
pub fn load(path: &Path) -> Result<Input, InputError> {
    let mut file = File::open(path).map_err(|e| InputError::Io(e.to_string()))?;
    let mut limited = (&mut file).take(MAX_INPUT_BYTES + 1);
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .map_err(|e| InputError::Io(e.to_string()))?;
    if buf.len() as u64 > MAX_INPUT_BYTES {
        return Err(InputError::TooLarge {
            bytes: buf.len() as u64,
            max: MAX_INPUT_BYTES,
        });
    }

    let wire: Wire = serde_json::from_slice(&buf).map_err(|e| InputError::Json(e.to_string()))?;
    if wire.schema_version != SCHEMA_VERSION {
        return Err(InputError::UnsupportedSchemaVersion(wire.schema_version));
    }

    if wire.events.is_empty() {
        return Err(InputError::NoEvents);
    }
    if wire.events.len() > MAX_SCENARIO_EVENTS {
        return Err(InputError::TooManyEvents {
            found: wire.events.len(),
            max: MAX_SCENARIO_EVENTS,
        });
    }
    if wire.windows.len() > MAX_SCENARIO_WINDOWS {
        return Err(InputError::TooManyWindows {
            found: wire.windows.len(),
            max: MAX_SCENARIO_WINDOWS,
        });
    }

    let mut seen = HashSet::new();
    for window in &wire.windows {
        if !seen.insert(window.id()) {
            return Err(InputError::DuplicateWindowId(window.id().0.to_string()));
        }
    }

    // `events` is non-empty (checked above), so `first_event_at` always
    // has a value to validate the policy's `time.deadline` against —
    // never the wall clock, so a replay is fully deterministic.
    let first_event_at: OffsetDateTime = wire
        .events
        .iter()
        .map(PacingEvent::at)
        .min()
        .expect("checked non-empty above");

    // `Policy` derives a plain `Deserialize` (unlike `QuotaWindow`, which
    // validates via `#[serde(try_from = ...)]`) — a raw parse can produce
    // a structurally well-typed but semantically invalid policy (e.g. a
    // target exceeding its own hard ceiling). Re-run the same validation
    // `Policy::validated_at` always requires, rather than trusting the
    // wire bytes.
    let policy = Policy::validated_at(
        wire.policy.name,
        wire.policy.resource,
        wire.policy.time,
        wire.policy.quality_floor,
        wire.policy.min_confidence,
        wire.policy.autonomy,
        first_event_at,
    )
    .map_err(InputError::Policy)?;

    // Never deserialize `TaskSet` directly (it has no public
    // `Deserialize` of its own) — rebuilding via `validated` is what
    // actually runs the duplicate-id/cycle checks.
    let tasks = TaskSet::validated(wire.tasks).map_err(InputError::Scenario)?;

    Ok(Input {
        policy,
        preference: wire.preference,
        contract: wire.contract,
        tasks,
        windows: wire.windows,
        snapshots: wire.snapshots,
        events: wire.events,
    })
}
