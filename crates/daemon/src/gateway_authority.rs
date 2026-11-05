//! [`LedgerSpendAuthority`] — the daemon side of the gateway's
//! `SpendAuthority` and `RequestRecorder` traits (HORO-1144).
//!
//! # This is where the deciding happens
//!
//! `ARCHITECTURE.md`: *"The gateway enforces; it does not decide."* The
//! gateway crate cannot open the ledger or construct a `Policy` — it has
//! no dependency through which it could. Every decision it acts on is
//! made here, in the daemon, which `ARCHITECTURE.md` names as the source
//! of truth for admission, ledger integrity, and policy evaluation.
//!
//! # Its own connection to the same file
//!
//! `rusqlite::Connection` is not `Sync`, and the ledger crate's own docs
//! prescribe one `LedgerStore` per thread or process. So the gateway's
//! authority holds a *separate* store, opened against the same SQLite
//! file, behind an `Arc<Mutex<..>>`. HORO-1141's WAL + `busy_timeout` +
//! `BEGIN IMMEDIATE` design is what makes two connections to one file
//! safe under concurrent writes, and its `reservation_concurrency.rs`
//! tests exercise exactly that shape.
//!
//! The mutex serialises this process's own gateway requests against each
//! other; SQLite's write lock serialises them against the daemon's
//! Unix-socket handler. Neither is load-bearing for correctness on its
//! own — the `BEGIN IMMEDIATE` transaction inside `LedgerStore::reserve`
//! is.

use std::sync::{Arc, Mutex};

use crate::shared_budget::{
    gateway_key, gateway_release, gateway_renew, gateway_reserve, gateway_settle, GatewayRefusal,
    GatewayReleaseOutcome, GatewayRenewOutcome, GatewayReserveOutcome, GatewayReserveRequest,
    GatewaySettleOutcome, GatewaySettleRequest, SharedBudgetRuntime,
};
use crate::shared_budget_file::{Enforcement, SharedBudgetFileState};
use libra_governor_domain::SharedPoolReservationId;
use libra_governor_domain::{Admission, Policy, ReservationClass, ReservationId, ResourceAmount};
use libra_governor_gateway::authority::{
    AuthorityError, BudgetContext, SharedBudgetAuthority, SharedDecision, SharedDenial, SharedHold,
    SharedMode, SharedReserveRequest, SharedSettled, SharedSettlement, SpendAuthority,
    SpendDecision, SpendDenial, SpendRequest,
};
use libra_governor_gateway::cost::SettleFlags;
use libra_governor_gateway::proxy::{GatewayRequestRecord, RequestRecorder};
use libra_governor_ledger::{
    GatewayRequestClose, GatewayRequestOpen, LedgerStore, ReserveOutcome, ReserveRequest,
};

/// Evaluates only the **resource** dimension of `policy` against a
/// projected spend.
///
/// The gateway sees one HTTP request. It has no duration estimate for it
/// and no confidence figure about it — those were decided once, at
/// admission, when the task was preflighted, and re-deciding them per
/// request would be both meaningless and wrong (every request would fail
/// a confidence floor it has no evidence for).
///
/// So the two dimensions the gateway cannot speak to are supplied as
/// deliberate no-ops: a projected duration of zero, which admits under
/// every `ConstraintMode`, and the policy's *own* `min_confidence`, which
/// makes `confidence_ok` exactly true. What remains is the resource
/// constraint — the only one a per-request boundary can honestly
/// enforce.
///
/// This exists as a named function rather than an inline
/// `evaluate(projected, 0, policy.min_confidence)` precisely so those two
/// arguments read as a decision rather than as a bug.
fn evaluate_resource_dimension_only(
    policy: &Policy,
    projected: ResourceAmount,
) -> Result<Admission, AuthorityError> {
    policy
        .evaluate(projected, 0, policy.min_confidence)
        .map(|decision| decision.admission)
        .map_err(|e| AuthorityError(e.to_string()))
}

/// Renders a `Deny` list into a short detail string. Amounts and limits
/// only — nothing here is derived from a request body.
fn render_denial(admission: &Admission) -> String {
    match admission {
        Admission::Deny(reasons) => reasons
            .iter()
            .map(|reason| format!("{reason:?}"))
            .collect::<Vec<_>>()
            .join("; "),
        other => format!("{other:?}"),
    }
}

