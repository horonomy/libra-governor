//! Gateway request provenance: `gateway_requests` (HORO-1144).
//!
//! # Open then close, not one write at the end
//!
//! A metered request is recorded twice: [`LedgerStore::open_gateway_request`]
//! when the decision is made, and [`LedgerStore::close_gateway_request`]
//! when the response finishes. The alternative — one row written at the
//! end — loses exactly the case that matters most: a request that was
//! admitted, forwarded, and then never finished because the daemon died
//! mid-stream would leave no trace at all, and the reservation the TTL
//! later reclaims would have nothing explaining what it was for.
//!
//! Closing is an UPDATE of an existing row, keyed on the gateway's own
//! request id, and it is idempotent: a second close overwrites the same
//! row rather than appending a second one.
//!
//! # Privacy
//!
//! Nothing here accepts a body, a header, or prompt text — there is no
//! parameter for one. See `migrations/0007_gateway_requests.sql`.

use libra_governor_domain::{ReservationId, ResourceAmount, ResourceKind, TaskId};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{store::LedgerStore, LedgerError};

fn rfc3339(t: OffsetDateTime) -> Result<String, LedgerError> {
    t.format(&Rfc3339)
        .map_err(|e| LedgerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))
}

fn kind_to_str(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Usd => "usd",
        ResourceKind::Tokens => "tokens",
        ResourceKind::QuotaPercent => "quota_percent",
    }
}

/// Everything known about a gateway request at the moment its decision
/// was made.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayRequestOpen<'a> {
    pub id: &'a str,
    /// `None` for a request refused before it could be attributed to a
    /// task — see the migration's note on why this column is nullable.
    pub task_id: Option<TaskId>,
    pub session_id: Option<&'a str>,
    pub route: Option<&'a str>,
    pub model: Option<&'a str>,
    pub tier: &'a str,
    pub decision: &'a str,
    pub decision_detail: Option<&'a str>,
    pub reservation_id: Option<ReservationId>,
    pub reserved_amount: Option<ResourceAmount>,
    pub resource_kind: Option<ResourceKind>,
    pub max_tokens: Option<u64>,
    pub pricing_version: &'a str,
    pub terminal_state: &'a str,
    pub upstream_status: Option<u16>,
    pub now: OffsetDateTime,
}

/// Everything learned once the response finished, however it finished.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GatewayRequestClose {
    pub settled_amount: Option<ResourceAmount>,
    /// `true` when `settled_amount` came from the provider's own reported
    /// usage rather than the conservative reserved-amount fallback.
    pub usage_known: bool,
    pub input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub bound_violated: bool,
    pub upstream_status: Option<u16>,
    pub terminal_state_is_clean: bool,
    pub now: OffsetDateTime,
}

