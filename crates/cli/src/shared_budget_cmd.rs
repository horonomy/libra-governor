//! `libra-governor shared-budget <init|status|prune|export>`: first-run
//! configuration (HORO-2248), a read-only status view (HORO-2217), retention
//! and a local-only export (HORO-2219) for the local monthly shared budget
//! (ADR-0026).
//!
//! `init` is the only subcommand that writes `shared-budget.json`. It builds
//! the file's bytes, proves the daemon's own parser accepts them, and only
//! then puts them in place atomically (owner-only, never half-written).
//!
//! `status` asks a running daemon (never starts one) and, with no
//! `shared-budget.json`, prints "not configured" without connecting or
//! writing anything; see [`crate::shared_budget_view`].
//!
//! Both subcommands open the ledger file directly, like the other offline
//! commands: a 5 s `busy_timeout`, one short `BEGIN IMMEDIATE` transaction
//! per pool for `prune --apply`, so they are safe to run while the daemon is
//! up. A dry-run prune and `export` open the ledger **read-only** (no
//! migration, no journal-mode change, no WAL checkpoint) and refuse a schema
//! that is not current; only `prune --apply` opens it for writing. Neither
//! creates a missing ledger and neither contacts the daemon.
//!
//! Export ids are pseudonymised, not anonymised: a 32-bit unsalted digest is
//! guessable for low-entropy ids and linkable across exports, so a redacted
//! export is not safe to publish.
//!
//! Local only: nothing here opens a socket or spawns a process. `export`
//! writes one owner-only file and refuses any `--out` that is not a plain
//! local file path. Delivering an export anywhere is a separate, unbuilt
//! workstream (ADR-0026).
//!
//! The group is a flat dispatch on the first argument: a further subcommand
//! is one more arm in [`run`] plus its own function, with no change to the
//! logic below.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::shared_budget_view;
use libra_governor_daemon::shared_budget_file::{LoadedSharedBudget, SharedBudgetFileState};
use libra_governor_domain::{
    calendar_month_bounds, format_local_rfc3339, monthly_pool_alias, redacted_display_id,
    IanaTimeZone, MeteredPoolKey, PrincipalId, QuotaUnit, LOCAL_BUDGET_OWNER,
};
use libra_governor_evidence_adapter::{AdapterConfig, TransportPolicy};
use libra_governor_ledger::{LedgerStore, PoolExport, PruneRequest, RetentionError};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const HELP: &str = "Usage:\n  \
    libra-governor shared-budget init --monthly-usd <amount> --time-zone <IANA> \
    [--turn-hold-usd <amount>] [--per-session-soft-usd <amount>] \
    [--per-agent-soft-usd <amount>] [--force]\n  \
    libra-governor shared-budget status [--json]\n  \
    libra-governor shared-budget prune --keep-months <n> --time-zone <IANA> [--apply] [--json]\n  \
    libra-governor shared-budget export --period <YYYY-MM> [--out <file>] [--time-zone <IANA>] \
    [--raw-ids] [--overwrite]\n\n\
    init     Writes <state-dir>/shared-budget.json to turn the monthly budget on. --monthly-usd is \
    dollars (at most 2 decimals, e.g. 200 or 12.50). --time-zone is required: the budget period is \
    a calendar month in that zone and there is deliberately no UTC default. --turn-hold-usd is what \
    each prompt reserves (default 10; must not exceed the cap). Refuses to replace an existing \
    file unless --force (which replaces the whole file: pricing_overrides, \
    subagent_unmeasured_charge_usd_cents and any soft limit not repeated are dropped, but the \
    previous file is kept as shared-budget.json.bak; the ledger is untouched). The optional \
    per-session and per-agent soft limits are dollars, warn only, and never grant capacity or deny a prompt. A running \
    daemon picks the file up on the next prompt.\n  \
    status   Read-only budget posture from the running daemon (never starts one): config state, \
    zone-local period, cap, settled, held, remaining (negative = overrun), per-executor table \
    reconciled against the pool total, soft-limit warnings (warn-only), flagged settlements and \
    clock anomaly. With no shared-budget.json it prints \"not configured\" and does nothing else. \
    Exits 0 even for a discrepancy, clock anomaly or invalid config (doctor is the failing check). \
    Amounts are Libra-accounted list-price-equivalent USD, never a provider bill or quota.\n  \
    prune   Dry run by default (deletes nothing); deletes only with --apply. Removes closed rows of \
    ended monthly periods older than the last <n> closed months (n >= 1). Export before prune: \
    pruned periods can no longer be exported, and charges for events in or before the newest \
    pruned period are refused by the daemon. --time-zone defaults to <state-dir>/shared-budget.json's zone and \
    must match it when both are given; without that file it is required. JSON stays on stdout; \
    a summary on stderr explains the action and skipped holds (--json suppresses the summary).\n  \
    export  Writes a local JSON file (0600; default under <state-dir>/exports/). Session and \
    agent ids are pseudonymised (32-bit unsalted digest; guessable for low-entropy ids; linkable \
    across exports) unless --raw-ids is given; this is not anonymisation and not safe to \
    publish. Nothing is sent anywhere. --time-zone defaults to the configured zone, else UTC.";

