//! Upstream credential custody (HORO-1144).
//!
//! # The invariant this module exists to enforce
//!
//! The real provider API key must never appear in the agent's
//! environment, arguments, or configuration; never in a log line at any
//! level; never in the ledger; and never in an error message. Claude Code
//! authenticates to the *local* gateway with an opaque capability token
//! (see [`LocalCapabilityToken`]) that is worth nothing off this machine;
//! the gateway substitutes the real credential only on the outbound
//! request.
//!
//! That invariant is carried by the type system rather than by care:
//! [`UpstreamCredential`] has no `Display`, no `Serialize`, no
//! `Deserialize`, and no accessor returning its contents as a `String`.
//! Its `Debug` writes `<redacted>`, so a `{:?}` on a struct that happens
//! to contain one — the single most common way a secret escapes — cannot
//! print it. The only way out is [`UpstreamCredential::header_value`],
//! which produces an `http::HeaderValue` destined for the wire.
//!
//! # Errors never carry captured output
//!
//! A credential command that fails is exactly the situation where a
//! secret is most likely to be sitting in a pipe. [`CredentialError`]'s
//! `Display` therefore never includes the command's captured stdout or
//! stderr — only the program name, an exit status, and a category.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest a credential command may run before it is killed. A keychain
/// or password-manager lookup that has not answered in ten seconds is
/// waiting on something interactive, and the daemon must not block its
/// startup on it indefinitely.
pub const CREDENTIAL_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Most stdout a credential command may produce. An API key is on the
/// order of a hundred bytes; anything approaching 8 KiB is a
/// misconfigured command printing a file, and reading it in full would be
/// both pointless and a memory hazard.
pub const MAX_CREDENTIAL_BYTES: usize = 8 * 1024;

/// Minimum interval between two resolutions of the same credential
/// command. An upstream `401` triggers a re-resolve exactly once; this
/// cooldown stops a persistently-rejecting credential from spawning a
/// keychain prompt on every request.
pub const CREDENTIAL_REFRESH_COOLDOWN: Duration = Duration::from_secs(60);

/// Why a credential could not be produced. Deliberately coarse: a finer
/// error would tempt a caller into echoing captured output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("credential command `{program}` could not be spawned")]
    SpawnFailed { program: String },
    #[error("credential command `{program}` exited with status {status}")]
    NonZeroExit { program: String, status: String },
    #[error("credential command `{program}` did not finish within the timeout")]
    TimedOut { program: String },
    #[error("credential command `{program}` produced no credential on stdout")]
    EmptyOutput { program: String },
    #[error("credential command `{program}` produced more than {MAX_CREDENTIAL_BYTES} bytes")]
    TooLarge { program: String },
    #[error("credential is not usable as an HTTP header value")]
    NotHeaderSafe,
}

/// An upstream provider credential.
///
/// See module docs: no `Display`, no `Serialize`, no `Deserialize`, no
/// accessor returning the contents. The `Debug` impl below is the whole
/// point of the newtype.
#[derive(Clone, PartialEq, Eq)]
pub struct UpstreamCredential(String);

impl std::fmt::Debug for UpstreamCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UpstreamCredential(<redacted>)")
    }
}

impl UpstreamCredential {
    /// Wraps an already-obtained credential string. `pub(crate)` on
    /// purpose: outside this crate the only way to obtain one is
    /// [`CredentialCommand::resolve`], so there is no public constructor
    /// through which a credential could be minted from, say, a
    /// deserialized config file.
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The credential as an HTTP header value, marked sensitive so
    /// `hyper`'s own `Debug` rendering of the header map elides it too.
    pub(crate) fn header_value(&self) -> Result<hyper::header::HeaderValue, CredentialError> {
        let mut value = hyper::header::HeaderValue::from_str(&self.0)
            .map_err(|_| CredentialError::NotHeaderSafe)?;
        value.set_sensitive(true);
        Ok(value)
    }

