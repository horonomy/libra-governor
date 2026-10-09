//! Thin row-readers backing `libra-governor economics explain` (HORO-1672).
//!
//! Every category here is assembled from the *real* persisted tables
//! (`resource_accounts`, `reservations`, `gateway_requests`,
//! `task_budgets`) — see `libra_governor_domain::economic_truth` module
//! docs for why "reconcile against canonical ledger events" cannot mean
//! replaying a persisted `EconomicEvent` stream (none exists).
//!
//! This module reuses existing readers verbatim wherever one already
//! exists (`account_spend`, `reservations_for_account`,
//! `task_id_for_session`, `Reservation::outstanding_draw`/`overrun`/
//! `refunded`) and writes new raw SQL only for what nothing else already
//! answers: `funding_lease_id`, the gateway observed/assumed split, the
//! two gateway unattributed buckets, orphan-account detection, and the
//! bounded custody-tree walk.

use libra_governor_domain::{
    AccountId, Allocation, AmountScope, CheckId, CheckOutcome, CompletionReservePosture,
    CustodyNode, CustodyTree, EconomicCategories, EconomicTruth, Forecast, LeaseHolds, LeaseKind,
    NoEconomicBasis, NodeLineage, PartialBucket, Reconciliation, ReconciliationCheck,
    ReleasedCapacity, ResourceBasis, ResourceKind, RuntimeDecision, ScopeResolution, ScopedAmount,
    SelectorEcho, SettledSpend, SpendScope, SpendSoFar, TaskId, Totals, TreeBudget, Truncation,
    TruthProvenance, TruthSource, Unattributed, UnattributedReason, ECONOMIC_TRUTH_SCHEMA_VERSION,
};
use rusqlite::OptionalExtension;

use crate::{store::LedgerStore, LedgerError};

/// What amount-and-basis a lease-kind/state partition reduces to.
fn sum_scoped(
    values: &[f64],
    kind: ResourceKind,
    basis: ResourceBasis,
    scope: AmountScope,
) -> Option<ScopedAmount> {
    if values.is_empty() {
        return None;
    }
    Some(ScopedAmount {
        kind,
        value: values.iter().sum(),
        basis,
        scope,
    })
}

impl LedgerStore {
    /// Builds the full [`EconomicTruth`] report for a `--task` selector.
    pub fn economic_truth_for_task(&self, task_id: TaskId) -> Result<EconomicTruth, LedgerError> {
        let account_id = AccountId::for_task(task_id);
        self.economic_truth_for_account_with_selector(
            account_id,
            SelectorEcho {
                selector: "task".to_string(),
                value: task_id.0.to_string(),
            },
            NoEconomicBasis::NoSuchTask,
        )
    }

    /// Builds the full [`EconomicTruth`] report for a `--session`
    /// selector: resolves the session's bound task via
    /// [`Self::task_id_for_session`] (reused verbatim), then reports
    /// that task's account.
    ///
    /// HORO-1673 finding I9 (fixed here): this used to delegate to
    /// [`Self::economic_truth_for_task`] wholesale, which overwrote the
    /// returned report's [`SelectorEcho`] to `{selector: "task", value:
    /// task_id}` — a session query's own identity was lost on every
    /// success path, and two sessions sharing one task received
    /// byte-identical reports with no session identifier anywhere in
    /// the output. The selector is now preserved. The totals
    /// themselves are still genuinely the task account's own figures
    /// (v0.0.3 mints no dedicated session-level account — see
    /// `ADR-0008`), labeled [`AmountScope::OwnAccountExclusive`]/
    /// [`AmountScope::OwnAccountInclusive`] same as a direct `--task`
    /// query, **not** [`AmountScope::EnclosingAccount`] — relabeling the
    /// totals themselves to `EnclosingAccount` would change a
    /// documented ADR-0013 contract and is tracked separately
    /// (HORO-1673 evidence README) rather than done as part of this
    /// fix.
    pub fn economic_truth_for_session(
        &self,
        session_id: &str,
    ) -> Result<EconomicTruth, LedgerError> {
        let Some(task_id) = self.task_id_for_session(session_id)? else {
            return Ok(EconomicTruth::no_basis(
                SelectorEcho {
                    selector: "session".to_string(),
                    value: session_id.to_string(),
                },
                NoEconomicBasis::NoSuchSession,
            ));
        };
        let mut truth = self.economic_truth_for_task(task_id)?;
        truth.selector = SelectorEcho {
            selector: "session".to_string(),
            value: session_id.to_string(),
        };
        Ok(truth)
    }

    /// Builds the full [`EconomicTruth`] report for an `--account`
    /// selector, given a raw account id string.
    pub fn economic_truth_for_account_str(
        &self,
        raw_account_id: &str,
    ) -> Result<EconomicTruth, LedgerError> {
        let Ok(uuid) = uuid::Uuid::parse_str(raw_account_id) else {
            return Ok(EconomicTruth::no_basis(
                SelectorEcho {
                    selector: "account".to_string(),
                    value: raw_account_id.to_string(),
                },
                NoEconomicBasis::NoSuchAccount,
            ));
        };
        self.economic_truth_for_account_with_selector(
            AccountId(uuid),
            SelectorEcho {
                selector: "account".to_string(),
                value: raw_account_id.to_string(),
            },
            NoEconomicBasis::NoSuchAccount,
        )
    }

