//! [`ExecutionIdentity`] — Libra's Rust implementation of the shared,
//! vendor-neutral execution identity envelope (HORO-1597/1598/1599).
//!
//! Normative contract:
//! <https://github.com/horonomy/.github/blob/main/governance/product/execution-identity-contract.md>,
//! reference implementation (Python, the source of truth for exact
//! semantics):
//! <https://github.com/horonomy/.github/blob/main/scripts/execution_identity.py>.
//! This module mirrors that reference implementation's validation,
//! correlation, cache-key, and redaction behavior exactly, in Rust idiom —
//! it does not restate the contract's prose, only its property names.
//!
//! # Not [`crate::TaskIdentity`]
//!
//! [`crate::TaskId`] is Libra's own durable economic-unit anchor — one task
//! may span multiple provider sessions and multiple replans (see
//! `docs/adr/0002-task-not-session-as-economic-unit.md`). An
//! [`ExecutionIdentity`] describes one provider's session/agent/turn at one
//! observed instant. The two are deliberately separate types: a caller may
//! associate the [`ExecutionIdentity`] that first observed a task with that
//! task's [`crate::TaskId`], but [`crate::TaskIdentity`] itself is never
//! widened to carry envelope fields.
//!
//! # Opacity
//!
//! Every provider-native field (`provider_session_id`, `agent_id`,
//! `parent_agent_id`, `turn_id`, `tool_instance_id`) is stored and compared
//! byte-for-byte, verbatim. Nothing in this module parses, reformats, or
//! derives one of these values from another.

use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// Traceability tag this envelope's shape carries, mirroring
/// [`crate::CAPABILITY_SCHEMA_VERSION`]/[`crate::POLICY_SCHEMA_VERSION`]'s
/// convention. Matches the contract's `envelope_version: 1` — not a Rust
/// crate version.
pub const EXECUTION_IDENTITY_ENVELOPE_VERSION: i64 = 1;

fn is_supported_envelope_version(version: i64) -> bool {
    version == EXECUTION_IDENTITY_ENVELOPE_VERSION
}

/// A short, deterministic, non-reversible token for display/logs. Free
/// function (HORO-1672) so other redaction call sites — e.g. a custody
/// tree's `natural_key_display` — can redact a raw value without needing
/// an [`ExecutionIdentity`] instance. `field_name` is included in the
/// digest input so two different dimensions holding the same raw string
/// never redact identically.
pub fn redacted_display_id(field_name: &str, raw_value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(field_name.as_bytes());
    hasher.update(b":");
    hasher.update(raw_value.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{field_name}:{hex}")
}

/// Whether `value` is syntactically a legal `tool_provider` string:
/// `^[a-z][a-z0-9_-]{0,31}$`. Deliberately permissive about *which*
/// provider (open-ended — a new provider is a new string, never a version
/// bump) and strict about *shape*, since this string reaches cache keys and
/// logs.
pub fn is_valid_tool_provider(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    if value.len() > 32 {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The smallest truthful dimension that owns one datum.
///
/// Distinct from (and not interchangeable with) the shared statusline
/// provider contract's own `scope` field — see the execution identity
/// contract's "Relationship to the statusline contract's scope". This enum
/// has no "unknown-and-therefore-not-serializable" gap: [`Scope::Unknown`]
/// is a legal, permanent member, because pre-attribution legacy data is a
/// real, permanent case here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Host,
    Session,
    Agent,
    TurnTask,
    ProjectWorktree,
    Unknown,
}

/// Whether an agent's parent relationship is proven, absent, or unknown.
///
/// Not a nullable `parent_agent_id`: `None` would be ambiguous between
/// "this agent is the root" and "the provider doesn't tell us". Collapsing
/// those is the exact heuristic-joining failure this contract exists to
/// prevent — see the contract's "Agent lineage" section and HORO-1597's
/// `agent_id known, parent_agent_id unavailable` example.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageStatus {
    /// The provider has confirmed this agent has no parent.
    Root,
    /// The provider has confirmed a specific parent, carried in
    /// `parent_agent_id`.
    Child,
    /// The provider exposes `agent_id` but does not expose lineage at all.
    /// The correct default when lineage support is simply absent — never
    /// [`LineageStatus::Root`].
    Unknown,
}

/// Why constructing or parsing an [`ExecutionIdentity`] failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecutionIdentityError {
    #[error("unsupported envelope_version: {0}")]
    UnsupportedEnvelopeVersion(i64),
    #[error("observed_at must be UTC, got offset {0}")]
    ObservedAtNotUtc(time::UtcOffset),
    #[error("host_id is required and must be non-empty")]
    EmptyHostId,
    #[error("invalid tool_provider: {0:?}")]
    InvalidToolProvider(String),
    #[error("lineage_status=Child requires a non-empty parent_agent_id")]
    ChildLineageMissingParent,
    #[error("parent_agent_id must be absent unless lineage_status=Child")]
    ParentAgentIdWithoutChildLineage,
}

