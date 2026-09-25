//! Webhook HMAC secret custody (HORO-1174).
//!
//! # The invariant this module exists to enforce
//!
//! The shared secret used to sign outbound requests/events must never
//! appear in the agent's environment, arguments, or configuration; never
//! in a log line at any level; never in the ledger; and never in an error
//! message. It is resolved once, from a user-configured command
//! (`security find-generic-password ...`, `pass show ...`, `op read
//! ...`), and used only to compute an HMAC — never placed literally on
//! the wire.
//!
//! # Mirrors `crates/gateway/src/credential.rs`'s discipline, deliberately
//!
//! Same subprocess discipline: stdin `/dev/null`, stderr captured and
//! discarded unread, an 8 KiB cap, a 10s timeout, and an error type whose
//! `Display` never quotes captured output. Not shared code with
//! `crates/gateway`'s `CredentialCommand`/`UpstreamCredential` — a
//! deliberate ~60-line duplication (rule of three), because the exposure
//! contract is genuinely different: [`WebhookSecret`] has no
//! `header_value()`-equivalent accessor at all. Its *only* accessor is
//! [`WebhookSecret::sign`], which returns a signature, never the secret
//! itself — so this type is structurally incapable of ever being placed
//! literally on the wire, unlike `UpstreamCredential`, which by design
//! *is* placed on the wire (as an `Authorization` header) after
//! [`std::fmt::Debug`]-redaction. `crates/gateway`'s credential type was
//! not refactored to share code with this one — see
//! `docs/adr/0005-local-extension-points.md`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// `KeyInit` is imported separately because hmac 0.13 / digest 0.11 dropped it
// as a supertrait of `Mac`; `new_from_slice` lives on `KeyInit` alone now.
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Mirrors `crates/gateway::credential::CREDENTIAL_COMMAND_TIMEOUT`.
pub const SECRET_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Mirrors `crates/gateway::credential::MAX_CREDENTIAL_BYTES`.
pub const MAX_SECRET_BYTES: usize = 8 * 1024;

/// Why a webhook secret could not be produced. Deliberately coarse — see
/// module docs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("secret command `{program}` could not be spawned")]
    SpawnFailed { program: String },
    #[error("secret command `{program}` exited with status {status}")]
    NonZeroExit { program: String, status: String },
    #[error("secret command `{program}` did not finish within the timeout")]
    TimedOut { program: String },
    #[error("secret command `{program}` produced no secret on stdout")]
    EmptyOutput { program: String },
    #[error("secret command `{program}` produced more than {MAX_SECRET_BYTES} bytes")]
    TooLarge { program: String },
}

/// A resolved webhook HMAC secret.
///
/// No `Display`, no `Serialize`, no `Deserialize`, no accessor returning
/// the raw bytes — see module docs. `Debug` writes `<redacted>`,
/// including when this type is nested inside a containing struct that
/// itself derives `Debug` (see the `debug_never_renders_the_secret_even_when_nested`
/// test).
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookSecret(Vec<u8>);

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookSecret(<redacted>)")
    }
}

impl WebhookSecret {
    fn new(raw: Vec<u8>) -> Self {
        Self(raw)
    }

    /// Computes `HMAC-SHA256(secret, payload)`. The **only** accessor
    /// this type exposes — structurally incapable of placing the secret
    /// itself on the wire. See module docs.
    pub fn sign(&self, payload: &[u8]) -> [u8; 32] {
        let mut mac =
            HmacSha256::new_from_slice(&self.0).expect("HMAC accepts a key of any length");
        mac.update(payload);
        mac.finalize().into_bytes().into()
    }
}

