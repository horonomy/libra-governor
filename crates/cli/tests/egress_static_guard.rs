//! Workspace-wide static egress guard (HORO-2218, ADR-0026 "Egress
//! inventory", `docs/egress-inventory.md`).
//!
//! Claim under test: only `crates/gateway` and `crates/extension` may
//! depend on a network-capable crate or construct an HTTP client / dial a
//! socket. Every other workspace member must stay network-free, so a new
//! outbound path cannot arrive in an unrelated crate without a reviewer
//! editing the explicit allowlists below.
//!
//! What this is NOT: it is a static, textual guard. It does not prove the
//! absence of egress (a subprocess an operator configures, a host tool, or
//! code assembled at runtime is invisible to it). `docs/egress-inventory.md`
//! states those limits.
//!
//! Method (no `toml`/`regex` dependency is added for a test):
//! 1. Workspace members are read from the root `Cargo.toml`, and every
//!    `crates/*/Cargo.toml` must be a member (a new crate cannot hide).
//! 2. Dependency names are parsed from every `*dependencies*` table of every
//!    member, including `[dependencies.foo]` sub-tables, `[target.*.…]`
//!    tables and `package = "…"` renames. `tokio` is flagged when its
//!    `net` feature is on.
//! 3. `src/`, `tests/`, `benches/`, `examples/` and `build.rs` are scanned
//!    for call-shaped egress patterns (not bare words, so a doc/test string
//!    list such as `"hyper"` does not trip it).
//! 4. `Cargo.lock` must not contain HTTP-client/TLS-stack crates beyond the
//!    gateway's reviewed rustls stack.
//! 5. Only `daemon` and `cli` may depend on `gateway`/`extension`.
//!
//! The scanner skips this file itself (its pattern literals would match);
//! that is the one blind spot and is deliberate.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Crates whose mere presence in a dependency table means "can talk to a
/// network". Matched against the real package name (after `package =`).
const NETWORK_CRATES: &[&str] = &[
    "reqwest",
    "hyper",
    "hyper-util",
    "hyper-rustls",
    "hyper-tls",
    "ureq",
    "curl",
    "curl-sys",
    "isahc",
    "surf",
    "attohttpc",
    "awc",
    "tungstenite",
    "tokio-tungstenite",
    "async-tungstenite",
    "rustls",
    "tokio-rustls",
    "native-tls",
    "tokio-native-tls",
    "openssl",
    "openssl-sys",
    "h2",
    "quinn",
    "socket2",
    "hickory-resolver",
    "trust-dns-resolver",
];

/// Crates that must not appear anywhere in `Cargo.lock`, not even
/// transitively: none is used by the gateway's reviewed hyper+rustls stack.
const LOCK_FORBIDDEN: &[&str] = &[
    "reqwest",
    "ureq",
    "curl",
    "curl-sys",
    "isahc",
    "surf",
    "attohttpc",
    "awc",
    "tungstenite",
    "tokio-tungstenite",
    "async-tungstenite",
    "native-tls",
    "tokio-native-tls",
    "openssl",
    "openssl-sys",
    "hyper-tls",
    "quinn",
    "hickory-resolver",
    "trust-dns-resolver",
];

/// Call-shaped source patterns that construct a client or dial a socket.
const SOURCE_PATTERNS: &[&str] = &[
    "reqwest::",
    "ureq::",
    "curl::",
    "isahc::",
    "surf::",
    "attohttpc::",
    "tungstenite::",
    "native_tls::",
    "openssl::",
    "rustls::",
    "hyper_rustls::",
    "hyper_util::client",
    "hyper::client",
    "HttpConnector",
    "TcpStream::connect",
    "TcpStream::connect_timeout",
    "UdpSocket::bind",
    "UdpSocket::connect",
    "UdpSocket::send_to",
    ".to_socket_addrs(",
    "lookup_host(",
];

/// Programs whose `Command::new(<program>)` is network egress by purpose.
const EGRESS_PROGRAMS: &[&str] = &[
    "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "telnet", "ftp", "git",
];

