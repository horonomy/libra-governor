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
//! - `install` — wires this binary's hooks and statusline into
//!   `~/.claude/settings.json`, preserving every other key (HORO-1150).
//! - `uninstall [--yes]` — removes exactly what `install` added, plus
//!   (with confirmation) the state directory and, if this tool installed
//!   it, the daemon binary (HORO-1150).

mod bucket_prose;
mod calibration_cmd;
mod claude_settings;
mod client;
mod daemon_cmd;
mod doctor_cmd;
mod gateway_cmd;
mod hook;
mod hook_post_tool_use;
mod hook_stop;
mod install_cmd;
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
        ["statusline"] => statusline::run(),
        ["calibration", "report"] => calibration_cmd::run(),
        ["gateway", "token"] => gateway_cmd::run_token(),
        ["gateway", "status"] => gateway_cmd::run_status(),
        ["doctor"] => doctor_cmd::run(false),
        ["doctor", "--json"] => doctor_cmd::run(true),
        ["install"] => install_cmd::run(),
        ["uninstall"] => uninstall_cmd::run(false),
        ["uninstall", "--yes"] => uninstall_cmd::run(true),
        _ => {
            eprintln!(
                "libra-governor: unknown or missing subcommand\n\n\
                 Usage:\n  \
                 libra-governor daemon run\n  \
                 libra-governor hook user-prompt-submit\n  \
                 libra-governor hook post-tool-use\n  \
                 libra-governor hook stop\n  \
                 libra-governor statusline\n  \
                 libra-governor calibration report\n  \
                 libra-governor gateway token\n  \
                 libra-governor gateway status\n  \
                 libra-governor doctor [--json]\n  \
                 libra-governor install\n  \
                 libra-governor uninstall [--yes]"
            );
            std::process::exit(2);
        }
    }
}
