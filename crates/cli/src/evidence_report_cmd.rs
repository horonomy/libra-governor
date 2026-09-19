//! `libra-governor evidence-report` (HORO-1154) — a manual, opt-in,
//! local-only self-report export tool. NOT telemetry: nothing here runs
//! unless the evaluator explicitly invokes it, and nothing here ever
//! makes a network call — see the module-level "no network call, ever"
//! guarantee below.
//!
//! # What this is for
//!
//! HORO-1154 asks whether the founder should invest further in
//! team/shared-policy/Codex infrastructure, and that decision needs real
//! behavioral evidence from real external power users. This tool cannot
//! recruit those users or manufacture that evidence — it can only give a
//! consenting evaluator a way to package their own local, coarse,
//! privacy-safe usage signals plus their own typed qualitative answers
//! into one file they *manually* decide whether to send back.
//!
//! # Two subcommands
//!
//! - `evidence-report consent` — records explicit opt-in as a durable,
//!   timestamped local marker file. Required before anything else here
//!   will run.
//! - `evidence-report` — refuses without a consent marker; with one,
//!   aggregates coarse local ledger signals (see
//!   `libra_governor_ledger::query::EvidenceAggregates`), prompts the
//!   evaluator for the ticket's own open-ended questions, and writes one
//!   JSON and one Markdown file locally. Prints the file paths and does
//!   nothing else — the evaluator reviews the files and decides whether
//!   to send them anywhere.
//!
//! # No network call, ever
//!
//! This module makes zero HTTP/socket-to-a-remote-host calls. The only
//! I/O here is: reading `~/.claude/settings.json` (local), opening the
//! local SQLite ledger (local), reading/writing files under the local
//! state directory, and reading `stdin`/writing `stdout`. Grep this file
//! for `TcpStream`, `reqwest`, `http`, or `ureq` and you will find none —
//! `crates/cli/tests/evidence_report_privacy.rs` also asserts this by
//! injecting a prompt/path nonce into the real ledger, generating a real
//! export, and grepping the written files for it.
//!
//! # What is never collected
//!
//! No prompt text, no file path, no source code, no tool-call argument,
//! no tool output. [`EvidenceAggregates`][libra_governor_ledger::EvidenceAggregates]
//! is structurally incapable of carrying any of that — every field is a
//! count, a timestamp, or a parsed admission-outcome tag. The qualitative
//! answers are exactly what the evaluator chooses to type in response to
//! this tool's own prompts — genuinely evaluator-authored content, never
//! inferred or fabricated.
//!
//! # What "bypass" honestly can and cannot mean here
//!
//! This tool can report whether Claude Code's `~/.claude/settings.json`
//! currently has Libra's hooks/statusline wired in (a real, observable
//! fact). It **cannot** detect whether a user ran Claude Code with the
//! hooks temporarily removed, ran a different tool entirely for a given
//! session, or otherwise worked around the hooks while they were wired —
//! that is fundamentally invisible from inside this product, and this
//! tool says so explicitly rather than inventing a signal for it.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use libra_governor_ledger::{EvidenceAggregates, LedgerStore};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::claude_settings;

