# Apollo Roadmap

What's left to turn the current scaffold (commit `56d77c1`) into a working
product: a Discord bot that lets a user link their own YouTube account and
stream their playlists, liked videos, or search results into a voice
channel.

Phases are ordered roughly by dependency — later phases build on earlier
ones — but each is independently testable.

## Phase 1 — Discord bot core

Land in `src/main.rs`.

- [x] Build the `serenity::Client` with the `poise::Framework`, using
      `Config::from_env()` (`src/config.rs`) for the token.
- [x] Set gateway intents: `GUILDS`, `GUILD_VOICE_STATES` (required for
      songbird), plus whatever the command set ends up needing.
- [x] Register the songbird voice manager on the client.
- [x] Register application (slash) commands — guild-scoped for fast
      iteration in dev, global once ready to ship. (`DISCORD_GUILD_ID`
      env var selects guild-scoped vs. global registration.)
- [x] Basic `ready`/`resume` event logging.

**Done when:** the bot connects, shows online in a test guild, and a
trivial `/ping` command responds.

## Phase 2 — Google OAuth2 account linking

Land in `src/youtube/`.

- [x] `/link` command: generate a Google OAuth2 authorization URL (via the
      `oauth2` crate) with a CSRF `state` parameter bound to the invoking
      Discord user ID, and reply with the link.
- [x] Local `axum` server bound to `GOOGLE_OAUTH_REDIRECT_URI` that
      receives the callback, validates `state`, and exchanges the
      authorization code for an access + refresh token.
- [x] `/unlink` command: revoke the token with Google and delete the
      stored record.
- [x] Background/lazy refresh: use the stored refresh token to mint a new
      access token once the current one is near expiry. (`get_valid_access_token`
      in `src/youtube/oauth.rs`; not called yet — Phase 4's API client will
      use it before each request.)
- [x] Scope selection: request the narrowest YouTube scope that covers
      playlists/liked videos/subscriptions (e.g.
      `https://www.googleapis.com/auth/youtube.readonly`).

**Done when:** a user runs `/link`, completes Google's consent screen, and
the bot confirms the account is linked.

## Phase 3 — Token persistence

New `migrations/` directory + `src/youtube/`.

- [x] Add an sqlx SQLite migration for a `users` table:
      `discord_user_id`, `access_token`, `refresh_token`, `expires_at`,
      `scopes`.
- [x] Add `DATABASE_URL` to `.env.example` and `Config`
      (`src/config.rs`) — intentionally left out of the initial scaffold
      until this lands.
- [x] Wire `sqlx::SqlitePool` into the bot's shared state (poise
      `Data` type) so commands can read/write tokens.

**Done when:** linked tokens survive a bot restart.

## Phase 4 — YouTube Data API v3 client

Land in `src/youtube/`.

- [x] Thin client wrapper around the relevant endpoints:
      `playlists.list` (a user's playlists), `playlistItems.list`
      (including the special `LL` liked-videos playlist and `uploads`),
      `search.list` (ad-hoc query/URL lookup).
- [x] Map API responses into a simple internal `Track` type (title,
      video ID, channel, duration).
- [x] Handle quota errors / rate limiting with a clear internal error
      type (surfaced to the user in Phase 6/7).

**Done when:** given a linked user, the client can list their liked
videos and playlists as `Track`s.

## Phase 5 — Audio resolution + voice pipeline

Land in `src/voice/`.

- [ ] Given a YouTube video ID, shell out to `yt-dlp` to resolve a
      playable audio stream URL.
- [ ] Feed that URL through `ffmpeg` into a songbird input/track.
- [ ] Handle common failure modes (age-restricted, region-locked,
      private/deleted video) with a user-facing error rather than a
      panic.
- [ ] Confirm `yt-dlp` and `ffmpeg` are on `PATH` at startup (fail fast
      with a clear message if missing) — both are already called out as
      prerequisites in `README.md`.

**Done when:** a hardcoded video ID plays audio into a test voice channel.

## Phase 6 — Playback commands & queue

Land in `src/commands/`.

- [ ] `/join`, `/leave` — voice channel connect/disconnect.
- [ ] `/play <query|url>` — resolves via Phase 4/5 and enqueues.
- [ ] `/search` — ad-hoc YouTube search with a pick-one-of-N reply.
- [ ] `/playlists`, `/liked` — browse the linked account's library and
      queue from it.
- [ ] `/queue`, `/skip`, `/pause`, `/resume`, `/stop`, `/nowplaying`.
- [ ] An in-memory per-guild queue (`VecDeque<Track>`) driven by the
      songbird track-end event to advance automatically.
- [ ] Auto-disconnect on empty voice channel or idle timeout.

**Done when:** a user can `/link`, `/play` something from their liked
videos, and control playback end-to-end in a live guild.

## Phase 7 — Error handling & UX polish

- [ ] Ephemeral (user-only) replies for errors.
- [ ] Graceful re-link prompt when a token is revoked or refresh fails.
- [ ] Now-playing embeds (title, thumbnail, requester, progress).

## Phase 8 — Ops/deployment

- [ ] Process management: systemd unit or a Dockerfile.
- [ ] Document the Discord application setup: bot invite URL, required
      OAuth2 scopes/permissions, gateway intents to enable in the
      Developer Portal.
- [ ] Document the Google Cloud project setup: OAuth consent screen,
      YouTube Data API v3 enablement, verification requirements if the
      `youtube`/`youtube.readonly` scope triggers Google's sensitive-scope
      review for a public bot.
- [ ] Production secrets handling (not just `.env`).
- [ ] Basic logging/observability beyond local `tracing` output.

## Phase 9 — Testing

- [ ] Unit tests for token refresh/expiry logic (Phase 2/3).
- [ ] Unit tests for `Track` mapping from YouTube API responses
      (Phase 4).
- [ ] A manual end-to-end test plan: link → browse liked videos → play →
      queue → skip → unlink, run against a staging Discord server.

## Stretch / nice-to-haves

- [ ] Multiple linked accounts per Discord user.
- [ ] Shuffle / loop modes.
- [ ] Per-guild default volume.
- [ ] Playlist caching to cut YouTube API quota usage.
- [ ] "Save to my YouTube playlist" from a Discord reaction/command.