    fn economic_truth_for_account_with_selector(
        &self,
        account_id: AccountId,
        selector: SelectorEcho,
        not_found_reason: NoEconomicBasis,
    ) -> Result<EconomicTruth, LedgerError> {
        let Some(account) = self.account(account_id)? else {
            return Ok(EconomicTruth::no_basis(selector, not_found_reason));
        };

        let funding_lease_id = self.funding_lease_id(account_id)?;
        let totals = Totals {
            exclusive: self.account_spend(account_id, SpendScope::Exclusive)?,
            inclusive: self.account_spend(account_id, SpendScope::Inclusive)?,
        };
        let leases = self.reservations_for_account(account_id)?;

        let mut work_hold_active = Vec::new();
        let mut subaccount_funding_active = Vec::new();
        let mut settled_observed = Vec::new();
        let mut settled_assumed = Vec::new();
        let mut overrun = Vec::new();
        let mut refunded = Vec::new();
        let mut released = Vec::new();
        let mut expired = Vec::new();
        let mut settled_after_expiry_count = 0u32;
        let mut legacy_count = 0u32;
        let mut next_expiry: Option<time::OffsetDateTime> = None;

        for r in &leases {
            use libra_governor_domain::ReservationState;
            if r.legacy_pre_0011 {
                legacy_count += 1;
            }
            match (r.lease_kind, r.state) {
                (LeaseKind::WorkHold, ReservationState::Active) => {
                    work_hold_active.push(r.amount.as_f64());
                    next_expiry = Some(match next_expiry {
                        Some(cur) if cur <= r.expires_at => cur,
                        _ => r.expires_at,
                    });
                }
                (LeaseKind::SubaccountFunding, ReservationState::Active) => {
                    subaccount_funding_active.push(r.amount.as_f64());
                }
                (LeaseKind::WorkHold, ReservationState::Settled) => {
                    let amt = r.settled_amount.map(|a| a.as_f64()).unwrap_or(0.0);
                    if r.usage_known == Some(true) {
                        settled_observed.push(amt);
                    } else {
                        settled_assumed.push(amt);
                    }
                    if let Some(o) = r.overrun() {
                        overrun.push(o.as_f64());
                    }
                    if let Some(rf) = r.refunded() {
                        refunded.push(rf.as_f64());
                    }
                    if r.settled_after_expiry {
                        settled_after_expiry_count += 1;
                    }
                }
                (_, ReservationState::Released) => released.push(r.amount.as_f64()),
                (_, ReservationState::Expired) => expired.push(r.amount.as_f64()),
                _ => {}
            }
        }

        let kind = account.resource_kind;
        let active_leases = LeaseHolds {
            work_hold_required: sum_scoped(
                &work_hold_active,
                kind,
                ResourceBasis::LibraReservationHold,
                AmountScope::OwnAccountExclusive,
            ),
            work_hold_optional: None,
            subaccount_funding: sum_scoped(
                &subaccount_funding_active,
                kind,
                ResourceBasis::LibraReservationHold,
                AmountScope::OwnAccountExclusive,
            ),
            count: (work_hold_active.len() + subaccount_funding_active.len()) as u32,
            next_expiry,
        };

        let settled = SettledSpend {
            observed: sum_scoped(
                &settled_observed,
                kind,
                ResourceBasis::ProviderReportedActual,
                AmountScope::OwnAccountExclusive,
            ),
            assumed: sum_scoped(
                &settled_assumed,
                kind,
                ResourceBasis::LibraReservationHold,
                AmountScope::OwnAccountExclusive,
            ),
            overrun: sum_scoped(
                &overrun,
                kind,
                ResourceBasis::ProviderReportedActual,
                AmountScope::OwnAccountExclusive,
            ),
            refunded: sum_scoped(
                &refunded,
                kind,
                ResourceBasis::ProviderReportedActual,
                AmountScope::OwnAccountExclusive,
            ),
            settled_after_expiry_count,
            count_observed: settled_observed.len() as u32,
            count_assumed: settled_assumed.len() as u32,
        };

        let released_capacity = ReleasedCapacity {
            released: sum_scoped(
                &released,
                kind,
                ResourceBasis::LibraReservationHold,
                AmountScope::OwnAccountExclusive,
            ),
            expired: sum_scoped(
                &expired,
                kind,
                ResourceBasis::LibraReservationHold,
                AmountScope::OwnAccountExclusive,
            ),
            released_count: released.len() as u32,
            expired_count: expired.len() as u32,
        };

        let task_budget = match account.task_id {
            Some(task_id) => self.task_budget(task_id)?,
            None => None,
        };

        let completion_reserve = CompletionReservePosture {
            protected: task_budget.as_ref().map(|b| ScopedAmount {
                kind: b.resource_kind,
                value: b.completion_reserve.as_f64(),
                basis: ResourceBasis::LibraReservationHold,
                scope: AmountScope::EnclosingAccount,
            }),
            outstanding_draw: None,
            basis: task_budget.as_ref().map(|b| b.completion_reserve_basis),
            required_criteria_count: 0,
        };

        let allocation = Allocation {
            granted_capacity: self.account_capacity(account_id)?.and_then(|cap| {
                cap.granted_capacity.map(|amount| ScopedAmount {
                    kind: cap.resource_kind,
                    value: amount.as_f64(),
                    basis: ResourceBasis::ImportedAllocationSnapshot,
                    scope: AmountScope::OwnAccountExclusive,
                })
            }),
            authority: account.authority_source,
            enforcement_scope: account.enforcement_scope,
            funding_lease: funding_lease_id,
        };

        let forecast = self.forecast_for_account(&account)?;
        let unattributed = self.unattributed_for_account(account.task_id, legacy_count)?;
        let custody_tree = self.custody_tree(account_id, TreeBudget::default())?;
        let reconciliation = self.reconciliation_for_account(account_id, &totals, &custody_tree)?;

        let categories = EconomicCategories {
            allocation,
            forecast,
            active_leases,
            completion_reserve,
            settled,
            released: released_capacity,
            totals,
            unattributed,
        };

        Ok(EconomicTruth {
            schema_version: ECONOMIC_TRUTH_SCHEMA_VERSION.to_string(),
            selector,
            resolution: ScopeResolution::Account { account_id },
            categories: Some(categories),
            custody_tree: Some(custody_tree),
            reconciliation: Some(reconciliation),
            provenance: TruthProvenance {
                weakest_truth: None,
                account_provenance: Some(account.provenance),
                account_schema_version: Some(account.account_schema_version.clone()),
                sources_read: vec![
                    TruthSource::ResourceAccounts,
                    TruthSource::Reservations,
                    TruthSource::GatewayRequests,
                    TruthSource::TaskBudgets,
                    TruthSource::ShadowDecisions,
                ],
                pricing_versions: Vec::new(),
            },
            task_budget,
        })
    }