/// Raised by [`ExecutionIdentity::cache_key`] when the envelope lacks the
/// identity a scope's cache key requires. Never caught to silently fall
/// back to a broader scope's key — that fallback (a `latest:<host_id>`
/// lookup standing in for session-local state) is the exact anti-pattern
/// HORO-1597 names explicitly.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ScopeIdentityMissing(String);

/// One instant's execution identity envelope.
///
/// Every optional field's *absence* means "not known for this record",
/// never "does not apply" and never a zero/empty-string stand-in. Fields
/// are private; construct via [`ExecutionIdentity::validated`] so the
/// lineage tri-state/parent coupling and the UTC/shape checks below can
/// never be bypassed by a struct literal — including via deserialization:
/// `#[serde(try_from = "ExecutionIdentityWire")]` routes every
/// deserialization through [`ExecutionIdentity::validated`], so an
/// unsupported `envelope_version`, a non-UTC `observed_at`, or a
/// `lineage_status: child` with no `parent_agent_id` is refused on parse,
/// not merely on construction through Rust code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ExecutionIdentityWire")]
pub struct ExecutionIdentity {
    envelope_version: i64,
    #[serde(with = "observed_at_wire")]
    observed_at: OffsetDateTime,

    host_id: String,

    tool_provider: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    tool_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    provider_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    turn_id: Option<String>,

    lineage_status: LineageStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    parent_agent_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    session_lineage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    event_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    worktree_id: Option<String>,
}

/// Builds an [`ExecutionIdentity`], field by field, so a capturer with few
/// known dimensions does not need a long positional constructor call. Every
/// field defaults to absent/unknown; [`ExecutionIdentityBuilder::build`]
/// runs the same validation [`ExecutionIdentity::validated`] does.
#[derive(Debug, Clone, Default)]
pub struct ExecutionIdentityBuilder {
    host_id: String,
    tool_provider: String,
    tool_instance_id: Option<String>,
    provider_session_id: Option<String>,
    agent_id: Option<String>,
    turn_id: Option<String>,
    lineage_status: Option<LineageStatus>,
    parent_agent_id: Option<String>,
    session_lineage_id: Option<String>,
    event_id: Option<String>,
    repo_id: Option<String>,
    worktree_id: Option<String>,
}

