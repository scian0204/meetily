# syntax=docker/dockerfile:1.7
#
# Meetily web: the desktop app's Rust core (recording pipeline, Whisper/Parakeet,
# built-in summary model, ffmpeg) runs here as meetily-server; browsers get the UI.
#
#   docker compose up -d --build                         # see docker-compose.yml
#   docker build -t meetily-web .                        # CPU
#   docker build -t meetily-web:cuda \
#     --build-arg BUILD_IMAGE=nvidia/cuda:12.4.1-devel-ubuntu22.04 \
#     --build-arg RUNTIME_IMAGE=nvidia/cuda:12.4.1-runtime-ubuntu22.04 \
#     --build-arg WHISPER_FEATURES=cuda --build-arg LLAMA_FEATURES=cuda .
# Only "" (CPU) and "cuda" are wired up here: vulkan/openblas/hipblas need extra
# build and runtime packages (or a ROCm base image) that these stages do not install.
#
# Docs: docs/WEB_SERVER.md

ARG BUILD_IMAGE=rust:1.90-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

# --- 1. Static UI (the same Next.js export the desktop app embeds) -----------
FROM node:22-bookworm-slim AS web
WORKDIR /src/frontend
# Same pnpm as the lockfile (package.json has no packageManager pin).
RUN corepack enable && corepack prepare pnpm@10.4.1 --activate
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN --mount=type=cache,id=meetily-pnpm,target=/pnpm-store \
    pnpm config set store-dir /pnpm-store && pnpm install --frozen-lockfile
COPY frontend/next.config.js frontend/tsconfig.json frontend/tailwind.config.js \
     frontend/postcss.config.js frontend/components.json frontend/eslint.config.mjs ./
COPY frontend/public ./public
COPY frontend/src ./src
ENV NEXT_TELEMETRY_DISABLED=1
RUN pnpm run build && test -f out/index.html

# --- 2. meetily-server + llama-helper -----------------------------------------
FROM ${BUILD_IMAGE} AS server
# whisper.cpp backend for transcription: "" (CPU) or "cuda"
ARG WHISPER_FEATURES=""
# llama.cpp backend for the built-in summary model: "" (CPU) or "cuda"
ARG LLAMA_FEATURES=""
# Parallel rustc jobs; lower it on hosts with little RAM (cargo defaults to all cores).
ARG CARGO_BUILD_JOBS
ENV DEBIAN_FRONTEND=noninteractive
# The crate links tauri (GTK/WebKit) even though the server never opens a window.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake clang libclang-dev pkg-config curl ca-certificates \
        libssl-dev libasound2-dev libdbus-1-dev ffmpeg \
        libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev libgtk-3-dev \
    && rm -rf /var/lib/apt/lists/*
# CUDA images ship without Rust.
ENV CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup PATH=/usr/local/cargo/bin:$PATH
RUN command -v cargo >/dev/null || \
    (curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.90.0)

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY llama-helper ./llama-helper
COPY frontend/src-tauri ./frontend/src-tauri
COPY frontend/web-runtime ./frontend/web-runtime
RUN --mount=type=cache,id=meetily-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=meetily-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=meetily-target,target=/src/target \
    set -eux; \
    triple="$(rustc -vV | sed -n 's/^host: //p')"; \
    cargo build --release --locked -p llama-helper ${LLAMA_FEATURES:+--features "$LLAMA_FEATURES"}; \
    # tauri-build requires every externalBin to exist; reuse the binaries we ship.
    mkdir -p frontend/src-tauri/binaries; \
    cp target/release/llama-helper "frontend/src-tauri/binaries/llama-helper-$triple"; \
    cp /usr/bin/ffmpeg "frontend/src-tauri/binaries/ffmpeg-$triple"; \
    cargo build --release --locked -p meetily --bin meetily-server --features "server $WHISPER_FEATURES"; \
    mkdir -p /out/bin; \
    cp target/release/meetily-server target/release/llama-helper /out/bin/

# --- 3. Runtime ----------------------------------------------------------------
FROM ${RUNTIME_IMAGE}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates tini ffmpeg libssl3 libasound2 libgomp1 libdbus-1-3 libgtk-3-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --home-dir /data --no-create-home meetily \
    && mkdir -p /data && chown meetily:meetily /data
COPY --from=server /out/bin/ /app/bin/
# tauri resolves bundled resources to <exe dir>/../lib/<productName>.
COPY frontend/src-tauri/templates/ /app/lib/meetily/templates/
COPY --from=web /src/frontend/out/ /app/web/

# Everything the app writes lands on the /data volume:
#   /data/com.meetily.ai/  database, models, settings, recordings/, uploads/
ENV HOME=/data \
    XDG_DATA_HOME=/data \
    XDG_CONFIG_HOME=/data/.config \
    MEETILY_BIND=0.0.0.0:8080 \
    MEETILY_WEB_ROOT=/app/web \
    RUST_LOG=info
USER meetily
VOLUME ["/data"]
EXPOSE 8080
ENTRYPOINT ["tini", "--", "/app/bin/meetily-server"]