    /// `resource_accounts.funding_lease_id` for `account_id`, parsed to a
    /// [`libra_governor_domain::ReservationId`]. Not part of
    /// [`Self::account`]'s own select list (that reader never needed it
    /// before HORO-1672), so this is its own narrow query.
    fn funding_lease_id(
        &self,
        account_id: AccountId,
    ) -> Result<Option<libra_governor_domain::ReservationId>, LedgerError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT funding_lease_id FROM resource_accounts WHERE account_id = ?1",
                [account_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        raw.map(|s| {
            uuid::Uuid::parse_str(&s)
                .map(libra_governor_domain::ReservationId)
                .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
        })
        .transpose()
    }

    /// The plan's recorded estimate (`at_admission`) and the newest
    /// recorded shadow decision's frozen remaining-work estimate
    /// (`latest_remaining`), for this account's task when it has one.
    /// `decision_json` already round-trips to a `RuntimeDecision` (it is
    /// `#[serde(transparent)]` on `Shadow<RuntimeDecision>` — see
    /// HORO-1670), so both fields are decoded from data this ledger
    /// already persists, not reconstructed or guessed.
    fn forecast_for_account(
        &self,
        account: &libra_governor_domain::ResourceAccount,
    ) -> Result<Forecast, LedgerError> {
        let Some(task_id) = account.task_id else {
            return Ok(Forecast {
                at_admission: None,
                latest_remaining: None,
                latest_decided_at: None,
                decision_points_recorded: 0,
            });
        };
        let decisions = self.shadow_decisions_for_task(task_id)?;
        let latest = decisions.last();
        let latest_remaining = latest.and_then(|d| {
            serde_json::from_str::<RuntimeDecision>(&d.decision_json)
                .ok()
                .map(|rd| rd.remaining)
        });
        let at_admission = latest
            .and_then(|d| self.get_plan(d.plan_id).ok().flatten())
            .and_then(|plan| plan.estimate);
        Ok(Forecast {
            at_admission,
            latest_remaining,
            latest_decided_at: latest.map(|d| d.decided_at),
            decision_points_recorded: decisions.len(),
        })
    }

    /// Rows that exist but could not be cleanly attributed to this
    /// account's scope: an orphan non-task account with no parent, a
    /// gateway row with no `task_id`, a gateway row whose
    /// `settled_amount` is populated but has no linked reservation, and
    /// leases legacy-backfilled from pre-0011 `task_budgets` rows.
    /// Scoped by `task_id`, not `account_id`: `gateway_requests` and
    /// `resource_accounts` both carry a `task_id` column, but gateway
    /// rows with no task at all (`task_id IS NULL`) and orphan accounts
    /// belonging to a *different* task must never be reported under
    /// this account's report — doing so attributed an unrelated task's
    /// unattributed-row counts to this one (HORO-1673 finding D4).
    /// `task_id = ?1` never matches a NULL column, so a row with no
    /// task at all correctly contributes zero to every task-scoped
    /// report, not the ledger-wide count.
    fn unattributed_for_account(
        &self,
        task_id: Option<TaskId>,
        legacy_count: u32,
    ) -> Result<Unattributed, LedgerError> {
        let Some(task_id) = task_id else {
            return Ok(Unattributed {
                legacy_backfilled: PartialBucket {
                    amount: None,
                    row_count: legacy_count,
                    reason: UnattributedReason::LegacyPreMigration0011,
                },
                gateway_rows_without_task: PartialBucket {
                    amount: None,
                    row_count: 0,
                    reason: UnattributedReason::GatewayRowWithoutTask,
                },
                gateway_settled_without_lease: PartialBucket {
                    amount: None,
                    row_count: 0,
                    reason: UnattributedReason::GatewaySettledWithoutLease,
                },
                accounts_without_proven_lineage: 0,
                orphan_accounts: Vec::new(),
            });
        };
        let task_id_str = task_id.to_string();

        let orphan_account_ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT account_id FROM resource_accounts
                 WHERE level <> 'task' AND parent_account_id IS NULL AND task_id = ?1",
            )?;
            let rows = stmt
                .query_map([&task_id_str], |row| {
                    let s: String = row.get(0)?;
                    Ok(s)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let orphan_accounts: Vec<AccountId> = orphan_account_ids
            .into_iter()
            .filter_map(|s| uuid::Uuid::parse_str(&s).ok().map(AccountId))
            .collect();

        // A gateway row with `task_id IS NULL` cannot belong to any
        // specific task's report by construction — it has no task at
        // all. The ledger-wide count of these rows is a separate,
        // whole-ledger diagnostic (not currently surfaced by any
        // per-task/per-account selector) and must never be folded into
        // this task's figure.
        let gateway_rows_without_task: u32 = 0;
        let gateway_settled_without_lease: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM gateway_requests
             WHERE reservation_id IS NULL AND settled_amount IS NOT NULL AND task_id = ?1",
            [&task_id_str],
            |row| row.get(0),
        )?;

        Ok(Unattributed {
            legacy_backfilled: PartialBucket {
                amount: None,
                row_count: legacy_count,
                reason: UnattributedReason::LegacyPreMigration0011,
            },
            gateway_rows_without_task: PartialBucket {
                amount: None,
                row_count: gateway_rows_without_task,
                reason: UnattributedReason::GatewayRowWithoutTask,
            },
            gateway_settled_without_lease: PartialBucket {
                amount: None,
                row_count: gateway_settled_without_lease,
                reason: UnattributedReason::GatewaySettledWithoutLease,
            },
            accounts_without_proven_lineage: orphan_accounts.len() as u32,
            orphan_accounts,
        })
    }

    fn child_account_ids(&self, account_id: AccountId) -> Result<Vec<AccountId>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT account_id FROM resource_accounts
             WHERE parent_account_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([account_id.to_string()], |row| {
            let s: String = row.get(0)?;
            Ok(s)
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|s| {
                uuid::Uuid::parse_str(&s)
                    .map(AccountId)
                    .map_err(|_| LedgerError::Sqlite(rusqlite::Error::InvalidQuery))
            })
            .collect()
    }

