# Meetily self-hosted web deployment.
#
#   docker build -f docker/Dockerfile -t meetily-web .
#   docker build -f docker/Dockerfile --build-arg FEATURES=cuda \
#       --build-arg RUST_BASE=nvidia/cuda:12.4.1-devel-ubuntu22.04 -t meetily-web:cuda .
#
# Three stages: build the Next.js export, build the Rust server, ship a slim runtime.

# Must be declared before the first FROM: an ARG used in a FROM line is only
# resolved from the global scope, and expands to empty when declared later.
#
# 1.88+ is a hard floor: the cidre dependency is edition 2024, and older cargo
# cannot parse its manifest, so it drops out of the resolver with a misleading
# "no matching package named `cidre` found".
ARG RUST_BASE=rust:1.90-bookworm

# ---------------------------------------------------------------------------
# 1. Web UI (static export, Tauri IPC modules aliased to the HTTP shim)
# ---------------------------------------------------------------------------
FROM node:22-bookworm-slim AS web
WORKDIR /app/frontend

RUN corepack enable

COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile

COPY frontend/ ./
ENV NEXT_PUBLIC_TARGET=web
ENV NEXT_TELEMETRY_DISABLED=1
RUN pnpm run build && test -f out/index.html

# ---------------------------------------------------------------------------
# 2. Rust server binary
# ---------------------------------------------------------------------------
FROM ${RUST_BASE} AS server

# CUDA base images ship without a Rust toolchain; install one when needed.
RUN if ! command -v cargo >/dev/null 2>&1; then \
      apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && \
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain 1.90.0 && \
      rm -rf /var/lib/apt/lists/*; \
    fi
ENV PATH="/usr/local/cargo/bin:/root/.cargo/bin:${PATH}"

# build-essential/cmake/clang: whisper.cpp and onnxruntime.
# libwebkit2gtk/libsoup: the crate depends on tauri unconditionally, so its build
# script needs these even though the server binary never opens a window.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake clang libclang-dev pkg-config \
        libssl-dev libasound2-dev \
        libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
        libgtk-3-dev librsvg2-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY llama-helper/ llama-helper/
COPY frontend/src-tauri/ frontend/src-tauri/
# tauri::generate_context!() embeds frontendDist ("../out") at compile time, so the
# web export has to be in place before cargo runs.
COPY --from=web /app/frontend/out/ frontend/out/

# tauri-build only checks that every externalBin exists. ffmpeg is fetched by
# build.rs, but llama-helper is the bundled llama.cpp summary sidecar, which the
# web deployment does not offer at all — summaries go through Ollama or a cloud
# provider. An empty placeholder satisfies the check without compiling llama.cpp;
# the runtime stage copies only the server binary, so it never ships.
RUN mkdir -p frontend/src-tauri/binaries \
    && touch "frontend/src-tauri/binaries/llama-helper-$(rustc -vV | sed -n 's/^host: //p')"

ARG FEATURES=""
RUN if [ -n "$FEATURES" ]; then \
        cargo build --release --bin meetily-server --features "$FEATURES"; \
    else \
        cargo build --release --bin meetily-server; \
    fi \
    && cp target/release/meetily-server /usr/local/bin/meetily-server

# ---------------------------------------------------------------------------
# 3. Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ffmpeg decodes uploads symphonia cannot handle (webm/mkv/wma).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 libasound2 libgomp1 ffmpeg \
        libwebkit2gtk-4.1-0 libsoup-3.0-0 libjavascriptcoregtk-4.1-0 libgtk-3-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 meetily

WORKDIR /app
COPY --from=server /usr/local/bin/meetily-server /usr/local/bin/meetily-server
COPY --from=web /app/frontend/out/ /app/web/

RUN mkdir -p /data && chown -R meetily:meetily /data /app
USER meetily

ENV MEETILY_BIND=0.0.0.0:8080 \
    MEETILY_DATA_DIR=/data \
    MEETILY_WEB_ROOT=/app/web \
    RUST_LOG=info

VOLUME ["/data"]
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/meetily-server"]
