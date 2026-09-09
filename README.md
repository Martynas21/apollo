# Apollo

Apollo is a Discord bot that streams audio from YouTube — search, a direct
URL, or a playlist — into a Discord voice channel, controlled from its own
web dashboard rather than Discord slash commands. All YouTube access
(search, metadata, playback) goes through `yt-dlp`, so there's no Google
API quota, no OAuth client to register, and no per-user linking step.

## Status

Functionally complete: browsing and playback all work end to end in code
(`cargo build`/`clippy`/`test` all pass — see `git log` for what's
implemented phase by phase). It has **not** been run against a live
Discord bot token yet — do that before trusting it in a real server.
`yt-dlp` also needs to actually be installed wherever you run it (see
Prerequisites).

## Architecture

- **Discord**: [serenity](https://github.com/serenity-rs/serenity) (gateway/REST) + [songbird](https://github.com/serenity-rs/songbird) (voice), on tokio. Discord is used only for the gateway connection and voice — there are no slash commands; all control is via the web dashboard below.
- **YouTube**: `yt-dlp` subprocess calls for everything — search
  (`ytsearch<n>:<query>`), a single video's metadata, and playlist
  listings, all via `yt-dlp -j --flat-playlist`/`--no-playlist`. No Google
  Cloud project, API key, or OAuth consent screen needed.
- **Audio, in two processes**: `apollo` resolves a track with a metadata-only
  `yt-dlp -j` call to get a direct streamable media URL (no download), and
  sends that URL over to a separate `apollo-audio-worker` process. That
  process holds the actual `songbird::Driver`/voice connection and streams
  the URL straight into it over HTTP (`songbird::input::HttpRequest` +
  symphonia), isolated into its own OS process (its own container, in
  Docker) so ordinary load on `apollo` itself — Discord gateway traffic, DB
  writes, `yt-dlp` spawns — can't starve the mixer's packet-send timing and
  cause audible stutter. The two talk over a small length-prefixed protocol
  on a plain TCP connection (`ipc/`); see `src/voice/ipc_backend.rs` and
  `audio-worker/`. `ffmpeg` is not needed anywhere in this pipeline — decoding
  happens in-process via symphonia, and the `yt-dlp` calls only ever
  simulate (no download, so no post-processing step that could shell out to
  it).
- A local SQLite database (via `sqlx`) persists per-guild playback settings
  (currently just volume) and saved playlists (a named pointer to a
  `YouTube` playlist URL, managed from the web dashboard) across restarts.
  Only `apollo` touches it — `apollo-audio-worker` has no DB access.

## Prerequisites

- Rust (stable, edition 2024 — see `rustc --version`)
- A Discord application + bot token — see [Discord application setup](#discord-application-setup) below.
- `yt-dlp` installed and on `PATH`. The bot checks for it at startup and
  refuses to run if it's missing. **Keep `yt-dlp` updated** (`yt-dlp -U`, or reinstall
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
4. Generate an invite URL with the `bot` scope (no `applications.commands` —
   there are no slash commands), and these bot permissions: View Channels,
   Connect, Speak. You can build this in the portal's OAuth2 → URL Generator
   page, or use this template with your Application ID:
   ```
   https://discord.com/oauth2/authorize?client_id=<APPLICATION_ID>&scope=bot&permissions=3146752
   ```
5. `DISCORD_GUILD_ID` (in `.env`) is optional and only matters if this bot
   previously registered slash commands (from before they were removed) —
   set it to the same guild ID they were registered to so startup can clear
   them; leave it unset to clear global commands instead. New installs can
   ignore it entirely.

## Setup

1. Copy `.env.example` to `.env` and fill in the values from the section
   above.
2. `cargo run` (fails fast at startup if `yt-dlp` is missing, or if any
   required `.env` value is unset).

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

## Code style

`clippy.toml` and the workspace `[lints]` table (`Cargo.toml`) enforce most of
this — see `cargo clippy` in Development above:

- No recursion — use iteration.
- Every loop needs either a statically-visible bound (fixed range,
  decrementing counter, capped retry count) or must be one of the small set
  of intentional long-running service loops: `apollo-audio-worker`'s driver
  loop, the IPC accept/read loops (`src/voice/ipc_backend.rs`,
  `audio-worker/src/rpc.rs`), and the DB migration-retry loop (`src/db.rs`).
  A new unbounded loop outside that list needs explicit justification in
  review.
- Functions stay under ~60 lines (`clippy::too_many_lines`) — split by
  sub-step, not by arbitrary line count.
- No `unwrap()`/`expect()`/`panic!()` outside tests
  (`clippy::unwrap_used`/`expect_used`/`panic`, exempted under `#[cfg(test)]`
  via each crate root) — propagate `anyhow::Result` instead. A poisoned
  `Mutex` is the one common exception: recover with
  `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` rather than
  propagating, since poisoning here doesn't indicate corrupted state worth
  crashing over.

## Environment variables

See `.env.example` for the full list and inline docs:

- `DISCORD_TOKEN`, `DISCORD_APPLICATION_ID` — required.
- `DISCORD_GUILD_ID` — optional, only relevant for clearing stale slash
  commands from an older install; see [Discord application setup](#discord-application-setup).
- `DATABASE_URL` — required (e.g. `sqlite://apollo.db`).
- `YT_DLP_COOKIES_FILE` — optional but increasingly necessary in practice:
  YouTube requires a proof-of-origin signal from a real logged-in browser
  session before serving a stream to `yt-dlp` at all, especially from a
  datacenter/cloud IP (which is where this bot will typically run) —
  without it, video resolution (and even search) can fail with "Sign in to
  confirm you're not a bot." Point this at a Netscape-format `cookies.txt`
  exported from a real browser session.
- `RUST_LOG` — optional log verbosity (`tracing-subscriber` `EnvFilter` syntax).
- `DASHBOARD_BIND_ADDR` — optional, defaults to `127.0.0.1:8787`. See
  [Web dashboard](#web-dashboard).
- `DASHBOARD_USERNAME`, `DASHBOARD_PASSWORD` — optional, bootstrap the
  dashboard's first login. See [Web dashboard](#web-dashboard).

## Web dashboard

The web dashboard is Apollo's only control surface — there are no Discord
slash commands. It covers "Now Playing" plus transport controls
(pause/resume, skip, stop, shuffle, radio toggle, volume, live-updated over
a WebSocket), queue management (remove/reorder/clear), YouTube search and
add-to-queue, saved-playlist management (import/play/refresh/remove), and
per-guild play-count favourites, for whichever of the bot's servers you
select. Unlike a Discord command (which could infer the caller's current
voice channel), the dashboard has no such context — it prompts you to pick
a voice channel the first time you play something for a guild. The bot
leaves on its own ~2.5 minutes after the queue drains empty (see
`IDLE_DISCONNECT` in `src/voice/player.rs`).

It listens on `DASHBOARD_BIND_ADDR` (default `127.0.0.1:8787`, i.e.
localhost-only until you put something in front of it) and serves plain
HTTP with no TLS of its own — if you expose it beyond your own machine, put
a reverse proxy with TLS in front rather than binding it directly to a
public interface. Running it directly with `cargo run`, the default just
works. Running it via `compose.yaml`, the default does **not** — a
container-loopback bind is unreachable from the host by design, so it sets
`DASHBOARD_BIND_ADDR=0.0.0.0:8787` and publishes it back to
`127.0.0.1:8787` on the host instead (see `ports:` in `compose.yaml`).

Sign-in is local accounts (username + password, hashed with argon2), stored
in the same SQLite database as everything else — there's no per-Discord-user
identity or per-guild permission check, so anyone who logs in can control
any server the bot is in. Set `DASHBOARD_USERNAME` and `DASHBOARD_PASSWORD`
before the first run to create the first account, which is both an
**admin** and the sole **root** account; they're only read while no account
exists yet, so changing them later has no effect. Leave both unset to
disable the dashboard's login entirely — it still comes up, but rejects
every sign-in.

Admins get a "Users" tab in the top nav, where they can create further
accounts (optionally as admins themselves) and remove them; there's no
self-service signup, so account creation is entirely admin-driven from that
page. Every account can change its own password from that same page — but
only root can change someone *else's* (e.g. to reset a forgotten one); a
regular admin can't, and root itself can't be deleted, since that would
permanently remove the only account able to do so.

## Running it

Apollo is meant to run **locally** (your own machine, not a remote
server) — either directly with `cargo run` (or a release build) and a
`.env` file next to it, or in Docker if you'd rather not install Rust,
`yt-dlp` and Deno on the host yourself. Either way it's the
same local, single-user setup — Docker here is just a convenience
wrapper, not a deployment.

`apollo` now needs `apollo-audio-worker` running and reachable to play
anything — it's a separate process holding the actual voice connection
(see Architecture above), not an optional extra. `docker compose up` starts
both. Running directly with `cargo run`, start both binaries yourself, e.g.:
```sh
AUDIO_WORKER_SOCKET=127.0.0.1:7878 cargo run --bin apollo-audio-worker &
AUDIO_WORKER_SOCKET=127.0.0.1:7878 cargo run --bin apollo
```
(the default, `audio-worker:7878`, assumes Docker's internal DNS — override
it to a loopback address like above when running both processes directly
on the same machine.)

For the `.env` route, restrictive file permissions (`chmod 600 .env`) are
a genuinely sufficient way to hold secrets — there's no multi-tenant
server or remote attack surface to defend against, so a secrets
manager/vault would be solving a problem this setup doesn't have.

For Docker: `Dockerfile` is a multi-target build producing two images from
one Cargo workspace — `apollo` (with `yt-dlp`, upstream's standalone
binary, and Deno installed) and `apollo-audio-worker` (much
smaller: no `yt-dlp`/Deno, no Discord bot token, no DB access —
just the audio driver). `compose.yaml` runs both, wired together over a
plain TCP connection (Docker's internal DNS resolves the `audio-worker`
hostname), plus the usual volume for `DATABASE_URL`'s SQLite file (so it
survives container recreation) — `docker compose up -d --build` is all you
need. `.env` is read at runtime, not baked into the image — see
`.dockerignore`.

Beyond local `tracing` output to stdout/stderr, no additional
logging/metrics backend is wired in — reasonable for a local single-user
setup, and premature before this has even been run live once.

## Project layout

- `src/main.rs` — entrypoint: config, logging, Discord client wiring.
- `src/config.rs` — environment-based configuration.
- `src/db.rs` — SQLite persistence for per-guild playback settings and saved
  playlists (`sqlx`).
- `src/youtube/api.rs` — `yt-dlp`-backed search/single-video/playlist client.
- `src/voice/` — `player.rs` (per-guild queue engine), `resolve.rs`
  (yt-dlp-backed audio resolution + startup dependency check), `radio.rs`
  (Mix listing for radio mode), `ipc_backend.rs` (the `VoiceBackend` that
  talks to `apollo-audio-worker`).
- `src/web/` — the web dashboard (`axum`): `api.rs` (HTTP/WebSocket
  handlers), `auth.rs` (password hashing + in-memory session tokens),
  `dashboard.html` (the single-page frontend, served as-is). See
  [Web dashboard](#web-dashboard).
- `ipc/` — shared wire protocol/DTOs between `apollo` and
  `apollo-audio-worker` (a separate workspace crate, `apollo-ipc`).
- `audio-worker/` — `apollo-audio-worker`: the standalone process holding
  the actual songbird voice driver (a separate workspace crate).
- `migrations/` — sqlx SQLite migrations (embedded into the binary at
  compile time).
- `Dockerfile` / `compose.yaml` — optional local Docker build, see above.
