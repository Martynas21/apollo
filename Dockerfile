# syntax=docker/dockerfile:1

# ---- build stage ------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /app

# sqlx::migrate! embeds migrations/*.sql into the binary at compile time
# (see src/db.rs) — nothing extra to copy into the runtime stage for it.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release

# ---- runtime stage ------------------------------------------------------
FROM debian:bookworm-slim

# ffmpeg from Debian's repo; yt-dlp as the standalone upstream binary
# (no Python runtime needed, and it's the build yt-dlp's own maintainers
# test against — apt's yt-dlp package lags upstream and breaks against
# YouTube's extraction changes far more often).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg curl \
    && curl -fL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
        -o /usr/local/bin/yt-dlp \
    && chmod a+rx /usr/local/bin/yt-dlp \
    && apt-get purge -y curl \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --home-dir /app apollo
WORKDIR /app
COPY --from=build /app/target/release/apollo /usr/local/bin/apollo

# DATABASE_URL should point at a path under a mounted volume (e.g.
# sqlite:///data/apollo.db with -v apollo-data:/data) so the token store
# survives container recreation — see README.md.
USER apollo
ENV RUST_LOG=info,apollo=info

ENTRYPOINT ["/usr/local/bin/apollo"]