    /// Length in bytes. A presence/shape check that cannot reveal the
    /// value — used by tests and by startup diagnostics that must confirm
    /// a credential was obtained without inspecting it.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A user-configured command that prints the upstream credential on
/// stdout — `security find-generic-password -w ...`, `pass show ...`,
/// `op read ...`.
///
/// A command rather than a config field holding the key itself: the key
/// then lives in the operating system's own secret store, and this
/// process holds it only in memory for as long as it is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl CredentialCommand {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    /// Runs the command and reads the credential from its stdout.
    ///
    /// stdin is `/dev/null` so a command that would otherwise prompt
    /// fails fast instead of hanging; stderr is captured and **discarded
    /// unread** so a diagnostic that happens to echo the secret cannot
    /// reach a log; stdout is capped at [`MAX_CREDENTIAL_BYTES`] and
    /// trailing whitespace is trimmed (every one of these tools emits a
    /// trailing newline).
    pub fn resolve(&self) -> Result<UpstreamCredential, CredentialError> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| CredentialError::SpawnFailed {
                program: self.program.clone(),
            })?;

        // Read concurrently with waiting: a command that filled the pipe
        // buffer would otherwise block forever on write while we block on
        // wait. The reader stops at the cap plus one byte so "exactly at
        // the cap" and "over the cap" stay distinguishable.
        let mut stdout = child.stdout.take().expect("stdout was piped above");
        let program = self.program.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout
                .by_ref()
                .take(MAX_CREDENTIAL_BYTES as u64 + 1)
                .read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + CREDENTIAL_COMMAND_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = reader.join();
                        return Err(CredentialError::TimedOut { program });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = reader.join();
                    return Err(CredentialError::SpawnFailed { program });
                }
            }
        };

        let raw = reader.join().unwrap_or_default();
        if raw.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::TooLarge { program });
        }
        if !status.success() {
            return Err(CredentialError::NonZeroExit {
                program,
                status: status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
            });
        }

        // `from_utf8_lossy` rather than a UTF-8 error: the error type
        // would be tempted to quote the offending bytes.
        let text = String::from_utf8_lossy(&raw);
        let trimmed = text.trim_matches(|c: char| c == '\n' || c == '\r' || c == ' ' || c == '\t');
        if trimmed.is_empty() {
            return Err(CredentialError::EmptyOutput { program });
        }
        Ok(UpstreamCredential::new(trimmed))
    }
}

/// Holds the resolved upstream credential and re-resolves it at most once
/// per [`CREDENTIAL_REFRESH_COOLDOWN`].
///
/// The refresh path exists for exactly one situation: the upstream
/// answered `401`, which usually means the key was rotated out from under
/// a long-lived daemon. Re-running the credential command once and
/// retrying that single request recovers without a restart. The cooldown
/// is what keeps a genuinely-invalid credential from turning every
/// request into a keychain prompt.
#[derive(Debug)]
pub struct CredentialStore {
    command: CredentialCommand,
    credential: UpstreamCredential,
    last_resolved: Instant,
}

impl CredentialStore {
    /// Resolves the credential once, eagerly. A gateway that cannot get a
    /// credential must fail at startup, not on the first real request.
    pub fn resolve(command: CredentialCommand) -> Result<Self, CredentialError> {
        let credential = command.resolve()?;
        Ok(Self {
            command,
            credential,
            last_resolved: Instant::now(),
        })
    }

    pub fn current(&self) -> &UpstreamCredential {
        &self.credential
    }

    /// Re-runs the credential command if the cooldown has elapsed.
    /// Returns `Ok(true)` when a fresh credential was actually obtained,
    /// `Ok(false)` when the cooldown suppressed the attempt. A failed
    /// re-resolution leaves the previous credential in place — a
    /// momentarily unavailable keychain must not take the gateway down.
    pub fn refresh_if_cooled_down(&mut self) -> Result<bool, CredentialError> {
        if self.last_resolved.elapsed() < CREDENTIAL_REFRESH_COOLDOWN {
            return Ok(false);
        }
        let fresh = self.command.resolve()?;
        self.credential = fresh;
        self.last_resolved = Instant::now();
        Ok(true)
    }
}

