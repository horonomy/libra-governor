//! `libra-governor shared-budget <prune|export>` (HORO-2219): retention and
//! a local-only export for the local monthly shared budget (ADR-0026).
//!
//! Both subcommands open the ledger file directly, exactly as the other
//! offline commands do (`evidence-report`, `dogfood-evidence`): the store
//! runs in WAL mode with a 5 s `busy_timeout`, prune takes one short
//! `BEGIN IMMEDIATE` transaction per pool, and export is a single read
//! snapshot, so both are safe to run while the daemon is up. Neither
//! creates a missing ledger and neither contacts the daemon.
//!
//! Local only: nothing here opens a socket or spawns a process. `export`
//! writes one owner-only file and refuses any `--out` that is not a plain
//! local file path. Delivering an export anywhere is a separate, unbuilt
//! workstream (ADR-0026).
//!
//! The group is a flat dispatch on the first argument: a later subcommand
//! (`status`, `doctor`) is one more arm in [`run`] plus its own function,
//! with no change to the logic below.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
    libra-governor shared-budget prune --keep-months <n> --time-zone <IANA> [--apply]\n  \
    libra-governor shared-budget export --period <YYYY-MM> [--out <file>] [--time-zone <IANA>] \
    [--raw-ids] [--overwrite]\n\n\
    prune   Dry run by default; deletes only with --apply. Removes closed rows of ended monthly \
    periods older than the last <n> closed months (n >= 1). Pruned periods can no longer be \
    exported.\n  \
    export  Writes a local JSON file (0600; default under <state-dir>/exports/). Session and \
    agent ids are replaced by short digests unless --raw-ids is given. Nothing is sent anywhere.";

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
// prune
// ---------------------------------------------------------------------

fn run_prune(args: &[String]) -> i32 {
    let flags = match Flags::parse(args, &["keep-months", "time-zone"], &["apply"]) {
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
    let Some(zone_raw) = flags.value("time-zone") else {
        return usage_error("--time-zone is required (the current month depends on it)");
    };
    let time_zone = match parse_time_zone(zone_raw) {
        Ok(z) => z,
        Err(e) => return usage_error(&e),
    };
    let apply = flags.switch("apply");

    let ledger_path = match ledger_path_or_report("prune") {
        Ok(p) => p,
        Err(code) => return code,
    };
    let report = if ledger_path.exists() {
        let mut store = match LedgerStore::open(&ledger_path) {
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

    let note = if apply {
        "Applied. Pruned periods can no longer be exported."
    } else {
        "Dry run: nothing was deleted or written. Re-run with --apply to delete; pruned \
         periods can no longer be exported, so export first if you need them."
    };
    let out = json!({
        "schema": PRUNE_SCHEMA,
        "report": report,
        "note": note,
    });
    println!("{out}");
    0
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
    let time_zone = match parse_time_zone(flags.value("time-zone").unwrap_or("UTC")) {
        Ok(z) => z,
        Err(e) => return usage_error(&e),
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
    let store = match LedgerStore::open(&ledger_path) {
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
}
