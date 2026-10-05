//! Measured, bounded local code identity. This neither executes nor grants trust.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{
    fstat, open, openat, readlinkat_raw, statat, AtFlags, FileType, Mode, OFlags, Stat,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::contract::ValidatedManifest;
use super::RegistryFailure;

const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LINKS: usize = 40;
const CHUNK_BYTES: usize = 64 * 1024;
const HEADER_BYTES: usize = 256;

fn fail(reason: &'static str) -> RegistryFailure {
    RegistryFailure::new("identity", reason)
}

fn digest_string(bytes: &[u8]) -> String {
    let hexadecimal: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hexadecimal}")
}

fn literal(value: &str) -> Result<(), RegistryFailure> {
    if value.contains(['\0', '\r', '\n', '`']) || value.contains("$(") || value.contains("${") {
        return Err(fail("unsupported_invocation"));
    }
    Ok(())
}

fn absolute(value: &str) -> Result<PathBuf, RegistryFailure> {
    literal(value)?;
    let path = Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
        || path.file_name().is_none()
    {
        return Err(fail("unsupported_invocation"));
    }
    Ok(path.to_owned())
}

// Directory content changes do not change which directory was traversed. File
// and symlink observations include change times to catch same-inode rewrites.
fn stamp(stat: &Stat) -> String {
    let base = format!(
        "{}:{}:{}:{}:{}",
        stat.st_dev, stat.st_ino, stat.st_mode, stat.st_uid, stat.st_gid
    );
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        base
    } else {
        format!(
            "{base}:{}:{}:{}:{}:{}:{}",
            stat.st_nlink,
            stat.st_size,
            stat.st_mtime,
            stat.st_mtime_nsec,
            stat.st_ctime,
            stat.st_ctime_nsec
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    path: PathBuf,
    stamp: String,
    link: Option<PathBuf>,
}

struct Resolved {
    path: PathBuf,
    file: File,
    observations: Vec<Observation>,
    stamp: String,
    inode: String,
    size: u64,
    executable: bool,
}

enum Part {
    Root,
    Parent,
    Name(OsString),
}

fn parts(path: &Path) -> VecDeque<Part> {
    path.components()
        .filter_map(|component| match component {
            Component::RootDir => Some(Part::Root),
            Component::ParentDir => Some(Part::Parent),
            Component::Normal(name) => Some(Part::Name(name.to_owned())),
            _ => None,
        })
        .collect()
}

fn root_directory() -> Result<File, RegistryFailure> {
    open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| fail("identity_unavailable"))
}

/// Every component is opened relative to an already held directory. Observing
/// links explicitly also bounds resolution and preserves a venv's launch path.
fn resolve(path: &Path) -> Result<Resolved, RegistryFailure> {
    let mut queue = parts(path);
    let mut directories = vec![root_directory()?];
    let mut current = PathBuf::from("/");
    let mut observations = Vec::new();
    let mut links = 0;
    while let Some(part) = queue.pop_front() {
        let name = match part {
            Part::Root => {
                directories.truncate(1);
                current = PathBuf::from("/");
                continue;
            }
            Part::Parent => {
                if directories.len() > 1 {
                    directories.pop();
                    current.pop();
                }
                continue;
            }
            Part::Name(name) => name,
        };
        let parent = directories
            .last()
            .ok_or_else(|| fail("identity_unavailable"))?;
        let before = statat(parent, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| fail("identity_unavailable"))?;
        let kind = FileType::from_raw_mode(before.st_mode);
        let next = current.join(&name);
        if kind == FileType::Symlink {
            links += 1;
            if links > MAX_LINKS {
                return Err(fail("identity_limit"));
            }
            let mut bytes = [0_u8; 4096];
            let count = readlinkat_raw(parent, &name, &mut bytes[..])
                .map_err(|_| fail("identity_unavailable"))?;
            if count == bytes.len() {
                return Err(fail("identity_limit"));
            }
            let target = PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..count]));
            let after = statat(parent, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| fail("identity_drift"))?;
            if stamp(&before) != stamp(&after) {
                return Err(fail("identity_drift"));
            }
            observations.push(Observation {
                path: next,
                stamp: stamp(&before),
                link: Some(target.clone()),
            });
            let mut expanded = parts(&target);
            expanded.append(&mut queue);
            queue = expanded;
            continue;
        }
        let expected = if queue.is_empty() {
            FileType::RegularFile
        } else {
            FileType::Directory
        };
        if kind != expected {
            return Err(fail("identity_unsafe_file"));
        }
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if expected == FileType::Directory {
            flags |= OFlags::DIRECTORY;
        }
        let file = File::from(
            openat(parent, &name, flags, Mode::empty())
                .map_err(|_| fail("identity_unavailable"))?,
        );
        let opened = fstat(&file).map_err(|_| fail("identity_unavailable"))?;
        if stamp(&before) != stamp(&opened) {
            return Err(fail("identity_drift"));
        }
        observations.push(Observation {
            path: next.clone(),
            stamp: stamp(&opened),
            link: None,
        });
        if expected == FileType::RegularFile {
            return Ok(Resolved {
                path: next,
                file,
                observations,
                stamp: stamp(&opened),
                inode: format!("{}:{}", opened.st_dev, opened.st_ino),
                size: u64::try_from(opened.st_size).map_err(|_| fail("identity_limit"))?,
                executable: opened.st_mode & 0o111 != 0,
            });
        }
        current = next;
        directories.push(file);
    }
    Err(fail("identity_unsafe_file"))
}

