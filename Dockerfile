# ── Frontend build stage (Phase C, 2026-05-09) ─────────────────────────
# The Rust binary embeds `web-dist/` via rust-embed at compile time.
# Build the SPA in a separate node image to keep the rust builder lean.
FROM node:20-alpine AS web-builder

WORKDIR /web

# Cache npm install when only source changes
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build
# `npm run build` writes to ../web-dist/ (per vite.config.ts), so the
# output lands at /web-dist (one dir up from /web).

# ── Rust build stage ───────────────────────────────────────────────────
FROM rust:1-alpine AS builder

# pkg-config + openssl-dev are needed because reqwest's default-tls feature
# pulls in openssl-sys.  perl is needed by ring's assembly codegen.
RUN apk add --no-cache musl-dev pkgconf openssl-dev perl make

WORKDIR /app

# Cache dependencies
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/sourcebox-sentry-cameranode*

# Pull the freshly-built SPA bundle from the web-builder stage so
# rust-embed picks it up at compile time.  build.rs would otherwise
# write a "Web UI not built" placeholder and the binary would 503
# the browser dashboard.
COPY --from=web-builder /web-dist ./web-dist

# Build application
COPY src ./src
RUN cargo build --release

# Runtime stage
FROM alpine:3.21

# Install FFmpeg for HLS generation and v4l-utils for USB camera
# diagnostics (v4l2-ctl).  We don't link libv4l — the Linux camera
# code uses raw v4l2 ioctls via libc — but the `v4l-utils` package is
# handy inside the container for `docker exec … v4l2-ctl --list-devices`
# when a user reports "camera not detected" in a containerized deploy.
# Alpine has no `libv4l` package; the shared libraries live inside
# `v4l-utils-libs` which is pulled in transitively by `v4l-utils`.
RUN apk add --no-cache \
    ffmpeg \
    v4l-utils

# Create non-root user
RUN adduser -D -s /bin/sh sentry

WORKDIR /app

# Copy binary
COPY --from=builder /app/target/release/sourcebox-sentry-cameranode /usr/local/bin/

# Create storage directories
RUN mkdir -p /app/data/hls && \
    mkdir -p /app/data/recordings && \
    mkdir -p /app/data/snapshots && \
    chown -R sentry:sentry /app/data

USER sentry

# Expose HTTP server port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD wget --no-verbose --tries=1 --spider http://localhost:8080/health || exit 1

# Volume for persistence
VOLUME ["/app/data"]

# Tell CameraNode where to persist its fallback machine-ID when /etc/machine-id
# isn't bind-mounted from the host. The ID lives in the volume so it survives
# container rebuilds. For stronger encryption (key tied to host, not data
# volume), run with `-v /etc/machine-id:/etc/machine-id:ro`.
ENV SOURCEBOX_SENTRY_DATA_DIR=/app/data

ENTRYPOINT ["sourcebox-sentry-cameranode"]