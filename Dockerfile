# syntax=docker/dockerfile:1
#
# Strata web service image — multi-stage build with **cargo-chef**.
#
# Why cargo-chef: Rust dependency compilation dominates build time. cargo-chef
# splits the build into "recipe" (dependency graph) and "cook" (compile deps)
# stages so that Docker can cache the expensive dependency layer and reuse it
# whenever only our source code changes.
#
#   planner  → computes recipe.json (dep graph only, source-agnostic)
#   builder  → caches deps (cook), then compiles just our crates
#   runtime  → slim Debian with the single binary, non-root user
#
# Note: only `strata-web` is built here. The legacy Dioxus desktop crate is
# excluded on purpose (`-p strata-web`): it pulls WebKit/GUI libraries we do
# not want in a server image.

FROM lukemathwalker/cargo-chef:latest-rust-1.95 AS chef
WORKDIR /app

# --- 1. planner: dependency recipe -----------------------------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- 2. builder: cached deps + our release build ---------------------------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Compile every dependency of the web service once (cached layer).
RUN cargo chef cook --release --recipe-path recipe.json -p strata-web
COPY . .
# Now only workspace crates compile; deps come from the cached layer.
RUN cargo build --release -p strata-web --bin strata-web

# --- 3. runtime: slim, non-root, healthchecked -----------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root by default: the service only needs its data volume.
RUN useradd --create-home --uid 10001 strata

ENV STRATA_WORKSPACE_ROOT=/data/workspaces \
    STRATA_SOURCE_ROOTS=/data/sources \
    STRATA_ADDR=0.0.0.0:8080 \
    RUST_LOG=strata_web=info

WORKDIR /app
COPY --from=builder /app/target/release/strata-web /usr/local/bin/strata-web

# Data layout: workspaces (schemas/, data/) and allowed source roots.
RUN mkdir -p /data/workspaces /data/sources && chown -R strata:strata /data
USER strata

EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/strata-web"]
