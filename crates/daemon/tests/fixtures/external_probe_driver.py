
import json
import sys
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
    # indefinitely running adapter. Production request budget is two seconds.
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
    mutation = mode[:-6] if mode.endswith("_probe") else mode
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
if operation == "handshake" and mode == "replace_user_cwd":
    import shutil
    directory = os.path.dirname(registry_path)
    held = directory + ".held"
    os.rename(directory, held)
    os.mkdir(directory)
    os.chmod(directory, 0o700)
    shutil.copy2(os.path.join(held, "registry.json"), registry_path)
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
    reply(base)
else:
    raise SystemExit(3)
