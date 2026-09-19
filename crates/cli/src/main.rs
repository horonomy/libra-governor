//! `libra-governor` CLI binary.
//!
//! Subcommands:
//! - `daemon run` — runs the Governor daemon in the foreground.
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
//! - `statusline` — the Claude Code `statusLine` command.
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
//! - `install [--agent codex]` — wires this binary's hooks (and, for
//!   Claude Code, statusline) into `~/.claude/settings.json` or
//!   `~/.codex/hooks.json`, preserving every other key (HORO-1150,
//!   `--agent codex` in HORO-1157).
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
//! - `outcome record` — pushes an outcome attestation for a task, read as
//!   JSON from stdin, over the daemon's existing Unix socket (HORO-1174).
//!   The entry point an external Outcome Provider shells out to; see
//!   `examples/local-providers/report_outcome.sh`.

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
mod evidence_report_cmd;
mod gateway_cmd;
mod hook;
mod hook_post_tool_use;
mod hook_stop;
mod install_cmd;
mod outcome_cmd;
mod statusline;
mod uninstall_cmd;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["daemon", "run"] => daemon_cmd::run(),
        ["hook", "user-prompt-submit"] => hook::run(),
        ["hook", "post-tool-use"] => hook_post_tool_use::run(),
        ["hook", "stop"] => hook_stop::run(),
        ["codex-hook", "user-prompt-submit"] => codex_hook::run_prompt_submit(),
        ["codex-hook", "post-tool-use"] => codex_hook::run_tool_completed(),
        ["codex-hook", "stop"] => codex_hook::run_turn_completed(),
        ["statusline"] => statusline::run(),
        ["calibration", "report"] => calibration_cmd::run(),
        ["gateway", "token"] => gateway_cmd::run_token(),
        ["gateway", "status"] => gateway_cmd::run_status(),
        ["doctor"] => doctor_cmd::run(false),
        ["doctor", "--json"] => doctor_cmd::run(true),
        ["install"] => install_cmd::run(),
        ["install", "--agent", "codex"] => install_cmd::run_codex(),
        ["uninstall"] => uninstall_cmd::run(false),
        ["uninstall", "--yes"] => uninstall_cmd::run(true),
        ["uninstall", "--agent", "codex"] => uninstall_cmd::run_codex(false),
        ["uninstall", "--agent", "codex", "--yes"] => uninstall_cmd::run_codex(true),
        ["agents"] => agents_cmd::run(false),
        ["agents", "--json"] => agents_cmd::run(true),
        ["evidence-report", "consent"] => evidence_report_cmd::run_consent(),
        ["evidence-report"] => evidence_report_cmd::run(),
        ["outcome", "record"] => outcome_cmd::run(),
        _ => {
            eprintln!(
                "libra-governor: unknown or missing subcommand\n\n\
                 Usage:\n  \
                 libra-governor daemon run\n  \
                 libra-governor hook user-prompt-submit\n  \
                 libra-governor hook post-tool-use\n  \
                 libra-governor hook stop\n  \
                 libra-governor codex-hook user-prompt-submit\n  \
                 libra-governor codex-hook post-tool-use\n  \
                 libra-governor codex-hook stop\n  \
                 libra-governor statusline\n  \
                 libra-governor calibration report\n  \
                 libra-governor gateway token\n  \
                 libra-governor gateway status\n  \
                 libra-governor doctor [--json]\n  \
                 libra-governor install [--agent codex]\n  \
                 libra-governor uninstall [--agent codex] [--yes]\n  \
                 libra-governor agents [--json]\n  \
                 libra-governor evidence-report consent\n  \
                 libra-governor evidence-report\n  \
                 libra-governor outcome record"
            );
            std::process::exit(2);
        }
    }
}