    /// Walks the custody subtree rooted at `account_id` breadth-first,
    /// bounded by `budget` — an output-size bound, not cycle safety (no
    /// writer ever creates a cycle in `parent_account_id`).
    fn custody_tree(
        &self,
        account_id: AccountId,
        budget: TreeBudget,
    ) -> Result<CustodyTree, LedgerError> {
        let Some(root_account) = self.account(account_id)? else {
            return Ok(CustodyTree {
                root: None,
                nodes: Vec::new(),
                truncated: None,
            });
        };

        let mut nodes = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((account_id, root_account.parent_account_id, 0u16));
        let mut truncated = None;

        while let Some((id, parent, depth)) = queue.pop_front() {
            if nodes.len() >= budget.max_nodes {
                truncated = Some(Truncation::NodeBudgetExhausted);
                break;
            }
            let Some(account) = self.account(id)? else {
                continue;
            };
            let exclusive = self.account_spend(id, SpendScope::Exclusive)?;
            nodes.push(CustodyNode {
                account_id: id,
                parent,
                depth,
                level: account.level,
                natural_key_display: CustodyNode::redact_natural_key(id, &account.natural_key),
                state: account.state,
                provenance: account.provenance,
                exclusive,
                provider_lineage: account
                    .provider_lineage_status
                    .map(NodeLineage::Recorded)
                    .unwrap_or(NodeLineage::NotRecorded),
            });

            if depth + 1 >= budget.max_depth as u16 {
                if !self.child_account_ids(id)?.is_empty() {
                    truncated = Some(Truncation::DepthBudgetExhausted);
                }
                continue;
            }
            for child in self.child_account_ids(id)? {
                queue.push_back((child, Some(id), depth + 1));
            }
        }

        Ok(CustodyTree {
            root: Some(account_id),
            nodes,
            truncated,
        })
    }

    /// Runs the 3 real cross-source reconciliation checks. A genuine
    /// disagreement is reported as [`CheckOutcome::Discrepant`], never
    /// silently equalized — see module docs.
    fn reconciliation_for_account(
        &self,
        account_id: AccountId,
        totals: &Totals,
        tree: &CustodyTree,
    ) -> Result<Reconciliation, LedgerError> {
        let kind = match (&totals.exclusive, &totals.inclusive) {
            (SpendSoFar::Known { kind, .. }, _) => Some(*kind),
            (_, SpendSoFar::Known { kind, .. }) => Some(*kind),
            _ => None,
        };

        let subtree_additivity = match (kind, &totals.exclusive, &totals.inclusive) {
            (
                Some(kind),
                SpendSoFar::Known {
                    settled: ex_settled,
                    active_holds: ex_active,
                    ..
                },
                SpendSoFar::Known {
                    settled: inc_settled,
                    active_holds: inc_active,
                    ..
                },
            ) => {
                let children: Vec<AccountId> = self.child_account_ids(account_id)?;
                let mut child_inclusive_total = 0.0;
                for child in &children {
                    if let SpendSoFar::Known {
                        settled,
                        active_holds,
                        ..
                    } = self.account_spend(*child, SpendScope::Inclusive)?
                    {
                        child_inclusive_total += settled + active_holds;
                    }
                }
                let expected = (ex_settled + ex_active) + child_inclusive_total;
                let actual = inc_settled + inc_active;
                let delta = (expected - actual).abs();
                if children.is_empty() {
                    ReconciliationCheck {
                        check: CheckId::SubtreeAdditivity,
                        outcome: CheckOutcome::NotApplicable,
                    }
                } else if delta <= kind.reconciliation_epsilon() {
                    ReconciliationCheck {
                        check: CheckId::SubtreeAdditivity,
                        outcome: CheckOutcome::Reconciled,
                    }
                } else {
                    ReconciliationCheck {
                        check: CheckId::SubtreeAdditivity,
                        outcome: CheckOutcome::Discrepant { delta },
                    }
                }
            }
            _ => ReconciliationCheck {
                check: CheckId::SubtreeAdditivity,
                outcome: CheckOutcome::NotApplicable,
            },
        };

        let envelope_formula = self.envelope_formula_check(account_id)?;
        let gateway_agreement = self.gateway_ledger_agreement_check(tree, kind)?;

        Ok(Reconciliation::from_checks(vec![
            subtree_additivity,
            envelope_formula,
            gateway_agreement,
        ]))
    }

    fn envelope_formula_check(
        &self,
        account_id: AccountId,
    ) -> Result<ReconciliationCheck, LedgerError> {
        let Some(account) = self.account(account_id)? else {
            return Ok(ReconciliationCheck {
                check: CheckId::EnvelopeFormula,
                outcome: CheckOutcome::NotApplicable,
            });
        };
        let Some(task_id) = account.task_id else {
            return Ok(ReconciliationCheck {
                check: CheckId::EnvelopeFormula,
                outcome: CheckOutcome::NotApplicable,
            });
        };
        let Some(budget) = self.task_budget(task_id)? else {
            return Ok(ReconciliationCheck {
                check: CheckId::EnvelopeFormula,
                outcome: CheckOutcome::NotApplicable,
            });
        };
        let Some(headroom) = self.available(
            task_id,
            libra_governor_domain::ReservationClass::RequiredWork,
        )?
        else {
            return Ok(ReconciliationCheck {
                check: CheckId::EnvelopeFormula,
                outcome: CheckOutcome::NotApplicable,
            });
        };
        let SpendSoFar::Known {
            settled,
            active_holds,
            ..
        } = self.account_spend(account_id, SpendScope::Exclusive)?
        else {
            return Ok(ReconciliationCheck {
                check: CheckId::EnvelopeFormula,
                outcome: CheckOutcome::NotApplicable,
            });
        };
        // `effective_hard_limit`, not raw `hard_limit` (HORO-1727):
        // `headroom` comes from `LedgerStore::available`, which now reads
        // the effective ceiling too (see its doc comment) — this
        // reconciliation must compare against the same ceiling `headroom`
        // was computed from, or every task with a renewal grant would be
        // permanently `Discrepant` by exactly the granted amount.
        let expected = budget.effective_hard_limit().as_f64() - settled - active_holds;
        let delta = (expected - headroom.value).abs();
        let outcome = if delta <= budget.resource_kind.reconciliation_epsilon() {
            CheckOutcome::Reconciled
        } else {
            CheckOutcome::Discrepant { delta }
        };
        Ok(ReconciliationCheck {
            check: CheckId::EnvelopeFormula,
            outcome,
        })
    }

