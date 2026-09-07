# syntax=docker/dockerfile:1

# A single UID shared by both runtime images below, so the Unix domain
# socket and buffer files one container creates are readable/writable by
# the other regardless of which image assigned it — `useradd --system`
# alone would pick whatever UID happens to be next-free in each image
# independently, which isn't guaranteed to match. Declared before the first
# `FROM` (global scope) so its default carries into every stage that
# redeclares `ARG APOLLO_UID` below — an `ARG` declared inside a stage, as
# this used to be, is scoped to that stage only and won't do that.
ARG APOLLO_UID=10001

# ---- build stage --------------------------------------------------------
# Builds both `apollo` and `apollo-audio-worker` in one workspace build so
# they always ship from the same songbird/apollo-ipc versions.
FROM rust:1-bookworm AS build
WORKDIR /app

# cmake is required to build libopus_sys (a songbird dependency) from its
# bundled Opus source — rust:1-bookworm doesn't ship it. Needed for both
# binaries, since both link songbird (`apollo` for `join_gateway`,
# `apollo-audio-worker` for the actual driver).
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

# Dependencies (crates.io deps) are built in their own layer, keyed only on
# the three Cargo.toml/Cargo.lock files — so editing src/ later doesn't
# invalidate this and force recompiling the whole dependency graph (~200
# crates, including the libopus_sys CMake build) on every change.
COPY Cargo.toml Cargo.lock ./
COPY ipc/Cargo.toml ipc/Cargo.toml
COPY audio-worker/Cargo.toml audio-worker/Cargo.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    mkdir -p src ipc/src audio-worker/src \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > ipc/src/lib.rs \
    && echo "fn main() {}" > audio-worker/src/main.rs \
    && cargo build --release --workspace \
    && rm -rf src ipc/src audio-worker/src

# sqlx::migrate! embeds migrations/*.sql into the binary at compile time
# (see src/db.rs) — nothing extra to copy into the runtime stage for it.
COPY src ./src
COPY ipc/src ./ipc/src
COPY audio-worker/src ./audio-worker/src
COPY migrations ./migrations
# /app/target is a cache mount (unavailable in later stages), so both
# binaries are copied out to a normal layer path before this RUN's mount is
# dropped.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    touch src/main.rs ipc/src/lib.rs audio-worker/src/main.rs \
    && cargo build --release --workspace \
    && cp target/release/apollo /app/apollo \
    && cp target/release/apollo-audio-worker /app/apollo-audio-worker

# ---- apollo runtime -------------------------------------------------------
FROM debian:bookworm-slim AS apollo
ARG APOLLO_UID

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
#
# None of this is needed by `apollo-audio-worker` — it never touches
# yt-dlp/YouTube, only a pre-resolved file on the shared buffer volume.
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

RUN useradd --system --uid ${APOLLO_UID} --create-home --home-dir /app apollo
WORKDIR /app
COPY --from=build /app/apollo /usr/local/bin/apollo

# DATABASE_URL should point at a path under a mounted volume (e.g.
# sqlite:///data/apollo.db with -v apollo-data:/data) so per-guild playback
# settings survive container recreation — see README.md. Pre-created and
# chowned here (rather than leaving Docker to create it root-owned on first
# mount) so the non-root `apollo` user can actually use it.
RUN mkdir /data \
    && chown apollo:apollo /data
USER apollo
ENV RUST_LOG=info,apollo=info

ENTRYPOINT ["/usr/local/bin/apollo"]

# ---- apollo-audio-worker runtime ------------------------------------------
# Deliberately minimal: no yt-dlp/ffmpeg/Deno, no Discord bot token, no DB —
# this process only ever holds songbird `Driver`s and streams already-resolved
# URLs straight into them over HTTP. `ca-certificates` is still needed for
# the voice gateway's TLS websocket handshake and for the HTTPS audio stream.
FROM debian:bookworm-slim AS audio-worker
ARG APOLLO_UID

# netcat-openbsd is only for compose.yaml's healthcheck (`nc -z` against
# the IPC port) — nothing in the worker itself uses it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid ${APOLLO_UID} --create-home --home-dir /app apollo
WORKDIR /app
COPY --from=build /app/apollo-audio-worker /usr/local/bin/apollo-audio-worker

USER apollo
ENV RUST_LOG=info,apollo_audio_worker=info

ENTRYPOINT ["/usr/local/bin/apollo-audio-worker"]
