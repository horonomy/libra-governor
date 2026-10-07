
import sys
with open(sys.argv[2] + ".entry", "a", encoding="utf-8") as stream:
    stream.write("entry\n")
import json
import os
import time
import hashlib

registry_path, operation_log, snapshot_path, mode = sys.argv[1:5]
with open(operation_log + ".starts", "a", encoding="utf-8") as stream:
    stream.write("start\n")
request = json.load(sys.stdin)
operation = request["operation"]
with open(operation_log, "a", encoding="utf-8") as stream:
    stream.write(operation + "\n")

if mode == "wait_for_signal":
    with open(operation_log + ".pid", "w", encoding="utf-8") as stream:
        stream.write(str(os.getpid()))
    # Inert finite fixture: even a failed CLI signal route cannot leave an
    # indefinitely running adapter. It outlives the two-second request budget.
    time.sleep(5)

if mode == "invalid_json":
    sys.stdout.write("{")
    raise SystemExit(0)
if mode == "stdout_flood":
    sys.stdout.write("x" * 1048577)
    raise SystemExit(0)
if mode == "stderr_flood":
    sys.stderr.write("x" * 16385)
if mode == "nonzero_exit":
    raise SystemExit(7)
if mode == "mutate_script":
    with open(sys.argv[0], "a", encoding="utf-8") as stream:
        stream.write("\n# fixture code changed after handshake\n")
def mutate_registry():
    if mode.endswith("_normalize"):
        mutation = mode[:-10]
    elif mode.endswith("_probe"):
        mutation = mode[:-6]
    else:
        mutation = mode
    with open(registry_path, encoding="utf-8") as stream:
        document = json.load(stream)
    document["revision"] += 1
    revision = document["revision"]
    if mutation == "revoke_trust":
        document["adapters"]["external_probe_fixture"]["trust"] = None
        document["adapters"]["external_probe_fixture"]["trust_revision"] = revision
    elif mutation == "unregister_self":
        del document["adapters"]["external_probe_fixture"]
    elif mutation == "reregister_self":
        record = document["adapters"]["external_probe_fixture"]
        record["registration_revision"] = revision
        record["trust_revision"] = revision
        trust = record["trust"]
        material = "libra-host-adapter-trust-v1\n{}\n{}\n{}\n{}".format(
            document["registry_id"], revision, record["manifest_digest"], trust["implementation_digest"]
        )
        trust["confirmation_digest"] = "sha256:" + hashlib.sha256(material.encode()).hexdigest()
    else:
        original = document["adapters"]["external_probe_fixture"]
        manifest = json.loads(original["manifest_json"])
        manifest["adapter_id"] = "unrelated_metadata"
        raw = json.dumps(manifest, separators=(",", ":"))
        document["adapters"]["unrelated_metadata"] = {
            "manifest_json": raw,
            "manifest_digest": "sha256:" + hashlib.sha256(raw.encode()).hexdigest(),
            "registration_revision": revision,
            "trust_revision": 0,
            "trust": None,
        }
    with open(registry_path, "w", encoding="utf-8") as stream:
        json.dump(document, stream, separators=(",", ":"))

if operation == "handshake" and mode in ("revoke_trust", "unregister_self", "reregister_self", "unrelated_update"):
    mutate_registry()
def replace_user_cwd():
    import shutil
    directory = os.path.dirname(registry_path)
    held = directory + ".held"
    os.rename(directory, held)
    os.mkdir(directory)
    os.chmod(directory, 0o700)
    shutil.copy2(os.path.join(held, "registry.json"), registry_path)

if operation == "handshake" and mode == "replace_user_cwd":
    replace_user_cwd()