pub const EXPORT_SCHEMA: &str = "libra.shared-budget-export.v1";
pub const PRUNE_SCHEMA: &str = "libra.shared-budget-prune.v1";
const GUARANTEE: &str =
    "cooperative local accounting estimate; not a provider bill and not a hard spending cap";

fn usage_error(message: &str) -> i32 {
    eprintln!("libra-governor shared-budget: {message}\n\n{HELP}");
    2
}

/// Flags parsed from a fixed vocabulary: value flags and boolean switches.
struct Flags {
    values: BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Flags {
    fn parse(args: &[String], value_flags: &[&str], switch_flags: &[&str]) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        let mut switches = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument {arg:?}"));
            };
            if switch_flags.contains(&name) {
                switches.push(name.to_string());
                i += 1;
            } else if value_flags.contains(&name) {
                let Some(value) = args.get(i + 1) else {
                    return Err(format!("--{name} requires a value"));
                };
                if values.insert(name.to_string(), value.clone()).is_some() {
                    return Err(format!("--{name} given more than once"));
                }
                i += 2;
            } else {
                return Err(format!("unknown flag --{name}"));
            }
        }
        Ok(Self { values, switches })
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            println!("{HELP}");
            0
        }
        Some("init") => run_init(&args[1..]),
        Some("status") => run_status(&args[1..]),
        Some("prune") => run_prune(&args[1..]),
        Some("export") => run_export(&args[1..]),
        Some(other) => usage_error(&format!("unknown subcommand {other:?}")),
        None => usage_error("missing subcommand"),
    }
}

fn parse_time_zone(name: &str) -> Result<IanaTimeZone, String> {
    IanaTimeZone::new(name).map_err(|_| format!("unknown IANA time zone {name:?}"))
}

fn ledger_path_or_report(command: &str) -> Result<PathBuf, i32> {
    libra_governor_daemon::paths::ledger_path().map_err(|e| {
        eprintln!("libra-governor shared-budget {command}: could not resolve state dir: {e}");
        1
    })
}

// ---------------------------------------------------------------------
// init
// ---------------------------------------------------------------------

/// Largest monthly cap or turn hold `init` accepts. A guard against a
/// mistyped amount, not a product limit; edit the file for anything larger.
const MAX_INIT_USD_CENTS: u64 = 100_000_000;

/// Dollars (digits, optionally `.` and one or two decimals) -> cents.
/// Rejects signs, exponents, `inf`/`NaN`, a third decimal, zero and anything
/// above [`MAX_INIT_USD_CENTS`]. Integer arithmetic only: no float rounding.
fn parse_usd_cents(flag: &str, raw: &str) -> Result<u64, String> {
    let bad = || {
        format!(
            "{flag} must be a dollar amount with at most 2 decimals (e.g. 200 or 12.50), got {raw:?}"
        )
    };
    let (whole, frac) = match raw.split_once('.') {
        Some((w, f)) => (w, f),
        None => (raw, ""),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let dangling_point = raw.contains('.') && frac.is_empty();
    if whole.is_empty() || dangling_point || !digits(whole) || !digits(frac) || frac.len() > 2 {
        return Err(bad());
    }
    let dollars: u64 = whole.parse().map_err(|_| bad())?;
    let frac_cents: u64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<u64>().map_err(|_| bad())? * 10,
        _ => frac.parse::<u64>().map_err(|_| bad())?,
    };
    let cents = dollars
        .checked_mul(100)
        .and_then(|c| c.checked_add(frac_cents))
        .ok_or_else(bad)?;
    if cents == 0 {
        return Err(format!("{flag} must be greater than 0"));
    }
    if cents > MAX_INIT_USD_CENTS {
        return Err(format!(
            "{flag} {raw} is above the {} sanity limit; edit shared-budget.json by hand for a larger value",
            shared_budget_view::usd_u(MAX_INIT_USD_CENTS)
        ));
    }
    Ok(cents)
}

struct InitRequest {
    monthly_cap_usd_cents: u64,
    time_zone: String,
    turn_hold_usd_cents: Option<u64>,
    per_session_soft_usd_cents: Option<u64>,
    per_agent_soft_usd_cents: Option<u64>,
    force: bool,
}

#[derive(Debug)]
struct InitReport {
    path: PathBuf,
    config: libra_governor_daemon::shared_budget_file::SharedBudgetConfig,
    replaced: bool,
}