/// Consent marker filename inside the daemon's state directory.
pub const CONSENT_FILE_NAME: &str = "evidence_consent.json";
/// Subdirectory (inside the state directory) that finished reports are
/// written to.
pub const REPORTS_DIR_NAME: &str = "evidence-reports";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ConsentMarker {
    /// RFC 3339 timestamp of when consent was recorded.
    consented_at: String,
    /// This binary's version at the time consent was recorded, so a
    /// later report can note if the tool changed since consent.
    tool_version: String,
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

fn state_dir_or_exit(command: &str) -> PathBuf {
    match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor {command}: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// `libra-governor evidence-report consent` — records explicit opt-in.
pub fn run_consent() {
    let state_dir = state_dir_or_exit("evidence-report consent");
    let path = state_dir.join(CONSENT_FILE_NAME);
    let marker = ConsentMarker {
        consented_at: now_rfc3339(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let json = match serde_json::to_string_pretty(&marker) {
        Ok(j) => j,
        Err(e) => {
            eprintln!(
                "libra-governor evidence-report consent: could not serialize consent marker: {e}"
            );
            std::process::exit(1);
        }
    };
    if let Err(e) = write_owner_only(&path, json.as_bytes()) {
        eprintln!(
            "libra-governor evidence-report consent: could not write {}: {e}",
            path.display()
        );
        std::process::exit(1);
    }
    println!(
        "Consent recorded at {} ({}).\n\
         You can now run `libra-governor evidence-report` to generate a local export.\n\
         Nothing is transmitted anywhere by this command or by `evidence-report` itself \
         — you decide if and when to send the exported file to anyone.",
        path.display(),
        marker.consented_at
    );
}

fn read_consent(path: &Path) -> Option<ConsentMarker> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The evaluator's own typed answers to the ticket's open-ended
/// questions (HORO-1154). Every field here is exactly what the
/// evaluator typed — never inferred, never defaulted to a fabricated
/// value. An evaluator who presses Enter with no answer gets an empty
/// string, reported as such, not silently omitted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct QualitativeAnswers {
    pub perceived_friction: String,
    pub would_route_more_work_through_it: String,
    pub team_or_codex_demand: String,
    pub willingness_to_pay_signal: String,
    pub other_notes: String,
}

const QUESTIONS: &[(&str, &str)] = &[
    (
        "perceived_friction",
        "How did admission/preflight and replan interruptions feel in practice — \
         genuinely useful, mostly ignorable, or actual friction you worked around?",
    ),
    (
        "would_route_more_work_through_it",
        "Would you route MORE of your real work through Libra than you have so far, \
         and why or why not?",
    ),
    (
        "team_or_codex_demand",
        "Is there real pull from your team for shared/team policy, or from your own \
         workflow for Codex (or another agent) support? What would that need to look \
         like to matter?",
    ),
    (
        "willingness_to_pay_signal",
        "If this graduated from a free developer preview to a paid product, would you \
         (or your team) pay for it — and roughly what would justify that to you?",
    ),
    (
        "other_notes",
        "Anything else worth the founder knowing — a specific incident, a specific \
         annoyance, a specific thing that worked better than expected?",
    ),
];

/// Prompts (on stdout) and reads (from stdin) one free-text line per
/// question, in order. Works both interactively (a real terminal) and
/// non-interactively (piped stdin, e.g. in a script or test) — the only
/// difference is whether the prompt text is meaningfully visible before
/// the answer arrives. EOF or a read error yields an empty answer for
/// the remaining questions rather than aborting the whole report.
fn collect_qualitative_answers() -> QualitativeAnswers {
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        println!(
            "\nA few open-ended questions (per HORO-1154) — type a free-text answer and \
             press Enter. Leave blank and press Enter to skip a question.\n"
        );
    }
    let mut answers = std::collections::HashMap::new();
    for (key, question) in QUESTIONS {
        if interactive {
            print!("{question}\n> ");
            let _ = std::io::stdout().flush();
        }
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        let answer = if read.is_ok() {
            line.trim().to_string()
        } else {
            String::new()
        };
        answers.insert(*key, answer);
    }
    QualitativeAnswers {
        perceived_friction: answers.remove("perceived_friction").unwrap_or_default(),
        would_route_more_work_through_it: answers
            .remove("would_route_more_work_through_it")
            .unwrap_or_default(),
        team_or_codex_demand: answers.remove("team_or_codex_demand").unwrap_or_default(),
        willingness_to_pay_signal: answers
            .remove("willingness_to_pay_signal")
            .unwrap_or_default(),
        other_notes: answers.remove("other_notes").unwrap_or_default(),
    }
}

/// The full local export written by `evidence-report`. Every field is
/// either a coarse count/timestamp ([`EvidenceAggregates`]), a wiring
/// fact ([`hooks_wired`]/[`statusline_wired`]), or evaluator-typed free
/// text ([`QualitativeAnswers`]) — nothing that could carry prompt or
/// source content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct EvidenceReport {
    pub generated_at: String,
    pub consent: ConsentMarker,
    pub ledger_present: bool,
    pub aggregates: EvidenceAggregatesDto,
    pub hooks_wired: [bool; 3],
    pub statusline_wired: bool,
    pub bypass_detection_note: String,
    pub daemon_restart_tracking_note: String,
    pub qualitative_answers: QualitativeAnswers,
}

/// Serializable mirror of [`EvidenceAggregates`] (that type itself is
/// not `Serialize`/`Deserialize` — it lives in the ledger crate and has
/// no reason to depend on serde for internal use — so this CLI-side DTO
/// re-states the same fields for the JSON export).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct EvidenceAggregatesDto {
    pub task_count: u64,
    pub preflight_count: u64,
    pub replan_count: u64,
    pub completed_task_count: u64,
    pub completed_execution_count: u64,
    pub admission_admit_count: u64,
    pub admission_deny_count: u64,
    pub admission_approval_required_count: u64,
    pub admission_unrecorded_count: u64,
    pub earliest_task_created_at: Option<String>,
}

impl From<EvidenceAggregates> for EvidenceAggregatesDto {
    fn from(a: EvidenceAggregates) -> Self {
        Self {
            task_count: a.task_count,
            preflight_count: a.preflight_count,
            replan_count: a.replan_count,
            completed_task_count: a.completed_task_count,
            completed_execution_count: a.completed_execution_count,
            admission_admit_count: a.admission_admit_count,
            admission_deny_count: a.admission_deny_count,
            admission_approval_required_count: a.admission_approval_required_count,
            admission_unrecorded_count: a.admission_unrecorded_count,
            earliest_task_created_at: a.earliest_task_created_at,
        }
    }
}

