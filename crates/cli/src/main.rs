//! `libra-governor` CLI binary.
//!
//! - `--help` / `-h` / `help` — prints usage and exits 0. Never spawns the
//!   daemon or touches state (HORO-1614).
//! - `--version` / `-V` / `version` — prints `libra-governor <version>`
//!   from `CARGO_PKG_VERSION` (the same workspace-version source every
//!   other version-reporting surface in this crate already uses) and
//!   exits 0. Side-effect free, offline (HORO-1614).
//!
//! Subcommands:
//! - `daemon run` — runs the Governor daemon in the foreground.
//! - `daemon stop` — identity-checked shutdown of exactly the daemon this
//!   operator started (HORO-1380): reads its pid record, verifies it is
//!   alive, running the recorded executable, and answering its own
//!   socket, then sends `SIGTERM`. Refuses (no signal, non-zero exit) if
//!   any check fails. Never a broad `pkill -f` by process name.
//! - `hook user-prompt-submit` — the Claude Code `UserPromptSubmit` hook
//!   entry point (see `integrations/claude-code/README.md`).
//! - `hook post-tool-use` — the Claude Code `PostToolUse` hook entry
//!   point: fire-and-forget tool-call counting (HORO-1126).
//! - `hook stop` — the Claude Code `Stop` hook entry point: finalizes
//!   the session's task into an Execution Receipt (HORO-1126).
//! - `codex-hook user-prompt-submit` / `post-tool-use` / `stop` — the
//!   same three hook entry points for the Codex CLI (HORO-1157), sharing
//!   every byte of translation logic with Claude Code's via
//!   `crate::agent::run` — see `integrations/codex/README.md`.
//! - `statusline` — the legacy one-line Claude Code `statusLine`
//!   command, kept byte-for-byte because the founder's DogFood wrapper
//!   parses its exact phrases (HORO-1569).
//! - `statusline provider` — Libra's side of the shared Horonom
//!   statusline provider contract: one versioned JSON document on stdout,
//!   always exit 0 (HORO-1564/HORO-1569). This is the supported surface;
//!   bare `statusline` is on a migration path to retirement.
//! - `statusline explain` — the read-only long form of the same state, for
//!   the shared statusline `explain` surface. Says plainly what the daemon
//!   does *not* hold, and never prints task content.
//! - `statusline presentation` — records, in Libra's own state directory,
//!   how much of the budget envelope the compact budget phrase should
//!   spell out (HORO-1709). Never touches the user's statusline script or
//!   `~/.claude/settings.json`.
//! - `calibration report` — real duration-coverage and admission-replay
//!   calibration evidence over local history (HORO-1132).
//! - `gateway token` — prints the local capability token Claude Code's
//!   `apiKeyHelper` presents to the enforcement gateway (HORO-1144).
//!   Never a provider credential — see that module's docs.
//! - `gateway status` — whether the gateway is running, what it may
//!   honestly claim to enforce, and what it has admitted or refused.
//! - `doctor [--json]` — a read-only diagnostic snapshot: daemon
//!   availability/version, SQLite schema health, Claude Code hook/
//!   statusline wiring, gateway configuration and capability tier, and
//!   `config.json` validity (HORO-1150). Never spawns the daemon, never
//!   prints a secret.
//! - `install [--hooks-only] [--agent codex]` — wires this binary's hooks (and,
//!   for Claude Code by default, statusline) into `~/.claude/settings.json` or
//!   `~/.codex/hooks.json`, preserving every other key (HORO-1150,
//!   `--agent codex` in HORO-1157, `--hooks-only` in HORO-1743).
//! - `uninstall [--agent codex] [--yes]` — removes exactly what `install`
//!   added, plus (with confirmation) the state directory and, if this
//!   tool installed it, the daemon binary (HORO-1150, `--agent codex` in
//!   HORO-1157).
//! - `agents [--json]` — the honest per-agent capability matrix (which
//!   hook/lifecycle/gateway capabilities each governed agent host
//!   actually has) for every agent this integration knows about
//!   (HORO-1157). Pure rendering of
//!   `libra_governor_domain::AgentCapabilities::for_agent` — no probing.
//! - `evidence-report consent` — records explicit local opt-in for the
//!   HORO-1154 evidence-collection tool.
//! - `evidence-report` — refuses without that consent; with it, exports
//!   coarse local behavioral aggregates plus evaluator-typed qualitative
//!   answers to a local JSON/Markdown file. Off by default, opt-in only,
//!   local-only — never a network call (HORO-1154).
//! - `dogfood-evidence export` — projects the local ledger into
//!   ADR-0012 §3 evidence events and writes them as local NDJSON, under
//!   the same consent gate as `evidence-report`. Local-only transport
//!   only, no network capability (HORO-1376).
//! - `outcome record` — pushes an outcome attestation for a task, read as
//!   JSON from stdin, over the daemon's existing Unix socket (HORO-1174).
//!   The entry point an external Outcome Provider shells out to; see
//!   `examples/local-providers/report_outcome.sh`.
//! - `economics explain --task|--session|--account|--principal|--organization <value> [--json]`
//!   — reconstructs "what did this cost, really" entirely from the real
//!   persisted ledger tables (HORO-1672). `--principal`/`--organization`
//!   are accepted selectors, always answered honestly as not-yet-configured
//!   in v0.0.3 rather than rejected as unknown.

mod adapter_cmd;
mod adapter_hook;
mod adapter_probe_signal;
mod agent;
mod agents_cmd;
mod bucket_prose;
mod calibration_cmd;
mod claude_settings;
mod client;
mod codex_hook;
mod codex_hooks_file;
mod daemon_cmd;
mod doctor_cmd;
mod dogfood_evidence_cmd;
mod economics_cmd;
mod evidence_report_cmd;
mod gateway_cmd;
mod hook;
mod hook_post_tool_use;
mod hook_stop;
mod install_cmd;
mod outcome_cmd;
mod presentation;
mod quota_cmd;
mod statusline;
mod statusline_provider;
mod uninstall_cmd;
mod write_lock;

