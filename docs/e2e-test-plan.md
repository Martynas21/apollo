# Manual end-to-end test plan

Everything in this repo is unit-tested where that's possible without a
live Discord gateway connection, a real Google OAuth2 client, or working
`yt-dlp`/`ffmpeg` binaries. This plan covers what's left: the parts that
can only be verified by actually running the bot against a real Discord
server and a real linked Google account. Run through it once after setup
(`README.md`), and again after any change that touches `src/commands/`,
`src/voice/`, or `src/youtube/`.

Use a private test/staging Discord server for this, not a real one you
share with other people — some steps involve deliberately breaking things
(revoking access, disconnecting mid-playback).

## 0. Prerequisites

- [x] `.env` filled in per README's Discord/Google Cloud setup sections.
- [x] `which yt-dlp ffmpeg` both resolve; `cargo run` gets past the
      startup dependency check without erroring.
- [x] The bot's invite URL (README) has been used to add it to your test
      server, and it shows **online** within a few seconds of `cargo run`.
- [x] `/ping` replies "Pong!" — confirms slash command registration
      actually worked before testing anything more complex.

## 1. Linking

- [x] `/link` replies ephemerally with a Google auth URL.
- [x] Opening it in a browser shows Google's consent screen for the
      correct app name, requesting only the `youtube.readonly` scope (not
      more).
- [x] Completing consent shows a "Linked!" page from the bot's local
      callback server, and the browser tab can be closed.
- [ ] Running `/link` again (already linked) still works, and the reply
      mentions it replaces the existing link.
- [ ] **CSRF/replay check**: reload the callback URL from step above a
      second time (browser back button + refresh, or copy/paste the exact
      URL again) — it should be rejected ("unrecognized or already-used
      link attempt"), not silently re-processed.

## 2. Browsing

- [ ] `/playlists` lists your account's playlists (or "you don't have any
      playlists" if there genuinely aren't any).
- [ ] `/playlistplay <n>` on a non-empty playlist lists its tracks.
- [ ] `/liked` lists liked videos (or the empty-state message).
- [ ] `/search <query>` returns up to 5 results for a query you know has
      results (e.g. an artist name).
- [ ] Invalid selections (`/playlistplay 99`, `/searchplay <query> 0`)
      reply "Invalid selection" rather than panicking or hanging.

## 3. Playback

- [ ] Join a voice channel yourself, then `/play <a known YouTube URL>` —
      the bot joins your channel and audio plays.
- [ ] `/play <plain text query>` (no URL) queues the top search result.
- [ ] `/play <youtu.be short link>` and `/play <.../shorts/... link>` both
      resolve correctly (not just the full `/watch?v=` form).
- [ ] While something is playing, `/play` a second track — it queues
      rather than interrupting; `/queue` shows both the now-playing track
      and the queued one.
- [ ] `/nowplaying` shows an embed: title (linking to the actual video),
      thumbnail image, channel, requester mention, and a progress value
      that visibly increases if you run it twice a few seconds apart.
- [ ] `/pause` then `/resume` — audio actually stops and restarts, not
      just the command replying successfully.
- [ ] `/skip` — the queued track starts playing next automatically (no
      manual `/play` needed), and `/nowplaying` reflects the new track.
- [ ] `/stop` — audio stops and `/queue`/`/nowplaying` both show nothing
      playing.
- [ ] `/leave` while connected — bot leaves the channel; `/queue` shows
      nothing (state was cleared).
- [ ] **Auto-disconnect**: queue a track, let it finish with nothing else
      queued, and wait out the idle timeout (5 minutes) — the bot should
      leave the voice channel on its own. (Long wait — worth doing once,
      not on every test pass.)

## 4. Failure modes

- [ ] `/play` an age-restricted video — replies with a clear
      "age-restricted" message, not a raw error dump or a hang.
- [ ] `/play` a deleted/private video ID — replies "unavailable", same
      standard.
- [ ] `/play` (or any browsing command) from an account that has never
      run `/link` — replies telling them to `/link` first.
- [ ] Run any command that hits the YouTube API while **not** in a voice
      channel and the bot isn't already connected — replies "join a voice
      channel first, or use `/join`", doesn't silently fail.

## 5. Re-link handling

This is the one that needs deliberate setup: it exercises the
`AccessTokenError::RefreshFailed { revoked: true, .. }` path from
`src/youtube/oauth.rs`.

- [ ] With an account already linked, go to
      [Google Account → Third-party access](https://myaccount.google.com/connections)
      and revoke Apollo's access there (not via `/unlink` — this simulates
      the user revoking it externally, which `/unlink` can't reach).
- [ ] Run any command that needs a fresh access token (wait past the
      current token's ~1hr expiry, or just try a command right after
      revoking if the access token has already expired). It should reply
      with the "revoked or expired — run `/link` again" message, not the
      generic "you need to link" message a never-linked user would see.
- [ ] `/link` again afterward — should work cleanly (the stale row was
      deleted automatically per the `invalid_grant` handling).

## 6. Unlinking

- [ ] `/unlink` on a linked account — confirms unlinked, and a subsequent
      `/play` correctly says "you need to link your Google account
      first."
- [ ] `/unlink` when nothing is linked — replies "you don't have a linked
      Google account" rather than erroring.
- [ ] Restart the bot process entirely (`Ctrl+C`, `cargo run` again) after
      linking — the link should still be there afterward (SQLite
      persistence surviving a restart, Phase 3's actual "done when").

## 7. Multi-guild sanity (skip if you only have one test server)

- [ ] Playing in two different guilds at once doesn't cross-contaminate
      queues — `/queue` in guild A never shows guild B's tracks.