struct Measured {
    literal: PathBuf,
    resolved: Resolved,
    kind: String,
    digest: String,
    header: Vec<u8>,
}

fn unchanged(literal: &Path, original: &Resolved) -> Result<(), RegistryFailure> {
    let fresh = resolve(literal)?;
    let held = fstat(&original.file).map_err(|_| fail("identity_drift"))?;
    if fresh.path != original.path
        || fresh.observations != original.observations
        || fresh.stamp != original.stamp
        || stamp(&held) != original.stamp
    {
        return Err(fail("identity_drift"));
    }
    Ok(())
}

fn measure(raw: &Value, total: &mut u64) -> Result<Measured, RegistryFailure> {
    let literal = absolute(
        raw["path"]
            .as_str()
            .ok_or_else(|| fail("identity_unavailable"))?,
    )?;
    let mut resolved = resolve(&literal)?;
    if resolved.size > MAX_FILE_BYTES || *total + resolved.size > MAX_TOTAL_BYTES {
        return Err(fail("identity_limit"));
    }
    let mut digest = Sha256::new();
    let mut bytes = [0_u8; CHUNK_BYTES];
    let mut size = 0_u64;
    let mut header = Vec::with_capacity(HEADER_BYTES);
    loop {
        let count = match resolved.file.read(&mut bytes) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result.map_err(|_| fail("identity_unavailable"))?,
        };
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > MAX_FILE_BYTES || *total + size > MAX_TOTAL_BYTES {
            return Err(fail("identity_limit"));
        }
        let keep = (HEADER_BYTES - header.len()).min(count);
        header.extend_from_slice(&bytes[..keep]);
        digest.update(&bytes[..count]);
    }
    if size != resolved.size {
        return Err(fail("identity_drift"));
    }
    unchanged(&literal, &resolved)?;
    let digest = digest_string(&digest.finalize());
    if raw["digest"].as_str() != Some(digest.as_str()) {
        return Err(fail("identity_digest_mismatch"));
    }
    *total += size;
    Ok(Measured {
        literal,
        resolved,
        kind: raw["kind"]
            .as_str()
            .ok_or_else(|| fail("identity_unavailable"))?
            .to_owned(),
        digest,
        header,
    })
}

fn python(name: &str) -> bool {
    let Some(version) = name.strip_prefix("python") else {
        return false;
    };
    let version = version.trim_end_matches(['d', 't']);
    version.is_empty()
        || version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
}

