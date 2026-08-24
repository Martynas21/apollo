# Apollo

Apollo is a Discord bot that lets a Discord user link their own YouTube
account (via Google OAuth2) and stream audio from YouTube — their
playlists, liked videos, subscriptions, or ad-hoc search/URL — directly
into a Discord voice channel.

## Status

This repository is currently scaffolding only. The Discord gateway/voice
plumbing (serenity + songbird + poise), the Google OAuth2 login flow, and
the yt-dlp/ffmpeg-based audio pipeline are implemented incrementally in
follow-up work — see `src/commands/`, `src/youtube/`, and `src/voice/`
for placeholders.

## Architecture

- **Discord**: [serenity](https://github.com/serenity-rs/serenity) (gateway/REST) + [songbird](https://github.com/serenity-rs/songbird) (voice) + [poise](https://github.com/serenity-rs/poise) (slash commands), on tokio.
- **YouTube**: real per-user Google OAuth2 login (via the `oauth2` crate and a
  small local `axum` web server for the OAuth redirect/callback), so the
  bot can call the YouTube Data API v3 on the user's behalf.
- **Audio**: the YouTube Data API only returns metadata. Actual audio is
  resolved with `yt-dlp` and decoded with `ffmpeg` into songbird's voice
  pipeline (not yet implemented).
- Per-user OAuth tokens (including refresh tokens) are persisted in a
  local SQLite database via `sqlx`.

## Prerequisites

- Rust (stable, edition 2024 — see `rustc --version`)
- A Discord application + bot token (https://discord.com/developers/applications)
- A Google Cloud project with the YouTube Data API v3 enabled and an
  OAuth2 client ID/secret (https://console.cloud.google.com/apis/credentials)
- `yt-dlp` and `ffmpeg` installed and on `PATH` (required once audio
  playback lands)

## Setup

1. Copy `.env.example` to `.env` and fill in the values.
2. `cargo run`

## Environment variables

See `.env.example` for the full list: `DISCORD_TOKEN`,
`DISCORD_APPLICATION_ID`, `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`,
`GOOGLE_OAUTH_REDIRECT_URI`.

## Project layout

- `src/main.rs` — entrypoint: loads config, initializes logging.
- `src/config.rs` — environment-based configuration.
- `src/commands/` — poise slash commands (`/link`, `/play`, `/queue`, ...).
- `src/youtube/` — Google OAuth2 login, OAuth callback server, YouTube Data API v3 client.
- `src/voice/` — songbird voice connection + playback pipeline.
