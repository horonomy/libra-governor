# Libra Governor documentation

Libra is a local Governor for coding-agent work. Its North Star is **never
start work you are unlikely to afford to finish**. This Developer Preview is
installed from source and runs on your machine; it provides no hosted runtime,
account system, control plane, or SaaS dependency.

## Start with the real product guide

The chapters in this site are rendered from the repository's canonical Markdown.
They cover the source installer, first preflight, four policy presets, statusline,
diagnostics, platform support, privacy and security, release compatibility, and
the actual capability differences between Claude Code and Codex.

- [Install, quickstart, policy and diagnostics](developer-preview.md) using the
  repository's [`scripts/install.sh`](https://github.com/horonomy/libra-governor/blob/main/scripts/install.sh)
- [Codex capability differences and setup](codex.md)
- [Composable statusline](statusline.md)
- [Security reporting and supported versions](security.md)

The [public repository](https://github.com/horonomy/libra-governor) remains the
source of truth. Use [GitHub Issues](https://github.com/horonomy/libra-governor/issues)
for sanitized product questions. Do not include prompts, source code, tool output,
credentials, private repository names, or other sensitive content in an issue.

This documentation site has no analytics. Libra keeps full prompts, source code,
tool output, ledger state, and policy decisions local by default. Read the
Developer Preview guide before enabling optional outbound providers, webhooks, or
the explicit traffic gateway.