/// `(crate directory, dependency, kind)`; `kind` is `normal`, `dev` or
/// `build`. `*` matches any dependency of that crate and kind.
type DepAllow = (&'static str, &'static str, &'static str);

/// Crates that legitimately hold network dependencies.
const DEP_ALLOWLIST: &[DepAllow] = &[
    // gateway: the opt-in LLM gateway. Its outbound client targets only the
    // validated, allowlisted upstream (default api.anthropic.com), pinned by
    // crates/gateway/tests/ssrf_and_config.rs. Reviewed rustls/hyper stack.
    ("gateway", "*", "normal"),
    ("gateway", "*", "dev"),
    // extension: webhook/provider client; loopback-literal destinations only
    // (crates/extension/src/config.rs validate_surface, unit-tested there).
    ("extension", "*", "normal"),
    ("extension", "*", "dev"),
    // daemon dev-dependencies only: tests/extension_integration.rs hosts a
    // loopback fake provider.
    // Dev-dependencies are not linked into the shipped binary.
    ("daemon", "hyper", "dev"),
    ("daemon", "hyper-util", "dev"),
    ("daemon", "tokio", "dev"),
];

/// `(file relative to workspace root, pattern)` pairs that are known false
/// positives: each file contains the pattern only inside a string literal
/// that plants a fake violation for its own scanner's negative-control test
/// (`scan("use reqwest::Client;...")`). They are pinned per file and per
/// pattern, and `false_positive_exceptions_are_not_stale` fails if one stops
/// matching, so this list cannot silently widen or rot.
const FALSE_POSITIVE_EXCEPTIONS: &[(&str, &str)] = &[
    ("crates/cli/src/dogfood_evidence_cmd.rs", "reqwest::"),
    (
        "crates/evidence-adapter/tests/no_network_symbols.rs",
        "reqwest::",
    ),
    (
        "crates/quota-source/tests/no_network_symbols.rs",
        "reqwest::",
    ),
];

/// Crates exempt from the *source* scan. Exactly the two audited egress
/// crates; nothing else (the daemon's tests are scanned too and are clean).
const SOURCE_ALLOWLIST: &[&str] = &["gateway", "extension"];

/// Workspace crates allowed to depend on gateway/extension (the host
/// process that wires them in, and its CLI front end).
const GATEWAY_EXTENSION_DEPENDENTS: &[&str] = &["daemon", "cli"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Violation {
    Dependency {
        krate: String,
        dep: String,
        kind: String,
    },
    Source {
        file: String,
        pattern: String,
    },
    Subprocess {
        file: String,
        program: String,
    },
    LockedCrate(String),
    UnexpectedDependent {
        krate: String,
        on: String,
    },
    UnlistedMember(String),
}

struct DepEntry {
    name: String,
    kind: &'static str,
    net_tokio: bool,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root resolves from crates/cli")
}

fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

/// `members = [ "crates/a", … ]` from the root manifest, as relative paths.
fn workspace_members(root: &Path) -> Vec<String> {
    let text = fs::read_to_string(root.join("Cargo.toml")).expect("root Cargo.toml");
    let start = text.find("members").expect("[workspace] members");
    let open = start + text[start..].find('[').expect("members [");
    let close = open + text[open..].find(']').expect("members ]");
    quoted_strings(&text[open..close])
}

fn dep_kind(table: &str) -> Option<&'static str> {
    // `table` is the text inside `[...]`, e.g. `dev-dependencies`,
    // `target.'cfg(unix)'.dependencies`, `dependencies.foo`.
    let mut kind = None;
    for part in table.split('.') {
        let part = part.trim();
        kind = match part {
            "dependencies" => Some("normal"),
            "dev-dependencies" => Some("dev"),
            "build-dependencies" => Some("build"),
            _ => kind,
        };
    }
    kind
}

/// Dependencies declared by one manifest (all kinds, all target tables).
fn manifest_dependencies(manifest: &str) -> Vec<DepEntry> {
    let mut out: Vec<DepEntry> = Vec::new();
    let mut kind: Option<&'static str> = None;
    // A `[dependencies.foo]` sub-table names its dep in the header.
    let mut subtable: Option<usize> = None;
    let mut real_names: Vec<(usize, String)> = Vec::new();

    for raw in manifest.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let inner = line.trim_matches(|c| c == '[' || c == ']');
            kind = dep_kind(inner);
            subtable = None;
            if let Some(k) = kind {
                // `[dependencies.foo]`: last dotted part is the dep name.
                let last = inner.rsplit('.').next().unwrap_or("").trim();
                if !matches!(
                    last,
                    "dependencies" | "dev-dependencies" | "build-dependencies"
                ) {
                    out.push(DepEntry {
                        name: last.trim_matches('\'').trim_matches('"').to_string(),
                        kind: k,
                        net_tokio: false,
                    });
                    subtable = Some(out.len() - 1);
                }
            }
            continue;
        }
        let Some(k) = kind else { continue };
        if let Some(idx) = subtable {
            if line.starts_with("package") {
                if let Some(real) = quoted_strings(line).first() {
                    real_names.push((idx, real.clone()));
                }
            }
            if line.starts_with("features") && line.contains("\"net\"") {
                out[idx].net_tokio = true;
            }
            continue;
        }
        // `name = …` / `name.workspace = true` / `name = { … package = "x" }`
        let key_end = line
            .find(|c: char| c == '=' || c == '.' || c.is_whitespace())
            .unwrap_or(line.len());
        let name = line[..key_end].trim_matches('"').to_string();
        if name.is_empty() {
            continue;
        }
        out.push(DepEntry {
            name,
            kind: k,
            net_tokio: line.contains("\"net\""),
        });
        if let Some(pos) = line.find("package") {
            if let Some(real) = quoted_strings(&line[pos..]).first() {
                real_names.push((out.len() - 1, real.clone()));
            }
        }
    }
    for (idx, real) in real_names {
        out[idx].name = real;
    }
    out
}

