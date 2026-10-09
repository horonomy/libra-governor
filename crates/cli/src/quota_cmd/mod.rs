//! `libra-governor quota explain --replay <path> [--as-of <rfc3339>]
//! [--json] [--width <n>]` (HORO-1767).
//!
//! Explains a *simulated* pacing/window scenario — never the daemon's
//! live state. This command:
//! - never connects to the daemon's socket,
//! - never opens the real ledger,
//! - never touches the real state directory,
//!
//! because the daemon has no live pacing-admission authority at all yet
//! (HORO-1727 has not resolved that architecture question — see
//! `crates/domain/src/pacing/mod.rs`'s own module docs and
//! `docs/adr/0016-sustain-burst-pacing.md`). Its only inputs are the
//! `--replay` fixture file and `--as-of`; see `tests/quota_explain.rs`'s
//! `ac4_quota_explain_never_touches_home_or_state_dir` test, which
//! asserts this with an actual file-hash/emptiness comparison rather
//! than "didn't crash".
//!
//! The scope is deliberately one level above `libra_governor_domain::pacing`:
//! this module owns every output type and every honesty rule about what
//! a value may and may not claim (see `view.rs`'s own docs) — it never
//! passes a domain enum straight through to JSON/text.

#[cfg(test)]
mod fixtures_gen;
mod input;
mod render;
mod view;

use std::collections::BTreeSet;
use std::path::Path;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_domain::pacing::simulate::simulate_until;
use libra_governor_domain::pacing::step::Scenario;
use libra_governor_domain::pacing::view::{evaluate_at, task_actuals};
use libra_governor_domain::pacing::PacingPreference;
use libra_governor_domain::{QuotaSubject, WindowState};

const USAGE: &str = "Usage:\n  libra-governor quota explain --replay <path> [--as-of <rfc3339>] [--json] [--width <n>]";

/// Parsed `quota explain` invocation.
#[derive(Debug)]
struct Args {
    replay_path: String,
    as_of: Option<String>,
    json: bool,
    width: usize,
}

const DEFAULT_WIDTH: usize = 100;

fn parse(args: &[String]) -> Result<Args, String> {
    let mut replay_path = None;
    let mut as_of = None;
    let mut json = false;
    let mut width = DEFAULT_WIDTH;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--replay" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "--replay requires a path argument".to_string())?;
                replay_path = Some(value.clone());
                i += 2;
            }
            "--as-of" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "--as-of requires an RFC3339 timestamp argument".to_string())?;
                as_of = Some(value.clone());
                i += 2;
            }
            "--width" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| "--width requires a numeric argument".to_string())?;
                width = value
                    .parse::<usize>()
                    .map_err(|_| format!("--width value {value:?} is not a valid number"))?;
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            other => return Err(format!("unrecognized argument {other:?}")),
        }
    }

    let replay_path = replay_path.ok_or_else(|| "--replay <path> is required".to_string())?;
    Ok(Args {
        replay_path,
        as_of,
        json,
        width,
    })
}

/// Entry point for `["quota", "explain", ...]`. Returns the process exit
/// code; `main.rs` is responsible for calling `std::process::exit` with
/// it. A bare `--help`/`-h`/`help` (anywhere, including as the only
/// argument) prints usage and exits 0 without touching the filesystem
/// beyond nothing at all — never falls through to argument parsing.
pub fn run(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "--help" | "-h" | "help"))
    {
        println!("{USAGE}");
        return 0;
    }

    let parsed = match parse(args) {
        Ok(p) => p,
        Err(message) => {
            eprintln!("libra-governor quota explain: {message}\n\n{USAGE}");
            return 2;
        }
    };

    let as_of = match &parsed.as_of {
        Some(raw) => match OffsetDateTime::parse(raw, &Rfc3339) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("libra-governor quota explain: invalid --as-of value {raw:?}: {e}");
                return 2;
            }
        },
        None => OffsetDateTime::now_utc(),
    };

    let input = match input::load(Path::new(&parsed.replay_path)) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("libra-governor quota explain: {e}");
            return 1;
        }
    };

    let explanation = build_explanation(&input, as_of);

    if parsed.json {
        println!("{}", render::render_json(&explanation));
    } else {
        print!("{}", render::render_human(&explanation, parsed.width));
    }
    0
}