/// Serialises the request into the file's bytes. Only the fields the user
/// chose are written, so the daemon's defaults keep applying to the rest.
fn init_bytes(request: &InitRequest) -> Vec<u8> {
    let mut doc = serde_json::Map::new();
    doc.insert("schema_version".into(), json!(1));
    doc.insert(
        "monthly_cap_usd_cents".into(),
        json!(request.monthly_cap_usd_cents),
    );
    doc.insert("time_zone".into(), json!(request.time_zone));
    if let Some(hold) = request.turn_hold_usd_cents {
        doc.insert("turn_hold_usd_cents".into(), json!(hold));
    }
    let mut soft = serde_json::Map::new();
    if let Some(limit) = request.per_session_soft_usd_cents {
        soft.insert("per_session_usd_cents".into(), json!(limit));
    }
    if let Some(limit) = request.per_agent_soft_usd_cents {
        soft.insert("per_agent_usd_cents".into(), json!(limit));
    }
    if !soft.is_empty() {
        doc.insert("soft_limits".into(), Value::Object(soft));
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(doc)).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// A staging file name no earlier run can have left behind: process id,
/// wall-clock nanoseconds and a per-process counter, so a stale temp file
/// from a crashed run never makes `init` fail.
fn unique_temp_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        ".{}.tmp-{}-{nanos}-{}",
        libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Writes `shared-budget.json` into the existing directory `dir`.
///
/// The bytes must parse as a valid configuration with the daemon's own
/// parser before anything is put in place, so an invalid file is never
/// written. They go to an owner-only temp file in `dir` first; without
/// `force` they are then linked into place (fails if anything exists, with
/// no check-then-write race), with `force` they replace a regular file via
/// rename, after the old file is kept as `shared-budget.json.bak`. A reader
/// therefore sees the old file or the new one, never a partial one.
fn init_in(dir: &Path, request: &InitRequest) -> Result<InitReport, String> {
    use libra_governor_daemon::shared_budget_file::{
        parse, SharedBudgetFileState, SHARED_BUDGET_FILE_NAME,
    };

    let bytes = init_bytes(request);
    let config = match parse(&bytes) {
        SharedBudgetFileState::Valid(config) => config,
        SharedBudgetFileState::Invalid(reason) => return Err(init_invalid_message(&reason)),
        SharedBudgetFileState::Absent => return Err("internal error: empty configuration".into()),
    };

    let path = dir.join(SHARED_BUDGET_FILE_NAME);
    let replaced = match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(format!(
                "{} exists and is not a regular file; refusing to touch it",
                path.display()
            ));
        }
        Ok(_) if !request.force => {
            return Err(format!(
                "{} already exists; refusing to overwrite it (pass --force to replace it, or edit \
                 the file by hand)",
                path.display()
            ));
        }
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("could not inspect {}: {e}", path.display())),
    };

    if replaced {
        // A hard link keeps the old inode (and its owner-only mode) alive
        // under the .bak name, and the rename over an existing .bak is
        // atomic. Done before anything is replaced so a failure here leaves
        // the original untouched.
        let link = dir.join(unique_temp_name());
        let backed_up = std::fs::hard_link(&path, &link).and_then(|()| {
            std::fs::rename(&link, dir.join(format!("{SHARED_BUDGET_FILE_NAME}.bak")))
        });
        if let Err(e) = backed_up {
            let _ = std::fs::remove_file(&link);
            return Err(format!("could not back up {}: {e}", path.display()));
        }
    }

    let tmp = dir.join(unique_temp_name());
    let staged = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()));
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write {}: {e}", tmp.display()));
    }
    let placed = if request.force {
        std::fs::rename(&tmp, &path)
    } else {
        std::fs::hard_link(&tmp, &path)
    };
    if !request.force {
        let _ = std::fs::remove_file(&tmp);
    }
    if placed.is_ok() {
        // Best effort: make the new directory entry durable. Failure here
        // does not undo a placed file, so it is ignored.
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&tmp);
        return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "{} already exists; refusing to overwrite it (pass --force to replace it)",
                path.display()
            )
        } else {
            format!("could not put {} in place: {e}", path.display())
        });
    }
    Ok(InitReport {
        path,
        config,
        replaced,
    })
}

/// Turns a parser rejection into advice a first-time user can act on.
fn init_invalid_message(reason: &str) -> String {
    if reason.starts_with("turn_hold_usd_cents") {
        format!(
            "{reason}. Each prompt reserves a hold (default $10.00) that must fit inside the cap: \
             pass a smaller --turn-hold-usd or a larger --monthly-usd"
        )
    } else {
        reason.to_string()
    }
}

fn run_init(args: &[String]) -> i32 {
    let flags = match Flags::parse(
        args,
        &[
            "monthly-usd",
            "time-zone",
            "turn-hold-usd",
            "per-session-soft-usd",
            "per-agent-soft-usd",
        ],
        &["force"],
    ) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let Some(cap_raw) = flags.value("monthly-usd") else {
        return usage_error("--monthly-usd <amount> is required (dollars, e.g. 200 or 12.50)");
    };
    let Some(zone_raw) = flags.value("time-zone") else {
        return usage_error(
            "--time-zone <IANA> is required (e.g. America/New_York or Asia/Taipei). The budget \
             period is a calendar month in that zone and there is deliberately no UTC default",
        );
    };
    let monthly_cap_usd_cents = match parse_usd_cents("--monthly-usd", cap_raw) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let turn_hold_usd_cents = match flags
        .value("turn-hold-usd")
        .map(|raw| parse_usd_cents("--turn-hold-usd", raw))
        .transpose()
    {
        Ok(h) => h,
        Err(e) => return usage_error(&e),
    };
    let time_zone = match parse_time_zone(zone_raw) {
        Ok(z) => z,
        Err(e) => return usage_error(&e),
    };
    let soft_limit = |flag| {
        flags
            .value(flag)
            .map(|raw| parse_usd_cents(&format!("--{flag}"), raw))
            .transpose()
    };
    let per_session_soft_usd_cents = match soft_limit("per-session-soft-usd") {
        Ok(limit) => limit,
        Err(e) => return usage_error(&e),
    };
    let per_agent_soft_usd_cents = match soft_limit("per-agent-soft-usd") {
        Ok(limit) => limit,
        Err(e) => return usage_error(&e),
    };
    let request = InitRequest {
        monthly_cap_usd_cents,
        time_zone: time_zone.as_str().to_string(),
        turn_hold_usd_cents,
        per_session_soft_usd_cents,
        per_agent_soft_usd_cents,
        force: flags.switch("force"),
    };

    // The directory is only created once every argument is known to be good.
    let dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("libra-governor shared-budget init: could not prepare state dir: {e}");
            return 1;
        }
    };
    match init_in(&dir, &request) {
        Ok(report) => {
            print!("{}", render_init(&report));
            0
        }
        Err(e) => {
            eprintln!("libra-governor shared-budget init: {e}");
            1
        }
    }
}