impl LedgerStore {
    /// Records a gateway request's decision. Idempotent on `id`: a replay
    /// replaces the row rather than adding a second one.
    pub fn open_gateway_request(
        &mut self,
        open: GatewayRequestOpen<'_>,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO gateway_requests (
                id, task_id, session_id, route, model, tier, decision,
                decision_detail_json, reservation_id, reserved_amount, settled_amount,
                resource_kind, usage_known, input_tokens, cache_creation_input_tokens,
                cache_read_input_tokens, output_tokens, max_tokens, bound_violated,
                pricing_version, upstream_status, terminal_state, created_at, closed_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL,
                ?11, 0, NULL, NULL, NULL, NULL, ?12, 0, ?13, ?14, ?15, ?16, NULL
             )
             ON CONFLICT(id) DO UPDATE SET
                task_id = excluded.task_id,
                session_id = excluded.session_id,
                route = excluded.route,
                model = excluded.model,
                decision = excluded.decision,
                decision_detail_json = excluded.decision_detail_json,
                reservation_id = excluded.reservation_id,
                reserved_amount = excluded.reserved_amount,
                resource_kind = excluded.resource_kind,
                max_tokens = excluded.max_tokens,
                upstream_status = excluded.upstream_status,
                terminal_state = excluded.terminal_state",
            rusqlite::params![
                open.id,
                open.task_id.map(|t| t.to_string()),
                open.session_id,
                open.route,
                open.model,
                open.tier,
                open.decision,
                open.decision_detail,
                open.reservation_id.map(|r| r.0.to_string()),
                open.reserved_amount.map(|a| a.as_f64()),
                open.resource_kind.map(kind_to_str),
                open.max_tokens.map(|m| m as i64),
                open.pricing_version,
                open.upstream_status.map(|s| s as i64),
                open.terminal_state,
                rfc3339(open.now)?,
            ],
        )?;
        Ok(())
    }

    /// Records how a gateway request finished. A no-op (zero rows
    /// updated) if no matching open row exists — a close without an open
    /// means the open write failed, which is already the more serious
    /// event and is not worth compounding with an error here.
    pub fn close_gateway_request(
        &mut self,
        id: &str,
        close: GatewayRequestClose,
        terminal_state: &str,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE gateway_requests SET
                settled_amount = ?1,
                usage_known = ?2,
                input_tokens = ?3,
                cache_creation_input_tokens = ?4,
                cache_read_input_tokens = ?5,
                output_tokens = ?6,
                bound_violated = ?7,
                upstream_status = COALESCE(?8, upstream_status),
                terminal_state = ?9,
                closed_at = ?10
             WHERE id = ?11",
            rusqlite::params![
                close.settled_amount.map(|a| a.as_f64()),
                close.usage_known,
                close.input_tokens.map(|t| t as i64),
                close.cache_creation_input_tokens.map(|t| t as i64),
                close.cache_read_input_tokens.map(|t| t as i64),
                close.output_tokens.map(|t| t as i64),
                close.bound_violated,
                close.upstream_status.map(|s| s as i64),
                terminal_state,
                rfc3339(close.now)?,
                id,
            ],
        )?;
        Ok(())
    }

    /// How many gateway requests are recorded for `task_id`, and what
    /// they settled to in total. Feeds the `gateway status` command
    /// without exposing per-request rows to a caller that only needs an
    /// aggregate.
    pub fn gateway_spend_for_task(
        &self,
        task_id: TaskId,
    ) -> Result<(u64, Option<ResourceAmount>), LedgerError> {
        let (count, total, kind): (i64, Option<f64>, Option<String>) = self.conn.query_row(
            "SELECT COUNT(*), SUM(settled_amount), MAX(resource_kind)
             FROM gateway_requests WHERE task_id = ?1",
            [task_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let amount = match (total, kind.as_deref()) {
            (Some(total), Some("usd")) => {
                Some(ResourceAmount::from_kind_f64(ResourceKind::Usd, total))
            }
            (Some(total), Some("tokens")) => {
                Some(ResourceAmount::from_kind_f64(ResourceKind::Tokens, total))
            }
            (Some(total), Some("quota_percent")) => Some(ResourceAmount::from_kind_f64(
                ResourceKind::QuotaPercent,
                total,
            )),
            _ => None,
        };
        Ok((count as u64, amount))
    }

    /// The decision tag recorded for `id`, if any. Test- and
    /// diagnostics-facing: it reads one scalar, never a body.
    pub fn gateway_request_decision(&self, id: &str) -> Result<Option<String>, LedgerError> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT decision FROM gateway_requests WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(LedgerError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::TaskIdentity;

    fn store_with_task() -> (LedgerStore, TaskId) {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let identity = TaskIdentity::new(None);
        let task_id = identity.id;
        store
            .insert_task(&identity, OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        (store, task_id)
    }

    fn open_for<'a>(id: &'a str, task_id: Option<TaskId>) -> GatewayRequestOpen<'a> {
        GatewayRequestOpen {
            id,
            task_id,
            session_id: Some("sess-1"),
            route: Some("/v1/messages"),
            model: Some("claude-sonnet-4-5"),
            tier: "gateway_metered",
            decision: "allowed",
            decision_detail: None,
            reservation_id: None,
            reserved_amount: Some(ResourceAmount::Tokens(1_500)),
            resource_kind: Some(ResourceKind::Tokens),
            max_tokens: Some(1_000),
            pricing_version: "pricing-test-v1",
            terminal_state: "completed_cleanly",
            upstream_status: None,
            now: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_rejection_with_no_task_at_all_still_records_a_row() {
        let (mut store, _) = store_with_task();
        let mut open = open_for("req-unbound", None);
        open.session_id = None;
        open.model = None;
        open.reserved_amount = None;
        open.resource_kind = None;
        open.max_tokens = None;
        open.decision = "task_unbound";
        open.terminal_state = "rejected_before_upstream";

        store
            .open_gateway_request(open)
            .expect("a taskless rejection is exactly the row an auditor most wants");
        assert_eq!(
            store.gateway_request_decision("req-unbound").unwrap(),
            Some("task_unbound".to_string())
        );
    }

    #[test]
    fn opening_twice_updates_the_same_row_rather_than_duplicating_it() {
        let (mut store, task_id) = store_with_task();
        store
            .open_gateway_request(open_for("req-1", Some(task_id)))
            .unwrap();
        let mut second = open_for("req-1", Some(task_id));
        second.decision = "budget_exceeded";
        store.open_gateway_request(second).unwrap();

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM gateway_requests", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            store.gateway_request_decision("req-1").unwrap(),
            Some("budget_exceeded".to_string())
        );
    }

    #[test]
    fn closing_records_the_settled_figures_and_the_terminal_state() {
        let (mut store, task_id) = store_with_task();
        store
            .open_gateway_request(open_for("req-2", Some(task_id)))
            .unwrap();
        store
            .close_gateway_request(
                "req-2",
                GatewayRequestClose {
                    settled_amount: Some(ResourceAmount::Tokens(640)),
                    usage_known: true,
                    input_tokens: Some(100),
                    cache_creation_input_tokens: Some(40),
                    cache_read_input_tokens: Some(0),
                    output_tokens: Some(500),
                    bound_violated: false,
                    upstream_status: Some(200),
                    terminal_state_is_clean: true,
                    now: OffsetDateTime::UNIX_EPOCH,
                },
                "completed_cleanly",
            )
            .unwrap();

        let (settled, usage_known, output, status, closed_at): (
            f64,
            bool,
            i64,
            i64,
            Option<String>,
        ) = store
            .conn
            .query_row(
                "SELECT settled_amount, usage_known, output_tokens, upstream_status, closed_at
                 FROM gateway_requests WHERE id = 'req-2'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(settled, 640.0);
        assert!(usage_known);
        assert_eq!(output, 500);
        assert_eq!(status, 200);
        assert!(closed_at.is_some());
    }

    #[test]
    fn closing_an_unknown_id_is_a_silent_no_op() {
        let (mut store, _) = store_with_task();
        store
            .close_gateway_request(
                "never-opened",
                GatewayRequestClose {
                    settled_amount: None,
                    usage_known: false,
                    input_tokens: None,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    output_tokens: None,
                    bound_violated: false,
                    upstream_status: None,
                    terminal_state_is_clean: false,
                    now: OffsetDateTime::UNIX_EPOCH,
                },
                "stream_aborted",
            )
            .expect("a close without an open must not be a second failure");
    }

    #[test]
    fn a_bound_violation_is_persisted_rather_than_absorbed() {
        let (mut store, task_id) = store_with_task();
        store
            .open_gateway_request(open_for("req-3", Some(task_id)))
            .unwrap();
        store
            .close_gateway_request(
                "req-3",
                GatewayRequestClose {
                    settled_amount: Some(ResourceAmount::Tokens(3_000)),
                    usage_known: true,
                    input_tokens: Some(100),
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    output_tokens: Some(2_900),
                    bound_violated: true,
                    upstream_status: Some(200),
                    terminal_state_is_clean: true,
                    now: OffsetDateTime::UNIX_EPOCH,
                },
                "completed_cleanly",
            )
            .unwrap();
        let violated: bool = store
            .conn
            .query_row(
                "SELECT bound_violated FROM gateway_requests WHERE id = 'req-3'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(violated);
    }

    #[test]
    fn spend_for_task_aggregates_settled_amounts_in_the_recorded_kind() {
        let (mut store, task_id) = store_with_task();
        for (id, settled) in [("a", 100.0), ("b", 250.0)] {
            store
                .open_gateway_request(open_for(id, Some(task_id)))
                .unwrap();
            store
                .close_gateway_request(
                    id,
                    GatewayRequestClose {
                        settled_amount: Some(ResourceAmount::Tokens(settled as u64)),
                        usage_known: true,
                        input_tokens: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        output_tokens: None,
                        bound_violated: false,
                        upstream_status: Some(200),
                        terminal_state_is_clean: true,
                        now: OffsetDateTime::UNIX_EPOCH,
                    },
                    "completed_cleanly",
                )
                .unwrap();
        }
        let (count, total) = store.gateway_spend_for_task(task_id).unwrap();
        assert_eq!(count, 2);
        assert_eq!(total, Some(ResourceAmount::Tokens(350)));
    }

    #[test]
    fn the_schema_has_no_column_that_could_hold_a_body_or_a_credential() {
        let store = LedgerStore::open_in_memory().unwrap();
        let mut stmt = store
            .conn
            .prepare("SELECT name FROM pragma_table_info('gateway_requests')")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        // Exact names, not substrings: `input_tokens`/`output_tokens`/
        // `max_tokens` are legitimate scalar counts, and a substring
        // match on "token" would flag them while missing a column
        // actually named `body`.
        for forbidden in [
            "body",
            "request_body",
            "response_body",
            "headers",
            "request_headers",
            "response_headers",
            "prompt",
            "prompt_text",
            "content",
            "tool_output",
            "api_key",
            "credential",
            "auth_token",
            "secret",
            "authorization",
        ] {
            assert!(
                !columns.iter().any(|c| c == forbidden),
                "gateway_requests must never gain a `{forbidden}` column — see 0007's header \
                 comment and the privacy invariant in 0001_init.sql"
            );
        }
        assert!(columns.contains(&"pricing_version".to_string()));
        assert!(columns.contains(&"bound_violated".to_string()));
    }
}
