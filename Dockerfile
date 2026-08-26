# syntax=docker/dockerfile:1

# ---- build stage ------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /app

# cmake is required to build libopus_sys (a songbird dependency) from its
# bundled Opus source — rust:1-bookworm doesn't ship it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

# Dependencies (crates.io + the patched vendor/ deps) are built in their own
# layer, keyed only on Cargo.toml/Cargo.lock/vendor — so editing src/ later
# doesn't invalidate this and force recompiling the whole dependency graph
# (~200 crates, including the libopus_sys CMake build) on every change.
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# sqlx::migrate! embeds migrations/*.sql into the binary at compile time
# (see src/db.rs) — nothing extra to copy into the runtime stage for it.
COPY src ./src
COPY migrations ./migrations
# /app/target is a cache mount (unavailable in later stages), so the binary
# is copied out to a normal layer path before this RUN's mount is dropped.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    touch src/main.rs && cargo build --release \
    && cp target/release/apollo /app/apollo

# ---- runtime stage ------------------------------------------------------
FROM debian:bookworm-slim

# ffmpeg from Debian's repo; yt-dlp as the standalone upstream binary
# (no Python runtime needed, and it's the build yt-dlp's own maintainers
# test against — apt's yt-dlp package lags upstream and breaks against
# YouTube's extraction changes far more often).
#
# Deno is installed alongside it because yt-dlp now needs an external JS
# runtime to reliably solve YouTube's "n" parameter challenge; without one
# it falls back to a pure-Python solver that intermittently fails
# ("n challenge solving failed"), which surfaces as random playback
# failures. See https://github.com/yt-dlp/yt-dlp/wiki/EJS.
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg curl unzip \
    && case "$TARGETARCH" in \
        amd64) deno_arch=x86_64; ytdlp_asset=yt-dlp_linux ;; \
        arm64) deno_arch=aarch64; ytdlp_asset=yt-dlp_linux_aarch64 ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && curl -fL "https://github.com/yt-dlp/yt-dlp/releases/latest/download/${ytdlp_asset}" \
        -o /usr/local/bin/yt-dlp \
    && chmod a+rx /usr/local/bin/yt-dlp \
    && curl -fL "https://github.com/denoland/deno/releases/latest/download/deno-${deno_arch}-unknown-linux-gnu.zip" \
        -o /tmp/deno.zip \
    && unzip -q /tmp/deno.zip -d /usr/local/bin \
    && chmod a+rx /usr/local/bin/deno \
    && rm /tmp/deno.zip \
    && apt-get purge -y curl unzip \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /app apollo
WORKDIR /app
COPY --from=build /app/apollo /usr/local/bin/apollo

# DATABASE_URL should point at a path under a mounted volume (e.g.
# sqlite:///data/apollo.db with -v apollo-data:/data) so per-guild playback
# settings survive container recreation — see README.md. Pre-creating and
# chown'ing it here (rather than leaving Docker to create it root-owned on
# first mount) lets the non-root `apollo` user below actually write to it.
RUN mkdir /data && chown apollo:apollo /data
USER apollo
ENV RUST_LOG=info,apollo=info

ENTRYPOINT ["/usr/local/bin/apollo"]
