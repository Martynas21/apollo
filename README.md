# Apollo

Apollo is a Discord bot that lets a Discord user link their own YouTube
account (via Google OAuth2) and stream audio from YouTube — their
playlists, or ad-hoc search/URL — directly into a Discord voice channel.

## Status

Functionally complete: linking, browsing, and playback all work end to
end in code (`cargo build`/`clippy`/`test` all pass — see `ROADMAP.md` for
what's implemented phase by phase). It has **not** been run against a
live Discord bot token or a real Google OAuth2 client yet — do that before
trusting it in a real server. `yt-dlp` and `ffmpeg` also need to actually
be installed wherever you run it (see Prerequisites).

## Architecture

- **Discord**: [serenity](https://github.com/serenity-rs/serenity) (gateway/REST) + [songbird](https://github.com/serenity-rs/songbird) (voice) + [poise](https://github.com/serenity-rs/poise) (slash commands), on tokio.
- **YouTube**: real per-user Google OAuth2 login (via the `oauth2` crate and a
  small local `axum` web server for the OAuth redirect/callback), so the
  bot can call the YouTube Data API v3 on the user's behalf.
- **Audio**: the YouTube Data API only returns metadata. Actual audio is
  resolved via songbird's built-in `yt-dlp`-backed input source, which
  streams straight into songbird's symphonia-based decoder — no separate
  `ffmpeg` subprocess in the common (Opus-in-WebM) path. `ffmpeg` is still
  a required dependency: it's checked for at startup since `yt-dlp` itself
  may shell out to it for some post-processing paths.
- Per-user OAuth tokens (including refresh tokens) are persisted in a
  local SQLite database via `sqlx`.

## Prerequisites

- Rust (stable, edition 2024 — see `rustc --version`)
- A Discord application + bot token — see [Discord application setup](#discord-application-setup) below.
- A Google Cloud project with the YouTube Data API v3 enabled and an
  OAuth2 client ID/secret — see [Google Cloud project setup](#google-cloud-project-setup) below.
- `yt-dlp` and `ffmpeg` installed and on `PATH`. The bot checks for both at
  startup and refuses to run if either is missing, with a message naming
  which one. **Keep `yt-dlp` updated** (`yt-dlp -U`, or reinstall
  periodically) — YouTube changes its site internals often enough that a
  stale `yt-dlp` silently starts failing to resolve videos.

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

## Google Cloud project setup

1. Create or select a project at the [Google Cloud Console](https://console.cloud.google.com/).
2. **Enable the API**: APIs & Services → Library → search "YouTube Data
   API v3" → Enable.
3. **Configure the OAuth consent screen** (APIs & Services → OAuth
   consent screen): choose "External" user type (unless this is a Google
   Workspace-internal deployment), fill in the required app info, and add
   the scope `https://www.googleapis.com/auth/youtube.readonly`.
4. **Create the OAuth2 client** (APIs & Services → Credentials → Create
   Credentials → OAuth client ID → Application type "Web application").
   Add an Authorized redirect URI that exactly matches
   `GOOGLE_OAUTH_REDIRECT_URI` (e.g. `http://localhost:8080/oauth/callback`
   for local dev). Copy the Client ID/Secret → `GOOGLE_CLIENT_ID` /
   `GOOGLE_CLIENT_SECRET`. **If anyone other than you will run `/link`**,
   also add a second Authorized redirect URI pointing at a public HTTPS
   URL — see [Exposing the OAuth callback publicly](#exposing-the-oauth-callback-publicly)
   below; `localhost` only ever works for whoever's sitting at the bot's
   own machine.
5. **While your consent screen's publishing status is "Testing"**, only
   explicitly-added test users (up to 100, added on the OAuth consent
   screen page) can complete `/link` at all — anyone else gets blocked by
   Google before reaching your bot.
6. **Important trap**: Google expires refresh tokens issued by an
   unverified ("Testing" status, External audience) app after exactly 7
   days, regardless of use. A linked account will silently need to
   `/link` again every week — Apollo's `/link`/`/unlink` handle this
   gracefully (see `ROADMAP.md` Phase 7), but it's still a bad experience
   for anyone actually using the bot day to day. To get an indefinite
   refresh token lifetime, move the consent screen to "In production" —
   for a sensitive scope like `youtube.readonly` (not "restricted", so no
   security assessment is required, but Google's standard app
   verification review still applies), which can take Google several days
   to review. For a small, personal-use deployment, staying in Testing
   and accepting weekly re-links is a legitimate tradeoff — just decide
   deliberately rather than being surprised by it.

## Exposing the OAuth callback publicly

The bot's OAuth callback server binds to loopback only (`127.0.0.1`). That's
enough to complete `/link` yourself on the same machine, but Google
redirects *whoever ran `/link`*'s own browser back to
`GOOGLE_OAUTH_REDIRECT_URI` — for any other Discord member, `localhost`
resolves to their machine, not the bot's, and the redirect just fails to
load. If more than you will ever link an account, the callback needs a
stable public HTTPS URL that forwards to the same local port.

[Tailscale Funnel](https://tailscale.com/kb/1223/funnel) is the
recommended way to get one, free, without touching your router or opening
a port on your home network:

1. Install Tailscale on the bot's host and run `tailscale up` to join (or
   create) a tailnet.
2. In the [Tailscale admin console](https://login.tailscale.com/admin/dns),
   under DNS, enable **HTTPS Certificates** — Funnel needs this to
   terminate TLS.
3. With the bot running (so port 8080, or whatever port
   `GOOGLE_OAUTH_REDIRECT_URI` implies, is listening), run:
   ```
   tailscale funnel 8080
   ```
   (add `--bg` to keep it running in the background after your shell
   exits). This prints the public URL, e.g.
   `https://your-machine.your-tailnet.ts.net`. `tailscale funnel status`
   shows it again later.
4. Set `GOOGLE_OAUTH_REDIRECT_URI` in `.env` to that URL plus the callback
   path, e.g. `https://your-machine.your-tailnet.ts.net/oauth/callback`,
   and add the exact same URL as a second Authorized redirect URI on the
   Google OAuth2 client (step 4 above) — Google rejects a redirect URI at
   `/link` time if it isn't registered.
5. Funnel needs to stay running alongside the bot itself (both processes,
   same machine) — if you're using the systemd unit in `deploy/`, start
   `tailscaled`'s funnel config as its own enabled unit too, or add it as
   an `ExecStartPre`/sidecar rather than something you remember to run by
   hand.

Cloudflare Tunnel is a reasonable alternative if you'd rather use a domain
you already own instead of a `ts.net` one, but Funnel needs no domain at
all, which is the simpler default here.

## Setup

1. Copy `.env.example` to `.env` and fill in the values from the two
   sections above.
2. `cargo run` (fails fast at startup if `yt-dlp`/`ffmpeg` are missing, or
   if any required `.env` value is unset).

## Environment variables

See `.env.example` for the full list and inline docs:

- `DISCORD_TOKEN`, `DISCORD_APPLICATION_ID` — required.
- `DISCORD_GUILD_ID` — optional, see [Discord application setup](#discord-application-setup).
- `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`, `GOOGLE_OAUTH_REDIRECT_URI` — required.
- `DATABASE_URL` — required (e.g. `sqlite://apollo.db`).
- `TOKEN_ENCRYPTION_KEY` — required. AES-256 key (base64, 32 raw bytes) that
  encrypts linked accounts' OAuth2 tokens at rest. Generate with
  `openssl rand -base64 32`; keep it secret and back it up alongside the
  database — losing or rotating it makes existing stored tokens
  undecryptable.
- `YT_DLP_COOKIES_FILE` — optional but increasingly necessary in practice:
  YouTube requires a proof-of-origin signal from a real logged-in browser
  session before serving a stream to `yt-dlp` at all, especially from a
  datacenter/cloud IP (which is where this bot will typically run) —
  without it, video resolution can fail with "Sign in to confirm you're
  not a bot." Point this at a Netscape-format `cookies.txt` exported from
  a real browser session.
- `RUST_LOG` — optional log verbosity (`tracing-subscriber` `EnvFilter` syntax).

## Commands

- **Account**: `/link`, `/unlink`
- **Playback**: `/play <query|url>` (auto-joins your voice channel), `/queue`,
  `/skip`, `/pause`, `/resume`, `/stop`, `/now_playing`, `/shuffle`,
  `/volume <0-100>` (persists per-guild across restarts)
- **Library browsing**: `/add_to_queue <query>` and `/playlists` each show a
  numbered listing alongside a select menu — clicking an entry queues it (or,
  for a playlist, browses its tracks) immediately. The numbered form still
  works too: `/add_to_queue <query> <number>` queues a search hit directly,
  and `/playlist_play <n>` queues an entire playlist in one shot without
  browsing it first.

  There's no manual `/join`/`/leave` — the bot joins automatically on
  `/play`/`/add_to_queue`/etc., and leaves on its own 5 minutes after the
  queue drains empty (see `IDLE_DISCONNECT` in `src/voice/player.rs`).

## Deployment

Apollo is meant to run **locally** (your own machine, not a remote
server) — so the simplest option is just `cargo run` (or a release build)
with a `.env` file next to it. For that setup, `.env` with restrictive
file permissions (`chmod 600 .env`) is a genuinely sufficient way to hold
secrets — there's no multi-tenant server or remote attack surface to
defend against, so a secrets manager/vault would be solving a problem
this deployment doesn't have.

Two other starting points are provided if you'd rather run it under a
process supervisor on the same machine — pick whichever fits, neither is
required over the other:

- **systemd** (the more natural fit for "runs continuously on my own
  Linux machine"): `deploy/apollo.service` runs the binary via
  `EnvironmentFile`. It expects the binary and an `.env` file at
  `/opt/apollo/`, owned by a dedicated `apollo` user, with the `.env` file
  `chmod 600` — same reasoning as above, just formalized as a service.
- **Docker**: `Dockerfile` builds a release binary and a runtime image
  with `yt-dlp` (upstream's standalone binary, not the often-stale distro
  package) and `ffmpeg` installed. Mount a volume for `DATABASE_URL`'s
  SQLite file so it survives container recreation, and pass the
  environment variables above via `--env-file`/`-e` (don't bake `.env`
  into the image — see `.dockerignore`).

If this ever moves to a remote/shared host, the secrets-handling calculus
changes — a proper secrets manager, systemd-creds, or your platform's
native secret store would then be worth it — but that's not this
project's current shape, so it isn't built in.

Beyond local `tracing` output to stdout/stderr (captured by
`journalctl`/your terminal either way), no additional logging/metrics
backend is wired in — reasonable for a local single-user deployment, and
premature before this has even been run live once.

## Project layout

- `src/main.rs` — entrypoint: config, logging, client/framework wiring,
  OAuth callback server.
- `src/config.rs` — environment-based configuration.
- `src/db.rs` — SQLite token persistence (`sqlx`).
- `src/commands/` — poise slash commands: `youtube.rs` (`/link`,
  `/unlink`), `playback.rs` (playback control), `library.rs` (search/
  browse/queue).
- `src/youtube/` — `oauth.rs` (Google OAuth2 client + token lifecycle),
  `server.rs` (the OAuth callback web server), `api.rs` (YouTube Data API
  v3 client).
- `src/voice/` — `player.rs` (per-guild queue engine), `resolve.rs`
  (yt-dlp-backed audio resolution + startup dependency check).
- `migrations/` — sqlx SQLite migrations (embedded into the binary at
  compile time).
- `Dockerfile`, `deploy/apollo.service` — deployment starting points, see
  above.
