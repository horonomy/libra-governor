#!/usr/bin/env bash
# Disposable two-principal harness provisioning (ADR-0019 §5, HORO-1835).
#
# This is the ONLY script in this repository that runs
# groupadd/useradd. It creates OS users, groups and directories that
# live ONLY inside this container's own /etc/passwd and filesystem and
# vanish with it — it must NEVER run anywhere else.
#
# Hard safety gate (non-negotiable, three independent checks, checked
# before anything else runs): refuses unless ALL of
#   (1) running as uid 0,
#   (2) /.dockerenv exists (i.e. this is a container), and
#   (3) LG_TWO_PRINCIPAL_DISPOSABLE=1 is set (baked into the image's own
#       ENV by harness/two-principal/Dockerfile, never passed in by a
#       caller).
# are true. Any one missing aborts immediately with a clear message.
set -euo pipefail

fail() {
    echo "provision.sh: REFUSING TO RUN: $1" >&2
    exit 1
}

[ "$(id -u)" = "0" ] || fail "must run as uid 0 (got uid $(id -u))"
[ -e /.dockerenv ] || fail "/.dockerenv not present — this does not look like a container"
[ "${LG_TWO_PRINCIPAL_DISPOSABLE:-}" = "1" ] || fail "LG_TWO_PRINCIPAL_DISPOSABLE != 1 (got '${LG_TWO_PRINCIPAL_DISPOSABLE:-<unset>}')"

echo "provision.sh: disposability gate passed (uid 0, /.dockerenv present, LG_TWO_PRINCIPAL_DISPOSABLE=1)."

# ---------------------------------------------------------------------
# Principals (ADR-0019 §1 "Principals" table; UIDs/GIDs pinned to match
# the constants `crates/two-principal-harness/src/lib.rs` hardcodes, so
# the fixture crate never has to parse /etc/passwd at runtime).
# ---------------------------------------------------------------------

# lg-clients: membership grants permission to reach the socket
# directory, nothing more.
if ! getent group lg-clients >/dev/null 2>&1; then
    groupadd --gid 11000 lg-clients
fi

# lg-provider: system user, no login shell, not in any admin/sudo group
# — mirrors production `libra-provider`. Runs the daemon and owns the
# ledger, state and socket directories.
if ! id -u lg-provider >/dev/null 2>&1; then
    groupadd --gid 10001 lg-provider
    useradd --system --no-create-home --shell /usr/sbin/nologin \
        --uid 10001 --gid 10001 lg-provider
fi

# lg-agent: the attacker in every threat test. A normal, non-admin,
# non-sudoer user, member of lg-clients only.
if ! id -u lg-agent >/dev/null 2>&1; then
    groupadd --gid 10002 lg-agent
    useradd --create-home --shell /bin/bash \
        --uid 10002 --gid 10002 --groups lg-clients lg-agent
fi

# lg-sudoprobe: container-only convenience account with a NOPASSWD
# sudoers grant, used solely as T9's sudo sensitivity control — it
# proves `sudo -n` itself functions in this container (so lg-agent's
# failure is attributable to lg-agent having no grant, not to sudo being
# broken or absent here). This account, and the sudoers drop-in below,
# exist ONLY inside this disposable container and mirror nothing about
# any production account.
if ! id -u lg-sudoprobe >/dev/null 2>&1; then
    groupadd --gid 10003 lg-sudoprobe
    useradd --create-home --shell /bin/bash \
        --uid 10003 --gid 10003 lg-sudoprobe
fi
cat > /etc/sudoers.d/lg-sudoprobe <<'EOF'
lg-sudoprobe ALL=(lg-provider) NOPASSWD: /usr/bin/true
EOF
chown root:root /etc/sudoers.d/lg-sudoprobe
chmod 0440 /etc/sudoers.d/lg-sudoprobe

# ---------------------------------------------------------------------
# Filesystem layout (ADR-0019 §1 "Filesystem layout" table).
# ---------------------------------------------------------------------

mkdir -p /var/lib/libra-governor
chown lg-provider:lg-provider /var/lib/libra-governor
chmod 0700 /var/lib/libra-governor

# Created for layout parity with ADR-0019's design, but UNUSED in PR-1:
# today's unmodified binary puts its socket inside the state directory
# (`LIBRA_GOVERNOR_STATE_DIR`), not at this separate path. A dedicated
# `/run/libra-governor` socket directory is wired up starting PR-2,
# which introduces the pinned `trusted-principal.json` config that
# names it explicitly.
mkdir -p /run/libra-governor
chown lg-provider:lg-clients /run/libra-governor
# NOSONAR(shell:S2612): 2710 is owner rwx, group --x (traverse only, no
# list/create/unlink), other none — "others" gets nothing here. The
# setgid bit (2000) is what the rule's pattern-matching flags; it grants
# no write access to anyone and is required so sockets created inside
# this directory inherit the lg-clients group (ADR-0019 §1).
chmod 2710 /run/libra-governor

# Sensitivity-control mirror of today's actual single-account deployment
# — the SAME layout, entirely owned by lg-agent (ADR-0019 §5 "Roles at
# runtime": proves an attack technique is real, not an artifact of this
# container blocking everything).
mkdir -p /srv/lg-sameuid
chown lg-agent:lg-agent /srv/lg-sameuid
chmod 0700 /srv/lg-sameuid

# Reserved for PR-2's `/etc/libra-governor/trusted-principal.json`.
# Created now for layout parity; nothing reads it in PR-1.
mkdir -p /etc/libra-governor
chown root:root /etc/libra-governor
chmod 0755 /etc/libra-governor

echo "provision.sh: users, groups and directories provisioned."

# ---------------------------------------------------------------------
# Hand off to the test runner. The compiled test binary already exists
# (built in harness/two-principal/Dockerfile's builder stage); this
# `cargo test` invocation reuses it rather than recompiling, since no
# source file changed between the build and run stages.
# ---------------------------------------------------------------------
exec cargo test -p libra-governor-two-principal-harness --locked --offline -- \
    --ignored --test-threads=1 --nocapture "$@"