/// Assembles the full [`view::Explanation`] from a validated [`input::Input`]
/// as of `as_of`. The one place this command's domain-facing logic
/// lives — everything above calls this, and every test that wants to
/// check a specific scenario's rendering calls it directly rather than
/// going through the process boundary.
fn build_explanation(input: &input::Input, as_of: OffsetDateTime) -> view::Explanation {
    let scenario = Scenario {
        tasks: &input.tasks,
        windows: &input.windows,
        contract: input.contract.as_ref(),
        preference: input.preference.clone(),
    };

    let (state, trace, tick_budget_exhausted) =
        simulate_until(&input.events, &input.policy, &scenario, Some(as_of));

    let evaluations = evaluate_at(&state, &scenario, &input.snapshots, as_of);
    let actuals = task_actuals(&state);

    let any_gauge_window = evaluations
        .iter()
        .any(|e| matches!(e.state, WindowState::Gauge(_)));
    let any_indeterminate_window = evaluations.iter().any(|e| {
        matches!(
            e.blocking,
            libra_governor_domain::BlockingStatus::Indeterminate(_)
        )
    });

    let principals: BTreeSet<&str> = input
        .tasks
        .tasks()
        .iter()
        .map(|t| t.principal.0.as_str())
        .collect();
    let multi_principal = principals.len() > 1;

    let mut host = Vec::new();
    let mut principal = Vec::new();
    for (window, eval) in input.windows.iter().zip(evaluations.iter()) {
        let subject = &window.scope().subject;
        let mut wv = view::window_view(eval, subject, window.unit());
        if matches!(subject, QuotaSubject::Principal(_)) && multi_principal {
            wv = view::redact_for_unattributed_principal(wv);
        }
        match view::pool_scope(subject) {
            view::PoolScope::Host => host.push(wv),
            view::PoolScope::Principal => principal.push(wv),
        }
    }

    let last_tick_proposals: &[libra_governor_domain::pacing::Proposal] = trace
        .last()
        .map(|(_, proposals)| proposals.as_slice())
        .unwrap_or(&[]);
    let resolution = view::resolve_next(
        state.pending_timer(),
        last_tick_proposals,
        as_of,
        any_gauge_window,
        any_indeterminate_window,
        tick_budget_exhausted,
    );

    let configured_cap: u16 = match &input.preference {
        PacingPreference::Burst { max_fanout, .. } => *max_fanout,
        PacingPreference::Sustain { .. } => 1,
    };

    let tasks = input
        .tasks
        .tasks()
        .iter()
        .map(|task| {
            let (hold, actual_basis) = actuals.get(&task.id).cloned().unwrap_or((None, None));
            let completed = state.completed().contains(&task.id);
            view::task_view(task, hold.as_ref(), actual_basis, completed, &input.policy)
        })
        .collect();

    view::Explanation {
        schema_version: view::EXPLANATION_SCHEMA_VERSION,
        as_of: view::rfc3339(as_of),
        mode: view::Simulated::new(view::mode_view(&input.preference)),
        binding_window: view::Simulated::new(resolution.binding_window),
        next_safe_action: view::Simulated::new(resolution.next),
        active_tasks: state.active_count(),
        configured_cap,
        host,
        principal,
        tasks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_requires_replay_path() {
        let err = parse(&["--json".to_string()]).unwrap_err();
        assert!(err.contains("--replay"));
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let err = parse(&[
            "--replay".to_string(),
            "x".to_string(),
            "--bogus".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("--bogus"));
    }

    #[test]
    fn parse_accepts_every_flag() {
        let args = parse(&[
            "--replay".to_string(),
            "/tmp/x.json".to_string(),
            "--as-of".to_string(),
            "2024-01-01T00:00:00Z".to_string(),
            "--json".to_string(),
            "--width".to_string(),
            "40".to_string(),
        ])
        .expect("all flags recognized");
        assert_eq!(args.replay_path, "/tmp/x.json");
        assert_eq!(args.as_of.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert!(args.json);
        assert_eq!(args.width, 40);
    }
}