fn render_init(report: &InitReport) -> String {
    use shared_budget_view::usd_u;
    let c = &report.config;
    let mut soft = String::new();
    for (label, limit) in [
        ("Per-session", c.soft_limits.per_session_usd_cents),
        ("Per-agent", c.soft_limits.per_agent_usd_cents),
    ] {
        if let Some(limit) = limit {
            soft.push_str(&format!(
                "  {label} soft limit: {} (warn only)\n",
                usd_u(limit)
            ));
        }
    }
    format!(
        "{} {}\n\
         \x20 Monthly cap:  {} (calendar month in {})\n\
         \x20 Turn hold:    {} reserved per prompt\n\
         {soft}\x20 Guarantee:    admission-bounded (cooperative); Libra-accounted list-price-equivalent \
         USD, not a provider bill or quota\n\
         Next: run `libra-governor shared-budget status`. Edit or delete the file to change or \
         remove the budget; a running daemon applies it on the next prompt.\n",
        if report.replaced { "Replaced" } else { "Wrote" },
        report.path.display(),
        usd_u(c.monthly_cap_usd_cents),
        c.time_zone.as_str(),
        usd_u(c.turn_hold_usd_cents),
    )
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

fn run_status(args: &[String]) -> i32 {
    let flags = match Flags::parse(args, &[], &["json"]) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let posture = shared_budget_view::collect(shared_budget_view::REPORT_BUDGET);
    let state_dir = shared_budget_view::state_dir();
    if flags.switch("json") {
        let doc = shared_budget_view::render_json(&posture, state_dir.as_deref());
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        print!(
            "{}",
            shared_budget_view::render_human(&posture, state_dir.as_deref())
        );
    }
    0
}

// ---------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------

fn run_prune(args: &[String]) -> i32 {
    let flags = match Flags::parse(args, &["keep-months", "time-zone"], &["apply", "json"]) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let Some(keep_raw) = flags.value("keep-months") else {
        return usage_error("--keep-months is required");
    };
    let keep_months: u32 = match keep_raw.parse() {
        Ok(n) if n >= 1 => n,
        _ => return usage_error("--keep-months must be an integer >= 1"),
    };
    let time_zone = match resolve_prune_zone(flags.value("time-zone")) {
        Ok(z) => z,
        Err(e) => return usage_error(&e),
    };
    let apply = flags.switch("apply");

    let ledger_path = match ledger_path_or_report("prune") {
        Ok(p) => p,
        Err(code) => return code,
    };
    let report = if ledger_path.exists() {
        // Only --apply needs write access; a dry run stays read-only.
        let opened = if apply {
            LedgerStore::open(&ledger_path)
        } else {
            LedgerStore::open_read_only(&ledger_path)
        };
        let mut store = match opened {
            Ok(s) => s,
            Err(e) => {
                eprintln!("libra-governor shared-budget prune: could not open ledger: {e}");
                return 1;
            }
        };
        match store.prune_closed_periods(PruneRequest {
            keep_months,
            now: OffsetDateTime::now_utc(),
            time_zone: &time_zone,
            dry_run: !apply,
        }) {
            Ok(r) => r,
            Err(RetentionError::KeepMonthsZero) => {
                return usage_error("--keep-months must be an integer >= 1")
            }
            Err(e) => {
                eprintln!("libra-governor shared-budget prune: {e}");
                return 1;
            }
        }
    } else {
        empty_report(keep_months, !apply)
    };

    const RETENTION_WARNING: &str = "Export before prune: pruned periods can no longer be \
         exported, and charges for events in or before the newest pruned period are refused by the daemon.";
    let note = if apply {
        format!("Applied. {RETENTION_WARNING}")
    } else {
        format!(
            "Dry run: nothing was deleted (the ledger was opened read-only). Re-run with --apply \
             to delete. {RETENTION_WARNING}"
        )
    };
    if !flags.switch("json") {
        let active = report
            .pools
            .iter()
            .filter(|p| p.disposition == libra_governor_ledger::PruneDisposition::SkippedActiveHold)
            .count();
        if report.dry_run {
            eprintln!(
                "Dry run: would prune {} period(s), {} rows; nothing deleted. Re-run with --apply.",
                report.pools_selected, report.rows_selected
            );
        } else {
            eprintln!(
                "Applied: pruned {} period(s), deleted {} rows.",
                report.pools_pruned, report.rows_deleted
            );
        }
        eprintln!("Kept {} rows; skipped {active} active-hold period(s); retaining {} closed month(s) in {}.",
            report.rows_kept, report.keep_months, time_zone.as_str());
        eprintln!("{RETENTION_WARNING}");
    }
    let out = json!({
        "schema": PRUNE_SCHEMA,
        "report": report,
        "time_zone": time_zone.as_str(),
        "note": note,
    });
    println!("{out}");
    0
}

/// The zone recorded in `<state-dir>/shared-budget.json`, when that file is
/// present and valid. An invalid file yields `None` plus a stderr warning.
fn configured_zone() -> Option<IanaTimeZone> {
    let state_dir = libra_governor_daemon::paths::state_dir().ok()?;
    let loaded = LoadedSharedBudget::load(&state_dir);
    match &loaded.state {
        SharedBudgetFileState::Valid(config) => Some(config.time_zone.clone()),
        SharedBudgetFileState::Invalid(reason) => {
            eprintln!(
                "libra-governor shared-budget: shared-budget.json is invalid ({reason}); \
                 ignoring it for the time zone"
            );
            None
        }
        SharedBudgetFileState::Absent => None,
    }
}

/// The prune zone: the daemon's configured zone by default; an explicit
/// `--time-zone` must equal it when the file is valid; required otherwise.
fn resolve_prune_zone(explicit: Option<&str>) -> Result<IanaTimeZone, String> {
    let explicit = explicit.map(parse_time_zone).transpose()?;
    match (configured_zone(), explicit) {
        (Some(configured), Some(given)) if configured != given => Err(format!(
            "--time-zone {} differs from shared-budget.json's time_zone {}; the current month \
             must be computed in the zone the daemon uses",
            given.as_str(),
            configured.as_str()
        )),
        (Some(configured), _) => Ok(configured),
        (None, Some(given)) => Ok(given),
        (None, None) => {
            Err("--time-zone is required (no valid shared-budget.json to take it from)".to_string())
        }
    }
}

fn utc_zone() -> IanaTimeZone {
    IanaTimeZone::new("UTC").expect("UTC is in the bundled tz database")
}

fn empty_report(keep_months: u32, dry_run: bool) -> libra_governor_ledger::PruneReport {
    libra_governor_ledger::PruneReport {
        dry_run,
        keep_months,
        pools_considered: 0,
        pools_selected: 0,
        pools_pruned: 0,
        rows_selected: 0,
        rows_deleted: 0,
        rows_kept: 0,
        pools: Vec::new(),
    }
}

// ---------------------------------------------------------------------
// export
// ---------------------------------------------------------------------

/// `YYYY-MM` -> `(year, month)`; exactly four digits, a dash, two digits.
fn parse_period(s: &str) -> Option<(i32, u8)> {
    let (y, m) = s.split_once('-')?;
    if y.len() != 4 || m.len() != 2 || !y.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m): (i32, u8) = (y.parse().ok()?, m.parse().ok()?);
    (1..=12).contains(&m).then_some((y, m))
}