const BYPASS_DETECTION_NOTE: &str = "Libra can only report whether ~/.claude/settings.json \
     currently has its hooks/statusline wired in. It CANNOT detect whether hooks were \
     temporarily removed, a session ran without them, or a user otherwise worked around them \
     while wired — that is fundamentally invisible from inside this product. No fabricated \
     bypass-incident count is reported here.";

const DAEMON_RESTART_TRACKING_NOTE: &str = "This daemon build does not track process uptime or \
     restart count anywhere in its ledger or state files, so no such number is reported here.";

/// `libra-governor evidence-report` — refuses without consent; with
/// consent, builds and writes the local export.
pub fn run() {
    let state_dir = state_dir_or_exit("evidence-report");
    let consent_path = state_dir.join(CONSENT_FILE_NAME);
    let Some(consent) = read_consent(&consent_path) else {
        eprintln!(
            "libra-governor evidence-report: no consent on record at {}.\n\
             This tool refuses to run without your explicit opt-in — nothing is collected \
             or exported until you consent.\n\
             Run `libra-governor evidence-report consent` first, then re-run this command.",
            consent_path.display()
        );
        std::process::exit(1);
    };

    let ledger_path = match libra_governor_daemon::paths::ledger_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor evidence-report: could not resolve ledger path: {e}");
            std::process::exit(1);
        }
    };
    let ledger_present = ledger_path.exists();
    let aggregates: EvidenceAggregatesDto = if ledger_present {
        match LedgerStore::open(&ledger_path).and_then(|s| s.evidence_aggregates()) {
            Ok(a) => a.into(),
            Err(e) => {
                eprintln!(
                    "libra-governor evidence-report: could not read local ledger at {}: {e}",
                    ledger_path.display()
                );
                std::process::exit(1);
            }
        }
    } else {
        EvidenceAggregatesDto::default()
    };

    let (hooks_wired, statusline_wired) = match claude_settings::settings_path() {
        Ok(path) => {
            let inspection = claude_settings::inspect(&path);
            (inspection.hooks_wired, inspection.statusline_wired)
        }
        Err(_) => ([false; 3], false),
    };

    let qualitative_answers = collect_qualitative_answers();

    let report = EvidenceReport {
        generated_at: now_rfc3339(),
        consent,
        ledger_present,
        aggregates,
        hooks_wired,
        statusline_wired,
        bypass_detection_note: BYPASS_DETECTION_NOTE.to_string(),
        daemon_restart_tracking_note: DAEMON_RESTART_TRACKING_NOTE.to_string(),
        qualitative_answers,
    };

    let reports_dir = state_dir.join(REPORTS_DIR_NAME);
    if let Err(e) = std::fs::create_dir_all(&reports_dir) {
        eprintln!(
            "libra-governor evidence-report: could not create {}: {e}",
            reports_dir.display()
        );
        std::process::exit(1);
    }

    let stamp = report.generated_at.replace([':', '.'], "-");
    let json_path = reports_dir.join(format!("{stamp}.json"));
    let md_path = reports_dir.join(format!("{stamp}.md"));

    let json = match serde_json::to_string_pretty(&report) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("libra-governor evidence-report: could not serialize report: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = write_owner_only(&json_path, json.as_bytes()) {
        eprintln!(
            "libra-governor evidence-report: could not write {}: {e}",
            json_path.display()
        );
        std::process::exit(1);
    }

    let markdown = render_markdown(&report);
    if let Err(e) = write_owner_only(&md_path, markdown.as_bytes()) {
        eprintln!(
            "libra-governor evidence-report: could not write {}: {e}",
            md_path.display()
        );
        std::process::exit(1);
    }

    println!(
        "\nLocal evidence export written:\n  {}\n  {}\n\n\
         Nothing was sent anywhere. Review both files, then decide yourself whether and how \
         to send them back (e.g. attach to an email).",
        json_path.display(),
        md_path.display()
    );
}

