# syntax=docker/dockerfile:1.7
# Multi-stage build for the Carbon migration's two Rust binaries
# (`crates/indexer`, `crates/api`): one builder stage compiles both, and the
# final stages copy them out onto a slim, glibc-identical runtime base.
# Build one target at a time via `--target indexer` / `--target api`
# (docker-compose.yml's `build.target` does this for each service).
#
# Usage:
#   docker build -f docker/rust.Dockerfile --target indexer -t indexer:local .
#   docker build -f docker/rust.Dockerfile --target api     -t indexer-api:local .
#
# --------------------------------------------------------------------------------------
# protoc choice (task-7-brief.md asks this to be checked and documented):
#
# `crates/indexer` depends on `carbon-yellowstone-grpc-datasource`, which pulls in
# `yellowstone-grpc-proto`. Its build.rs (verified directly in the vendored source,
# yellowstone-grpc-proto-10.1.1/build.rs) does:
#
#     std::env::set_var("PROTOC", protobuf_src::protoc());
#
# unconditionally, BEFORE compiling geyser.proto -- so simply installing a system protoc
# and exporting a PROTOC env var has NO effect; it is overwritten before it would ever be
# read. `protobuf_src::protoc()` (protobuf-src-1.1.0+21.5/src/lib.rs) always points at a
# vendored protobuf 3.19.1 that ITS OWN build.rs compiles from source via autotools --
# again unconditionally (protobuf-src-1.1.0+21.5/build.rs just runs
# `autotools::Config::new("protobuf").build()`, no existing-protoc check of any kind).
# That vendored compile is what task-3-report.md calls "works but slow" on Linux (on
# Windows it doesn't work at all -- see that report -- but this Dockerfile only ever
# builds on Linux).
#
# The actual bypass is a different, fully-stable Cargo mechanism: `protobuf-src`
# declares `links = "protobuf-src"` in its Cargo.toml, which makes it eligible for
# Cargo's "Overriding Build Scripts" feature
# (https://doc.rust-lang.org/cargo/reference/build-scripts.html#overriding-build-scripts)
# -- a `[target.<triple>.protobuf-src]` section in `.cargo/config.toml` REPLACES the
# build script's execution outright (it never runs), supplying the `rustc-env`s it would
# have emitted directly. `protobuf_src::protoc()`/`::include()` read exactly
# `$INSTALL_DIR/bin/protoc` and `$INSTALL_DIR/include`, which is precisely the layout
# Debian's `protobuf-compiler` + `libprotobuf-dev` packages install to under `/usr` --
# so `INSTALL_DIR=/usr` needs no extra staging. geyser.proto's only external import
# (`google/protobuf/timestamp.proto`) is satisfied by `libprotobuf-dev`. Nothing else in
# the dependency graph links against protobuf-src (checked: it appears exactly once,
# as yellowstone-grpc-proto's own build-dependency, in Cargo.lock), so this override is
# fully scoped to the one crate it is meant for.
#
# This is faster (skips a from-source C++ compile of protobuf entirely) and reliable
# (a documented, stable Cargo feature, not an env-var guess) -- so it is the primary
# path below. If a future yellowstone-grpc-proto/protobuf-src upgrade breaks it (e.g.
# yellowstone-grpc-proto stops calling protobuf_src::protoc(), or protobuf-src changes
# its expected install layout), delete the `.cargo/config.toml` generation step and the
# `protobuf-compiler`/`libprotobuf-dev` install -- protobuf-src's vendored autotools
# build still works unattended on Linux, just slower.
# --------------------------------------------------------------------------------------

