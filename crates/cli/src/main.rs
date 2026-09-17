//! `libra-governor` CLI binary.
//!
//! Subcommands:
//! - `daemon run` — runs the Governor daemon in the foreground.
//! - `hook user-prompt-submit` — the Claude Code `UserPromptSubmit` hook
//!   entry point (see `integrations/claude-code/README.md`).
//! - `statusline` — the Claude Code `statusLine` command.

mod client;
mod daemon_cmd;
mod hook;
mod statusline;

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
        ["statusline"] => statusline::run(),
        _ => {
            eprintln!(
                "libra-governor: unknown or missing subcommand\n\n\
                 Usage:\n  \
                 libra-governor daemon run\n  \
                 libra-governor hook user-prompt-submit\n  \
                 libra-governor statusline"
            );
            std::process::exit(2);
        }
    }
}
