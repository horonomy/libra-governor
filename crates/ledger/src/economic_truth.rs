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
        let expected = budget.hard_limit.as_f64() - settled - active_holds;
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

