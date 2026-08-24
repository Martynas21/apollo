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

- [x] Given a YouTube video ID, shell out to `yt-dlp` to resolve a
      playable audio stream URL. (`voice::track_input`, via songbird's
      built-in `input::YoutubeDl` source — see note below.)
- [x] Feed that URL through `ffmpeg` into a songbird input/track.
      (Superseded in practice: songbird 0.6's `YoutubeDl` source streams
      the resolved URL straight into symphonia — Opus-in-WebM, YouTube's
      typical best-audio pick, decodes without a separate `ffmpeg`
      subprocess. `ffmpeg` is still checked for at startup since yt-dlp
      may shell out to it for some post-processing paths.)
- [x] Handle common failure modes (age-restricted, region-locked,
      private/deleted video) with a user-facing error rather than a
      panic. (`voice::preflight_check` + `classify_ytdlp_stderr`.)
- [x] Confirm `yt-dlp` and `ffmpeg` are on `PATH` at startup (fail fast
      with a clear message if missing) — both are already called out as
      prerequisites in `README.md`. (`voice::check_playback_dependencies`,
      called from `main.rs` at startup as of Phase 6 — note this means
      `cargo run` in a sandbox without yt-dlp/ffmpeg installed, like this
      one, will fail immediately at startup; that's intentional.)

**Done when:** a hardcoded video ID plays audio into a test voice channel.

## Phase 6 — Playback commands & queue

Land in `src/commands/`.

- [x] `/join`, `/leave` — voice channel connect/disconnect.
- [x] `/play <query|url>` — resolves via Phase 4/5 and enqueues.
- [x] `/search` — ad-hoc YouTube search with a pick-one-of-N reply.
      (No interactive button/select picker — shows a numbered list and a
      follow-up `/searchplay <query> <number>` command queues the pick.
      Component interactions weren't worth the risk to get right without
      live Discord testing; can be upgraded later.)
- [x] `/playlists`, `/liked` — browse the linked account's library and
      queue from it. (Same numbered-list-then-follow-up-command pattern:
      `/playlistplay`, `/playlistqueue`, `/likedplay`. Each follow-up
      re-fetches the listing rather than caching it between commands —
      deliberate MVP simplicity, not an oversight.)
- [x] `/queue`, `/skip`, `/pause`, `/resume`, `/stop`, `/nowplaying`.
- [x] An in-memory per-guild queue (`VecDeque<Track>`) driven by the
      songbird track-end event to advance automatically.
      (`voice::player::PlayerRegistry`.)
- [x] Auto-disconnect on empty voice channel or idle timeout. (Idle
      timeout only — 5 minutes after the queue drains, re-checked when
      the timer fires. Explicit "channel has zero human members" detection
      via voice-state-update events was scoped out: idle timeout already
      covers that case, just not instantly, and the added event-handling
      surface wasn't worth it for this pass.)

**Done when:** a user can `/link`, `/play` something from their liked
videos, and control playback end-to-end in a live guild.

## Phase 7 — Error handling & UX polish

- [x] Ephemeral (user-only) replies for errors. (Established from Phase 2
      onward — every error path across `/link`, `/unlink`, and all Phase 6
      commands replies ephemerally; success confirmations are public.)
- [x] Graceful re-link prompt when a token is revoked or refresh fails.
      `youtube::oauth::get_valid_access_token` now returns a typed
      `AccessTokenError` (`NotLinked` vs. `RefreshFailed { revoked, .. }`)
      instead of a flat `anyhow::Error`, distinguishing "never linked" from
      "was linked but broke." A refresh failure specifically due to Google
      returning `invalid_grant` (RFC 6749 — the standard signal for a
      revoked/expired refresh token) deletes the stale DB row so `/link`
      cleanly re-establishes it; any other refresh failure (network blip,
      etc.) leaves the row alone, since a later call may succeed
      unassisted. `commands::playback::access_token_error_message` maps
      each case to a distinct user-facing message, shared by both
      `/play` and the library-browsing commands.
- [x] Now-playing embeds (title, thumbnail, requester, progress).
      `/nowplaying` now sends a `serenity::CreateEmbed` (title linked to
      the video, thumbnail via YouTube's public `i.ytimg.com` CDN
      convention — no extra API call needed, channel, requester mention,
      and live progress via a new `PlayerRegistry::now_playing_position`
      backed by songbird's `TrackHandle::get_info()`). `/play`-family
      "Queued: ..." confirmations and `/queue`'s listing stay plain text —
      only the roadmap's explicit "now-playing" ask got the embed
      treatment, to keep this pass's scope tight.

## Phase 8 — Ops/deployment

- [x] Process management: systemd unit or a Dockerfile. Both provided
      (`Dockerfile`, `deploy/apollo.service`) — neither is a
      recommendation over the other, pick whichever matches your
      hosting. **Not build/run-tested**: `docker` isn't functional in
      this sandbox (no daemon access), so the Dockerfile is
      hand-reviewed for correctness (multi-stage layout, user/ownership
      ordering, migration embedding) but not actually built. Confirm it
      builds before relying on it.
- [x] Document the Discord application setup: bot invite URL, required
      OAuth2 scopes/permissions, gateway intents to enable in the
      Developer Portal. (README.md — also notes neither `GUILDS` nor
      `GUILD_VOICE_STATES` is a privileged intent, so nothing needs
      toggling on in the portal.)
- [x] Document the Google Cloud project setup: OAuth consent screen,
      YouTube Data API v3 enablement, verification requirements if the
      `youtube`/`youtube.readonly` scope triggers Google's sensitive-scope
      review for a public bot. (README.md — also flags a real operational
      trap confirmed via research: an unverified "Testing"-status app
      gets refresh tokens that expire after exactly 7 days, so a linked
      account silently needs `/link` again weekly unless the consent
      screen is moved to "In production," which requires Google's
      standard verification review for `youtube.readonly`.)
- [ ] Production secrets handling (not just `.env`). README.md documents
      the systemd `EnvironmentFile` pattern (chmod 600, separate from the
      world-readable unit file) as one option and names alternatives
      (a secrets manager/vault, systemd-creds, a cloud provider's native
      secret store) without picking one — this is a real deployment
      decision I'm not making unilaterally. Left unchecked pending your
      choice of hosting target, which determines which option actually
      fits.
- [ ] Basic logging/observability beyond local `tracing` output. Not
      implemented — `tracing` output currently goes to stdout/stderr only
      (captured by `journalctl`/`docker logs` either way this gets
      deployed). Adding a metrics/tracing backend (Prometheus endpoint,
      OpenTelemetry export, a hosted log aggregator, etc.) is another
      deployment-target-dependent choice left open rather than picked for
      you.

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