fn refused_program(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "csh"
            | "tcsh"
            | "ash"
            | "busybox"
            | "env"
            | "which"
            | "xargs"
            | "node"
            | "nodejs"
            | "ruby"
            | "perl"
            | "lua"
            | "php"
            | "julia"
            | "deno"
            | "bun"
            | "pwsh"
            | "powershell"
            | "java"
            | "pypy"
            | "pypy3"
            | "pythonw"
    )
}

fn names(path: &Path, resolved: &Path) -> Result<(String, String), RegistryFailure> {
    let name = |path: &Path| {
        path.file_name()
            .and_then(|v| v.to_str())
            .map(str::to_owned)
            .ok_or_else(|| fail("unsupported_invocation"))
    };
    let pair = (name(path)?, name(resolved)?);
    if refused_program(&pair.0) || refused_program(&pair.1) {
        return Err(fail("unsupported_invocation"));
    }
    Ok(pair)
}

fn entrypoint<'a>(
    files: &'a [Measured],
    literal: &Path,
) -> Result<(&'a Measured, Resolved), RegistryFailure> {
    let observed = resolve(literal)?;
    let file = files
        .iter()
        .find(|file| file.resolved.path == observed.path)
        .ok_or_else(|| fail("identity_undeclared_entrypoint"))?;
    if file.kind != "entrypoint" {
        return Err(fail("identity_undeclared_entrypoint"));
    }
    if file.resolved.stamp != observed.stamp {
        return Err(fail("identity_drift"));
    }
    Ok((file, observed))
}

fn projection(file: &Measured) -> Result<Value, RegistryFailure> {
    Ok(json!({
        "literal_path": file.literal.to_str().ok_or_else(|| fail("unsupported_invocation"))?,
        "resolved_path": file.resolved.path.to_str().ok_or_else(|| fail("unsupported_invocation"))?,
        "kind": file.kind,
        "digest": file.digest
    }))
}

fn shebang(header: &[u8]) -> Result<PathBuf, RegistryFailure> {
    let line = header.split(|b| *b == b'\n').next().unwrap_or_default();
    if line.len() >= HEADER_BYTES {
        return Err(fail("unsupported_invocation"));
    }
    let interpreter =
        std::str::from_utf8(&line[2..]).map_err(|_| fail("unsupported_invocation"))?;
    if interpreter.is_empty() || interpreter.chars().any(char::is_whitespace) {
        return Err(fail("unsupported_invocation"));
    }
    absolute(interpreter)
}