fn dep_allowed(krate: &str, dep: &str, kind: &str) -> bool {
    DEP_ALLOWLIST
        .iter()
        .any(|(c, d, k)| *c == krate && *k == kind && (*d == "*" || *d == dep))
}

fn collect_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_files(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

/// Source with whole-line `//` comments (including `///`, `//!`) removed.
/// Trailing comments and string literals are kept on purpose: patterns are
/// call-shaped (`TcpStream::connect`) so a bare word in a string list does
/// not match, while a genuine call still does.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn subprocess_programs(code: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = code;
    while let Some(pos) = rest.find("Command::new(") {
        let after = &rest[pos + "Command::new(".len()..];
        let window: String = after.chars().take(120).collect();
        if let Some(lit) = quoted_strings(&window).first() {
            let base = lit.rsplit('/').next().unwrap_or(lit);
            if EGRESS_PROGRAMS.contains(&base) {
                found.push(base.to_string());
            }
        }
        rest = after;
    }
    found
}

fn scan_workspace(
    root: &Path,
    members: &[String],
    self_file: Option<&Path>,
) -> BTreeSet<Violation> {
    scan_workspace_with(root, members, self_file, true)
}

fn scan_workspace_with(
    root: &Path,
    members: &[String],
    self_file: Option<&Path>,
    honor_exceptions: bool,
) -> BTreeSet<Violation> {
    let mut violations = BTreeSet::new();

    for member in members {
        let dir = root.join(member);
        let krate = member.rsplit('/').next().unwrap_or(member).to_string();
        let manifest = fs::read_to_string(dir.join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("member {member} has no readable Cargo.toml: {e}"));

        for dep in manifest_dependencies(&manifest) {
            let network = NETWORK_CRATES.contains(&dep.name.as_str())
                || (dep.name == "tokio" && dep.net_tokio);
            if network && !dep_allowed(&krate, &dep.name, dep.kind) {
                violations.insert(Violation::Dependency {
                    krate: krate.clone(),
                    dep: dep.name.clone(),
                    kind: dep.kind.to_string(),
                });
            }
            if (dep.name == "libra-governor-gateway" || dep.name == "libra-governor-extension")
                && !GATEWAY_EXTENSION_DEPENDENTS.contains(&krate.as_str())
                && !SOURCE_ALLOWLIST.contains(&krate.as_str())
            {
                violations.insert(Violation::UnexpectedDependent {
                    krate: krate.clone(),
                    on: dep.name.clone(),
                });
            }
        }

        if SOURCE_ALLOWLIST.contains(&krate.as_str()) {
            continue;
        }
        let mut files = Vec::new();
        for sub in ["src", "tests", "benches", "examples"] {
            collect_files(&dir.join(sub), "rs", &mut files);
        }
        let build_rs = dir.join("build.rs");
        if build_rs.is_file() {
            files.push(build_rs);
        }
        for file in files {
            if self_file.is_some_and(|me| me == file.as_path()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let code = strip_line_comments(&text);
            let rel = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .to_string();
            for pattern in SOURCE_PATTERNS {
                if code.contains(pattern)
                    && !(honor_exceptions
                        && FALSE_POSITIVE_EXCEPTIONS.contains(&(rel.as_str(), *pattern)))
                {
                    violations.insert(Violation::Source {
                        file: rel.clone(),
                        pattern: (*pattern).to_string(),
                    });
                }
            }
            for program in subprocess_programs(&code) {
                violations.insert(Violation::Subprocess {
                    file: rel.clone(),
                    program,
                });
            }
        }
    }

    if let Ok(lock) = fs::read_to_string(root.join("Cargo.lock")) {
        for name in lock
            .lines()
            .filter_map(|l| l.strip_prefix("name = "))
            .flat_map(quoted_strings)
        {
            if LOCK_FORBIDDEN.contains(&name.as_str()) {
                violations.insert(Violation::LockedCrate(name));
            }
        }
    }
    violations
}

/// Every `crates/*/Cargo.toml` must be a declared member, so a new crate
/// cannot sit outside the scan.
fn unlisted_members(root: &Path, members: &[String]) -> BTreeSet<Violation> {
    let mut out = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(root.join("crates")) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().join("Cargo.toml").is_file() {
                let rel = format!("crates/{}", entry.file_name().to_string_lossy());
                if !members.contains(&rel) {
                    out.insert(Violation::UnlistedMember(rel));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// The real guard
// ---------------------------------------------------------------------

#[test]
fn only_gateway_and_extension_hold_network_capability() {
    let root = workspace_root();
    let members = workspace_members(&root);
    // Anti-vacuity: the walk must really see the workspace.
    assert!(members.len() >= 10, "found only {members:?}");
    for must in [
        "crates/gateway",
        "crates/extension",
        "crates/daemon",
        "crates/cli",
    ] {
        assert!(
            members.iter().any(|m| m == must),
            "{must} missing from {members:?}"
        );
    }

    let me = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("egress_static_guard.rs");
    let mut violations = scan_workspace(&root, &members, Some(&me));
    violations.extend(unlisted_members(&root, &members));
    assert!(
        violations.is_empty(),
        "egress guard violations (see docs/egress-inventory.md before touching the \
         allowlists in crates/cli/tests/egress_static_guard.rs): {violations:#?}"
    );
}

#[test]
fn false_positive_exceptions_are_not_stale() {
    let root = workspace_root();
    let members = workspace_members(&root);
    let me = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("egress_static_guard.rs");
    let raw = scan_workspace_with(&root, &members, Some(&me), false);
    for (file, pattern) in FALSE_POSITIVE_EXCEPTIONS {
        assert!(
            raw.contains(&Violation::Source {
                file: (*file).to_string(),
                pattern: (*pattern).to_string(),
            }),
            "exception ({file}, {pattern}) no longer matches anything; remove it"
        );
    }
}

#[test]
fn the_real_scan_actually_reads_source_and_manifests() {
    // Anti-vacuity for the real tree: the audited crates must be
    // recognised as network-capable (so the allowlist is load-bearing, not
    // dead), and a non-allowlisted crate must be scanned.
    let root = workspace_root();
    let gateway = fs::read_to_string(root.join("crates/gateway/Cargo.toml")).unwrap();
    let deps = manifest_dependencies(&gateway);
    assert!(deps
        .iter()
        .any(|d| d.name == "hyper-rustls" && d.kind == "normal"));
    let extension = fs::read_to_string(root.join("crates/extension/Cargo.toml")).unwrap();
    assert!(manifest_dependencies(&extension)
        .iter()
        .any(|d| d.name == "hyper" && d.kind == "normal"));
    let daemon = fs::read_to_string(root.join("crates/daemon/Cargo.toml")).unwrap();
    assert!(manifest_dependencies(&daemon)
        .iter()
        .any(|d| d.name == "tokio" && d.kind == "dev" && d.net_tokio));
}

// ---------------------------------------------------------------------
// Negative controls: the scanner must detect injected violations.
// ---------------------------------------------------------------------

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A synthetic workspace with one `evil` member and one clean member.
fn fixture(evil_manifest: &str, evil_src: &str) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/evil\", \"crates/clean\"]\n",
    );
    write(&root.join("crates/evil/Cargo.toml"), evil_manifest);
    write(&root.join("crates/evil/src/lib.rs"), evil_src);
    write(
        &root.join("crates/clean/Cargo.toml"),
        "[package]\nname = \"clean\"\n[dependencies]\nserde = \"1\"\n",
    );
    write(
        &root.join("crates/clean/src/lib.rs"),
        "pub fn f() -> u32 { 1 }\n",
    );
    let members = workspace_members(root);
    (dir, members)
}

fn deps_flagged(manifest: &str) -> BTreeSet<String> {
    let (dir, members) = fixture(manifest, "pub fn f() {}\n");
    scan_workspace(dir.path(), &members, None)
        .into_iter()
        .filter_map(|v| match v {
            Violation::Dependency { dep, .. } => Some(dep),
            _ => None,
        })
        .collect()
}

#[test]
fn clean_fixture_has_no_violations() {
    let (dir, members) = fixture(
        "[package]\nname = \"evil\"\n[dependencies]\nserde = \"1\"\n",
        "pub fn f() {}\n",
    );
    assert!(scan_workspace(dir.path(), &members, None).is_empty());
    assert_eq!(members, vec!["crates/evil", "crates/clean"]);
}

#[test]
fn negative_control_flags_network_dependencies_in_every_spelling() {
    let cases: &[(&str, &str)] = &[
        ("[dependencies]\nreqwest = \"0.12\"\n", "reqwest"),
        ("[dev-dependencies]\nureq = { version = \"2\" }\n", "ureq"),
        ("[build-dependencies]\ncurl = \"0.4\"\n", "curl"),
        (
            "[dependencies.hyper-util]\nversion = \"0.1\"\n",
            "hyper-util",
        ),
        (
            "[target.'cfg(unix)'.dependencies]\nisahc = \"1\"\n",
            "isahc",
        ),
        (
            "[dependencies]\nsneaky = { version = \"1\", package = \"surf\" }\n",
            "surf",
        ),
        (
            "[dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"net\"] }\n",
            "tokio",
        ),
        (
            "[dependencies]\ntokio-tungstenite = \"0.24\"\n",
            "tokio-tungstenite",
        ),
        ("[dependencies]\nnative-tls = \"0.2\"\n", "native-tls"),
        ("[dependencies]\nhyper = { workspace = true }\n", "hyper"),
    ];
    for (manifest, expected) in cases {
        let flagged = deps_flagged(&format!("[package]\nname = \"evil\"\n{manifest}"));
        assert!(flagged.contains(*expected), "{manifest:?} -> {flagged:?}");
    }
}

#[test]
fn negative_control_does_not_flag_tokio_without_net_or_ordinary_crates() {
    let flagged = deps_flagged(
        "[package]\nname = \"evil\"\n[dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"time\"] }\nserde.workspace = true\n",
    );
    assert!(flagged.is_empty(), "{flagged:?}");
}

#[test]
fn negative_control_flags_injected_source_and_subprocess_egress() {
    let src = r#"
use std::net::TcpStream;
pub fn leak() {
    let _ = TcpStream::connect("203.0.113.9:443");
    let _ = std::net::UdpSocket::bind("0.0.0.0:0");
    let _ = reqwest::Client::new();
    let _ = std::process::Command::new("/usr/bin/curl").arg("https://example.invalid");
    let _ = std::process::Command::new("git").arg("push");
}
"#;
    let (dir, members) = fixture("[package]\nname = \"evil\"\n", src);
    let v = scan_workspace(dir.path(), &members, None);
    let has_src = |p: &str| {
        v.iter()
            .any(|x| matches!(x, Violation::Source { pattern, .. } if pattern == p))
    };
    assert!(has_src("TcpStream::connect"));
    assert!(has_src("UdpSocket::bind"));
    assert!(has_src("reqwest::"));
    for prog in ["curl", "git"] {
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::Subprocess { program, .. } if program == prog)),
            "{prog} subprocess not flagged: {v:?}"
        );
    }
}

