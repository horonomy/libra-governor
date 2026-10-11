#!/usr/bin/env bash
# Host entry point for the disposable two-principal security harness
# (ADR-0019, HORO-1835): builds the image, then runs it as a disposable
# container. Exit code equals the harness's own test verdict. Used
# identically by a developer locally (Docker Desktop's VM) and by CI
# (.github/workflows/two-principal-harness.yml, ubuntu-latest) — the
# same image, the same isolation flags, in both places.
#
# Usage: harness/two-principal/run.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image_tag="libra-governor-two-principal-harness:local"

echo "run.sh: building ${image_tag} from ${repo_root} ..."
docker build \
    --file "${repo_root}/harness/two-principal/Dockerfile" \
    --tag "${image_tag}" \
    "${repo_root}"

echo "run.sh: running the harness — docker run --rm --network none, no --privileged, no --cap-add, no Docker socket mount (Docker's own default capability bounding set is kept as-is: root inside the container still needs CAP_SETUID/CAP_SETGID for setpriv and CAP_CHOWN for provisioning, so this deliberately does not --cap-drop ALL — isolation here rests on --network none plus the OS-user boundary provision.sh creates, not on stripping the capabilities root itself needs to do its own job)."
echo "run.sh: LG_TWO_PRINCIPAL_DISPOSABLE is baked into the image; this invocation does not set it, so a caller cannot satisfy the gate by accident outside the image this script itself builds."

# --init: the hook subcommand's own client code can spawn a detached
# daemon (`spawn_daemon_detached`), so a reaper is needed to avoid
# leaving zombies behind inside the container's pid 1.
set +e
docker run --rm --init \
    --network none \
    "${image_tag}"
status=$?
set -e

echo "run.sh: harness exited with status ${status}."
exit "${status}"
