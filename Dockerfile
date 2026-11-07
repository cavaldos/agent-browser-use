# agent-browser-use in a container: the Go server plus headless Chromium.
#   make docker      build the image and run it on http://localhost:3000

# ---- Build ----
FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/agent-browser-use .

# ---- Run ----
FROM debian:bookworm-slim
# Chromium from Debian works on both amd64 and arm64 (Apple Silicon).
# Fonts so pages in Vietnamese, CJK and emoji render properly.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      chromium ca-certificates tzdata \
      fonts-liberation fonts-dejavu-core fonts-noto-color-emoji fonts-noto-cjk \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/agent-browser-use /usr/local/bin/agent-browser-use
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
