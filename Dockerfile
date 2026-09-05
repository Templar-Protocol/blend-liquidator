# blend-liquidator — standalone multi-stage Docker build.
#
# Single-crate repo (see the `[workspace]` note in Cargo.toml), so the build
# context is the repo root and only this crate's own files are copied in.

# ============================================
# Build Stage
# ============================================
# Pinned by digest (not just tag) so the same source revision always produces
# the same image, even after upstream retags rust:1.97.0-bookworm or the Debian
# repository underneath it changes. Tag kept alongside the digest for
# readability; the digest is what's actually resolved. Matches
# rust-toolchain.toml's `channel` and Cargo.toml's `rust-version` — all three
# move together, and scripts/check-repo-invariants.sh fails the build if they
# drift. To refresh deliberately:
#   docker buildx imagetools inspect rust:1.97.0-bookworm
# and update both the tag and the digest below together.
FROM rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073 AS builder

# No apt-get layer here: the crate's only TLS-using dependency is `reqwest`,
# built against `rustls-tls-native-roots` (see Cargo.toml), which is pure
# Rust and never links OpenSSL. `pkg-config` and `libssl-dev` bought nothing
# — confirmed by building with them removed — so they are gone rather than
# kept "for later". If a future dependency needs native TLS, that build
# failure is exactly the signal to bring them back.
WORKDIR /app

# Only what a release build of the `liquidator` binary needs.
#
# Deliberately NOT copied:
#   - rust-toolchain.toml: the builder image above is already pinned to the
#     matching rustc, so this would only make rustup fetch rustfmt/clippy that
#     `cargo build` never uses.
#   - clippy.toml: lint configuration, irrelevant to `cargo build`.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# --locked, so a stale Cargo.lock fails here rather than silently resolving
# different versions than the ones that were tested.
RUN cargo build --locked --release --bin liquidator

# ============================================
# Runtime Stage
# ============================================
# Pinned by digest for the same reproducibility reason as the builder stage.
# To refresh deliberately:
#   docker buildx imagetools inspect debian:bookworm-slim
FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171

# ca-certificates: TLS to the RPC / price / notification endpoints this bot
#                  will talk to — verified through rustls's own root store,
#                  not OpenSSL, but the roots themselves still come from here.
# libssl3:         not linked by today's binary (it is rustls-only, see the
#                  builder stage); kept only so a future dependency that does
#                  link OpenSSL does not fail at container start instead of
#                  at build time.
# procps:          provides `pgrep`, used by HEALTHCHECK below — not installed
#                  in bookworm-slim by default.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    procps \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 -s /bin/bash liquidator

WORKDIR /app

COPY --from=builder /app/target/release/liquidator /app/liquidator
COPY --chown=liquidator:liquidator .env.example ./.env.example

USER liquidator

ENV RUST_LOG=info,blend_liquidator=debug
ENV RUST_BACKTRACE=1

# Liveness check: is the `liquidator` process still running. When a readiness
# endpoint exists, do NOT wire this to it — restarting a process stuck because
# an upstream is down does not fix the upstream being down, it just hides the
# outage behind a restart loop.
HEALTHCHECK --interval=60s --timeout=10s --start-period=10s --retries=3 \
    CMD ["pgrep", "-x", "liquidator"]

LABEL org.opencontainers.image.title="Blend Liquidator"
LABEL org.opencontainers.image.description="Liquidation bot for Blend Protocol lending pools on Stellar"
LABEL org.opencontainers.image.vendor="Templar Protocol"
LABEL org.opencontainers.image.licenses="GPL-3.0-only"
LABEL org.opencontainers.image.source="https://github.com/Templar-Protocol/blend-liquidator"

ENTRYPOINT ["/app/liquidator"]
