# agent-browser-use in a container: the Rust server plus headless Chromium.
#   make docker      build the image and run it on http://localhost:3000
# Build context is the repo root: docker build -f docker/Dockerfile .

# ---- Build ----
FROM rust:1.92-bookworm AS build
# Set automatically by buildx / BuildKit (amd64 or arm64); picks the matching cloudflared.
ARG TARGETARCH
# Version stamped into the binary (shown by -version).
ARG AGENT_BROWSER_USE_VERSION=dev
# cmake: needed by aws-lc-sys (TLS) when it builds from source.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# cloudflared is embedded in the binary (-tunnel). Its own layer: it only reruns when the script changes.
COPY scripts/fetch-cloudflared.sh scripts/fetch-cloudflared.sh
RUN sh scripts/fetch-cloudflared.sh linux "${TARGETARCH:-amd64}"

# Dependency layer: compile only the dependencies against a dummy main, so editing app/src
# does not recompile them.
WORKDIR /src/app
COPY app/Cargo.toml app/Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/.fingerprint/agent-browser-use-*

# The real sources (app/cloudflared and app/target are excluded by .dockerignore, so the
# cloudflared fetched above stays). touch: COPY keeps old mtimes, cargo must see main.rs as newer.
COPY app/ .
RUN touch src/main.rs \
 && AGENT_BROWSER_USE_VERSION="${AGENT_BROWSER_USE_VERSION}" cargo build --release --locked \
 && strip target/release/agent-browser-use

# ---- Run ----
FROM debian:bookworm-slim
# Chromium from Debian works on both amd64 and arm64 (Apple Silicon).
# Fonts so pages in Vietnamese, CJK and emoji render properly.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      chromium ca-certificates tzdata \
      fonts-liberation fonts-dejavu-core fonts-noto-color-emoji fonts-noto-cjk \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/app/target/release/agent-browser-use /usr/local/bin/agent-browser-use
COPY docker/chromium /usr/local/bin/chromium-docker
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/chromium-docker /usr/local/bin/entrypoint.sh \
 && useradd --create-home --uid 1000 app \
 && mkdir -p /data && chown app:app /data

USER app
# Chrome profile (cookies, logins) and chat history live here: mount a volume to keep them.
VOLUME /data
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