/// The 32-byte local capability token Claude Code presents to the
/// gateway.
///
/// This is **not** a provider credential. It authorizes use of a
/// loopback-bound proxy on this machine and has no value anywhere else.
/// It exists so that any other local process cannot spend this user's
/// budget merely by knowing the port number.
#[derive(Clone)]
pub struct LocalCapabilityToken(String);

impl std::fmt::Debug for LocalCapabilityToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalCapabilityToken(<redacted>)")
    }
}

impl LocalCapabilityToken {
    /// Loads the token from `path`, generating and persisting one at mode
    /// 0600 if none exists yet.
    pub fn load_or_create(path: &std::path::Path) -> std::io::Result<Self> {
        if let Ok(existing) = std::fs::read_to_string(path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return Ok(Self(trimmed.to_string()));
            }
        }
        let token = Self(random_hex_32());
        write_private(path, &token.0)?;
        Ok(token)
    }

    /// Constructs a token from an already-known value (tests, and the
    /// CLI's `gateway token` reader).
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The token as the CLI's `apiKeyHelper` must print it. This one IS
    /// meant to be handed to the agent — see the type's docs for why that
    /// is safe.
    pub fn expose_for_agent(&self) -> &str {
        &self.0
    }

    /// Constant-time comparison against a presented value.
    ///
    /// A `==` on `String` short-circuits at the first differing byte,
    /// which leaks the length of the matching prefix to anything that can
    /// time the response. The token is local-only, so the practical risk
    /// is small — but a variable-time comparison on a credential check is
    /// the kind of thing that is free to get right and awkward to explain
    /// later.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = presented.as_bytes();
        // Fold the length difference into the accumulator rather than
        // returning early on it, so the comparison runs the same way for
        // every input.
        let mut diff = (expected.len() ^ actual.len()) as u8;
        for i in 0..expected.len().max(actual.len()) {
            let e = expected.get(i).copied().unwrap_or(0);
            let a = actual.get(i).copied().unwrap_or(0);
            diff |= e ^ a;
        }
        diff == 0 && expected.len() == actual.len()
    }
}