impl ExecutionIdentityBuilder {
    pub fn new(host_id: impl Into<String>, tool_provider: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            tool_provider: tool_provider.into(),
            ..Default::default()
        }
    }

    pub fn tool_instance_id(mut self, value: impl Into<String>) -> Self {
        self.tool_instance_id = Some(value.into());
        self
    }

    pub fn provider_session_id(mut self, value: impl Into<String>) -> Self {
        self.provider_session_id = Some(value.into());
        self
    }

    pub fn agent_id(mut self, value: impl Into<String>) -> Self {
        self.agent_id = Some(value.into());
        self
    }

    pub fn turn_id(mut self, value: impl Into<String>) -> Self {
        self.turn_id = Some(value.into());
        self
    }

    /// Sets [`LineageStatus::Root`] — the provider has confirmed this
    /// agent has no parent.
    pub fn root_lineage(mut self) -> Self {
        self.lineage_status = Some(LineageStatus::Root);
        self
    }

    /// Sets [`LineageStatus::Child`] with the given provider-native parent
    /// agent id.
    pub fn child_lineage(mut self, parent_agent_id: impl Into<String>) -> Self {
        self.lineage_status = Some(LineageStatus::Child);
        self.parent_agent_id = Some(parent_agent_id.into());
        self
    }

    pub fn session_lineage_id(mut self, value: impl Into<String>) -> Self {
        self.session_lineage_id = Some(value.into());
        self
    }

    pub fn event_id(mut self, value: impl Into<String>) -> Self {
        self.event_id = Some(value.into());
        self
    }

    pub fn repo_id(mut self, value: impl Into<String>) -> Self {
        self.repo_id = Some(value.into());
        self
    }

    pub fn worktree_id(mut self, value: impl Into<String>) -> Self {
        self.worktree_id = Some(value.into());
        self
    }

    /// Validates and builds the envelope at `now`. [`LineageStatus`]
    /// defaults to [`LineageStatus::Unknown`] when never set — the correct
    /// default for "lineage support is simply absent", never
    /// [`LineageStatus::Root`].
    pub fn build_at(
        self,
        now: OffsetDateTime,
    ) -> Result<ExecutionIdentity, ExecutionIdentityError> {
        ExecutionIdentity::validated(
            EXECUTION_IDENTITY_ENVELOPE_VERSION,
            now,
            self.host_id,
            self.tool_provider,
            self.tool_instance_id,
            self.provider_session_id,
            self.agent_id,
            self.turn_id,
            self.lineage_status.unwrap_or(LineageStatus::Unknown),
            self.parent_agent_id,
            self.session_lineage_id,
            self.event_id,
            self.repo_id,
            self.worktree_id,
        )
    }
}

