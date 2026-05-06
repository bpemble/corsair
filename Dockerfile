# ── Stage 1: build the Rust hot-path extension as a Python wheel ─────
# Same Python base as the runtime stage so the wheel's ABI tag (cp311)
# matches when we install it downstream. We bring in Rust + a C linker
# manually rather than starting from `rust:slim` (which would pin us to
# whatever Python ships with Debian unstable, currently 3.13).
FROM python:3.11-slim AS rust-build

# Rust toolchain + linker. rustup writes to ~/.cargo/bin which is added
# to PATH so `cargo` and `rustc` are visible in subsequent layers.
#
# `mold` is the modern parallel linker — typically 5-10x faster on link
# steps than the default `ld` for our binary sizes (~10-20 MB). `clang`
# drives the linker via -fuse-ld; `.cargo/config.toml` (in repo) wires
# the rustflags. Together they shave ~5-10s off each rebuild's link
# phase, on top of the codegen-units gains from more cores.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        curl ca-certificates build-essential clang mold \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN pip install --no-cache-dir "maturin[patchelf]>=1.5,<2.0"

WORKDIR /build
COPY rust/ /build/rust/
WORKDIR /build/rust/corsair_pricing
# Build a release wheel. --release matches Cargo.toml's [profile.release]
# (opt-level=3, thin LTO, codegen-units=1) — we want max perf at the
# cost of compile time, since this stage is cached when the Rust code
# doesn't change.
#
# BuildKit cache mounts: the registry and target dirs persist across
# `docker compose build` invocations. Without them, every rebuild
# re-downloads crates and recompiles every dep from scratch — even
# when only one workspace crate changed. With them, an incremental
# rebuild only re-links the touched crate (~5-15s vs ~3-5min cold).
# The mounts are ephemeral inside the image (final layer doesn't
# carry the target dir), so binary size is unaffected.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/rust/target \
    maturin build --release --features extension-module --out /wheels --interpreter python3

# Build the corsair_trader binary (Rust hot-path trader). Replaced the
# original Python trader during Phase 6.7+ cleanup; the Python source
# was deleted in the dead-code purge. This is now the only trader.
WORKDIR /build/rust
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/rust/target \
    cargo build --release --bin corsair_trader \
    && cp target/release/corsair_trader /usr/local/bin/corsair_trader_rust

# Build the corsair_broker binary (v3 Phase 4 — replaces the Python
# corsair-corsair service). Phase 5A deploys this in shadow mode
# alongside the live Python broker for latency measurement.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/rust/target \
    cargo build --release --bin corsair_broker \
    && cp target/release/corsair_broker /usr/local/bin/corsair_broker_rust

# Synthetic place_order injector — used to validate v2 wire-timing
# plumbing when markets are closed. Kept in the runtime image so it
# can be invoked via `docker compose exec corsair-broker-rs ...`.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/rust/target \
    cargo build --release --package corsair_ipc --example inject_place_order \
    && cp target/release/examples/inject_place_order /usr/local/bin/corsair_inject_place_order

# Synthetic tick replay harness — drives the trader from a JSONL
# recording for offline latency regression A/B testing. Stands in for
# corsair_broker_rs so a stopped broker frees the SHM ring path.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/rust/target \
    cargo build --release --bin corsair_tick_replay \
    && cp target/release/corsair_tick_replay /usr/local/bin/corsair_tick_replay

# Position flattener — replaces broken scripts/flatten_persistent.py.
# Reads chain_snapshot.json, sends one closing order per non-zero
# position into the broker's IPC commands ring. Bypasses trader risk
# gates so it works while the trader is killed.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/build/rust/target \
    cargo build --release --package corsair_ipc --example flatten \
    && cp target/release/examples/flatten /usr/local/bin/corsair_flatten

# ── Stage 2: runtime image ────────────────────────────────────────────
FROM python:3.11-slim

WORKDIR /app

# tzdata: needed by chrono-tz (US/Central session rollover). The
# tzdata-legacy package was dropped along with the Python parity tools
# in the Phase 6.7+ cleanup — no remaining consumer of the legacy alias DB.
RUN apt-get update && apt-get install -y --no-install-recommends \
        tzdata \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Install the Rust hot-path wheel built in stage 1.
# Production runtime is the Rust broker + Rust trader binaries; the
# Python wheel is here for offline ops/latency tools that import
# corsair_pricing (e.g. scripts/simulate_latency.py).
COPY --from=rust-build /wheels/*.whl /wheels/
RUN pip install --no-cache-dir /wheels/*.whl && rm -rf /wheels

# Copy the corsair_trader Rust binary built in stage 1.
COPY --from=rust-build /usr/local/bin/corsair_trader_rust /usr/local/bin/corsair_trader_rust

# Copy the corsair_broker Rust binary (Phase 4 daemon). Runtime
# selection is via separate compose service (see corsair-broker-rs).
COPY --from=rust-build /usr/local/bin/corsair_broker_rust /usr/local/bin/corsair_broker_rust

# Synthetic place_order injector for v2 wire-timing smoke tests.
COPY --from=rust-build /usr/local/bin/corsair_inject_place_order /usr/local/bin/corsair_inject_place_order

# Synthetic tick replay harness for offline latency regression testing.
COPY --from=rust-build /usr/local/bin/corsair_tick_replay /usr/local/bin/corsair_tick_replay

# Position flattener (replaces broken scripts/flatten_persistent.py).
COPY --from=rust-build /usr/local/bin/corsair_flatten /usr/local/bin/corsair_flatten

COPY config/ config/

RUN mkdir -p logs data span_data

# Default CMD points at the Rust broker daemon — Phase 6.7 cutover
# retired the Python entry point. Compose services override this for
# trader / dashboard / etc.
CMD ["/usr/local/bin/corsair_broker_rust"]