/// A user-configured command that prints the webhook secret on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookSecretCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl WebhookSecretCommand {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    /// Runs the command and reads the secret from its stdout. Same
    /// discipline as `crates/gateway::credential::CredentialCommand::resolve` —
    /// see module docs.
    pub fn resolve(&self) -> Result<WebhookSecret, SecretError> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| SecretError::SpawnFailed {
                program: self.program.clone(),
            })?;

        let mut stdout = child.stdout.take().expect("stdout was piped above");
        let program = self.program.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout
                .by_ref()
                .take(MAX_SECRET_BYTES as u64 + 1)
                .read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + SECRET_COMMAND_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = reader.join();
                        return Err(SecretError::TimedOut { program });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = reader.join();
                    return Err(SecretError::SpawnFailed { program });
                }
            }
        };

        let raw = reader.join().unwrap_or_default();
        if raw.len() > MAX_SECRET_BYTES {
            return Err(SecretError::TooLarge { program });
        }
        if !status.success() {
            return Err(SecretError::NonZeroExit {
                program,
                status: status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
            });
        }

        let text = String::from_utf8_lossy(&raw);
        let trimmed = text.trim_matches(|c: char| c == '\n' || c == '\r' || c == ' ' || c == '\t');
        if trimmed.is_empty() {
            return Err(SecretError::EmptyOutput { program });
        }
        Ok(WebhookSecret::new(trimmed.as_bytes().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_secret() -> WebhookSecret {
        WebhookSecretCommand::new(
            "/bin/sh",
            vec![
                "-c".to_string(),
                "printf sk-fake-webhook-secret".to_string(),
            ],
        )
        .resolve()
        .unwrap()
    }

    #[test]
    fn debug_never_renders_the_secret() {
        let secret = fake_secret();
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("sk-fake"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn debug_never_renders_the_secret_even_when_nested() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            secret: WebhookSecret,
            other_field: String,
        }
        let holder = Holder {
            secret: fake_secret(),
            other_field: "ordinary value".to_string(),
        };
        let rendered = format!("{holder:?}");
        assert!(!rendered.contains("sk-fake"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("ordinary value"));
    }

    #[test]
    fn sign_produces_a_deterministic_hmac() {
        let secret = fake_secret();
        let a = secret.sign(b"payload-1");
        let b = secret.sign(b"payload-1");
        let c = secret.sign(b"payload-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn sign_matches_the_rfc_4231_hmac_sha256_vector() {
        // Pins the on-the-wire signature bytes to the standard, so a future
        // hmac/sha2/digest major bump cannot silently change what receivers
        // must verify. RFC 4231 test case 2.
        let secret = WebhookSecret::new(b"Jefe".to_vec());
        let signature = secret.sign(b"what do ya want for nothing?");
        assert_eq!(
            signature
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn resolve_trims_the_trailing_newline() {
        let command = WebhookSecretCommand::new(
            "/bin/sh",
            vec!["-c".to_string(), "printf 'sk-fake-secret\\n'".to_string()],
        );
        let secret = command.resolve().unwrap();
        // Indirect proof (can't inspect bytes directly by design): two
        // secrets differing only by trailing whitespace sign identically.
        let untrimmed = WebhookSecret::new(b"sk-fake-secret".to_vec());
        assert_eq!(secret.sign(b"x"), untrimmed.sign(b"x"));
    }

    #[test]
    fn empty_output_is_an_error() {
        let command =
            WebhookSecretCommand::new("/bin/sh", vec!["-c".to_string(), "printf ''".to_string()]);
        assert!(matches!(
            command.resolve().unwrap_err(),
            SecretError::EmptyOutput { .. }
        ));
    }

    #[test]
    fn a_non_zero_exit_is_an_error_whose_message_carries_no_output() {
        let command = WebhookSecretCommand::new(
            "/bin/sh",
            vec![
                "-c".to_string(),
                "printf 'sk-fake-leaked'; echo 'stderr sk-fake-leaked' >&2; exit 3".to_string(),
            ],
        );
        let err = command.resolve().unwrap_err();
        let rendered = err.to_string();
        assert!(!rendered.contains("sk-fake"));
        assert!(matches!(err, SecretError::NonZeroExit { .. }));
    }

    #[test]
    fn oversized_output_is_refused() {
        let command = WebhookSecretCommand::new(
            "/bin/sh",
            vec![
                "-c".to_string(),
                format!(
                    "head -c {} /dev/zero | tr '\\0' 'x'",
                    MAX_SECRET_BYTES + 100
                ),
            ],
        );
        assert!(matches!(
            command.resolve().unwrap_err(),
            SecretError::TooLarge { .. }
        ));
    }

    #[test]
    fn a_missing_program_fails_to_spawn() {
        let command = WebhookSecretCommand::new("/definitely/not/a/real/program", vec![]);
        assert!(matches!(
            command.resolve().unwrap_err(),
            SecretError::SpawnFailed { .. }
        ));
    }

    #[test]
    fn a_command_that_would_prompt_gets_no_stdin() {
        let command = WebhookSecretCommand::new("/bin/cat", vec![]);
        assert!(matches!(
            command.resolve().unwrap_err(),
            SecretError::EmptyOutput { .. }
        ));
    }
}