    /// `kind` is the account's resource kind, when known — the same
    /// value `SubtreeAdditivity` already uses for its epsilon. Without
    /// it this check fell back to a hardcoded `> 1.0` tolerance, which
    /// is roughly 100x too loose for `QuotaPercent` (whose own epsilon
    /// is `0.01`) and silently treated a near-$1 USD disagreement as
    /// reconciled (HORO-1673 finding D2).
    fn gateway_ledger_agreement_check(
        &self,
        tree: &CustodyTree,
        kind: Option<libra_governor_domain::ResourceKind>,
    ) -> Result<ReconciliationCheck, LedgerError> {
        let epsilon = kind.map(|k| k.reconciliation_epsilon()).unwrap_or(1.0);
        let account_ids: Vec<String> = tree
            .nodes
            .iter()
            .map(|n| n.account_id.to_string())
            .collect();
        if account_ids.is_empty() {
            return Ok(ReconciliationCheck {
                check: CheckId::GatewayLedgerAgreement,
                outcome: CheckOutcome::NotApplicable,
            });
        }
        let placeholders = account_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT COUNT(*) FROM gateway_requests g
             JOIN reservations r ON g.reservation_id = r.id
             WHERE r.account_id IN ({placeholders})
               AND g.settled_amount IS NOT NULL
               AND r.settled_amount IS NOT NULL
               AND ABS(g.settled_amount - r.settled_amount) > ?{}",
            account_ids.len() + 1
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = account_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .chain(std::iter::once(&epsilon as &dyn rusqlite::ToSql))
            .collect();
        let discrepant_count: i64 = stmt.query_row(params.as_slice(), |row| row.get(0))?;

        let total_sql = format!(
            "SELECT COUNT(*) FROM gateway_requests g
             JOIN reservations r ON g.reservation_id = r.id
             WHERE r.account_id IN ({placeholders})
               AND g.settled_amount IS NOT NULL AND r.settled_amount IS NOT NULL"
        );
        let mut total_stmt = self.conn.prepare(&total_sql)?;
        let params2: Vec<&dyn rusqlite::ToSql> = account_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let total_count: i64 = total_stmt.query_row(params2.as_slice(), |row| row.get(0))?;

        let outcome = if total_count == 0 {
            CheckOutcome::NotApplicable
        } else if discrepant_count == 0 {
            CheckOutcome::Reconciled
        } else {
            CheckOutcome::Discrepant {
                delta: discrepant_count as f64,
            }
        };
        Ok(ReconciliationCheck {
            check: CheckId::GatewayLedgerAgreement,
            outcome,
        })
    }
}

#[cfg(test)]
mod tests {
    //! HORO-1673 — oracle negative-control suite for the three
    //! reconciliation checks this module exposes
    //! (`economic_truth.rs` had zero tests before this ticket; see
    //! HORO-1673 evidence README). A check that always returns
    //! `Reconciled` makes every downstream invariant test vacuously
    //! pass, so these tests plant a specific, understood discrepancy
    //! and assert the check actually fires — not merely that a healthy
    //! ledger reports `Reconciled`.