/// The `[start, end)` bounds of calendar month `period` in `tz`, plus the
/// pool alias. Anchored on mid-month so the instant is inside that month in
/// every zone (UTC-12 through UTC+14).
fn period_bounds(
    year: i32,
    month: u8,
    tz: &IanaTimeZone,
) -> Result<(OffsetDateTime, OffsetDateTime, String), String> {
    let month_enum = time::Month::try_from(month).map_err(|e| e.to_string())?;
    let date = time::Date::from_calendar_date(year, month_enum, 15).map_err(|e| e.to_string())?;
    let anchor = date
        .with_hms(12, 0, 0)
        .map_err(|e| e.to_string())?
        .assume_utc();
    let (start, end) = calendar_month_bounds(anchor, tz);
    let alias = monthly_pool_alias(start, tz);
    if alias != format!("monthly-{year:04}-{month:02}") {
        return Err(format!(
            "period {year:04}-{month:02} is outside the supported range"
        ));
    }
    Ok((start, end, alias))
}

fn unit_label(unit: &QuotaUnit) -> String {
    serde_json::to_value(unit)
        .ok()
        .and_then(|v| v.get("unit").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn id_value(field: &str, raw: &str, redact: bool) -> String {
    if redact {
        redacted_display_id(field, raw)
    } else {
        raw.to_string()
    }
}

/// The hashed, deterministic export body: a function of the ledger state,
/// the period and the id mode only (no clock reading).
fn build_body(
    export: &PoolExport,
    period_label: &str,
    tz: &IanaTimeZone,
    bounds: (OffsetDateTime, OffsetDateTime),
    redact_ids: bool,
) -> Result<Value, String> {
    let (start, end) = bounds;
    let utc = |t: OffsetDateTime| t.format(&Rfc3339).map_err(|e| e.to_string());
    let unit = unit_label(&export.unit);
    let basis = if matches!(export.unit, QuotaUnit::UsdCents) {
        "libra-accounted-usd-list-price-equivalent".to_string()
    } else {
        format!("libra-accounted-{unit}")
    };

    // Sorted by the exported identifiers (then raw, only to break a digest
    // collision) so the order never depends on the raw ids when redacted.
    let mut rows: Vec<_> = export
        .executors
        .iter()
        .map(|e| {
            (
                id_value("session_id", &e.session_id, redact_ids),
                e.agent_id
                    .as_deref()
                    .map(|a| id_value("agent_id", a, redact_ids)),
                e,
            )
        })
        .collect();
    rows.sort_by(|a, b| {
        (
            &a.0,
            &a.1,
            a.2.executor_kind.as_str(),
            &a.2.session_id,
            &a.2.agent_id,
        )
            .cmp(&(
                &b.0,
                &b.1,
                b.2.executor_kind.as_str(),
                &b.2.session_id,
                &b.2.agent_id,
            ))
    });
    let executors: Vec<Value> = rows
        .iter()
        .map(|(session, agent, e)| {
            json!({
                "session_id": session,
                "agent_id": agent,
                "executor_kind": e.executor_kind.as_str(),
                "used": e.used,
                "reserved": e.reserved,
                "rows": {
                    "settled": e.rows_settled,
                    "active": e.rows_active,
                    "released": e.rows_released,
                    "expired": e.rows_expired,
                },
                "by_charge_source": e.by_charge_source,
            })
        })
        .collect();

    Ok(json!({
        "schema": EXPORT_SCHEMA,
        "transport_policy": "local_only",
        "basis": basis,
        "guarantee": GUARANTEE,
        "id_mode": if redact_ids { "redacted" } else { "raw" },
        "period": {
            "label": period_label,
            "time_zone": tz.as_str(),
            "start_utc": utc(start)?,
            "end_utc": utc(end)?,
            "start_local": format_local_rfc3339(start, tz),
            "end_local": format_local_rfc3339(end, tz),
        },
        "pool": {
            "pool_id": export.pool_id.0,
            "unit": unit,
            "capacity": export.capacity,
            // The export reads the ledger offline: this is the pool row's
            // stored capacity, which can lag a cap raised in the budget file
            // (`status` and admission use the live configured cap).
            "capacity_source": "pool_row",
        },
        "totals": {
            "used": export.total_used,
            "reserved": export.total_reserved,
            "rows": export.total_rows,
            "executors": export.executors.len(),
        },
        "executors": executors,
    }))
}

/// Wraps `body` with its digest and the generation time. The digest covers
/// the canonical body only, so re-exporting an unchanged ledger yields the
/// same `body_sha256` at a different `generated_at`.
fn seal_export(body: Value, generated_at: OffsetDateTime) -> Result<Value, String> {
    let digest = libra_governor_evidence_adapter::canon::content_hash_hex(&body);
    Ok(json!({
        "generated_at": generated_at.format(&Rfc3339).map_err(|e| e.to_string())?,
        "body_sha256": digest,
        "canonicalization": "sorted-key compact JSON over `body`; SHA-256 lowercase hex",
        "body": body,
    }))
}

/// A purely lexical check that `out` is a plain filesystem path, not a
/// URL, stdout marker or empty.
fn validate_out_path(out: &str) -> Result<PathBuf, String> {
    if out.is_empty() || out == "-" {
        return Err("--out must be a file path".to_string());
    }
    if out.contains("://") || out.starts_with("file:") {
        return Err("--out must be a local file path, not a URL".to_string());
    }
    Ok(PathBuf::from(out))
}

/// The default export location, creating `<state-dir>/exports` as `0700`.
fn default_out_path(period: &str, raw_ids: bool) -> Result<PathBuf, String> {
    let state_dir = libra_governor_daemon::paths::ensure_state_dir()
        .map_err(|e| format!("could not resolve state dir: {e}"))?;
    let dir = state_dir.join("exports");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("could not create exports directory: {e}"))?;
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|e| format!("could not inspect exports directory: {e}"))?;
    if !meta.file_type().is_dir() {
        return Err("the exports path is not a real directory (symlink?); refusing".to_string());
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("could not secure exports directory: {e}"))?;
    let suffix = if raw_ids { "-raw-ids" } else { "" };
    Ok(dir.join(format!("shared-budget-{period}{suffix}.json")))
}