impl ExecutionIdentity {
    /// Constructs and validates an envelope. Mirrors the reference
    /// implementation's `__post_init__` exactly: unsupported version,
    /// non-UTC `observed_at`, empty `host_id`, invalid `tool_provider`, and
    /// the lineage/parent coupling are all checked here, not left to the
    /// caller.
    #[allow(clippy::too_many_arguments)]
    pub fn validated(
        envelope_version: i64,
        observed_at: OffsetDateTime,
        host_id: impl Into<String>,
        tool_provider: impl Into<String>,
        tool_instance_id: Option<String>,
        provider_session_id: Option<String>,
        agent_id: Option<String>,
        turn_id: Option<String>,
        lineage_status: LineageStatus,
        parent_agent_id: Option<String>,
        session_lineage_id: Option<String>,
        event_id: Option<String>,
        repo_id: Option<String>,
        worktree_id: Option<String>,
    ) -> Result<Self, ExecutionIdentityError> {
        if !is_supported_envelope_version(envelope_version) {
            return Err(ExecutionIdentityError::UnsupportedEnvelopeVersion(
                envelope_version,
            ));
        }
        if observed_at.offset() != time::UtcOffset::UTC {
            return Err(ExecutionIdentityError::ObservedAtNotUtc(
                observed_at.offset(),
            ));
        }
        let host_id = host_id.into();
        if host_id.is_empty() {
            return Err(ExecutionIdentityError::EmptyHostId);
        }
        let tool_provider = tool_provider.into();
        if !is_valid_tool_provider(&tool_provider) {
            return Err(ExecutionIdentityError::InvalidToolProvider(tool_provider));
        }
        match lineage_status {
            LineageStatus::Child => {
                if parent_agent_id.as_deref().unwrap_or("").is_empty() {
                    return Err(ExecutionIdentityError::ChildLineageMissingParent);
                }
            }
            LineageStatus::Root | LineageStatus::Unknown => {
                if parent_agent_id.is_some() {
                    return Err(ExecutionIdentityError::ParentAgentIdWithoutChildLineage);
                }
            }
        }

        Ok(Self {
            envelope_version,
            observed_at,
            host_id,
            tool_provider,
            tool_instance_id,
            provider_session_id,
            agent_id,
            turn_id,
            lineage_status,
            parent_agent_id,
            session_lineage_id,
            event_id,
            repo_id,
            worktree_id,
        })
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn tool_provider(&self) -> &str {
        &self.tool_provider
    }

    pub fn provider_session_id(&self) -> Option<&str> {
        self.provider_session_id.as_deref()
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn lineage_status(&self) -> LineageStatus {
        self.lineage_status
    }

    pub fn parent_agent_id(&self) -> Option<&str> {
        self.parent_agent_id.as_deref()
    }

    pub fn repo_id(&self) -> Option<&str> {
        self.repo_id.as_deref()
    }

    pub fn worktree_id(&self) -> Option<&str> {
        self.worktree_id.as_deref()
    }

    pub fn observed_at(&self) -> OffsetDateTime {
        self.observed_at
    }

    /// Whether `self` and `other` describe the same identity position,
    /// ignoring `observed_at`/`event_id`. Deliberately does **not** treat
    /// two absent *optional* fields as matching — "we don't know for either
    /// of them" must never collapse into "therefore they're the same."
    /// `host_id` and `tool_provider` are required and always concretely
    /// populated, so matching on them alone is sufficient; beyond that, any
    /// additional dimension must match concretely on both sides, never on
    /// mutual absence.
    pub fn correlates_with(&self, other: &ExecutionIdentity) -> bool {
        if self.host_id != other.host_id || self.tool_provider != other.tool_provider {
            return false;
        }
        // `host_id` and `tool_provider` are required, non-empty fields, so
        // matching them already satisfies "concretely and identically
        // populated on both sides."
        let mut any_concrete_match = true;

        macro_rules! compare {
            ($field:ident) => {
                match (&self.$field, &other.$field) {
                    (Some(left), Some(right)) => {
                        if left != right {
                            return false;
                        }
                        any_concrete_match = true;
                    }
                    _ => {}
                }
            };
        }

        compare!(tool_instance_id);
        compare!(provider_session_id);
        compare!(agent_id);
        compare!(turn_id);
        compare!(parent_agent_id);
        compare!(session_lineage_id);

        any_concrete_match
    }

    /// A short, deterministic, non-reversible token for display/logs.
    /// Never used for correlation or cache keys — see the contract's
    /// "Redaction" section. `field_name` is included in the digest input so
    /// two different dimensions holding the same raw string never redact
    /// identically.
    pub fn display_id(&self, field_name: &str, raw_value: &str) -> String {
        redacted_display_id(field_name, raw_value)
    }

    fn scope_identity_missing(message: impl Into<String>) -> ScopeIdentityMissing {
        ScopeIdentityMissing(message.into())
    }

    /// The minimal identity tuple that `scope`'s data is keyed on. Raises
    /// [`ScopeIdentityMissing`] rather than falling back to a broader
    /// scope's key — see the contract's "Cache keys never widen on their
    /// own".
    pub fn cache_key(&self, scope: Scope) -> Result<Vec<String>, ScopeIdentityMissing> {
        match scope {
            Scope::Host => Ok(vec![self.host_id.clone()]),
            Scope::Session => {
                let session = self.provider_session_id.as_ref().ok_or_else(|| {
                    Self::scope_identity_missing("SESSION scope requires provider_session_id")
                })?;
                Ok(vec![
                    self.host_id.clone(),
                    self.tool_provider.clone(),
                    session.clone(),
                ])
            }
            Scope::Agent => {
                let session = self.provider_session_id.as_ref().ok_or_else(|| {
                    Self::scope_identity_missing(
                        "AGENT scope requires provider_session_id and agent_id",
                    )
                })?;
                let agent = self.agent_id.as_ref().ok_or_else(|| {
                    Self::scope_identity_missing(
                        "AGENT scope requires provider_session_id and agent_id",
                    )
                })?;
                Ok(vec![
                    self.host_id.clone(),
                    self.tool_provider.clone(),
                    session.clone(),
                    agent.clone(),
                ])
            }
            Scope::TurnTask => {
                let missing = || {
                    Self::scope_identity_missing(
                        "TURN_TASK scope requires provider_session_id, agent_id, and turn_id",
                    )
                };
                let session = self.provider_session_id.as_ref().ok_or_else(missing)?;
                let agent = self.agent_id.as_ref().ok_or_else(missing)?;
                let turn = self.turn_id.as_ref().ok_or_else(missing)?;
                Ok(vec![
                    self.host_id.clone(),
                    self.tool_provider.clone(),
                    session.clone(),
                    agent.clone(),
                    turn.clone(),
                ])
            }
            Scope::ProjectWorktree => {
                if let Some(worktree) = &self.worktree_id {
                    return Ok(vec![worktree.clone()]);
                }
                if let Some(repo) = &self.repo_id {
                    return Ok(vec![repo.clone()]);
                }
                Err(Self::scope_identity_missing(
                    "PROJECT_WORKTREE scope requires worktree_id or repo_id",
                ))
            }
            Scope::Unknown => Err(Self::scope_identity_missing(
                "UNKNOWN scope has no stable identity-keyed cache key by definition",
            )),
        }
    }
}

/// The raw deserialization shape for [`ExecutionIdentity`], mirroring its
/// field names and wire conventions exactly. Exists only so
/// `#[serde(try_from = "ExecutionIdentityWire")]` can route every
/// deserialization through [`ExecutionIdentity::validated`] — this type is
/// never constructed directly by callers.
#[derive(serde::Deserialize)]
struct ExecutionIdentityWire {
    envelope_version: i64,
    #[serde(with = "observed_at_wire")]
    observed_at: OffsetDateTime,
    host_id: String,
    tool_provider: String,
    #[serde(default)]
    tool_instance_id: Option<String>,
    #[serde(default)]
    provider_session_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    lineage_status: LineageStatus,
    #[serde(default)]
    parent_agent_id: Option<String>,
    #[serde(default)]
    session_lineage_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    repo_id: Option<String>,
    #[serde(default)]
    worktree_id: Option<String>,
}

impl TryFrom<ExecutionIdentityWire> for ExecutionIdentity {
    type Error = ExecutionIdentityError;

    fn try_from(wire: ExecutionIdentityWire) -> Result<Self, Self::Error> {
        Self::validated(
            wire.envelope_version,
            wire.observed_at,
            wire.host_id,
            wire.tool_provider,
            wire.tool_instance_id,
            wire.provider_session_id,
            wire.agent_id,
            wire.turn_id,
            wire.lineage_status,
            wire.parent_agent_id,
            wire.session_lineage_id,
            wire.event_id,
            wire.repo_id,
            wire.worktree_id,
        )
    }
}

/// `observed_at`'s wire format: `YYYY-MM-DDTHH:MM:SS.fffZ`, matching the
/// reference implementation's `to_wire`/`from_wire` exactly so a Libra
/// envelope round-trips through the same JSON shape Fornax/Circinus parse.
mod observed_at_wire {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    pub fn serialize<S: Serializer>(
        value: &OffsetDateTime,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let millis = value.millisecond();
        let truncated = value
            .replace_millisecond(millis)
            .map_err(serde::ser::Error::custom)?;
        // `Rfc3339` renders fractional seconds only when non-zero and may
        // render more than milliseconds; the contract wants exactly
        // `.fffZ`. Reformat explicitly rather than relying on `Rfc3339`'s
        // own fractional-second rendering.
        let formatted = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            truncated.year(),
            u8::from(truncated.month()),
            truncated.day(),
            truncated.hour(),
            truncated.minute(),
            truncated.second(),
            millis,
        );
        serializer.serialize_str(&formatted)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OffsetDateTime, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let rfc3339 = raw
            .strip_suffix('Z')
            .map(|s| format!("{s}+00:00"))
            .unwrap_or(raw);
        OffsetDateTime::parse(&rfc3339, &Rfc3339).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        time::macros::datetime!(2026-10-02 12:00:00 UTC)
    }

    fn make() -> ExecutionIdentity {
        ExecutionIdentityBuilder::new("host-abc123", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .root_lineage()
            .build_at(now())
            .unwrap()
    }

    #[test]
    fn declared_version_is_supported() {
        assert!(is_supported_envelope_version(
            EXECUTION_IDENTITY_ENVELOPE_VERSION
        ));
    }

    #[test]
    fn future_version_is_rejected_not_guessed() {
        let result = ExecutionIdentity::validated(
            999,
            now(),
            "host-1",
            "claude_code",
            None,
            None,
            None,
            None,
            LineageStatus::Unknown,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            result.unwrap_err(),
            ExecutionIdentityError::UnsupportedEnvelopeVersion(999)
        );
    }

    #[test]
    fn known_providers_are_valid() {
        assert!(is_valid_tool_provider("claude_code"));
        assert!(is_valid_tool_provider("codex"));
    }

    #[test]
    fn a_new_unknown_provider_is_still_syntactically_valid() {
        assert!(is_valid_tool_provider("future_agent_tool"));
    }

    #[test]
    fn uppercase_provider_is_rejected() {
        assert!(!is_valid_tool_provider("ClaudeCode"));
    }

    #[test]
    fn path_like_provider_is_rejected() {
        assert!(!is_valid_tool_provider("../etc/passwd"));
    }

    #[test]
    fn empty_host_id_is_rejected() {
        let result = ExecutionIdentityBuilder::new("", "claude_code").build_at(now());
        assert_eq!(result.unwrap_err(), ExecutionIdentityError::EmptyHostId);
    }

    #[test]
    fn non_utc_offset_is_rejected() {
        let non_utc = now().to_offset(time::macros::offset!(+8));
        let result = ExecutionIdentity::validated(
            EXECUTION_IDENTITY_ENVELOPE_VERSION,
            non_utc,
            "host-1",
            "claude_code",
            None,
            None,
            None,
            None,
            LineageStatus::Unknown,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(matches!(
            result,
            Err(ExecutionIdentityError::ObservedAtNotUtc(_))
        ));
    }

    #[test]
    fn child_lineage_requires_parent_id() {
        let result = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .agent_id("agent-child")
            .build_at(now());
        // No lineage set -> defaults to Unknown, which is valid; explicitly
        // force Child without a parent via the raw constructor instead.
        assert!(result.is_ok());

        let raw = ExecutionIdentity::validated(
            EXECUTION_IDENTITY_ENVELOPE_VERSION,
            now(),
            "host-1",
            "claude_code",
            None,
            None,
            Some("agent-child".to_string()),
            None,
            LineageStatus::Child,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            raw.unwrap_err(),
            ExecutionIdentityError::ChildLineageMissingParent
        );
    }

    #[test]
    fn root_with_parent_id_set_is_rejected() {
        let result = ExecutionIdentity::validated(
            EXECUTION_IDENTITY_ENVELOPE_VERSION,
            now(),
            "host-1",
            "claude_code",
            None,
            None,
            Some("agent-1".to_string()),
            None,
            LineageStatus::Root,
            Some("agent-root".to_string()),
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            result.unwrap_err(),
            ExecutionIdentityError::ParentAgentIdWithoutChildLineage
        );
    }

    #[test]
    fn child_with_parent_id_is_valid() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .agent_id("agent-child")
            .child_lineage("agent-root")
            .build_at(now())
            .unwrap();
        assert_eq!(identity.lineage_status(), LineageStatus::Child);
        assert_eq!(identity.parent_agent_id(), Some("agent-root"));
    }

    #[test]
    fn root_and_unknown_are_distinct_states() {
        let root = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .root_lineage()
            .build_at(now())
            .unwrap();
        let unknown = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        assert_ne!(root.lineage_status(), unknown.lineage_status());
        assert_ne!(root, unknown);
    }

    #[test]
    fn identical_envelopes_are_equal() {
        assert_eq!(make(), make());
    }

    #[test]
    fn different_observed_at_makes_envelopes_unequal() {
        let a = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        let b = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now() + time::Duration::seconds(1))
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn same_session_and_agent_correlates() {
        let a = make();
        let b = ExecutionIdentityBuilder::new("host-abc123", "claude_code")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .root_lineage()
            .event_id("evt-2")
            .build_at(now() + time::Duration::minutes(5))
            .unwrap();
        assert!(a.correlates_with(&b));
    }

    #[test]
    fn different_provider_session_id_does_not_correlate() {
        let a = make();
        let b = ExecutionIdentityBuilder::new("host-abc123", "claude_code")
            .provider_session_id("sess-2")
            .build_at(now())
            .unwrap();
        assert!(!a.correlates_with(&b));
    }

    #[test]
    fn different_host_does_not_correlate() {
        let a = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        let b = ExecutionIdentityBuilder::new("host-2", "claude_code")
            .build_at(now())
            .unwrap();
        assert!(!a.correlates_with(&b));
    }

    #[test]
    fn same_host_and_provider_alone_correlates() {
        let a = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        let b = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        // Same host_id and tool_provider ARE concretely populated and equal
        // on both sides, so this one correlates by host+provider alone —
        // distinguishing it from the cross-host case above, which must not.
        assert!(a.correlates_with(&b));
    }

    #[test]
    fn display_id_is_deterministic() {
        let identity = make();
        let first = identity.display_id("provider_session_id", "sess-1");
        let second = identity.display_id("provider_session_id", "sess-1");
        assert_eq!(first, second);
    }

    #[test]
    fn display_id_does_not_contain_the_raw_value() {
        let identity = make();
        let redacted = identity.display_id("provider_session_id", "super-secret-session-token");
        assert!(!redacted.contains("super-secret-session-token"));
    }

    #[test]
    fn different_dimensions_with_the_same_raw_value_redact_differently() {
        let identity = make();
        let a = identity.display_id("provider_session_id", "same-value");
        let b = identity.display_id("agent_id", "same-value");
        assert_ne!(a, b);
    }

    #[test]
    fn host_scope_key() {
        let identity = make();
        assert_eq!(
            identity.cache_key(Scope::Host).unwrap(),
            vec!["host-abc123".to_string()]
        );
    }

    #[test]
    fn session_scope_requires_provider_session_id() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        assert!(identity.cache_key(Scope::Session).is_err());
    }

    #[test]
    fn session_scope_key_includes_session_identity() {
        let identity = make();
        let key = identity.cache_key(Scope::Session).unwrap();
        assert!(key.contains(&"sess-1".to_string()));
        assert!(key.len() > 1);
    }

    #[test]
    fn agent_scope_requires_agent_id() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .provider_session_id("sess-1")
            .build_at(now())
            .unwrap();
        assert!(identity.cache_key(Scope::Agent).is_err());
    }