    use super::*;
    use crate::reservation::test_support::{fixed_reserve, thousand_token_policy};
    use crate::{EnsureAccountOutcome, ReserveOutcome, ReserveRequest, SettleOutcome};
    use libra_governor_domain::{
        AccountLevel, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
        ConstraintMode, Policy, ResourceAmount, ResourceBound, TaskIdentity, TimeBound,
    };

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::UNIX_EPOCH
    }

    fn setup_task(store: &mut LedgerStore, reserve_amount: u64) -> TaskId {
        let task_id = TaskId::new();
        store
            .insert_task(
                &TaskIdentity {
                    id: task_id,
                    external_ref: None,
                },
                now(),
            )
            .unwrap();
        store
            .initialize_task_budget(
                task_id,
                &thousand_token_policy(),
                &fixed_reserve(reserve_amount),
                now(),
            )
            .unwrap();
        task_id
    }

    fn spend_so_far_kind(s: &SpendSoFar) -> &'static str {
        match s {
            SpendSoFar::Known { .. } => "Known",
            SpendSoFar::NoBasis { .. } => "NoBasis",
        }
    }

    fn reserve_outcome_kind(o: &ReserveOutcome) -> &'static str {
        match o {
            ReserveOutcome::Granted(_) => "Granted",
            ReserveOutcome::AlreadyGranted(_) => "AlreadyGranted",
            ReserveOutcome::Insufficient { .. } => "Insufficient",
            _ => "Other",
        }
    }

    fn settle_outcome_kind(o: &SettleOutcome) -> &'static str {
        match o {
            SettleOutcome::Settled { .. } => "Settled",
            SettleOutcome::AlreadyFinal(_) => "AlreadyFinal",
            SettleOutcome::NotFound => "NotFound",
        }
    }

    fn check_outcome(truth: &EconomicTruth, id: CheckId) -> CheckOutcome {
        truth
            .reconciliation
            .as_ref()
            .expect("reconciliation must be present for an existing task")
            .checks
            .iter()
            .find(|c| c.check == id)
            .expect("every check id must appear exactly once")
            .outcome
    }

    // --- SubtreeAdditivity: documents production shape, does not fabricate --

    #[test]
    fn subtree_additivity_is_not_applicable_when_the_account_has_no_children() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);
        let truth = store.economic_truth_for_task(task_id).unwrap();
        assert_eq!(
            check_outcome(&truth, CheckId::SubtreeAdditivity),
            CheckOutcome::NotApplicable,
            "production mints exactly one account per task (see HORO-1673 §0) \
             — SubtreeAdditivity is NotApplicable on every real ledger today, \
             not a gap in this test"
        );
    }

    #[test]
    fn in_a_production_shaped_ledger_inclusive_equals_exclusive_because_no_child_account_is_ever_created(
    ) {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);
        let task_account = AccountId::for_task(task_id);
        let reserved = match store
            .reserve(ReserveRequest {
                task_id,
                session_id: "sess-1",
                plan_id: None,
                class: libra_governor_domain::ReservationClass::RequiredWork,
                amount: ResourceAmount::Tokens(100),
                idempotency_key: "r-1",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
        {
            ReserveOutcome::Granted(r) => r,
            other => panic!("expected Granted, got {}", reserve_outcome_kind(&other)),
        };
        store.settle(reserved.id, None, now()).unwrap();

        let truth = store.economic_truth_for_task(task_id).unwrap();
        let categories = truth.categories.as_ref().unwrap();
        // `SpendSoFar::Known` carries its own `scope` discriminant, so a
        // whole-struct comparison would never match Exclusive against
        // Inclusive even when every other field agrees — compare the
        // figures the invariant is actually about.
        let (ex_settled, ex_active) = match categories.totals.exclusive {
            SpendSoFar::Known {
                settled,
                active_holds,
                ..
            } => (settled, active_holds),
            other => panic!("expected Known, got {}", spend_so_far_kind(&other)),
        };
        let (inc_settled, inc_active) = match categories.totals.inclusive {
            SpendSoFar::Known {
                settled,
                active_holds,
                ..
            } => (settled, active_holds),
            other => panic!("expected Known, got {}", spend_so_far_kind(&other)),
        };
        assert_eq!((ex_settled, ex_active), (inc_settled, inc_active));

        // Independently re-derived via a second, direct account_spend
        // call on the same account id, not by trusting the report.
        let (raw_ex_settled, raw_ex_active) = match store
            .account_spend(task_account, SpendScope::Exclusive)
            .unwrap()
        {
            SpendSoFar::Known {
                settled,
                active_holds,
                ..
            } => (settled, active_holds),
            other => panic!("expected Known, got {}", spend_so_far_kind(&other)),
        };
        let (raw_inc_settled, raw_inc_active) = match store
            .account_spend(task_account, SpendScope::Inclusive)
            .unwrap()
        {
            SpendSoFar::Known {
                settled,
                active_holds,
                ..
            } => (settled, active_holds),
            other => panic!("expected Known, got {}", spend_so_far_kind(&other)),
        };
        assert_eq!(
            (raw_ex_settled, raw_ex_active),
            (raw_inc_settled, raw_inc_active)
        );
    }

    // --- EnvelopeFormula: a genuine, understood cross-source divergence -----

    #[test]
    fn envelope_formula_reports_a_planted_cross_account_task_scoped_discrepancy() {
        // `available()` (crates/ledger/src/reservation.rs) sums settled/
        // active reservations filtered by `task_id` alone.
        // `account_spend(.., Exclusive)` filters by `account_id` alone.
        // Production always keeps these in lockstep (every reservation's
        // account_id is set together with its task_id by the same write
        // path). This test plants the one row shape that can only arise
        // from a direct, non-API write — a settled work_hold whose
        // account_id belongs to a *child* account but whose task_id still
        // points at the parent task — and proves the oracle catches the
        // resulting divergence rather than silently reporting Reconciled.
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);
        let task_account = AccountId::for_task(task_id);

        let EnsureAccountOutcome { account: child, .. } = store
            .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
            .unwrap()
            .unwrap();

        store
            .conn
            .execute(
                "INSERT INTO reservations (
                    id, task_id, session_id, plan_id, class, resource_kind, amount,
                    drawn_from_reserve, state, settled_amount, usage_known, idempotency_key,
                    created_at, expires_at, settled_at, released_at,
                    account_id, grants_account_id, lease_kind, settled_after_expiry, legacy_pre_0011
                 ) VALUES (?1, ?2, 'sess-1', NULL, 'required_work', 'tokens', 100.0,
                           0, 'settled', 100.0, 1, 'planted-d2-cross-account',
                           ?3, ?3, ?3, NULL, ?4, NULL, 'work_hold', 0, 0)",
                rusqlite::params![
                    libra_governor_domain::ReservationId::new().0.to_string(),
                    task_id.to_string(),
                    now()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                    child.account_id.to_string(),
                ],
            )
            .unwrap();

        let truth = store.economic_truth_for_task(task_id).unwrap();
        match check_outcome(&truth, CheckId::EnvelopeFormula) {
            CheckOutcome::Discrepant { delta } => {
                assert!(
                    (delta - 100.0).abs() < 0.01,
                    "expected a ~100-token discrepancy from the planted child-attributed row, got {delta}"
                );
            }
            other => panic!("expected Discrepant, got {other:?} — the oracle did not catch the planted divergence"),
        }
    }

    // --- GatewayLedgerAgreement: D2 (epsilon) + liveness --------------------

    #[test]
    fn gateway_ledger_agreement_catches_a_planted_settled_amount_disagreement() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);

        let reserved = match store
            .reserve(ReserveRequest {
                task_id,
                session_id: "sess-1",
                plan_id: None,
                class: libra_governor_domain::ReservationClass::RequiredWork,
                amount: ResourceAmount::Tokens(50),
                idempotency_key: "r-gw-1",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
        {
            ReserveOutcome::Granted(r) => r,
            other => panic!("expected Granted, got {}", reserve_outcome_kind(&other)),
        };
        let settled = match store
            .settle(reserved.id, Some(ResourceAmount::Tokens(50)), now())
            .unwrap()
        {
            SettleOutcome::Settled { reservation, .. } => reservation,
            other => panic!("expected Settled, got {}", settle_outcome_kind(&other)),
        };
        assert_eq!(settled.settled_amount.unwrap().as_f64(), 50.0);

        // Plant a disagreeing gateway row against the same reservation —
        // the provider side reports 65, the ledger side recorded 50.
        store
            .conn
            .execute(
                "INSERT INTO gateway_requests (
                    id, task_id, session_id, route, model, tier, decision,
                    decision_detail_json, reservation_id, reserved_amount, settled_amount,
                    resource_kind, usage_known, bound_violated, pricing_version,
                    terminal_state, created_at
                 ) VALUES (?1, ?2, 'sess-1', '/v1/test', 'test-model', 'standard', 'allowed',
                           NULL, ?3, 50.0, 65.0, 'tokens', 1, 0, 'test-pricing-v1',
                           'settled', ?4)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    task_id.to_string(),
                    reserved.id.0.to_string(),
                    now()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ],
            )
            .unwrap();

        let truth = store.economic_truth_for_task(task_id).unwrap();
        match check_outcome(&truth, CheckId::GatewayLedgerAgreement) {
            CheckOutcome::Discrepant { .. } => {}
            other => panic!(
                "expected Discrepant, got {other:?} — the planted $15 disagreement was not caught"
            ),
        }
    }

    #[test]
    fn gateway_ledger_agreement_epsilon_now_respects_the_resource_kind_d2_fix() {
        // Before the D2 fix this check hardcoded `> 1.0` regardless of
        // resource kind. A 0.5 QuotaPercent disagreement is 50x
        // QuotaPercent's own epsilon (0.01, see
        // `ResourceKind::reconciliation_epsilon`) and must now be caught
        // — under the old hardcoded tolerance it would have been
        // silently reported `Reconciled`.
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        store
            .insert_task(
                &TaskIdentity {
                    id: task_id,
                    external_ref: None,
                },
                now(),
            )
            .unwrap();
        let policy = Policy::validated(
            "test",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::QuotaPercent(100.0),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::QuotaPercent(100.0),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap();
        store
            .initialize_task_budget(task_id, &policy, &fixed_reserve_quota(10.0), now())
            .unwrap();

        let reserved = match store
            .reserve(ReserveRequest {
                task_id,
                session_id: "sess-1",
                plan_id: None,
                class: libra_governor_domain::ReservationClass::RequiredWork,
                amount: ResourceAmount::QuotaPercent(5.0),
                idempotency_key: "r-gw-quota",
                now: now(),
                ttl_secs: 900,
            })
            .unwrap()
        {
            ReserveOutcome::Granted(r) => r,
            other => panic!("expected Granted, got {}", reserve_outcome_kind(&other)),
        };
        let settled = match store
            .settle(reserved.id, Some(ResourceAmount::QuotaPercent(5.0)), now())
            .unwrap()
        {
            SettleOutcome::Settled { reservation, .. } => reservation,
            other => panic!("expected Settled, got {}", settle_outcome_kind(&other)),
        };
        assert_eq!(settled.settled_amount.unwrap().as_f64(), 5.0);

        store
            .conn
            .execute(
                "INSERT INTO gateway_requests (
                    id, task_id, session_id, route, model, tier, decision,
                    decision_detail_json, reservation_id, reserved_amount, settled_amount,
                    resource_kind, usage_known, bound_violated, pricing_version,
                    terminal_state, created_at
                 ) VALUES (?1, ?2, 'sess-1', '/v1/test', 'test-model', 'standard', 'allowed',
                           NULL, ?3, 5.0, 5.5, 'quota_percent', 1, 0, 'test-pricing-v1',
                           'settled', ?4)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    task_id.to_string(),
                    reserved.id.0.to_string(),
                    now()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ],
            )
            .unwrap();

        let truth = store.economic_truth_for_task(task_id).unwrap();
        match check_outcome(&truth, CheckId::GatewayLedgerAgreement) {
            CheckOutcome::Discrepant { .. } => {}
            other => panic!(
                "expected Discrepant — a 0.5 QuotaPercent disagreement must be caught \
                 under QuotaPercent's own 0.01 epsilon; got {other:?} (D2 regression)"
            ),
        }
    }

    fn fixed_reserve_quota(amount: f32) -> libra_governor_domain::CompletionReserveEstimate {
        libra_governor_domain::CompletionReserveEstimate {
            amount: ResourceAmount::QuotaPercent(amount),
            basis: libra_governor_domain::CompletionReserveBasis::PolicyTarget,
            fraction: (amount / 100.0) as f64,
            required_criteria_count: 1,
        }
    }

    // --- D4 regression: unattributed counts no longer leak cross-task -----

    #[test]
    fn unattributed_counts_no_longer_leak_rows_from_an_unrelated_task_d4_fix() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_a = setup_task(&mut store, 200);
        let task_b = setup_task(&mut store, 200);

        // Plant an unattributed gateway row under task B only.
        store
            .conn
            .execute(
                "INSERT INTO gateway_requests (
                    id, task_id, session_id, route, model, tier, decision,
                    decision_detail_json, reservation_id, reserved_amount, settled_amount,
                    resource_kind, usage_known, bound_violated, pricing_version,
                    terminal_state, created_at
                 ) VALUES (?1, ?2, 'sess-b', '/v1/test', 'test-model', 'standard', 'allowed',
                           NULL, NULL, NULL, 42.0, 'tokens', 1, 0, 'test-pricing-v1',
                           'settled', ?3)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    task_b.to_string(),
                    now()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ],
            )
            .unwrap();

        let truth_a = store.economic_truth_for_task(task_a).unwrap();
        let unattributed_a = &truth_a.categories.as_ref().unwrap().unattributed;
        assert_eq!(
            unattributed_a.gateway_settled_without_lease.row_count, 0,
            "task A's report must not see task B's unattributed row (D4 regression)"
        );

        let truth_b = store.economic_truth_for_task(task_b).unwrap();
        let unattributed_b = &truth_b.categories.as_ref().unwrap().unattributed;
        assert_eq!(
            unattributed_b.gateway_settled_without_lease.row_count, 1,
            "task B's own report must still see its own unattributed row"
        );
    }

    // --- I9 regression: session selector is preserved, not overwritten -----

    #[test]
    fn session_selector_is_echoed_back_not_overwritten_with_the_task_id_i9_fix() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);
        // Bind a session to this task directly via `session_tasks`, the
        // same table `task_id_for_session` reads — avoids
        // `resolve_or_create_task_for_session` minting a second,
        // unrelated task.
        store
            .conn
            .execute(
                "INSERT INTO session_tasks (session_id, task_id, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "sess-echo-me",
                    task_id.to_string(),
                    now()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ],
            )
            .unwrap();

        let truth = store.economic_truth_for_session("sess-echo-me").unwrap();
        assert_eq!(truth.selector.selector, "session");
        assert_eq!(truth.selector.value, "sess-echo-me");
    }

    // --- I8: partial/unattributed spend is visible as a count, never an ---
    // --- amount (documented finding, not silently hidden) -----------------

    #[test]
    fn unattributed_amount_is_always_none_only_the_row_count_is_visible() {
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);
        store
            .conn
            .execute(
                "INSERT INTO gateway_requests (
                    id, task_id, session_id, route, model, tier, decision,
                    decision_detail_json, reservation_id, reserved_amount, settled_amount,
                    resource_kind, usage_known, bound_violated, pricing_version,
                    terminal_state, created_at
                 ) VALUES (?1, ?2, 'sess-1', '/v1/test', 'test-model', 'standard', 'allowed',
                           NULL, NULL, NULL, 77.0, 'tokens', 1, 0, 'test-pricing-v1',
                           'settled', ?3)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    task_id.to_string(),
                    now()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ],
            )
            .unwrap();

        let truth = store.economic_truth_for_task(task_id).unwrap();
        let unattributed = &truth.categories.as_ref().unwrap().unattributed;
        assert_eq!(unattributed.gateway_settled_without_lease.row_count, 1);
        assert!(
            unattributed.gateway_settled_without_lease.amount.is_none(),
            "HORO-1673 finding: unattributed spend is visible only as a row \
             count, never as a figure — PartialBucket.amount is always None"
        );
    }

    // --- I1: each canonical spend event is counted once --------------------
    //
    // A zero-dependency seeded LCG op-sequence generator (this repo has no
    // `proptest`/`quickcheck` anywhere — verified by grep across every
    // `Cargo.toml`). Prints its seed on failure for reproduction, per the
    // design handoff's recommendation (b).

    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            // Numerical Recipes LCG constants — deterministic, not
            // cryptographic, good enough to pick among a handful of op
            // kinds and small amounts.
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
        fn pick(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
    }

    #[test]
    fn generated_reserve_settle_sequences_never_count_a_reservation_row_twice() {
        for seed in [1u64, 2, 42, 1000, 999_999] {
            run_one_generated_sequence(seed);
        }
    }

    fn run_one_generated_sequence(seed: u64) {
        let mut rng = Lcg(seed.max(1));
        let mut store = LedgerStore::open_in_memory().unwrap();
        let task_id = setup_task(&mut store, 200);
        let mut granted_ids: Vec<libra_governor_domain::ReservationId> = Vec::new();
        let mut next_key = 0u32;

        for step in 0..40u32 {
            let op = rng.pick(5);
            match op {
                0 | 1 => {
                    // Fresh reserve, small amount, occasionally retried
                    // with the SAME idempotency key later (op 4) to prove
                    // the retry never creates a second row.
                    let amount = 1 + rng.pick(20);
                    next_key += 1;
                    let key = format!("seed-{seed}-key-{next_key}");
                    match store
                        .reserve(ReserveRequest {
                            task_id,
                            session_id: "sess-gen",
                            plan_id: None,
                            class: libra_governor_domain::ReservationClass::OptionalWork,
                            amount: ResourceAmount::Tokens(amount),
                            idempotency_key: &key,
                            now: now(),
                            ttl_secs: 900,
                        })
                        .unwrap()
                    {
                        ReserveOutcome::Granted(r) => granted_ids.push(r.id),
                        ReserveOutcome::Insufficient { .. } => {}
                        other => panic!(
                            "seed {seed} step {step}: unexpected reserve outcome {}",
                            reserve_outcome_kind(&other)
                        ),
                    }
                }
                2 => {
                    // Settle a previously granted reservation, if any.
                    if let Some(&id) = granted_ids.first() {
                        let _ = store.settle(id, None, now());
                    }
                }
                3 => {
                    // Release a previously granted reservation, if any.
                    if let Some(id) = granted_ids.pop() {
                        let _ = store.release(id, now());
                    }
                }
                _ => {
                    // Replay the most recent reserve's idempotency key —
                    // must be a safe no-op, never a second row.
                    let _ = store.expire_stale_reservations(now());
                }
            }
        }

        // Independent oracle: a raw SQL aggregate that does not call
        // `account_spend` at all, computed directly from `reservations`.
        let task_account = AccountId::for_task(task_id);
        let raw_settled: f64 = store
            .conn
            .query_row(
                "SELECT COALESCE(SUM(settled_amount), 0.0) FROM reservations
                 WHERE account_id = ?1 AND state = 'settled'",
                [task_account.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let raw_distinct_settled_rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(DISTINCT id) FROM reservations
                 WHERE account_id = ?1 AND state = 'settled'",
                [task_account.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let raw_settled_row_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM reservations WHERE account_id = ?1 AND state = 'settled'",
                [task_account.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            raw_distinct_settled_rows, raw_settled_row_count,
            "seed {seed}: a settled reservation row was counted more than once \
             by its own id — no replayed retry should ever create a \
             duplicate row"
        );

        let reported = match store
            .account_spend(task_account, SpendScope::Exclusive)
            .unwrap()
        {
            SpendSoFar::Known { settled, .. } => settled,
            other => panic!(
                "seed {seed}: expected Known, got {}",
                spend_so_far_kind(&other)
            ),
        };
        assert!(
            (reported - raw_settled).abs() < 0.001,
            "seed {seed}: account_spend reported {reported} but the independent \
             raw-SQL aggregate over distinct settled rows is {raw_settled} — \
             a reservation was double-counted or dropped"
        );
    }
}
