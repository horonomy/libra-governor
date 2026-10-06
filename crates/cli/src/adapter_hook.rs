//! Private, installed callback gate for the supported Claude hook profile.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use libra_governor_daemon::host_runtime::config_lifecycle::ConfigLifecycle;
use libra_governor_daemon::host_runtime::config_profile::ConfigSlot;
use libra_governor_daemon::host_runtime::contract::HostContract;

struct Invocation {
    state_root: PathBuf,
    binding: String,
    installation: String,
    slot: ConfigSlot,
}

pub fn run(args: &[String]) -> i32 {
    let invocation = match parse(args) {
        Some(invocation) => invocation,
        None => {
            eprintln!("libra-governor: invalid adapter hook arguments");
            return 2;
        }
    };

    let contract = match HostContract::load() {
        Ok(contract) => contract,
        Err(_) => {
            eprintln!("libra-governor: installed adapter hook unavailable");
            return 0;
        }
    };
    let lifecycle = ConfigLifecycle::new(invocation.state_root.clone(), contract);
    let consumer_binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            eprintln!("libra-governor: installed adapter hook refused");
            return 0;
        }
    };
    let admitted = match lifecycle.admit_callback(
        &invocation.binding,
        &invocation.installation,
        invocation.slot,
        &consumer_binary,
    ) {
        Ok(admitted) => admitted,
        Err(_) => {
            eprintln!("libra-governor: installed adapter hook refused");
            return 0;
        }
    };
    if !admitted {
        return 0;
    }

    let raw = match read_installed_input(std::io::stdin().lock()) {
        Some(raw) => raw,
        None => {
            eprintln!("libra-governor: installed adapter input refused");
            return 0;
        }
    };

    // The lifecycle gate has released its short reservation before returning
    // true. Existing advisory consumers use this captured state root.
    std::env::set_var("LIBRA_GOVERNOR_STATE_DIR", &invocation.state_root);
    match invocation.slot {
        ConfigSlot::PromptSubmit => crate::agent::run::run_prompt_submit_with_input(
            crate::agent::AgentKind::ClaudeCode,
            &raw,
        ),
        ConfigSlot::ToolCompleted => crate::agent::run::run_tool_completed_with_input(
            crate::agent::AgentKind::ClaudeCode,
            &raw,
        ),
        ConfigSlot::TurnCompleted => crate::agent::run::run_turn_completed_with_input(
            crate::agent::AgentKind::ClaudeCode,
            &raw,
        ),
    }
    0
}

const MAX_INSTALLED_INPUT_BYTES: usize = 1024 * 1024;

