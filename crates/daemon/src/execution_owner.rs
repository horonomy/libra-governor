//! Authoritative execution-to-task selection and effect admission.
//! All SQL effects and replay identity share one durable transaction. The
//! native context is a claim by a local socket client, not plugin authentication.
use crate::server::{DaemonConfig, DaemonError};
use libra_governor_domain::{
    AgentKind, AssociationUnavailable as Gap, ExecutionPosition, EXECUTION_ASSOCIATION_VERSION,
};
use libra_governor_ledger::{ExecutionLookup, LedgerStore};
use libra_governor_protocol::{
    ExecutionEffect, ExecutionOperation, ExecutionOwnerOutcome as Outcome, ExecutionOwnerRequest,
    FinalizeOutcome,
};

fn unavailable(reason: Gap) -> Outcome {
    Outcome::Unavailable { reason }
}

pub(crate) fn handle(
    event: &ExecutionOwnerRequest,
    ledger: &mut LedgerStore,
    config: &DaemonConfig,
) -> Result<Outcome, DaemonError> {
    if event.association_version != EXECUTION_ASSOCIATION_VERSION {
        return Ok(unavailable(Gap::Unsupported));
    }
    // Equality includes observation/event IDs here: these two copies must
    // describe the same acquisition, rather than a later cache lookup.
    if event.identity != event.native_context.identity {
        return Ok(unavailable(Gap::Ambiguous));
    }
    let position = match ExecutionPosition::from_identity(&event.identity) {
        Ok(position) => position,
        Err(reason) => return Ok(unavailable(reason)),
    };
    let operation = &event.native_context.operation;
    let (tag, native_ref) = match operation {
        ExecutionOperation::Prompt { .. } => ("prompt", position.turn.as_str()),
        ExecutionOperation::Tool {
            native_call_id,
            tool_name,
        } => {
            if native_call_id.is_empty() || tool_name.is_empty() {
                return Ok(unavailable(Gap::Unsupported));
            }
            ("tool", native_call_id.as_str())
        }
        ExecutionOperation::Stop {
            transcript_path, ..
        } => {
            // The session-window reader (`crate::usage::observe_transcript_window`)
            // cannot tell one lane's tokens from a sibling's, so offering it
            // here would cross-associate usage. HORO-1863 adds a turn-scoped
            // reader (`crate::usage::observe_codex_turn`) that sums only
            // this turn's own records, but Codex's rollout shape is the
            // only one it understands -- every other identity still
            // refuses `Some(_)` outright, unchanged from before.
            if transcript_path.is_some()
                && event.identity.tool_provider() != AgentKind::Codex.as_tool_provider()
            {
                return Ok(unavailable(Gap::Unsupported));
            }
            ("stop", position.turn.as_str())
        }
        ExecutionOperation::Query {} => ("query", ""),
    };
    if tag != "query"
        && (config.gateway.is_some()
            || config.extensions.as_ref().is_some_and(|e| {
                e.business_context_provider.is_some() || e.policy_webhook.is_some()
            }))
    {
        return Ok(unavailable(Gap::Unsupported));
    }
    // Recon is bounded and pure with respect to the ledger. It runs before
    // the write lock; the association and replay are rechecked under lock.
    let prepared = match operation {
        ExecutionOperation::Prompt { task_hint, cwd, .. } => Some(crate::recon::run_recon(
            cwd,
            task_hint,
            &config.recon_budget,
        )),
        _ => None,
    };
    // Measured the same way recon is: bounded, pure with respect to the
    // ledger, and finished before the write lock is taken (HORO-1863). A
    // slow-to-open or adversarial `transcript_path` (e.g. a FIFO with no
    // writer) must never be able to hold the database transaction open --
    // see the comment above `with_immediate_transaction` on recon staying
    // bounded and pure. Only ever `Some` for a Codex identity with
    // `transcript_path: Some(_)`; the arm above already refused every
    // other combination.
    let codex_turn_observation = match operation {
        ExecutionOperation::Stop {
            transcript_path: Some(path),
            ..
        } if event.identity.tool_provider() == AgentKind::Codex.as_tool_provider() => {
            Some(crate::usage::observe_codex_turn(
                std::path::Path::new(path),
                // `ExecutionPosition::from_identity` already required
                // `provider_session_id` to be present and non-empty to
                // reach this point.
                event.identity.provider_session_id().unwrap_or_default(),
                position.turn.as_str(),
                crate::usage::DEFAULT_SCAN_CAP_BYTES,
            ))
        }
        _ => None,
    };
    if tag != "query" {
        crate::server::prepare_execution_extensions(config);
    }
    ledger.with_immediate_transaction(|ledger| {
        if !ledger.execution_lane_context_matches(&position)? {
            return Ok(unavailable(Gap::Ambiguous));
        }
        let known_state = ledger.execution_turn_state(&position)?;
        if known_state.is_some() && !ledger.execution_position_matches(&position)? {
            return Ok(unavailable(Gap::Ambiguous));
        }
        let resolved = ledger.resolve_execution(&position)?;
        // A superseded turn remains stale even if its older effect succeeded.
        if known_state.as_deref() == Some("superseded") {
            return Ok(unavailable(Gap::Stale));
        }
        if matches!(resolved, ExecutionLookup::Unavailable(Gap::Ambiguous)) {
            return Ok(unavailable(Gap::Ambiguous));
        }
        if tag != "query" {
            if let Some((turn, target)) = ledger.execution_replay(&position, tag, native_ref)? {
                if turn != position.turn {
                    return Ok(unavailable(Gap::ReplayConflict));
                }
                return Ok(Outcome::Duplicate { target });
            }
        }
        if let ExecutionOperation::Query {} = operation {
            return match resolved {
                ExecutionLookup::Found(target) => Ok(Outcome::Resolved {
                    budget: ledger.budget_snapshot(target.task_id)?,
                    target,
                }),
                ExecutionLookup::Unavailable(reason) => Ok(unavailable(reason)),
            };
        }
        let session = LedgerStore::execution_lane_session(&position);
        let (target, effect) = match operation {
            ExecutionOperation::Prompt {
                task_hint,
                cwd,
                supersedes_turn,
            } => {
                // Same turn with no replay row is not permission to invent a
                // second plan. It indicates incompatible/incomplete state.
                if known_state.is_some() {
                    return Ok(unavailable(Gap::Stale));
                }
                if !ledger.execution_lane_exists(&position)?
                    && ledger.task_id_for_session(&session)?.is_some()
                {
                    return Ok(unavailable(Gap::Ambiguous));
                }
                let current_turn = ledger.execution_current_turn(&position)?;
                match (current_turn.as_deref(), supersedes_turn.as_deref()) {
                    (None, None) => {}
                    (Some(current), Some(expected)) if current == expected => {}
                    (None, Some(_)) => return Ok(unavailable(Gap::Missing)),
                    (Some(current), None) => {
                        // HORO-1714 decision B (2026-10-10): a narrowly
                        // scoped owner-managed succession with no native
                        // predecessor field is permitted ONLY when the
                        // lane's previous turn is confirmed finalized.
                        // Never inferred from timestamps, cwd, a
                        // latest-session lookup, or a hook-local cache --
                        // this is the one durable fact a finalized `Stop`
                        // already recorded.
                        if ledger
                            .execution_turn_state_for(&position.lane, current)?
                            .as_deref()
                            != Some("finalized")
                        {
                            return Ok(unavailable(Gap::Ambiguous));
                        }
                    }
                    (Some(_), Some(_)) => return Ok(unavailable(Gap::Stale)),
                }
                // Expiry is idempotent maintenance, but must also roll back if
                // this owner admission fails. Invalid/CAS-refused input reaches
                // no economic mutation at all.
                ledger.expire_stale_reservations(time::OffsetDateTime::now_utc())?;
                let result = crate::server::handle_preflight_prepared(
                    task_hint,
                    cwd,
                    &session,
                    prepared.expect("prompt recon prepared"),
                    ledger,
                    config,
                )?;
                ledger.bind_execution_turn(
                    &position,
                    &event.identity,
                    result.task_id,
                    result.plan_id,
                    current_turn.as_deref(),
                )?;
                let target = match ledger.resolve_execution(&position)? {
                    ExecutionLookup::Found(target) => target,
                    ExecutionLookup::Unavailable(_) => {
                        return Err(
                            libra_governor_ledger::LedgerError::InvalidExecutionAssociation.into(),
                        )
                    }
                };
                (
                    target,
                    ExecutionEffect::Prompt {
                        result: Box::new(result),
                    },
                )
            }
            ExecutionOperation::Tool { tool_name, .. } => {
                let ExecutionLookup::Found(_) = resolved else {
                    let ExecutionLookup::Unavailable(reason) = resolved else {
                        unreachable!()
                    };
                    return Ok(unavailable(reason));
                };
                let mut summary = None;
                crate::server::handle_tool_invoked(
                    &session,
                    tool_name,
                    ledger,
                    &mut summary,
                    config,
                )?;
                let target = match ledger.resolve_execution(&position)? {
                    ExecutionLookup::Found(target) => target,
                    ExecutionLookup::Unavailable(_) => {
                        return Err(
                            libra_governor_ledger::LedgerError::InvalidExecutionAssociation.into(),
                        )
                    }
                };
                (target, ExecutionEffect::Tool)
            }
            ExecutionOperation::Stop { model, .. } => {
                let target = match resolved {
                    ExecutionLookup::Found(target) => target,
                    ExecutionLookup::Unavailable(reason) => return Ok(unavailable(reason)),
                };
                let mut summary = None;
                // Replay/duplicate-detection and staleness were already
                // checked above, under this same lock, before this arm
                // runs -- a duplicate Stop never reaches a second
                // measurement or a second effect.
                let result = crate::server::finalize_with_usage(
                    &session,
                    model.clone(),
                    Some(event.identity.tool_provider().to_owned()),
                    ledger,
                    &mut summary,
                    config,
                    move |_usage_window_start| {
                        // Already fully computed above, before this
                        // transaction opened -- this closure never touches
                        // the transcript path itself.
                        codex_turn_observation.unwrap_or(
                            crate::usage::UsageObservation::Unavailable(
                                crate::usage::UsageUnavailable::NoTranscriptPath,
                            ),
                        )
                    },
                )?;
                if !matches!(result, FinalizeOutcome::Finalized(_)) {
                    return Err(
                        libra_governor_ledger::LedgerError::InvalidExecutionAssociation.into(),
                    );
                }
                ledger.finalize_execution_turn(&position)?;
                (
                    target,
                    ExecutionEffect::Stop {
                        result: Box::new(result),
                    },
                )
            }
            ExecutionOperation::Query {} => unreachable!(),
        };
        ledger.record_execution_replay(&position, tag, native_ref, &target)?;
        Ok(Outcome::Applied {
            target,
            effect: Box::new(effect),
        })
    })
}
