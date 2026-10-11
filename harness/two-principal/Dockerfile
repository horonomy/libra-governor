# Disposable two-principal security harness image (ADR-0019, HORO-1835).
#
# This image is built and run ONLY by `harness/two-principal/run.sh`,
# locally (`docker build` + `docker run --rm --network none`) or in CI
# (`.github/workflows/two-principal-harness.yml`, same `run.sh`). It is
# never pushed to a registry and never run with `--privileged`, added
# capabilities, or a Docker socket mount.
#
# `rust:1.90-bookworm` is pinned by the tag's multi-arch index digest
# (not a single-platform manifest digest), so the same reference resolves
# correctly on both an amd64 CI runner and an arm64 developer machine —
# verified locally via `docker buildx imagetools inspect
# rust:1.90-bookworm` before pinning.
FROM rust:1.90-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS builder

WORKDIR /usr/src/app
COPY . .

# PR-1 builds TODAY's existing binary, unmodified — no `trusted-principal`
# feature exists yet (that is PR-2/PR-3), so no `--features` flag is
# passed here (ADR-0019 §6 PR-1, §1 "Default-off mechanism").
RUN cargo build --release -p libra-governor-cli --locked

# Compiles the harness's own test binary ahead of time so the runtime
# stage never needs to invoke the compiler, only run an already-built
# test executable. `--no-run` per ADR-0019 §5 "Layout".
RUN cargo test -p libra-governor-two-principal-harness --locked --no-run

# ---------------------------------------------------------------------
# Runtime stage.
#
# ADR-0019 sketches a slim runtime stage that copies in only the
# compiled binary. That is relaxed here to also copy the builder's
# `/usr/src/app` (source plus the already-built `target/`), because
# `cargo test`'s own test-binary discovery is the simplest reliable way
# to invoke the pre-built harness test executable — reusing it at
# runtime needs no cached fingerprint to be invalidated (no source
# changes between stages), so no recompilation happens, but it does mean
# this stage is not meaningfully smaller than the builder. Documented in
# the implementing PR's description as a deliberate simplicity-over-size
# deviation from the ADR's sketch; it changes nothing about the
# isolation guarantees, which rest entirely on `--network none` plus the
# OS-user boundary `provision.sh` creates, not on image size.
#
# This stage's root user is required and intentional, not an oversight,
# and is made explicit below (rather than left implicit) for that
# reason. ENTRYPOINT is `provision.sh`, which must run as root to create
# OS users/groups and chown directories before any test executes; every
# actual test action then runs as an unprivileged principal via
# `setpriv` (see `crates/two-principal-harness/src/lib.rs`'s
# `run_as`/`command_as`), never as this image's root. The container is
# always `docker run --rm --network none`, never pushed to a registry,
# never run with added capabilities — root exists only inside this
# single disposable, isolated container's own setup step.
FROM rust:1.90-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
USER root

# Deviation from ADR-0019 §5's exact apt list (`sqlite3`, `util-linux`,
# `procps`, `gdb`): `sudo` is added so the T9 sudo-denial tests exercise
# a real sudoers policy decision (`sudo -n ...` exiting via sudo's own
# "not in sudoers"/"a password is required" path) rather than a bare
# "command not found" (exit 127), which would prove nothing about the
# UID boundary.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        sqlite3 \
        util-linux \
        procps \
        gdb \
        sudo \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app
COPY --from=builder /usr/src/app /usr/src/app
# The builder's downloaded registry index and crate sources live in
# $CARGO_HOME (/usr/local/cargo in this base image), OUTSIDE
# /usr/src/app — without this, the runtime stage's `cargo test`
# invocation (provision.sh) has no local registry cache at all and
# tries to re-fetch it from crates.io, which `docker run --network
# none` then blocks. Copying it over is what makes `cargo test
# --locked --offline` at runtime actually offline, not just labeled so.
COPY --from=builder /usr/local/cargo /usr/local/cargo

# The provider's executable: root-owned, root-owned ancestors, not
# group- or other-writable (ADR-0019 §1 filesystem layout table).
RUN install -D -o root -g root -m 0755 \
        /usr/src/app/target/release/libra-governor \
        /usr/local/libexec/libra-governor/libra-governor

COPY harness/two-principal/provision.sh /usr/local/sbin/provision.sh
RUN chmod 0755 /usr/local/sbin/provision.sh

# Baked into the image, never passed at `docker run` time by a caller —
# `provision.sh` additionally checks `/.dockerenv` and `id -u` so none of
# the three gates can be satisfied by accident outside this image.
ENV LG_TWO_PRINCIPAL_DISPOSABLE=1

ENTRYPOINT ["/usr/local/sbin/provision.sh"]