fn read_installed_input(reader: impl Read) -> Option<String> {
    let mut raw = Vec::new();
    reader
        .take((MAX_INSTALLED_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.len() > MAX_INSTALLED_INPUT_BYTES {
        return None;
    }
    String::from_utf8(raw).ok()
}

fn parse(args: &[String]) -> Option<Invocation> {
    if args.len() != 8 {
        return None;
    }
    let mut state_root = None;
    let mut binding = None;
    let mut installation = None;
    let mut slot = None;
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        let value = args.get(index + 1)?;
        match option {
            "--state-root" if state_root.is_none() => {
                let path = PathBuf::from(value);
                if value.len() > 4096
                    || value
                        .chars()
                        .any(|character| matches!(character, '\0' | '\n' | '\r'))
                    || !safe_absolute_path(&path)
                {
                    return None;
                }
                state_root = Some(path);
            }
            "--binding" if binding.is_none() && canonical_uuid(value) => {
                binding = Some(value.clone());
            }
            "--installation" if installation.is_none() && canonical_uuid(value) => {
                installation = Some(value.clone());
            }
            "--slot" if slot.is_none() => {
                slot = Some(match value.as_str() {
                    "prompt_submit" => ConfigSlot::PromptSubmit,
                    "tool_completed" => ConfigSlot::ToolCompleted,
                    "turn_completed" => ConfigSlot::TurnCompleted,
                    _ => return None,
                });
            }
            _ => return None,
        }
        index += 2;
    }
    Some(Invocation {
        state_root: state_root?,
        binding: binding?,
        installation: installation?,
        slot: slot?,
    })
}

fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn safe_absolute_path(path: &Path) -> bool {
    if !path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return false;
    }
    let mut normalized = String::from("/");
    for component in path.components() {
        if let Component::Normal(part) = component {
            if normalized.len() > 1 {
                normalized.push('/');
            }
            let Some(part) = part.to_str() else {
                return false;
            };
            normalized.push_str(part);
        }
    }
    path.to_str() == Some(normalized.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_input_limit_is_bytes_and_stops_at_the_first_excess_byte() {
        let mut input = std::io::Cursor::new(vec![b'x'; MAX_INSTALLED_INPUT_BYTES * 2]);
        assert!(read_installed_input(&mut input).is_none());
        assert_eq!(input.position(), (MAX_INSTALLED_INPUT_BYTES + 1) as u64);
        assert!(read_installed_input([0xff].as_slice()).is_none());
        assert_eq!(
            read_installed_input(b"{}".as_slice()).as_deref(),
            Some("{}")
        );
    }

    #[test]
    fn installed_input_accepts_exact_byte_limit_and_rejects_reader_failure() {
        let exact = vec![b'x'; MAX_INSTALLED_INPUT_BYTES];
        let accepted = read_installed_input(exact.as_slice()).unwrap();
        assert_eq!(accepted.len(), MAX_INSTALLED_INPUT_BYTES);
        assert!(accepted.bytes().all(|byte| byte == b'x'));

        struct FailingReader;
        impl std::io::Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture read failure"))
            }
        }
        assert!(read_installed_input(FailingReader).is_none());
    }

    const BINDING: &str = "00000000-0000-4000-8000-00000000000a";
    const INSTALLATION: &str = "00000000-0000-4000-8000-000000000002";

    fn args() -> Vec<String> {
        [
            "--state-root",
            "/this/path/is/not/read",
            "--binding",
            BINDING,
            "--installation",
            INSTALLATION,
            "--slot",
            "prompt_submit",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn parses_each_closed_argument_once_in_any_order() {
        let mut args = args();
        args.swap(0, 6);
        args.swap(1, 7);
        let parsed = parse(&args).unwrap();
        assert_eq!(parsed.state_root, PathBuf::from("/this/path/is/not/read"));
        assert_eq!(parsed.binding, BINDING);
        assert_eq!(parsed.installation, INSTALLATION);
        assert_eq!(parsed.slot, ConfigSlot::PromptSubmit);
    }

    #[test]
    fn rejects_missing_duplicate_unknown_relative_and_malformed_arguments() {
        let mut cases = Vec::new();

        let mut missing = args();
        missing.truncate(6);
        cases.push(missing);

        let mut duplicate = args();
        duplicate[4] = "--slot".into();
        duplicate[5] = "turn_completed".into();
        cases.push(duplicate);

        let mut unknown = args();
        unknown[0] = "--other".into();
        cases.push(unknown);

        let mut relative = args();
        relative[1] = "relative-state".into();
        cases.push(relative);

        let mut parent = args();
        parent[1] = "/tmp/../state".into();
        cases.push(parent);

        let mut dot_component = args();
        dot_component[1] = "/tmp/./state".into();
        cases.push(dot_component);

        let mut oversized = args();
        oversized[1] = format!("/{}", "a".repeat(4096));
        cases.push(oversized);

        let mut invalid_uuid = args();
        invalid_uuid[3] = BINDING.to_uppercase();
        cases.push(invalid_uuid);

        let mut invalid_slot = args();
        invalid_slot[7] = "post_tool_use".into();
        cases.push(invalid_slot);

        for case in cases {
            assert!(parse(&case).is_none());
        }
    }
}
