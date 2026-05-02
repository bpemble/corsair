# ── Stage 1: build the Rust hot-path extension as a Python wheel ─────
# Same Python base as the runtime stage so the wheel's ABI tag (cp311)
# matches when we install it downstream. We bring in Rust + a C linker
# manually rather than starting from `rust:slim` (which would pin us to
# whatever Python ships with Debian unstable, currently 3.13).
FROM python:3.11-slim AS rust-build

# Rust toolchain + linker. rustup writes to ~/.cargo/bin which is added
# to PATH so `cargo` and `rustc` are visible in subsequent layers.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates build-essential \
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
RUN maturin build --release --features extension-module --out /wheels --interpreter python3

# Build the corsair_trader binary (Rust port of src/trader/main.py).
# Cleanup pass 11, 2026-05-01: replaces the Python trader binary;
# selectable at runtime via env var (see compose).
WORKDIR /build/rust
RUN cargo build --release --bin corsair_trader \
    && cp target/release/corsair_trader /usr/local/bin/corsair_trader_rust

# ── Stage 2: runtime image ────────────────────────────────────────────
FROM python:3.11-slim

WORKDIR /app

# tzdata + tzdata-legacy: ib_insync parses IBKR timestamps with deprecated
# aliases like "US/Central" which only exist in the legacy zone DB.
RUN apt-get update && apt-get install -y --no-install-recommends \
        tzdata tzdata-legacy \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Install the Rust hot-path wheel built in stage 1. If the import fails
# at runtime for any reason, src/pricing.py falls back to the pure-Python
# implementation transparently — see the _USE_RUST guard there.
COPY --from=rust-build /wheels/*.whl /wheels/
RUN pip install --no-cache-dir /wheels/*.whl && rm -rf /wheels

# Copy the corsair_trader Rust binary built in stage 1.
COPY --from=rust-build /usr/local/bin/corsair_trader_rust /usr/local/bin/corsair_trader_rust

COPY config/ config/
COPY src/ src/
COPY tests/ tests/

RUN mkdir -p logs data span_data

CMD ["python3", "-m", "src.main"]