# --------------------------------------------------------------------------------------
# glibc parity (why the builder is `debian:bookworm-slim` + rustup, not a
# pre-built Rust image):
#
# The builder and the runtime MUST be the same Debian release, so a compiled
# binary can never reference libc symbols newer than the ones its runtime image
# provides. The previous builder stage was `lukemathwalker/cargo-chef:latest-rust-1`
# -- a FLOATING tag whose base had moved past bookworm's glibc 2.36 -- while the
# runtime stayed pinned at `debian:bookworm-slim`. After the tag's base refreshed
# (last push 2026-08-21), every freshly built binary required GLIBC_2.38 and
# crash-looped both containers at exec (`/lib/x86_64-linux-gnu/libc.so.6: version
# 'GLIBC_2.38' not found`) on a runtime image that cannot change on its own --
# the 2026-09-03 incident, see MIGRATION_LOG.md. Pinning an old cargo-chef tag
# would only buy time; the builder base and the runtime base would keep drifting
# independently again. Building on the runtime's own base makes the mismatch
# impossible by construction.
#
# cargo-chef itself is dropped along with its base: it is a dependency-cache
# optimization only, and the BuildKit cache mounts below (crates.io `registry`
# downloads + the `target/` build tree) already provide incremental rebuilds on
# a given host. The toolchain is rustup's unpinned `stable` -- the same
# toolchain CI's check jobs install (`dtolnay/rust-toolchain@stable` in
# ci.yml), which already proves it compiles this workspace.
# --------------------------------------------------------------------------------------

FROM debian:bookworm-slim AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential \
      cmake perl \
      protobuf-compiler libprotobuf-dev \
      ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# rustup `stable` on the runtime's own base (glibc parity -- see note above).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- \
      -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /app
# See the protoc-choice note above: this bypasses protobuf-src's vendored autotools
# build entirely via Cargo's build-script-override mechanism.
RUN target="$(rustc -vV | sed -n 's/^host: //p')" \
    && mkdir -p .cargo \
    && printf '[target.%s.protobuf-src]\nrustc-env = { INSTALL_DIR = "/usr" }\n' "$target" \
       > .cargo/config.toml \
    && cat .cargo/config.toml
# The generated decoder crates carry their OWN `[workspace]` tables (see the root Cargo.toml's
# comment -- generated by `carbon-cli`, cannot be members of this workspace), so each is a
# path dependency to a package outside the workspace. They are copied as their own layers
# ahead of the source `COPY` below so that layer only rebuilds when a decoder crate (or
# Cargo.toml/Cargo.lock) changes, not on every source edit under crates/indexer or crates/api.
COPY crates/whitelist-decoder crates/whitelist-decoder
COPY crates/marketplace-decoder crates/marketplace-decoder
COPY crates/property-decoder crates/property-decoder
COPY crates/regions-decoder crates/regions-decoder
COPY crates/realxhub-decoder crates/realxhub-decoder
COPY . .
# BuildKit cache mounts (registry downloads + incremental target/) survive even when the
# Dockerfile-layer cache above them is invalidated (e.g. by an application source change) --
# without them every crates.io fetch and every dependency compile would redo from zero on
# each `docker compose up --build`, not just on a Cargo.toml/Cargo.lock change. `target/` is
# a cache mount, so it is NOT part of the image filesystem after the RUN -- the step below
# copies the two binaries out to an ordinary layer path before it ends.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release -p indexer -p api \
    && mkdir -p /out \
    && cp target/release/indexer target/release/api /out/

# ----------------------------------------------------------------------------------------
# Final stages: debian-slim + ca-certificates (for outbound TLS to Alchemy/Postgres) +
# curl (every service's healthcheck in docker-compose.yml curls its own HTTP endpoint --
# task-7-brief.md requires this to actually work, not just be present).
# ----------------------------------------------------------------------------------------

FROM debian:bookworm-slim AS runtime-base
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

FROM runtime-base AS indexer
COPY --from=builder /out/indexer /usr/local/bin/indexer
ENTRYPOINT ["indexer"]
CMD ["run"]

FROM runtime-base AS api
COPY --from=builder /out/api /usr/local/bin/api
ENTRYPOINT ["api"]