const USAGE: &str = "\
Usage:
  libra-governor daemon run
  libra-governor daemon stop
  libra-governor hook user-prompt-submit
  libra-governor hook post-tool-use
  libra-governor hook stop
  libra-governor codex-hook user-prompt-submit
  libra-governor codex-hook post-tool-use
  libra-governor codex-hook stop
  libra-governor statusline
  libra-governor statusline provider
  libra-governor statusline explain
  libra-governor statusline presentation
  libra-governor statusline presentation --budget-display percent|remaining|remaining+total|used+remaining+total|full
  libra-governor calibration report
  libra-governor gateway token
  libra-governor gateway status
  libra-governor doctor [--json]
  libra-governor install [--hooks-only]
  libra-governor install --agent codex
  libra-governor uninstall [--agent codex] [--yes]
  libra-governor adapter <operation> [--json]
  libra-governor agents [--json]
  libra-governor evidence-report consent
  libra-governor evidence-report
  libra-governor dogfood-evidence export
  libra-governor outcome record
  libra-governor economics explain --task|--session|--account|--principal|--organization <value> [--json]
  libra-governor quota explain --replay <path> [--as-of <rfc3339>] [--json] [--width <n>]

Global options:
  -h, --help     Print this help and exit
  -V, --version  Print version and exit

Run `libra-governor doctor` for a read-only health/config snapshot.";

fn print_help() {
    println!(
        "libra-governor {} — local-first execution governor for agentic work (Claude Code and Codex today)\n\n{USAGE}",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_version() {
    println!("libra-governor {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--help"] | ["-h"] | ["help"] => print_help(),
        ["--version"] | ["-V"] | ["version"] => print_version(),
        ["adapter", ..] => std::process::exit(adapter_cmd::run(&args[1..])),
        ["adapter-hook", ..] => std::process::exit(adapter_hook::run(&args[1..])),
        ["daemon", "run"] => daemon_cmd::run(),
        ["daemon", "stop"] => std::process::exit(daemon_cmd::stop()),
        ["hook", "user-prompt-submit"] => hook::run(),
        ["hook", "post-tool-use"] => hook_post_tool_use::run(),
        ["hook", "stop"] => hook_stop::run(),
        ["codex-hook", "user-prompt-submit"] => codex_hook::run_prompt_submit(),
        ["codex-hook", "post-tool-use"] => codex_hook::run_tool_completed(),
        ["codex-hook", "stop"] => codex_hook::run_turn_completed(),
        ["statusline"] => statusline::run(),
        ["statusline", "provider"] => statusline_provider::run_provider(),
        ["statusline", "explain"] => statusline_provider::run_explain(),
        ["statusline", "presentation"] => presentation::run(None),
        ["statusline", "presentation", "--budget-display", choice] => {
            presentation::run(Some(choice))
        }
        ["calibration", "report"] => calibration_cmd::run(),
        ["gateway", "token"] => gateway_cmd::run_token(),
        ["gateway", "status"] => gateway_cmd::run_status(),
        ["doctor"] => doctor_cmd::run(false),
        ["doctor", "--json"] => doctor_cmd::run(true),
        ["install"] => install_cmd::run(),
        ["install", "--hooks-only"] => install_cmd::run_hooks_only(),
        ["install", "--agent", "codex"] => install_cmd::run_codex(),
        ["uninstall"] => uninstall_cmd::run(false),
        ["uninstall", "--yes"] => uninstall_cmd::run(true),
        ["uninstall", "--agent", "codex"] => uninstall_cmd::run_codex(false),
        ["uninstall", "--agent", "codex", "--yes"] => uninstall_cmd::run_codex(true),
        ["agents"] => agents_cmd::run(false),
        ["agents", "--json"] => agents_cmd::run(true),
        ["evidence-report", "consent"] => evidence_report_cmd::run_consent(),
        ["evidence-report"] => evidence_report_cmd::run(),
        ["dogfood-evidence", "export"] => dogfood_evidence_cmd::run(),
        ["outcome", "record"] => outcome_cmd::run(),
        ["quota", "explain", ..] => std::process::exit(quota_cmd::run(&args[2..])),
        ["economics", "explain", sel, value]
            if matches!(
                *sel,
                "--task" | "--session" | "--account" | "--principal" | "--organization"
            ) =>
        {
            economics_cmd::run(sel, value, false)
        }
        ["economics", "explain", sel, value, "--json"]
            if matches!(
                *sel,
                "--task" | "--session" | "--account" | "--principal" | "--organization"
            ) =>
        {
            economics_cmd::run(sel, value, true)
        }
        // Hidden, undocumented test fixture for HORO-1380/ADR-0014's
        // cross-process write-lock test suite
        // (`tests/write_lock_cross_process.rs`). Not part of the public
        // CLI surface — deliberately absent from USAGE/--help. Acquires
        // the real cross-process lock on `<config-path>`'s sidecar via
        // `write_lock::acquire`, prints exactly one line ("LOCKED" on
        // success, "LOCK_FAILED: <message>" on failure) and flushes
        // stdout immediately so a parent test process can synchronize on
        // it without sleeping, then holds the lock for `<hold-ms>`
        // milliseconds (releasing it on exit, or immediately on
        // acquisition failure).
        ["__lock_test_hold", config_path, hold_ms] => {
            write_lock::test_hold_cmd::run(config_path, hold_ms)
        }
        _ => {
            eprintln!("libra-governor: unknown or missing subcommand\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}
