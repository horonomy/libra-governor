//! Every fixture registered in `fixtures::ALL` parses, and every
//! `fixtures/*.json` file on disk is registered — an orphaned fixture
//! file (added but never wired into `lib.rs`) fails this test instead of
//! silently never being exercised.

use std::collections::BTreeSet;
use std::path::Path;

use libra_governor_pacing_gate::fixtures;

#[test]
fn every_registered_fixture_parses() {
    for (name, result) in fixtures::load_all() {
        assert!(
            result.is_ok(),
            "fixture {name:?} failed to parse: {:?}",
            result.err()
        );
    }
}

#[test]
fn every_registered_fixture_has_a_nonempty_task_set_and_matching_name() {
    for (name, result) in fixtures::load_all() {
        let spec = result.unwrap_or_else(|e| panic!("fixture {name:?} failed to parse: {e}"));
        assert!(!spec.tasks.is_empty(), "fixture {name:?} declares no tasks");
        assert_eq!(
            spec.name, name,
            "fixture file's own `name` field must match its registration key"
        );
    }
}

#[test]
fn every_fixture_json_file_on_disk_is_registered() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).expect("fixtures/ directory must exist") {
        let entry = entry.expect("readable fixtures/ entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("fixture file has a valid UTF-8 stem")
                .to_string();
            on_disk.insert(stem);
        }
    }
    let registered: BTreeSet<String> = fixtures::ALL.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(
        on_disk, registered,
        "fixtures/*.json on disk must exactly match fixtures::ALL's registration"
    );
}

#[test]
fn exactly_nine_fixtures_are_registered() {
    assert_eq!(
        fixtures::ALL.len(),
        9,
        "see lib.rs module docs for the t0..t7 plus x0_unguarded numbering this resolves to"
    );
}