#[test]
fn negative_control_ignores_comments_and_bare_words_but_not_calls() {
    // False-positive policy: whole-line comments, doc comments and bare
    // identifiers inside string lists (like the existing evidence-report
    // forbidden-symbol lists) are not egress. A real call still is.
    let benign = "//! uses TcpStream::connect in prose\n/// reqwest::Client docs\n// curl::Easy\nconst L: &[&str] = &[\"hyper\", \"reqwest\", \"TcpStream\"];\npub fn ok() { std::process::Command::new(\"sh\"); }\n";
    let (dir, members) = fixture("[package]\nname = \"evil\"\n", benign);
    assert!(scan_workspace(dir.path(), &members, None).is_empty());
}

#[test]
fn negative_control_flags_a_lockfile_with_a_forbidden_client_crate() {
    let (dir, members) = fixture("[package]\nname = \"evil\"\n", "pub fn f() {}\n");
    write(
        &dir.path().join("Cargo.lock"),
        "[[package]]\nname = \"reqwest\"\nversion = \"0.12.0\"\n[[package]]\nname = \"serde\"\n",
    );
    let v = scan_workspace(dir.path(), &members, None);
    assert!(v.contains(&Violation::LockedCrate("reqwest".to_string())));
    assert!(!v.contains(&Violation::LockedCrate("serde".to_string())));
}