    #[test]
    fn turn_task_scope_requires_turn_id() {
        let identity = make();
        assert!(identity.cache_key(Scope::TurnTask).is_err());
    }

    #[test]
    fn project_worktree_prefers_worktree_over_repo() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .worktree_id("wt-1")
            .repo_id("repo-1")
            .build_at(now())
            .unwrap();
        assert_eq!(
            identity.cache_key(Scope::ProjectWorktree).unwrap(),
            vec!["wt-1".to_string()]
        );
    }

    #[test]
    fn project_worktree_without_either_id_raises() {
        let identity = make();
        assert!(identity.cache_key(Scope::ProjectWorktree).is_err());
    }

    #[test]
    fn unknown_scope_has_no_cache_key() {
        let identity = make();
        assert!(identity.cache_key(Scope::Unknown).is_err());
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let original = ExecutionIdentityBuilder::new("host-1", "codex")
            .tool_instance_id("inst-1")
            .provider_session_id("sess-1")
            .agent_id("agent-1")
            .turn_id("turn-1")
            .child_lineage("agent-root")
            .session_lineage_id("lineage-1")
            .event_id("evt-1")
            .repo_id("repo-1")
            .worktree_id("wt-1")
            .build_at(now())
            .unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let restored: ExecutionIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn absent_optional_fields_are_not_emitted_on_the_wire() {
        let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
            .build_at(now())
            .unwrap();
        let json = serde_json::to_value(&identity).unwrap();
        assert!(json.get("turn_id").is_none());
        assert!(json.get("event_id").is_none());
    }

    #[test]
    fn unknown_top_level_field_is_ignored() {
        let mut json = serde_json::to_value(make()).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("a_future_field".to_string(), serde_json::json!("surprise"));
        let restored: ExecutionIdentity = serde_json::from_value(json).unwrap();
        assert_eq!(restored, make());
    }

    #[test]
    fn unrecognised_lineage_status_is_refused_not_guessed() {
        let mut json = serde_json::to_value(make()).unwrap();
        json["lineage_status"] = serde_json::json!("grandparent");
        let result: Result<ExecutionIdentity, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn unrecognised_scope_is_refused_not_guessed() {
        let result: Result<Scope, _> = serde_json::from_value(serde_json::json!("galaxy"));
        assert!(result.is_err());
    }

    #[test]
    fn deserialization_refuses_an_unsupported_envelope_version() {
        let mut json = serde_json::to_value(make()).unwrap();
        json["envelope_version"] = serde_json::json!(999);
        let result: Result<ExecutionIdentity, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "deserialization must route through validated() and refuse an unsupported version, \
             not merely construction through Rust code"
        );
    }

    #[test]
    fn deserialization_refuses_child_lineage_missing_parent_agent_id() {
        let mut json = serde_json::to_value(make()).unwrap();
        json["lineage_status"] = serde_json::json!("child");
        // `make()` has no parent_agent_id set and root_lineage() emits none.
        assert!(json.get("parent_agent_id").is_none());
        let result: Result<ExecutionIdentity, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "deserialization must refuse lineage_status=child with no parent_agent_id, \
             exactly as validated() does for direct construction"
        );
    }

    #[test]
    fn claude_code_doc_example_parses() {
        let json = serde_json::json!({
            "envelope_version": 1,
            "observed_at": "2026-10-02T12:00:00.000Z",
            "host_id": "host-9f2a1b",
            "tool_provider": "claude_code",
            "provider_session_id": "claude-sess-7e21",
            "agent_id": "claude-agent-task-3",
            "lineage_status": "child",
            "parent_agent_id": "claude-agent-root",
            "turn_id": "turn-14",
            "repo_id": "repo-libra-governor",
            "worktree_id": "wt-horo-1598"
        });
        let identity: ExecutionIdentity = serde_json::from_value(json).unwrap();
        assert_eq!(identity.lineage_status(), LineageStatus::Child);
        assert_eq!(identity.parent_agent_id(), Some("claude-agent-root"));
    }

    #[test]
    fn codex_doc_example_parses() {
        let json = serde_json::json!({
            "envelope_version": 1,
            "observed_at": "2026-10-02T12:05:00.000Z",
            "host_id": "host-9f2a1b",
            "tool_provider": "codex",
            "provider_session_id": "codex-sess-ab19",
            "lineage_status": "unknown"
        });
        let identity: ExecutionIdentity = serde_json::from_value(json).unwrap();
        assert_eq!(identity.lineage_status(), LineageStatus::Unknown);
        assert_eq!(identity.agent_id(), None);
        assert_eq!(identity.turn_id(), None);
    }
}