/// Writes `bytes` to `path` as an owner-only regular file. Without
/// `overwrite` the file is created exclusively (never replaces anything);
/// with it, an existing regular file is replaced atomically via a sibling
/// temp file. A symlink, directory or other special file is always refused.
fn write_export_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err("--out exists and is not a regular file".to_string());
        }
        Ok(_) if !overwrite => {
            return Err(
                "--out already exists; refusing to overwrite (pass --overwrite to replace it)"
                    .to_string(),
            );
        }
        Ok(_) | Err(_) => {}
    }
    let write = |target: &Path| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        if result.is_err() {
            let _ = std::fs::remove_file(target);
        }
        result
    };
    if !overwrite {
        return write(path).map_err(|e| format!("could not write export: {e}"));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| "--out has no file name".to_string())?
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    write(&tmp).map_err(|e| format!("could not write export: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not replace export: {e}")
    })
}

fn run_export(args: &[String]) -> i32 {
    let flags = match Flags::parse(
        args,
        &["period", "out", "time-zone"],
        &["raw-ids", "overwrite"],
    ) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let Some(period_raw) = flags.value("period") else {
        return usage_error("--period YYYY-MM is required");
    };
    let Some((year, month)) = parse_period(period_raw) else {
        return usage_error("--period must be YYYY-MM");
    };
    let time_zone = match flags.value("time-zone") {
        Some(name) => match parse_time_zone(name) {
            Ok(z) => z,
            Err(e) => return usage_error(&e),
        },
        None => configured_zone().unwrap_or_else(utc_zone),
    };
    let redact_ids = !flags.switch("raw-ids");
    let overwrite = flags.switch("overwrite");
    let out_arg = match flags.value("out").map(validate_out_path).transpose() {
        Ok(p) => p,
        Err(e) => return usage_error(&e),
    };

    // Static, not configurable: this command only ever produces a local file.
    AdapterConfig::new(TransportPolicy::LocalOnly)
        .expect("local_only is the one transport the adapter accepts");

    let (start, end, alias) = match period_bounds(year, month, &time_zone) {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let Some(key) = MeteredPoolKey::new(&PrincipalId(LOCAL_BUDGET_OWNER.to_string()), &alias).ok()
    else {
        eprintln!("libra-governor shared-budget export: invalid pool alias");
        return 1;
    };

    let ledger_path = match ledger_path_or_report("export") {
        Ok(p) => p,
        Err(code) => return code,
    };
    if !ledger_path.exists() {
        eprintln!(
            "libra-governor shared-budget export: no ledger found; nothing to export for {period_raw}"
        );
        return 1;
    }
    let store = match LedgerStore::open_read_only(&ledger_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("libra-governor shared-budget export: could not open ledger: {e}");
            return 1;
        }
    };
    let export = match store.shared_pool_export(&key.pool_id()) {
        Ok(Some(e)) => e,
        Ok(None) => {
            eprintln!(
                "libra-governor shared-budget export: no shared-budget pool exists for period \
                 {period_raw} (never created, or already pruned); nothing was written"
            );
            return 1;
        }
        Err(e) => {
            eprintln!("libra-governor shared-budget export: ledger read failed: {e}");
            return 1;
        }
    };

    let sealed = build_body(&export, period_raw, &time_zone, (start, end), redact_ids)
        .and_then(|body| seal_export(body, OffsetDateTime::now_utc()));
    let sealed = match sealed {
        Ok(v) => v,
        Err(e) => {
            eprintln!("libra-governor shared-budget export: could not build export: {e}");
            return 1;
        }
    };
    let mut bytes = match serde_json::to_vec_pretty(&sealed) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("libra-governor shared-budget export: could not encode export: {e}");
            return 1;
        }
    };
    bytes.push(b'\n');

    let out_path = match out_arg {
        Some(p) => p,
        None => match default_out_path(period_raw, !redact_ids) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("libra-governor shared-budget export: {e}");
                return 1;
            }
        },
    };
    if let Err(e) = write_export_file(&out_path, &bytes, overwrite) {
        eprintln!("libra-governor shared-budget export: {e}");
        return 1;
    }

    let summary = json!({
        "schema": EXPORT_SCHEMA,
        "written": out_path.display().to_string(),
        "body_sha256": sealed["body_sha256"],
        "id_mode": if redact_ids { "redacted" } else { "raw" },
        "note": "Local file only; nothing was sent anywhere.",
    });
    println!("{summary}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_parser_is_strict() {
        assert_eq!(parse_period("2026-09"), Some((2026, 9)));
        for bad in [
            "2026-9",
            "26-09",
            "2026-13",
            "2026-00",
            "2026/09",
            "2026-09-01",
            "",
        ] {
            assert_eq!(parse_period(bad), None, "{bad}");
        }
    }

    #[test]
    fn out_path_rejects_urls_and_stdout_markers() {
        for bad in ["", "-", "s3://bucket/key", "file:///tmp/x", "scheme://x"] {
            assert!(validate_out_path(bad).is_err(), "{bad}");
        }
        assert!(validate_out_path("/tmp/x.json").is_ok());
        assert!(validate_out_path("rel/x.json").is_ok());
    }

    #[test]
    fn period_bounds_cover_the_whole_month_in_any_zone() {
        for zone in [
            "UTC",
            "Pacific/Kiritimati",
            "Etc/GMT+12",
            "America/New_York",
        ] {
            let tz = IanaTimeZone::new(zone).unwrap();
            let (s, e, alias) = period_bounds(2026, 9, &tz).unwrap();
            assert_eq!(alias, "monthly-2026-09");
            assert!(e > s);
        }
    }

    // ----- init -----

    fn req(cap: u64, zone: &str, hold: Option<u64>, force: bool) -> InitRequest {
        InitRequest {
            monthly_cap_usd_cents: cap,
            time_zone: zone.to_string(),
            turn_hold_usd_cents: hold,
            per_session_soft_usd_cents: None,
            per_agent_soft_usd_cents: None,
            force,
        }
    }

    #[test]
    fn usd_amounts_become_exact_cents() {
        for (raw, cents) in [
            ("200", 20_000),
            ("12.50", 1_250),
            ("12.5", 1_250),
            ("0.01", 1),
            ("0.5", 50),
            ("1000000", MAX_INIT_USD_CENTS),
        ] {
            assert_eq!(parse_usd_cents("--x", raw), Ok(cents), "{raw}");
        }
    }

    #[test]
    fn usd_amount_boundaries() {
        assert_eq!(parse_usd_cents("--x", "1000000.00"), Ok(MAX_INIT_USD_CENTS));
        assert!(
            parse_usd_cents("--x", "1000000.01").is_err(),
            "max + 1 cent"
        );
        assert_eq!(
            parse_usd_cents("--x", "007"),
            Ok(700),
            "leading zeros are plain digits"
        );
        for bad in [
            "1.",
            ".5",
            "0.001",
            "1e3",
            "+5",
            "-1",
            "",
            "18446744073709551616",
            "184467440737095516150",
            "99999999999999999999999999999999",
        ] {
            assert!(parse_usd_cents("--x", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_stale_temp_file_from_an_earlier_run_does_not_block_init() {
        let dir = tempfile::tempdir().unwrap();
        // The old pid-only name, as a crashed run would have left it.
        std::fs::write(
            dir.path()
                .join(format!(".shared-budget.json.tmp-{}", std::process::id())),
            "stale",
        )
        .unwrap();
        init_in(dir.path(), &req(20_000, "UTC", None, false)).unwrap();
        assert_ne!(unique_temp_name(), unique_temp_name());
    }

    #[test]
    fn usd_amounts_reject_everything_but_plain_dollars() {
        for bad in [
            "",
            ".",
            "0",
            "0.00",
            "-5",
            "+5",
            "5.",
            ".5",
            "1.234",
            "1e3",
            "inf",
            "NaN",
            "1,000",
            "$5",
            " 5",
            "1000000.01",
            "99999999999999999999999",
        ] {
            assert!(
                parse_usd_cents("--x", bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn init_writes_a_file_the_daemons_parser_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let report = init_in(dir.path(), &req(20_000, "Asia/Taipei", None, false)).unwrap();
        let bytes = std::fs::read(&report.path).unwrap();
        match libra_governor_daemon::shared_budget_file::parse(&bytes) {
            SharedBudgetFileState::Valid(c) => {
                assert_eq!(c.monthly_cap_usd_cents, 20_000);
                assert_eq!(c.time_zone.as_str(), "Asia/Taipei");
                assert_eq!(c.turn_hold_usd_cents, 1_000, "the daemon default applies");
            }
            other => panic!("{other:?}"),
        }
        let mode = std::fs::metadata(&report.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!report.replaced);
    }

    #[test]
    fn init_never_writes_an_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        // Default hold ($10) exceeds a $5 cap: the daemon's parser rejects it.
        let err = init_in(dir.path(), &req(500, "UTC", None, false)).unwrap_err();
        assert!(err.contains("--turn-hold-usd"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // An unknown zone is rejected by the same validation.
        assert!(init_in(dir.path(), &req(5_000, "Mars/Olympus", None, false)).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn init_refuses_to_overwrite_without_force_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        init_in(dir.path(), &req(20_000, "UTC", None, false)).unwrap();
        let before = std::fs::read(dir.path().join("shared-budget.json")).unwrap();
        let err = init_in(dir.path(), &req(30_000, "UTC", None, false)).unwrap_err();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(
            std::fs::read(dir.path().join("shared-budget.json")).unwrap(),
            before
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn init_force_replaces_atomically_and_keeps_owner_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        init_in(dir.path(), &req(20_000, "UTC", None, false)).unwrap();
        let report = init_in(dir.path(), &req(30_000, "Asia/Taipei", Some(250), true)).unwrap();
        assert!(report.replaced);
        let loaded = LoadedSharedBudget::load(dir.path());
        let c = loaded.config().expect("valid");
        assert_eq!(c.monthly_cap_usd_cents, 30_000);
        assert_eq!(c.turn_hold_usd_cents, 250);
        let mode = std::fs::metadata(&report.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        // The config plus the .bak of the file it replaced.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn init_force_keeps_the_previous_file_as_an_owner_only_backup() {
        let dir = tempfile::tempdir().unwrap();
        init_in(dir.path(), &req(20_000, "UTC", None, false)).unwrap();
        let original = std::fs::read(dir.path().join("shared-budget.json")).unwrap();
        init_in(dir.path(), &req(30_000, "UTC", None, true)).unwrap();
        let backup = dir.path().join("shared-budget.json.bak");
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn init_force_backup_holds_only_the_most_recent_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        init_in(dir.path(), &req(20_000, "UTC", None, false)).unwrap();
        init_in(dir.path(), &req(30_000, "UTC", None, true)).unwrap();
        let second = std::fs::read(dir.path().join("shared-budget.json")).unwrap();
        init_in(dir.path(), &req(40_000, "UTC", None, true)).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("shared-budget.json.bak")).unwrap(),
            second
        );
    }

    #[test]
    fn init_refuses_a_symlink_even_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("shared-budget.json")).unwrap();
        assert!(init_in(dir.path(), &req(20_000, "UTC", None, true)).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }
}
