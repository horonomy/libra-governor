#!/usr/bin/env bash
# report_outcome.sh — a thin CLI wrapper an external Outcome Provider can
# shell out to, so it never has to speak the daemon's Unix-socket wire
# protocol directly. Builds the exact stdin JSON shape
# `crates/cli/src/outcome_cmd.rs::OutcomeInput` expects and pipes it into
# `libra-governor outcome record` (HORO-1174).
#
# Usage:
#   report_outcome.sh --task-id <uuid> [--plan-id <uuid>] \
#       --source-id <id> --idempotency-key <key> \
#       --kind completed|failed|aborted|unknown \
#       [--evidence <url-or-id> ...] \
#       [--binary /path/to/libra-governor]
#
# Example:
#   report_outcome.sh --task-id "$TASK_ID" --source-id example-provider \
#       --idempotency-key "ci-run-42" --kind completed \
#       --evidence "https://ci.example.com/runs/42"

set -euo pipefail

BINARY="libra-governor"
TASK_ID=""
PLAN_ID=""
SOURCE_ID=""
IDEMPOTENCY_KEY=""
KIND=""
EVIDENCE=()

usage() {
    grep '^#' "$0" | sed -e 's/^# \{0,1\}//' -e '1d'
    exit "${1:-1}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --task-id) TASK_ID="$2"; shift 2 ;;
        --plan-id) PLAN_ID="$2"; shift 2 ;;
        --source-id) SOURCE_ID="$2"; shift 2 ;;
        --idempotency-key) IDEMPOTENCY_KEY="$2"; shift 2 ;;
        --kind) KIND="$2"; shift 2 ;;
        --evidence) EVIDENCE+=("$2"); shift 2 ;;
        --binary) BINARY="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "report_outcome.sh: unknown argument: $1" >&2; usage 1 ;;
    esac
done

if [[ -z "$TASK_ID" || -z "$SOURCE_ID" || -z "$IDEMPOTENCY_KEY" || -z "$KIND" ]]; then
    echo "report_outcome.sh: --task-id, --source-id, --idempotency-key, and --kind are required" >&2
    usage 1
fi

case "$KIND" in
    completed|failed|aborted|unknown) ;;
    *)
        echo "report_outcome.sh: --kind must be one of completed|failed|aborted|unknown, got: $KIND" >&2
        exit 1
        ;;
esac

if ! command -v "$BINARY" >/dev/null 2>&1 && [[ ! -x "$BINARY" ]]; then
    echo "report_outcome.sh: cannot find or execute binary: $BINARY" >&2
    exit 1
fi

# Builds the JSON payload with python3's json module rather than manual
# string concatenation, so a task/source id or evidence URL containing a
# quote or backslash cannot corrupt the payload.
PAYLOAD="$(TASK_ID="$TASK_ID" PLAN_ID="$PLAN_ID" SOURCE_ID="$SOURCE_ID" \
    IDEMPOTENCY_KEY="$IDEMPOTENCY_KEY" KIND="$KIND" \
    python3 - "${EVIDENCE[@]}" <<'PY'
import json
import os
import sys

payload = {
    "task_id": os.environ["TASK_ID"],
    "source_id": os.environ["SOURCE_ID"],
    "idempotency_key": os.environ["IDEMPOTENCY_KEY"],
    "outcome": {
        "kind": os.environ["KIND"],
        "evidence": sys.argv[1:],
    },
}
if os.environ.get("PLAN_ID"):
    payload["plan_id"] = os.environ["PLAN_ID"]

print(json.dumps(payload))
PY
)"

echo "$PAYLOAD" | "$BINARY" outcome record
