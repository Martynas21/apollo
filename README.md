# Apollo

Apollo is a Discord bot that streams audio from YouTube — search, a direct
URL, or a playlist — into a Discord voice channel. All YouTube access
(search, metadata, playback) goes through `yt-dlp`, so there's no Google
API quota, no OAuth client to register, and no per-user linking step.

## Status

Functionally complete: browsing and playback all work end to end in code
(`cargo build`/`clippy`/`test` all pass — see `git log` for what's
implemented phase by phase). It has **not** been run against a live
Discord bot token yet — do that before trusting it in a real server.
`yt-dlp` and `ffmpeg` also need to actually be installed wherever you run
it (see Prerequisites).

## Architecture

- **Discord**: [serenity](https://github.com/serenity-rs/serenity) (gateway/REST) + [songbird](https://github.com/serenity-rs/songbird) (voice) + [poise](https://github.com/serenity-rs/poise) (slash commands), on tokio.
- **YouTube**: `yt-dlp` subprocess calls for everything — search
  (`ytsearch<n>:<query>`), a single video's metadata, and playlist
  listings, all via `yt-dlp -j --flat-playlist`/`--no-playlist`. No Google
  Cloud project, API key, or OAuth consent screen needed.
- **Audio, in two processes**: `apollo` resolves a track via songbird's
  `yt-dlp`-backed input source, fully decodes and buffers it (`yt-dlp` +
  symphonia — no separate `ffmpeg` subprocess in the common Opus-in-WebM
  path), and writes it to a shared volume. A separate `apollo-audio-worker`
  process holds the actual `songbird::Driver`/voice connection and plays
  that file, isolated into its own OS process (its own container, in
  Docker) so ordinary load on `apollo` itself — Discord gateway traffic, DB
  writes, `yt-dlp` spawns — can't starve the mixer's packet-send timing and
  cause audible stutter. The two talk over a small length-prefixed protocol
  on a Unix domain socket (`ipc/`); see `src/voice/ipc_backend.rs` and
  `audio-worker/`. `ffmpeg` is still a required dependency of `apollo`
  itself: it's checked for at startup since `yt-dlp` itself may shell out to
  it for some post-processing paths.
- A local SQLite database (via `sqlx`) persists per-guild playback settings
  (currently just volume) and saved playlists (a named pointer to a
  `YouTube` playlist URL, browsable from the `/player` panel) across
  restarts. Only `apollo` touches it — `apollo-audio-worker` has no DB
  access.

## Prerequisites

- Rust (stable, edition 2024 — see `rustc --version`)
- A Discord application + bot token — see [Discord application setup](#discord-application-setup) below.
- `yt-dlp` and `ffmpeg` installed and on `PATH`. The bot checks for both at
  startup and refuses to run if either is missing, with a message naming
  which one. **Keep `yt-dlp` updated** (`yt-dlp -U`, or reinstall
  periodically) — YouTube changes its site internals often enough that a
  stale `yt-dlp` silently starts failing to resolve videos.
- A JS runtime on `PATH` for `yt-dlp` — [Deno](https://github.com/denoland/deno/releases)
  is its default. Without one, `yt-dlp` falls back to a fragile built-in
  solver for YouTube's "n" signature challenge that intermittently fails
  ("n challenge solving failed"), causing random playback failures. Not
  checked at startup (see `Dockerfile` for an install example).

## Discord application setup

1. Go to the [Discord Developer Portal](https://discord.com/developers/applications) and create a New Application.
2. Under **Bot**: click "Reset Token" (or "Copy") to get the bot token →
   `DISCORD_TOKEN`. Under **General Information**, copy the "Application
   ID" → `DISCORD_APPLICATION_ID`.
3. No privileged gateway intents need to be toggled on in the portal —
   Apollo only uses `GUILDS` and `GUILD_VOICE_STATES`, neither of which is
   privileged (unlike, e.g., message content or member list access).
4. Generate an invite URL with the `bot` and `applications.commands`
   scopes, and these bot permissions: View Channels, Send Messages, Embed
   Links, Connect, Speak. You can build this in the portal's OAuth2 → URL
   Generator page, or use this template with your Application ID:
   ```
   https://discord.com/oauth2/authorize?client_id=<APPLICATION_ID>&scope=bot%20applications.commands&permissions=3165184
   ```
5. For local development, set `DISCORD_GUILD_ID` (in `.env`) to a test
   server's ID — slash commands registered to a specific guild show up
   within seconds; global registration (leaving it unset) can take up to
   an hour to propagate everywhere, which is annoying mid-development.

## Setup

1. Copy `.env.example` to `.env` and fill in the values from the section
   above.
2. `cargo run` (fails fast at startup if `yt-dlp`/`ffmpeg` are missing, or
   if any required `.env` value is unset).

## Development

CI (`.github/workflows/ci.yml`) runs these on every push/PR; run them
locally before pushing:

- `cargo fmt --check` / `cargo fmt` — formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` — linting.
- `cargo test` — the unit test suite (all in-crate, no live Discord/
  YouTube credentials needed).
- `cargo llvm-cov --all-features --workspace --summary-only` — coverage
  report in the terminal. Requires the `llvm-tools-preview` rustup
  component (`rustup component add llvm-tools-preview`) and
  [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov)
  (`cargo install cargo-llvm-cov`). Add `--open` instead of
  `--summary-only` for an HTML report per source line.

## Environment variables

See `.env.example` for the full list and inline docs:

- `DISCORD_TOKEN`, `DISCORD_APPLICATION_ID` — required.
- `DISCORD_GUILD_ID` — optional, see [Discord application setup](#discord-application-setup).
- `DATABASE_URL` — required (e.g. `sqlite://apollo.db`).
- `YT_DLP_COOKIES_FILE` — optional but increasingly necessary in practice:
  YouTube requires a proof-of-origin signal from a real logged-in browser
  session before serving a stream to `yt-dlp` at all, especially from a
  datacenter/cloud IP (which is where this bot will typically run) —
  without it, video resolution (and even search) can fail with "Sign in to
  confirm you're not a bot." Point this at a Netscape-format `cookies.txt`
  exported from a real browser session.
- `RUST_LOG` — optional log verbosity (`tracing-subscriber` `EnvFilter` syntax).

## Commands

- **Playback**: `/play <query|url|playlist-url>` (auto-joins your voice
  channel) — plays a video, queues an entire playlist given its URL, or
  searches and queues the top hit for free text. `/queue`, `/skip`,
  `/pause`, `/resume`, `/stop`, `/player`, `/shuffle`, `/radio`,
  `/volume <0-100>` (persists per-guild across restarts)
- **Library browsing**: `/add_to_queue <query>` shows a numbered listing
  alongside a select menu — clicking an entry queues it immediately. The
  numbered form still works too: `/add_to_queue <query> <number>` queues a
  search hit directly.
- **Player panel**: `/player` posts a persistent per-guild panel (playback
  controls plus Search/Playlists buttons into the library, kept in sync as
  state changes) or points back at the existing one if it's already active.
  Its Playlists button opens a picker for this guild's saved playlists —
  import one from a `YouTube` playlist URL, then browse/play/refresh/remove
  it later without re-pasting the URL into `/play`.

  There's no manual `/join`/`/leave` — the bot joins automatically on
  `/play`/`/add_to_queue`/etc., and leaves on its own ~2.5 minutes after the
  queue drains empty (see `IDLE_DISCONNECT` in `src/voice/player.rs`).

## Running it

Apollo is meant to run **locally** (your own machine, not a remote
server) — either directly with `cargo run` (or a release build) and a
`.env` file next to it, or in Docker if you'd rather not install Rust,
`yt-dlp`, `ffmpeg`, and Deno on the host yourself. Either way it's the
same local, single-user setup — Docker here is just a convenience
wrapper, not a deployment.

`apollo` now needs `apollo-audio-worker` running and reachable to play
anything — it's a separate process holding the actual voice connection
(see Architecture above), not an optional extra. `docker compose up` starts
both. Running directly with `cargo run`, start both binaries yourself, e.g.:
```sh
mkdir -p /tmp/apollo-ipc /tmp/apollo-audio-buf
AUDIO_WORKER_SOCKET=/tmp/apollo-ipc/audio.sock AUDIO_BUFFER_DIR=/tmp/apollo-audio-buf \
  cargo run --bin apollo-audio-worker &
AUDIO_WORKER_SOCKET=/tmp/apollo-ipc/audio.sock AUDIO_BUFFER_DIR=/tmp/apollo-audio-buf \
  cargo run --bin apollo
```
(the defaults, `/run/apollo-ipc/audio.sock` and `/audio-buf`, assume a
container's own root filesystem — override them to a writable path like
above when running outside Docker.)

For the `.env` route, restrictive file permissions (`chmod 600 .env`) are
a genuinely sufficient way to hold secrets — there's no multi-tenant
server or remote attack surface to defend against, so a secrets
manager/vault would be solving a problem this setup doesn't have.

For Docker: `Dockerfile` is a multi-target build producing two images from
one Cargo workspace — `apollo` (with `yt-dlp`, upstream's standalone
binary, `ffmpeg`, and Deno installed) and `apollo-audio-worker` (much
smaller: no `yt-dlp`/`ffmpeg`/Deno, no Discord bot token, no DB access —
just the audio driver). `compose.yaml` runs both, wired together with a
Unix domain socket volume and a tmpfs-backed volume for handing off
pre-buffered audio files, plus the usual volume for `DATABASE_URL`'s
SQLite file (so it survives container recreation) — `docker compose up -d
--build` is all you need. `.env` is read at runtime, not baked into the
image — see `.dockerignore`.

Beyond local `tracing` output to stdout/stderr, no additional
logging/metrics backend is wired in — reasonable for a local single-user
setup, and premature before this has even been run live once.

## Project layout

- `src/main.rs` — entrypoint: config, logging, client/framework wiring.
- `src/config.rs` — environment-based configuration.
- `src/db.rs` — SQLite persistence for per-guild playback settings and saved
  playlists (`sqlx`).
- `src/commands/` — poise slash commands: `playback.rs` (playback control),
  `library.rs` (search/queue/saved playlists), `radio.rs` (`/radio`).
- `src/youtube/api.rs` — `yt-dlp`-backed search/single-video/playlist client.
- `src/voice/` — `player.rs` (per-guild queue engine), `panel.rs` (the
  `/player` panel's rendering), `resolve.rs` (yt-dlp-backed audio resolution
  + startup dependency check), `radio.rs` (Mix listing for radio mode),
  `ipc_backend.rs` (the `VoiceBackend` that talks to `apollo-audio-worker`).
- `ipc/` — shared wire protocol/DTOs between `apollo` and
  `apollo-audio-worker` (a separate workspace crate, `apollo-ipc`).
- `audio-worker/` — `apollo-audio-worker`: the standalone process holding
  the actual songbird voice driver (a separate workspace crate).
- `migrations/` — sqlx SQLite migrations (embedded into the binary at
  compile time).
- `Dockerfile` / `compose.yaml` — optional local Docker build, see above.