def reply(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")

base = {"protocol": "horonom.host-adapter", "request_id": request["request_id"]}
if operation == "handshake":
    if mode == "handshake_error":
        base["error"] = {"code": "unsupported_version", "message": "PRIVATE_DRIVER_ERROR_CANARY"}
    elif mode == "wrong_handshake_id":
        base["request_id"] = "different-request"
        base["selected_version"] = 1
    elif mode == "wrong_handshake_version":
        base["selected_version"] = 2
    else:
        base["selected_version"] = 1.0
    reply(base)
    if mode == "double_document":
        reply(base)
elif operation == "probe":
    base["protocol_version"] = 1
    base["host_contract_version"] = 1
    with open(registry_path, encoding="utf-8") as stream:
        document = json.load(stream)
    adapter_id = "external_probe_fixture"
    record = document["adapters"][adapter_id]
    manifest = json.loads(record["manifest_json"])
    trust = record["trust"]
    assert record["manifest_digest"] == trust["manifest_digest"]
    with open(snapshot_path, encoding="utf-8") as stream:
        snapshot = json.load(stream)
    snapshot["adapter"] = {
        "id": manifest["adapter_id"],
        "version": manifest["adapter_version"],
        "manifest_digest": record["manifest_digest"],
        "implementation_digest": trust["implementation_digest"],
    }
    snapshot["host"]["tool_provider"] = manifest["host_version_constraints"][0]["provider"]
    snapshot["context"] = request["input"]["context"]
    snapshot["lifecycle"]["adapter_trust"] = "trusted"
    snapshot["lifecycle"]["installed"] = True
    snapshot["lifecycle"]["enabled"] = True
    snapshot["capabilities"][0]["state"] = "supported"
    if mode == "snapshot_host_version_absent":
        snapshot["host"]["version"] = None
    if mode == "candidate_adapter_mismatch":
        snapshot["adapter"]["id"] = "different_adapter"
    elif mode == "candidate_provider_mismatch":
        snapshot["host"]["tool_provider"] = "different_provider"
    elif mode == "candidate_context_mismatch":
        snapshot["context"] = {"scope": "project_worktree", "project_ref": "/fixture/project"}
    base["result"] = {"snapshot": snapshot}
    if mode == "mutate_script_probe":
        with open(sys.argv[0], "a", encoding="utf-8") as stream:
            stream.write("\n# fixture code changed after probe\n")
    if mode in ("revoke_trust_probe", "unregister_self_probe", "reregister_self_probe"):
        mutate_registry()
    if mode == "replace_user_cwd_probe":
        replace_user_cwd()
    reply(base)
elif operation == "normalize":
    base["protocol_version"] = 1
    base["host_contract_version"] = 1
    request_input = request["input"]
    with open(operation_log + ".normalization-input", "w", encoding="utf-8") as stream:
        stream.write(request_input["observed_at"])
    snapshot = request_input["capability_snapshot"]
    source = dict(request_input["source"])
    observation = request_input["observed_at"]
    if mode == "equivalent_timestamp":
        observation = observation.replace("Z", ".000Z")
    elif mode == "different_timestamp":
        observation = "2026-10-06T00:00:01Z"

    kind = "tool_after"
    scope = snapshot["context"]["scope"]
    facts = {"tool_name": "SyntheticTool", "native_call_id": "adapter-call-claim"}
    lineage = "root"
    include_session = True
    include_agent = True
    include_turn = True
    if mode in ("identity_unknown", "identity_missing", "identity_child"):
        lineage = {"identity_unknown": "unknown", "identity_missing": "root", "identity_child": "child"}[mode]
        include_session = mode != "identity_unknown"
        include_agent = mode not in ("identity_unknown", "identity_missing")
        include_turn = mode not in ("identity_unknown", "identity_missing")
    if mode == "lifecycle_turn_start":
        kind, facts = "lifecycle", {"event_type": "turn_start"}
    elif mode == "lifecycle_turn_end":
        kind, facts = "lifecycle", {"event_type": "turn_end"}
    elif mode == "usage":
        kind, facts = "usage", {"measure": "input_tokens", "amount": 12, "unit": "token", "aggregation": "delta", "observation_scope": "host"}

    identity = {
        "envelope_version": 1,
        "observed_at": observation,
        "host_id": request_input["host_id"],
        "tool_provider": snapshot["host"]["tool_provider"],
        "lineage_status": lineage,
    }
    if include_session:
        identity["provider_session_id"] = "adapter-session-claim"
    if include_agent:
        identity["agent_id"] = "adapter-agent-claim"
    if include_turn:
        identity["turn_id"] = "adapter-turn-claim"
    if lineage == "child":
        identity["parent_agent_id"] = "adapter-parent-claim"

    event = {
        "schema_version": 1,
        "event_id": "adapter-event-claim",
        "observed_at": observation,
        "adapter_id": snapshot["adapter"]["id"],
        "adapter_version": snapshot["adapter"]["version"],
        "source": source,
        "capability_snapshot_id": snapshot["snapshot_id"],
        "identity": identity,
        "scope": scope,
        "kind": kind,
        "facts": facts,
        "quality": "reconstructed",
        "field_provenance": {},
    }
    if snapshot["host"].get("version") is not None:
        event["host_version"] = snapshot["host"]["version"]
    event_modes = {
        "event_host_id_mismatch": lambda e: e["identity"].update(host_id="other-host"),
        "event_provider_mismatch": lambda e: e["identity"].update(tool_provider="other_provider"),
        "event_adapter_mismatch": lambda e: e.update(adapter_id="other_adapter"),
        "event_version_mismatch": lambda e: e.update(adapter_version="9.9.9"),
        "event_snapshot_mismatch": lambda e: e.update(capability_snapshot_id="other-snapshot"),
        "event_host_version_mismatch": lambda e: e.update(host_version="9.9.9"),
        "event_host_version_absent": lambda e: e.pop("host_version", None),
        "event_source_name_mismatch": lambda e: e["source"].update(native_event_name="other.native.name"),
        "event_source_kind_mismatch": lambda e: e["source"].update(kind="other"),
        "event_source_field_added": lambda e: e["source"].update(native_schema_ref="urn:unexpected:schema"),
        "event_source_id_mismatch": lambda e: e["source"].update(native_event_id="other-id"),
        "event_replay_key_mismatch": lambda e: e["source"].update(replay_key="other-replay"),
        "event_scope_mismatch": lambda e: e.update(scope="session" if scope != "session" else "host"),
        "event_scope_unknown": lambda e: e.update(scope="unknown"),
    }
    if mode in event_modes:
        event_modes[mode](event)
    if mode == "invalid_event_schema":
        event["quality"] = "not-a-quality"
    if mode == "invalid_event_id":
        event["event_id"] = ""
    if mode == "invalid_event_identity":
        event["identity"].pop("tool_provider")

    events = [event]
    if mode == "mixed_batch_mismatch":
        good = dict(event)
        events = [good, dict(event)]
        events[1] = json.loads(json.dumps(event))
        events[1]["identity"]["host_id"] = "other-host"

    base["result"] = {"events": events}
    if mode == "normalize_wrong_request_id":
        base["request_id"] = "different-normalize-id"
    elif mode == "normalize_wrong_version":
        base["protocol_version"] = 2
    elif mode == "normalize_error":
        base.pop("result")
        base["error"] = {"code": "failed", "message": "PRIVATE_NORMALIZE_ERROR_CANARY"}
    elif mode == "normalize_empty_events":
        base["result"] = {"events": []}
    elif mode == "normalize_wrong_result_class":
        base["result"] = {"snapshot": snapshot}

    if mode == "mutate_script_normalize":
        with open(sys.argv[0], "a", encoding="utf-8") as stream:
            stream.write("\n# fixture code changed after normalize\n")
    if mode in ("revoke_trust_normalize", "unregister_self_normalize", "reregister_self_normalize"):
        mutate_registry()
    if mode == "replace_user_cwd_normalize":
        replace_user_cwd()
    reply(base)
else:
    raise SystemExit(3)
