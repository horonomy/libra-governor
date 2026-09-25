//! HORO-1381 sub-ticket 6 — cross-product canonicalization-vector
//! agreement example.
//!
//! Reads a `canon-v1-vector-001.json`-shaped vector on stdin, parses the
//! `event` member as a raw `serde_json::Value` (not the typed
//! `DogfoodEvent` -- this is the purest procedure test: it exercises the
//! real, shipped `canon::content_hash_hex` against an arbitrary JSON
//! value, uniform with the Python drivers, with zero typed-model risk),
//! calls the real shipped `libra_governor_evidence_adapter::canon::
//! content_hash_hex`, and prints one JSON line to stdout:
//! `{"canonical": null, "content_hash": {"alg":"sha256","value":"<hex>"}}`.
//!
//! `canonical` is always `null` here: `content_hash_hex` only exposes
//! the hex digest, not the canonical string it hashed internally (that
//! string never escapes the function body) -- this example does not
//! re-serialize the event with plain `serde_json` as a fake "canonical"
//! diagnostic, since that would be the example's own canonicalization,
//! not the product's.
//!
//! This example does not compare against any pin -- the calling bash
//! driver (`scripts/dogfood-journey/libra_governor_canon_vector.sh`)
//! does the comparison and emits the DogFood conformance check row.
//! This example stays dumb.

use std::io::Read;

use libra_governor_evidence_adapter::canon::content_hash_hex;
use serde_json::Value;

fn main() {
    let mut input = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("FATAL: failed to read stdin: {err}");
        std::process::exit(1);
    }

    let vector: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("FATAL: failed to parse vector JSON: {err}");
            std::process::exit(1);
        }
    };

    let event = match vector.get("event") {
        Some(event) => event,
        None => {
            eprintln!("FATAL: vector JSON has no \"event\" member");
            std::process::exit(1);
        }
    };

    // content_hash_hex<T: Serialize> accepts any Serialize value whose
    // top-level shape is a JSON object with an `integrity` member --
    // exactly what `event` is here. It strips `integrity` internally
    // before hashing, per the pinned canonicalization procedure.
    let hex_digest = content_hash_hex(event);

    let output = serde_json::json!({
        "canonical": Value::Null,
        "content_hash": {"alg": "sha256", "value": hex_digest},
    });

    println!("{output}");
}