/// Return only a Libra-local digest of the measured declared closure and literal
/// invocation. Native binaries retain explicit ambient OS/dependency authority;
/// their names and hashes cannot prove a complete dynamic dependency closure.
pub fn measure_identity(manifest: &ValidatedManifest) -> Result<String, RegistryFailure> {
    let value = manifest.value();
    let declared = value["runtime_files"]
        .as_array()
        .ok_or_else(|| fail("identity_unavailable"))?;
    if declared.len() > MAX_FILES {
        return Err(fail("identity_limit"));
    }
    let launch = absolute(
        value["launch"]["executable"]
            .as_str()
            .ok_or_else(|| fail("identity_unavailable"))?,
    )?;
    let argv = value["launch"]["argv"]
        .as_array()
        .ok_or_else(|| fail("identity_unavailable"))?;
    for arg in argv {
        literal(arg.as_str().ok_or_else(|| fail("unsupported_invocation"))?)?;
    }
    let mut files = Vec::with_capacity(declared.len());
    let mut paths = HashSet::new();
    let mut inodes = HashSet::new();
    let mut total = 0;
    for raw in declared {
        let file = measure(raw, &mut total)?;
        if !paths.insert(file.resolved.path.clone()) || !inodes.insert(file.resolved.inode.clone())
        {
            return Err(fail("identity_alias"));
        }
        files.push(file);
    }
    let (executable, launch_observation) = entrypoint(&files, &launch)?;
    if !executable.resolved.executable {
        return Err(fail("unsupported_invocation"));
    }
    let (literal_name, resolved_name) = names(&launch, &executable.resolved.path)?;
    let mut material = BTreeMap::new();
    material.insert("identity_version", json!(1));
    material.insert("literal_executable", json!(launch.to_str()));
    material.insert(
        "resolved_executable",
        json!(executable.resolved.path.to_str()),
    );
    material.insert("executable_digest", json!(executable.digest));
    material.insert("argv", json!(argv));
    let mut other_invocations = Vec::new();
    if python(&literal_name) || python(&resolved_name) {
        if executable.header.starts_with(b"#!") {
            return Err(fail("unsupported_invocation"));
        }
        let script_path = absolute(
            argv.first()
                .and_then(Value::as_str)
                .ok_or_else(|| fail("unsupported_invocation"))?,
        )?;
        let (script, observed) = entrypoint(&files, &script_path)?;
        other_invocations.push((script_path, observed));
        material.insert("launch_kind", json!("python_script"));
        material.insert("script", projection(script)?);
    } else if executable.header.starts_with(b"#!") {
        let interpreter_path = shebang(&executable.header)?;
        let (interpreter, observed) = entrypoint(&files, &interpreter_path)?;
        let (literal_name, resolved_name) = names(&interpreter_path, &interpreter.resolved.path)?;
        if !(python(&literal_name) || python(&resolved_name))
            || interpreter.header.starts_with(b"#!")
            || !interpreter.resolved.executable
        {
            return Err(fail("unsupported_invocation"));
        }
        other_invocations.push((interpreter_path, observed));
        material.insert("launch_kind", json!("python_shebang"));
        material.insert("script", projection(executable)?);
        material.insert("shebang_interpreter", projection(interpreter)?);
    } else {
        material.insert("launch_kind", json!("native_declared_closure"));
    }
    files.sort_by(|a, b| a.resolved.path.cmp(&b.resolved.path));
    let entries = files
        .iter()
        .map(projection)
        .collect::<Result<Vec<_>, _>>()?;
    material.insert("runtime_files", json!(entries));
    for file in &files {
        unchanged(&file.literal, &file.resolved)?;
    }
    unchanged(&launch, &launch_observation)?;
    for (path, observed) in other_invocations {
        unchanged(&path, &observed)?;
    }
    let mut material = serde_json::to_value(material).map_err(|_| fail("identity_unavailable"))?;
    material.sort_all_objects();
    let bytes = serde_json::to_vec(&material).map_err(|_| fail("identity_unavailable"))?;
    Ok(digest_string(&Sha256::digest(bytes)))
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn held_observation_rejects_content_replacement_and_same_path_inode_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("code");
        std::fs::write(&path, b"one").unwrap();
        let observed = resolve(&path).unwrap();
        std::fs::write(&path, b"two").unwrap();
        assert_eq!(
            unchanged(&path, &observed).unwrap_err().reason,
            "identity_drift"
        );
        let observed = resolve(&path).unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"two").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            unchanged(&path, &observed).unwrap_err().reason,
            "identity_drift"
        );
    }

    #[test]
    fn held_observation_rejects_link_retarget_and_parent_namespace_replacement() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        let code = directory.join("code");
        std::fs::write(&code, b"one").unwrap();
        let alias = root.path().join("alias");
        symlink(&code, &alias).unwrap();
        let observed = resolve(&alias).unwrap();
        let other = directory.join("other");
        std::fs::write(&other, b"one").unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        assert_eq!(
            unchanged(&alias, &observed).unwrap_err().reason,
            "identity_drift"
        );
        let observed = resolve(&code).unwrap();
        std::fs::rename(&directory, root.path().join("moved")).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&code, b"one").unwrap();
        assert_eq!(
            unchanged(&code, &observed).unwrap_err().reason,
            "identity_drift"
        );
    }

    #[test]
    fn aggregate_limit_refuses_before_reading_the_next_declared_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("code");
        std::fs::write(&path, b"one").unwrap();
        let raw = json!({"path":path,"kind":"dependency","digest":"unused"});
        let mut used = MAX_TOTAL_BYTES - 2;
        let failure = measure(&raw, &mut used).err().unwrap();
        assert_eq!(failure.reason, "identity_limit");
        assert_eq!(used, MAX_TOTAL_BYTES - 2);
    }
}