fn render_markdown(report: &EvidenceReport) -> String {
    let a = &report.aggregates;
    let q = &report.qualitative_answers;
    format!(
        "# Libra Governor — Evidence Export\n\n\
         Generated: {generated_at}\n\
         Consent recorded: {consented_at} (tool v{tool_version})\n\n\
         ## Local behavioral aggregates\n\n\
         - Ledger present: {ledger_present}\n\
         - Earliest recorded task: {earliest}\n\
         - Tasks: {task_count}\n\
         - Preflights: {preflight_count} (admit={admit}, deny={deny}, approval_required={approval}, unrecorded={unrecorded})\n\
         - Replans: {replan_count}\n\
         - Completed tasks: {completed_task_count} ({completed_execution_count} total execution receipts)\n\n\
         ## Claude Code integration wiring\n\n\
         - Hooks wired [UserPromptSubmit, PostToolUse, Stop]: {hooks_wired:?}\n\
         - Statusline wired: {statusline_wired}\n\n\
         ## Honesty notes\n\n\
         - Bypass detection: {bypass_note}\n\
         - Daemon uptime/restart tracking: {restart_note}\n\n\
         ## Qualitative answers (evaluator-authored)\n\n\
         **Perceived friction**\n\n{friction}\n\n\
         **Would route more work through it**\n\n{route_more}\n\n\
         **Team / Codex demand**\n\n{demand}\n\n\
         **Willingness to pay**\n\n{wtp}\n\n\
         **Other notes**\n\n{other}\n",
        generated_at = report.generated_at,
        consented_at = report.consent.consented_at,
        tool_version = report.consent.tool_version,
        ledger_present = report.ledger_present,
        earliest = a.earliest_task_created_at.as_deref().unwrap_or("(none recorded)"),
        task_count = a.task_count,
        preflight_count = a.preflight_count,
        admit = a.admission_admit_count,
        deny = a.admission_deny_count,
        approval = a.admission_approval_required_count,
        unrecorded = a.admission_unrecorded_count,
        replan_count = a.replan_count,
        completed_task_count = a.completed_task_count,
        completed_execution_count = a.completed_execution_count,
        hooks_wired = report.hooks_wired,
        statusline_wired = report.statusline_wired,
        bypass_note = report.bypass_detection_note,
        restart_note = report.daemon_restart_tracking_note,
        friction = non_empty_or(&q.perceived_friction),
        route_more = non_empty_or(&q.would_route_more_work_through_it),
        demand = non_empty_or(&q.team_or_codex_demand),
        wtp = non_empty_or(&q.willingness_to_pay_signal),
        other = non_empty_or(&q.other_notes),
    )
}

fn non_empty_or(s: &str) -> &str {
    if s.is_empty() {
        "(no answer provided)"
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_marker_round_trips_through_json() {
        let marker = ConsentMarker {
            consented_at: "2026-09-19T00:00:00Z".to_string(),
            tool_version: "0.0.1".to_string(),
        };
        let json = serde_json::to_string(&marker).unwrap();
        let parsed: ConsentMarker = serde_json::from_str(&json).unwrap();
        assert_eq!(marker, parsed);
    }

    #[test]
    fn a_missing_consent_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_consent(&dir.path().join(CONSENT_FILE_NAME)).is_none());
    }

    #[test]
    fn a_corrupt_consent_file_reads_as_none_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONSENT_FILE_NAME);
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_consent(&path).is_none());
    }

    #[test]
    fn a_valid_consent_file_reads_back_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONSENT_FILE_NAME);
        let marker = ConsentMarker {
            consented_at: "2026-09-19T00:00:00Z".to_string(),
            tool_version: "0.0.1".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert_eq!(read_consent(&path), Some(marker));
    }

    #[test]
    fn markdown_render_never_panics_and_includes_every_section() {
        let report = EvidenceReport {
            generated_at: "2026-09-19T00:00:00Z".to_string(),
            consent: ConsentMarker {
                consented_at: "2026-09-18T00:00:00Z".to_string(),
                tool_version: "0.0.1".to_string(),
            },
            ledger_present: true,
            aggregates: EvidenceAggregatesDto {
                task_count: 3,
                preflight_count: 3,
                replan_count: 1,
                completed_task_count: 2,
                completed_execution_count: 2,
                admission_admit_count: 2,
                admission_deny_count: 1,
                admission_approval_required_count: 0,
                admission_unrecorded_count: 0,
                earliest_task_created_at: Some("2026-09-01T00:00:00Z".to_string()),
            },
            hooks_wired: [true, true, true],
            statusline_wired: true,
            bypass_detection_note: BYPASS_DETECTION_NOTE.to_string(),
            daemon_restart_tracking_note: DAEMON_RESTART_TRACKING_NOTE.to_string(),
            qualitative_answers: QualitativeAnswers {
                perceived_friction: "a bit annoying at first".to_string(),
                would_route_more_work_through_it: "yes".to_string(),
                team_or_codex_demand: "".to_string(),
                willingness_to_pay_signal: "maybe $20/mo".to_string(),
                other_notes: "".to_string(),
            },
        };
        let md = render_markdown(&report);
        assert!(md.contains("Evidence Export"));
        assert!(md.contains("a bit annoying at first"));
        assert!(md.contains("maybe $20/mo"));
        assert!(md.contains("(no answer provided)"));
        assert!(md.contains(BYPASS_DETECTION_NOTE));
    }
}