/// The daemon's implementation of the gateway's spend interface.
pub struct LedgerSpendAuthority {
    ledger: Arc<Mutex<LedgerStore>>,
}

impl LedgerSpendAuthority {
    pub fn new(ledger: Arc<Mutex<LedgerStore>>) -> Self {
        Self { ledger }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LedgerStore>, AuthorityError> {
        self.ledger
            .lock()
            .map_err(|_| AuthorityError("gateway ledger mutex was poisoned".to_string()))
    }
}

impl SpendAuthority for LedgerSpendAuthority {
    fn budget_context(&self, session_id: &str) -> Result<Option<BudgetContext>, AuthorityError> {
        let ledger = self.lock()?;
        let Some(task_id) = ledger
            .task_id_for_session(session_id)
            .map_err(|e| AuthorityError(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(budget) = ledger
            .task_budget(task_id)
            .map_err(|e| AuthorityError(e.to_string()))?
        else {
            // A session with a task but no budget was never admitted.
            // Reported as "no context" so the gateway refuses rather than
            // inventing a resource kind to cost the request in.
            return Ok(None);
        };
        Ok(Some(BudgetContext {
            task_id,
            resource_kind: budget.resource_kind,
        }))
    }

    fn authorize(&self, req: SpendRequest<'_>) -> Result<SpendDecision, AuthorityError> {
        let mut ledger = self.lock()?;
        let Some(budget) = ledger
            .task_budget(req.task_id)
            .map_err(|e| AuthorityError(e.to_string()))?
        else {
            return Ok(SpendDecision::Denied(SpendDenial::NoBudget));
        };

        // The policy in force at admission, read back off the task's own
        // budget row rather than from daemon configuration: the limit a
        // task was admitted under is the limit for the life of that task,
        // even across a daemon restart with a changed config.
        let policy = &budget.policy;

        // Project total committed capacity plus this request's own
        // worst case. `committed` is read directly from the budget
        // snapshot's `used()`/`reserved()`/`completion_reserve()`
        // (HORO-1727), never derived by subtracting `available`'s
        // headroom from a ceiling — see `crates/daemon/src/server.rs`'s
        // sibling admission site for the full rationale. The `+
        // completion_reserve()` term is deliberately kept here (unlike
        // `server.rs`'s `RequiredWork` site, which has no such term):
        // `available(OptionalWork)` — which this committed figure used
        // to be derived from — subtracts the protected Completion
        // Reserve as well as settled/active spend, because optional work
        // can never draw into that reserve (see ADR 0003 §3 below). A
        // gateway spend request must keep seeing the reserve as already
        // "spoken for", or an optional-work request could be admitted
        // into capacity this task's required completion work still
        // needs.
        let snapshot = ledger
            .budget_snapshot(req.task_id)
            .map_err(|e| AuthorityError(e.to_string()))?
            .ok_or_else(|| AuthorityError("budget disappeared mid-authorize".to_string()))?;
        let committed = snapshot.used().as_f64()
            + snapshot.reserved().as_f64()
            + snapshot.completion_reserve().as_f64();
        let granted = snapshot.granted();
        let projected =
            ResourceAmount::from_kind_f64(budget.resource_kind, committed + req.amount.as_f64());

        // The resource bound evaluated against is shifted by `granted`
        // (HORO-1727) — see `ResourceBound::extended_by`'s docs. A local
        // copy: `policy` (== `budget.policy`) is the persisted admission
        // snapshot and must not be mutated.
        let mut admission_policy = policy.clone();
        admission_policy.resource = admission_policy.resource.extended_by(granted);
        let admission = evaluate_resource_dimension_only(&admission_policy, projected)?;
        let approval_required = match &admission {
            Admission::Deny(_) => {
                return Ok(SpendDecision::Denied(SpendDenial::PolicyDenied {
                    detail: render_denial(&admission),
                }));
            }
            Admission::ApprovalRequired(_) => true,
            Admission::Admit => false,
        };

        // The real gate. `Policy::evaluate` above classifies; THIS is
        // what atomically commits capacity, and an `Insufficient` here
        // refuses the request even when the policy said yes — two
        // concurrent requests can each pass the projection and only one
        // can win the reservation.
        //
        // `ReservationClass::OptionalWork`: the gateway cannot tell from
        // an HTTP request whether the tokens will satisfy a required
        // completion criterion, so it takes the weaker claim and can
        // never draw against the protected Completion Reserve. See ADR
        // 0003 §3.
        //
        // `plan_id: None`: a replan calls `release_active_for_plan`,
        // which would otherwise release a live in-flight request's
        // reservation out from under it. A gateway reservation's lifetime
        // is one HTTP request, not one plan.
        let outcome = ledger
            .reserve(ReserveRequest {
                task_id: req.task_id,
                session_id: req.session_id,
                plan_id: None,
                class: ReservationClass::OptionalWork,
                amount: req.amount,
                idempotency_key: req.idempotency_key,
                now: time::OffsetDateTime::now_utc(),
                ttl_secs: req.ttl_secs,
            })
            .map_err(|e| AuthorityError(e.to_string()))?;

        Ok(match outcome {
            ReserveOutcome::Granted(reservation) | ReserveOutcome::AlreadyGranted(reservation) => {
                SpendDecision::Granted {
                    reservation_id: reservation.id,
                    reserved: reservation.amount,
                    approval_required,
                }
            }
            ReserveOutcome::Insufficient {
                available,
                requested,
                protected_reserve,
            } => SpendDecision::Denied(SpendDenial::Insufficient {
                available,
                requested,
                protected_reserve,
            }),
            ReserveOutcome::NoBudget => SpendDecision::Denied(SpendDenial::NoBudget),
        })
    }

    fn settle(
        &self,
        reservation_id: ReservationId,
        actual: Option<ResourceAmount>,
    ) -> Result<(), AuthorityError> {
        let mut ledger = self.lock()?;
        ledger
            .settle(reservation_id, actual, time::OffsetDateTime::now_utc())
            .map(|_| ())
            .map_err(|e| AuthorityError(e.to_string()))
    }

    fn release(&self, reservation_id: ReservationId) -> Result<(), AuthorityError> {
        let mut ledger = self.lock()?;
        ledger
            .release(reservation_id, time::OffsetDateTime::now_utc())
            .map(|_| ())
            .map_err(|e| AuthorityError(e.to_string()))
    }
}

/// Writes each finished gateway request's provenance row.
///
/// Errors are logged and swallowed, never propagated: losing an audit row
/// is bad, but failing a response the user has already received — or
/// refusing a request the budget allows — because a log write failed is
/// worse. See [`RequestRecorder::record`].
pub struct LedgerRequestRecorder {
    ledger: Arc<Mutex<LedgerStore>>,
    log_path: std::path::PathBuf,
}

impl LedgerRequestRecorder {
    pub fn new(ledger: Arc<Mutex<LedgerStore>>, log_path: std::path::PathBuf) -> Self {
        Self { ledger, log_path }
    }
}

impl RequestRecorder for LedgerRequestRecorder {
    fn record(&self, record: GatewayRequestRecord) {
        let Ok(mut ledger) = self.ledger.lock() else {
            crate::log::append_line(
                &self.log_path,
                "gateway: could not record a request (ledger mutex poisoned)",
            );
            return;
        };
        let now = time::OffsetDateTime::now_utc();

        // The open write is unconditional: a request that was admitted,
        // forwarded, and then lost to a crash mid-stream must still have
        // left a trace explaining what the reservation the TTL reclaims
        // was for.
        if let Err(e) = ledger.open_gateway_request(GatewayRequestOpen {
            id: &record.id,
            task_id: record.task_id,
            session_id: record.session_id.as_deref(),
            route: record.route,
            model: record.model.as_deref(),
            tier: record.tier,
            decision: record.decision,
            decision_detail: record.decision_detail.as_deref(),
            reservation_id: record.reservation_id,
            reserved_amount: record.reserved_amount,
            resource_kind: record.resource_kind,
            max_tokens: record.max_tokens,
            pricing_version: record.pricing_version,
            terminal_state: record.terminal_state,
            upstream_status: record.upstream_status,
            now,
        }) {
            crate::log::append_line(
                &self.log_path,
                &format!("gateway: could not record a request decision: {e}"),
            );
            return;
        }

        // Only a request that actually reached settlement has figures to
        // close with; a refusal's row is complete as written above.
        if record.settled_amount.is_some() || record.output_tokens.is_some() {
            if let Err(e) = ledger.close_gateway_request(
                &record.id,
                GatewayRequestClose {
                    settled_amount: record.settled_amount,
                    usage_known: record.usage_known,
                    input_tokens: record.input_tokens,
                    cache_creation_input_tokens: record.cache_creation_input_tokens,
                    cache_read_input_tokens: record.cache_read_input_tokens,
                    output_tokens: record.output_tokens,
                    bound_violated: record.bound_violated,
                    upstream_status: record.upstream_status,
                    terminal_state_is_clean: record.terminal_state == "completed_cleanly",
                    now,
                },
                record.terminal_state,
            ) {
                crate::log::append_line(
                    &self.log_path,
                    &format!("gateway: could not record a request settlement: {e}"),
                );
            }
        }

        if record.bound_violated {
            crate::log::append_line(
                &self.log_path,
                &format!(
                    "gateway: bound_violation on request {} — the provider reported more output \
                     tokens than the request's own max_tokens declared",
                    record.id
                ),
            );
        }
    }
}

/// Opens the gateway's own `LedgerStore` against `ledger_path`.
///
/// A separate connection from the daemon's, deliberately — see this
/// module's docs.
pub fn open_gateway_ledger(
    ledger_path: &std::path::Path,
) -> Result<Arc<Mutex<LedgerStore>>, libra_governor_ledger::LedgerError> {
    Ok(Arc::new(Mutex::new(LedgerStore::open(ledger_path)?)))
}

/// The daemon's implementation of the gateway's shared-budget interface
/// (S11e): the per-request reserve, settle, release and renew of the local
/// monthly cap, over the shared-budget primitives.
///
/// It owns the gateway process's `boot_id`, which prefixes every
/// idempotency key (`gw:<boot_id>:<request_id>`) so the next daemon start
/// can tell this process's holds from its own.
pub struct LedgerSharedAuthority {
    ledger: Arc<Mutex<LedgerStore>>,
    runtime: Arc<SharedBudgetRuntime>,
    boot_id: String,
    /// Whether the gateway holds the upstream credential. The shared cap is
    /// only hard when it does; a pass-through gateway cannot honour
    /// `enforcement = gateway`.
    governor_held: bool,
}

impl LedgerSharedAuthority {
    pub fn new(
        ledger: Arc<Mutex<LedgerStore>>,
        runtime: Arc<SharedBudgetRuntime>,
        boot_id: String,
        governor_held: bool,
    ) -> Self {
        Self {
            ledger,
            runtime,
            boot_id,
            governor_held,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LedgerStore>, AuthorityError> {
        self.ledger
            .lock()
            .map_err(|_| AuthorityError("gateway ledger mutex was poisoned".to_string()))
    }
}

fn shared_error(e: impl std::fmt::Display) -> AuthorityError {
    AuthorityError(e.to_string())
}

impl SharedBudgetAuthority for LedgerSharedAuthority {
    fn mode(&self) -> SharedMode {
        match self.runtime.effective().state {
            SharedBudgetFileState::Valid(cfg) if cfg.enforcement == Enforcement::Gateway => {
                if self.governor_held {
                    SharedMode::Enforce
                } else {
                    SharedMode::Unavailable
                }
            }
            // No file, `enforcement = hooks`, or a file that cannot be read:
            // the gateway behaves as it did before the shared budget. An
            // invalid file is deliberately NOT a refusal here (design 3.2
            // would fail closed); the hook path still denies it, and
            // changing the gateway's behaviour for existing governor_held
            // deployments is a separate decision.
            SharedBudgetFileState::Absent
            | SharedBudgetFileState::Invalid(_)
            | SharedBudgetFileState::Valid(_) => SharedMode::NotConsulted,
        }
    }

    fn reserve(&self, req: SharedReserveRequest<'_>) -> Result<SharedDecision, AuthorityError> {
        let Some(key) = gateway_key(&self.boot_id, req.request_id) else {
            return Ok(SharedDecision::Denied(SharedDenial::Unavailable));
        };
        let mut ledger = self.lock()?;
        let outcome = gateway_reserve(
            &mut ledger,
            &self.runtime,
            &GatewayReserveRequest {
                session_id: req.session_id,
                model: req.model,
                body_bytes: req.body_bytes,
                max_tokens: req.max_tokens,
                image_count: req.image_count,
                key: &key,
                ttl_secs: req.ttl_secs,
            },
        )
        .map_err(shared_error)?;
        Ok(match outcome {
            GatewayReserveOutcome::Granted(hold) => SharedDecision::Granted(SharedHold {
                id: hold.reservation_id,
                reserved_usd_cents: hold.reserved_usd_cents,
                input_bound_tokens: hold.input_bound_tokens,
                model: hold.model,
                max_tokens: hold.max_tokens,
            }),
            GatewayReserveOutcome::Refused(refusal) => match refusal {
                GatewayRefusal::NotConfigured | GatewayRefusal::NotGatewayEnforced => {
                    SharedDecision::NotConsulted
                }
                GatewayRefusal::CapExhausted {
                    remaining_usd_cents,
                } => SharedDecision::Denied(SharedDenial::CapExhausted {
                    remaining_usd_cents,
                }),
                GatewayRefusal::Identity(_) | GatewayRefusal::SessionIdTooLong => {
                    SharedDecision::Denied(SharedDenial::Unattributable)
                }
                GatewayRefusal::Unenforceable(gap) => {
                    SharedDecision::Denied(SharedDenial::Unenforceable(gap))
                }
                GatewayRefusal::ConfigInvalid(_)
                | GatewayRefusal::ClockAnomaly { .. }
                | GatewayRefusal::InvalidKey
                | GatewayRefusal::KeyConflict
                | GatewayRefusal::KeyNotActive
                | GatewayRefusal::PoolUnitMismatch => {
                    SharedDecision::Denied(SharedDenial::Unavailable)
                }
            },
        })
    }

    fn settle(&self, settlement: SharedSettlement<'_>) -> Result<SharedSettled, AuthorityError> {
        let hold = settlement.hold;
        let mut ledger = self.lock()?;
        let outcome = gateway_settle(
            &mut ledger,
            &self.runtime,
            &GatewaySettleRequest {
                reservation_id: hold.id,
                observed: settlement.observed,
                completed_cleanly: settlement.completed_cleanly,
                saw_message_start: settlement.saw_message_start,
                // Exactly what the reservation was priced with.
                model: &hold.model,
                max_tokens: hold.max_tokens,
                input_bound_tokens: hold.input_bound_tokens,
            },
        )
        .map_err(shared_error)?;
        match outcome {
            GatewaySettleOutcome::Settled {
                booked_usd_cents,
                usage_known,
                flags,
                ..
            } => Ok(SharedSettled {
                booked_usd_cents,
                usage_known,
                flags,
            }),
            // Booked already (orphan sweep or expiry): the conservative
            // amount stands and there is nothing to add.
            GatewaySettleOutcome::AlreadyFinal => Ok(SharedSettled {
                booked_usd_cents: hold.reserved_usd_cents,
                usage_known: false,
                flags: SettleFlags::default(),
            }),
            GatewaySettleOutcome::NotFound | GatewaySettleOutcome::NotGatewayHold => Err(
                AuthorityError("the shared hold is not a gateway hold".to_string()),
            ),
        }
    }

    fn release(&self, id: SharedPoolReservationId) -> Result<(), AuthorityError> {
        let mut ledger = self.lock()?;
        match gateway_release(&mut ledger, &self.runtime, id).map_err(shared_error)? {
            GatewayReleaseOutcome::Released | GatewayReleaseOutcome::AlreadyFinal => Ok(()),
            GatewayReleaseOutcome::NotFound | GatewayReleaseOutcome::NotGatewayHold => Err(
                AuthorityError("the shared hold is not a gateway hold".to_string()),
            ),
        }
    }

    fn renew(&self, id: SharedPoolReservationId, ttl_secs: u64) -> Result<(), AuthorityError> {
        let mut ledger = self.lock()?;
        match gateway_renew(&mut ledger, &self.runtime, id, ttl_secs).map_err(shared_error)? {
            GatewayRenewOutcome::Renewed => Ok(()),
            GatewayRenewOutcome::Refused
            | GatewayRenewOutcome::NotFound
            | GatewayRenewOutcome::NotGatewayHold => Err(AuthorityError(
                "the shared hold was not renewed".to_string(),
            )),
        }
    }
}