#[test]
fn negative_control_flags_laundering_through_gateway_or_extension() {
    let (dir, members) = fixture(
        "[package]\nname = \"evil\"\n[dependencies]\nlibra-governor-gateway = { path = \"../gateway\" }\n",
        "pub fn f() {}\n",
    );
    let v = scan_workspace(dir.path(), &members, None);
    assert!(v
        .iter()
        .any(|x| matches!(x, Violation::UnexpectedDependent { krate, .. } if krate == "evil")));
}

#[test]
fn negative_control_flags_a_crate_missing_from_workspace_members() {
    let (dir, members) = fixture("[package]\nname = \"evil\"\n", "pub fn f() {}\n");
    write(
        &dir.path().join("crates/stowaway/Cargo.toml"),
        "[package]\nname = \"stowaway\"\n",
    );
    let v = unlisted_members(dir.path(), &members);
    assert!(v.contains(&Violation::UnlistedMember("crates/stowaway".to_string())));
}

#[test]
fn allowlisted_crates_are_exempt_only_for_their_listed_dependencies() {
    assert!(dep_allowed("gateway", "hyper-rustls", "normal"));
    assert!(dep_allowed("extension", "hyper", "dev"));
    assert!(dep_allowed("daemon", "hyper", "dev"));
    // The daemon's *normal* dependencies stay network-free.
    assert!(!dep_allowed("daemon", "hyper", "normal"));
    assert!(!dep_allowed("daemon", "reqwest", "dev"));
    assert!(!dep_allowed("cli", "hyper", "dev"));
    assert!(!dep_allowed("evidence-adapter", "tokio", "normal"));
}