/// 32 bytes of OS randomness, hex-encoded.
///
/// Read from `/dev/urandom` rather than pulling in a CSPRNG crate: this
/// is one call on one path at startup, and the operating system's own
/// entropy source is exactly what a dependency would wrap. A read failure
/// is not silently downgraded to a weaker source — it propagates, and the
/// gateway does not start.
fn random_hex_32() -> String {
    use std::io::Read as _;
    let mut bytes = [0u8; 32];
    let mut urandom = std::fs::File::open("/dev/urandom").expect("/dev/urandom is readable");
    urandom
        .read_exact(&mut bytes)
        .expect("/dev/urandom always yields the requested bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `contents` to `path` with mode 0600, creating it if needed.
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_renders_the_credential() {
        let credential = UpstreamCredential::new("sk-fake-super-secret-value");
        let rendered = format!("{credential:?}");
        assert!(!rendered.contains("sk-fake"));
        assert!(rendered.contains("<redacted>"));

        // The realistic leak is a `{:?}` on a *containing* struct.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            credential: UpstreamCredential,
        }
        let holder = Holder { credential };
        assert!(!format!("{holder:?}").contains("sk-fake"));
    }

    #[test]
    fn header_value_is_marked_sensitive() {
        let credential = UpstreamCredential::new("sk-fake-header-safe");
        let value = credential.header_value().unwrap();
        assert!(
            value.is_sensitive(),
            "hyper elides sensitive header values from its own Debug output"
        );
    }

    #[test]
    fn a_credential_with_a_newline_is_rejected_rather_than_smuggled_into_a_header() {
        let credential = UpstreamCredential::new("sk-fake\r\nx-injected: yes");
        assert_eq!(
            credential.header_value().unwrap_err(),
            CredentialError::NotHeaderSafe
        );
    }

    #[test]
    fn resolve_reads_stdout_and_trims_the_trailing_newline() {
        let command = CredentialCommand::new(
            "/bin/sh",
            vec![
                "-c".to_string(),
                "printf 'sk-fake-from-command\\n'".to_string(),
            ],
        );
        let credential = command.resolve().unwrap();
        assert_eq!(credential.len(), "sk-fake-from-command".len());
        assert_eq!(
            credential.header_value().unwrap().to_str().unwrap(),
            "sk-fake-from-command"
        );
    }

    #[test]
    fn a_non_zero_exit_is_an_error_whose_message_carries_no_output() {
        let command = CredentialCommand::new(
            "/bin/sh",
            vec![
                "-c".to_string(),
                "printf 'sk-fake-leaked'; echo 'stderr sk-fake-leaked' >&2; exit 3".to_string(),
            ],
        );
        let err = command.resolve().unwrap_err();
        let rendered = err.to_string();
        assert!(
            !rendered.contains("sk-fake"),
            "a credential error must never quote captured output: {rendered}"
        );
        assert!(matches!(err, CredentialError::NonZeroExit { .. }));
    }

    #[test]
    fn empty_output_is_an_error_not_an_empty_credential() {
        let command =
            CredentialCommand::new("/bin/sh", vec!["-c".to_string(), "printf ''".to_string()]);
        assert!(matches!(
            command.resolve().unwrap_err(),
            CredentialError::EmptyOutput { .. }
        ));
    }

    #[test]
    fn whitespace_only_output_is_empty_not_a_credential() {
        let command = CredentialCommand::new(
            "/bin/sh",
            vec!["-c".to_string(), "printf '  \\n\\t '".to_string()],
        );
        assert!(matches!(
            command.resolve().unwrap_err(),
            CredentialError::EmptyOutput { .. }
        ));
    }

    #[test]
    fn oversized_output_is_refused_rather_than_buffered() {
        let command = CredentialCommand::new(
            "/bin/sh",
            vec![
                "-c".to_string(),
                format!(
                    "head -c {} /dev/zero | tr '\\0' 'x'",
                    MAX_CREDENTIAL_BYTES + 100
                ),
            ],
        );
        assert!(matches!(
            command.resolve().unwrap_err(),
            CredentialError::TooLarge { .. }
        ));
    }

    #[test]
    fn a_missing_program_fails_to_spawn_rather_than_panicking() {
        let command = CredentialCommand::new("/definitely/not/a/real/program", vec![]);
        assert!(matches!(
            command.resolve().unwrap_err(),
            CredentialError::SpawnFailed { .. }
        ));
    }

    #[test]
    fn a_command_that_would_prompt_gets_no_stdin() {
        // `cat` with stdin at /dev/null reads EOF immediately and exits 0
        // with empty output — proving stdin is closed rather than hanging.
        let command = CredentialCommand::new("/bin/cat", vec![]);
        assert!(matches!(
            command.resolve().unwrap_err(),
            CredentialError::EmptyOutput { .. }
        ));
    }

    #[test]
    fn capability_token_is_persisted_at_mode_0600_and_reloaded() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.token");

        let first = LocalCapabilityToken::load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token file must not be world-readable");

        let second = LocalCapabilityToken::load_or_create(&path).unwrap();
        assert_eq!(
            first.expose_for_agent(),
            second.expose_for_agent(),
            "a restart must not invalidate an already-configured apiKeyHelper"
        );
        assert_eq!(first.expose_for_agent().len(), 64, "32 bytes, hex-encoded");
    }

    #[test]
    fn two_generated_tokens_differ() {
        let dir = tempfile::tempdir().unwrap();
        let a = LocalCapabilityToken::load_or_create(&dir.path().join("a")).unwrap();
        let b = LocalCapabilityToken::load_or_create(&dir.path().join("b")).unwrap();
        assert_ne!(a.expose_for_agent(), b.expose_for_agent());
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_value() {
        let token = LocalCapabilityToken::from_raw("abc123");
        assert!(token.matches("abc123"));
        assert!(!token.matches("abc124"));
        assert!(!token.matches("abc12"), "a prefix must not match");
        assert!(!token.matches("abc1234"), "an extension must not match");
        assert!(!token.matches(""));
    }

    #[test]
    fn token_debug_never_renders_the_token() {
        let token = LocalCapabilityToken::from_raw("deadbeef");
        assert!(!format!("{token:?}").contains("deadbeef"));
    }
}
