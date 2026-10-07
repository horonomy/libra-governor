import sys

with open(sys.argv[2] + ".entry", "a", encoding="utf-8") as stream:
    stream.write("entry\n")

import copy
import json
import os
import time


registry_path, operation_log, mode = sys.argv[1:4]
with open(operation_log + ".starts", "a", encoding="utf-8") as stream:
    stream.write("start\n")

request = json.load(sys.stdin)
operation = request["operation"]
with open(operation_log, "a", encoding="utf-8") as stream:
    stream.write(operation + "\n")
with open(operation_log + "." + operation + ".pid", "w", encoding="utf-8") as stream:
    stream.write(str(os.getpid()))

if mode == "drift_" + ("handshake" if operation == "handshake" else "plan"):
    with open(sys.argv[4], "r", encoding="utf-8") as stream:
        encoded = stream.read(4097)
        if len(encoded) > 4096:
            raise SystemExit(4)
        control = json.loads(encoded)
    fixture_root = os.path.realpath(os.path.dirname(operation_log))
    target = os.path.abspath(control["target"])
    if os.path.commonpath([fixture_root, os.path.realpath(target)]) != fixture_root:
        raise SystemExit(4)
    action = control["action"]
    if action == "append":
        with open(target, "ab") as stream:
            stream.write(b"\nFIXTURE_IDENTITY_DRIFT\n")
    elif action == "write":
        with open(target, "w", encoding="utf-8") as stream:
            stream.write(control["content"])
    elif action in ("replace", "replace_parent"):
        with open(target, "rb") as stream:
            original = stream.read(8_388_609)
        if len(original) > 8_388_608:
            raise SystemExit(4)
        original_mode = os.stat(target).st_mode & 0o777
        if action == "replace_parent":
            parent = os.path.dirname(target)
            if parent == fixture_root:
                raise SystemExit(4)
            parent_mode = os.stat(parent).st_mode & 0o777
            os.rename(parent, parent + ".retained")
            os.mkdir(parent, parent_mode)
            replacement = target
        else:
            replacement = target + ".replacement"
        with open(replacement, "xb") as stream:
            stream.write(original)
        os.chmod(replacement, original_mode)
        if action == "replace":
            os.replace(replacement, target)
    else:
        raise SystemExit(4)


def reply(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


base = {"protocol": "horonom.host-adapter", "request_id": request["request_id"]}

if operation == "handshake":
    if mode == "handshake_error":
        base["error"] = {"code": "unsupported_version", "message": "PRIVATE_CONFIG_DRIVER_ERROR"}
    else:
        if mode == "wrong_handshake_id":
            base["request_id"] = "different-config-request"
        if mode == "wrong_handshake_version":
            base["selected_version"] = 2
        else:
            # JSON Schema treats 1 and 1.0 as the same mathematical integer.
            base["selected_version"] = 1.0
    reply(base)
elif operation == "plan_config":
    if mode in ("timeout", "timeout_plan"):
        # Finite beyond the production request budget: a harness error cannot
        # leave a forever-running candidate process.
        time.sleep(5)
    if mode in ("nonzero", "nonzero_plan"):
        raise SystemExit(7)
    if mode == "stdout_flood":
        sys.stdout.write("x" * 1_048_577)
        sys.stdout.flush()
        raise SystemExit(0)
    if mode == "stderr_flood":
        sys.stderr.write("x" * 16_385)
        sys.stderr.flush()

    supplied = request["input"]
    profile_request = copy.deepcopy(supplied["request"])
    changes = []
    events = {
        "prompt_submit": "UserPromptSubmit",
        "tool_completed": "PostToolUse",
        "turn_completed": "Stop",
    }
    for slot in profile_request["slots"]:
        observed = slot["observed"]
        desired = slot["desired"]
        if desired == "present":
            action = "add" if observed == "absent" else "preserve"
        else:
            action = "remove" if observed == "exact_owned" else "preserve"
        changes.append(
            {
                "slot": slot["slot"],
                "action": action,
                "callback_ref": slot["callback_ref"],
                "expected_owned_digest": slot["owned_digest"] if observed == "exact_owned" else None,
                "placement": {"event": events[slot["slot"]], "matcher": None},
            }
        )

    plan = {
        "schema_version": profile_request["schema_version"],
        "binding_id": profile_request["binding_id"],
        "installation_id": profile_request["installation_id"],
        "changes": changes,
    }
    if "target_revision" in profile_request:
        plan["target_revision"] = profile_request["target_revision"]

    # Each mode below changes one discriminating field in the otherwise
    # valid result, so callers can prove independent closed-plan validation.
    product_id = supplied["product_id"]
    if mode in ("versions_float", "positive_float_versions"):
        plan["schema_version"] = 1.0
    elif mode in ("invalid_bool_version", "bool_version"):
        plan["schema_version"] = True
    elif mode in ("invalid_version", "future_version"):
        plan["schema_version"] = 2
    elif mode in ("wrong_product", "product_id"):
        product_id = "unrelated-product"

    validator_ref = copy.deepcopy(supplied["validator_ref"])
    if mode in ("wrong_validator_ref", "validator_ref"):
        validator_ref["id"] = "unrelated.validator"
    elif mode in ("wrong_validator_digest", "validator_digest"):
        validator_ref["digest"] = "sha256:" + "0" * 64
    elif mode in ("versions_float", "positive_float_versions"):
        validator_ref["version"] = 1.0
    elif mode in ("invalid_validator_bool", "bool_validator_version"):
        validator_ref["version"] = True

    if mode == "wrong_binding":
        plan["binding_id"] = "12345678-1234-4234-8234-123456789aaa"
    elif mode == "wrong_installation":
        plan["installation_id"] = "12345678-1234-4234-8234-123456789aab"
    if mode in ("wrong_target_revision", "target_revision"):
        plan["target_revision"] = "sha256:" + "f" * 64

    if mode in ("wrong_callback_ref", "callback_ref"):
        changes[0]["callback_ref"] = "f" * 64
    elif mode in ("wrong_owned_digest", "ownership_digest"):
        changes[0]["expected_owned_digest"] = "sha256:" + "f" * 64
    elif mode in ("wrong_action", "action"):
        changes[0]["action"] = "remove" if changes[0]["action"] != "remove" else "add"
    elif mode in ("wrong_event", "event"):
        changes[0]["placement"]["event"] = "ForeignEvent"
    elif mode in ("wrong_matcher", "matcher"):
        changes[0]["placement"]["matcher"] = "Bash"
    elif mode in ("duplicate_slot", "duplicate"):
        changes[1]["slot"] = changes[0]["slot"]
    elif mode in ("extra_field", "extra"):
        plan["unexpected"] = "not-allowed"

    result = {
        "product_id": product_id,
        "validator_ref": validator_ref,
        "plan": plan,
    }
    base.update(
        {
            "protocol_version": 1,
            "host_contract_version": 1,
            "result": result,
        }
    )
    if mode == "wrong_plan_request_id":
        base["request_id"] = "different-plan-request"
    if mode == "wrong_response_class":
        base = {"protocol": "horonom.host-adapter", "request_id": request["request_id"], "selected_version": 1}
    if mode == "duplicate_json":
        encoded = json.dumps(base, separators=(",", ":"))
        encoded = encoded.replace('"schema_version":1', '"schema_version":1,"schema_version":1', 1)
        sys.stdout.write(encoded + "\n")
        sys.stdout.flush()
    else:
        reply(base)
else:
    raise SystemExit(3)
