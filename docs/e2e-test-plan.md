# Manual end-to-end test plan

Everything in this repo is unit-tested where that's possible without a
live Discord gateway connection or working `yt-dlp`/`ffmpeg` binaries.
This plan covers what's left: the parts that can only be verified by
actually running the bot against a real Discord server. Run through it
once after setup (`README.md`), and again after any change that touches
`src/commands/`, `src/voice/`, or `src/youtube/`.

Use a private test/staging Discord server for this, not a real one you
share with other people — some steps involve deliberately breaking things
(disconnecting mid-playback).

## 0. Prerequisites

- [x] `.env` filled in per README's Discord application setup section.
- [x] `which yt-dlp ffmpeg` both resolve; `cargo run` gets past the
      startup dependency check without erroring.
- [x] The bot's invite URL (README) has been used to add it to your test
      server, and it shows **online** within a few seconds of `cargo run`.
- [x] `/ping` replies "Pong!" — confirms slash command registration
      actually worked before testing anything more complex.

## 1. Browsing

- [ ] `/add_to_queue <query>` returns up to 5 results for a query you know
      has results (e.g. an artist name), with a select menu to queue one.
- [ ] `/add_to_queue <query> <number>` queues that result directly, without
      the menu.
- [ ] `/add_to_queue <query> 0` (or any out-of-range number) replies
      "Invalid selection" rather than panicking or hanging.
- [ ] `/playlist_play <a public playlist URL>` queues every track in it and
      reports the count; `/queue` shows them all in order.
- [ ] `/playlist_play <the same playlist's bare ID>` (no URL) works the same
      way.
- [ ] `/playlist_play <an empty or nonexistent playlist>` replies "That
      playlist is empty (or couldn't be found)" rather than erroring.

## 2. Playback

- [ ] Join a voice channel yourself, then `/play <a known YouTube URL>` —
      the bot joins your channel and audio plays.
- [ ] `/play <plain text query>` (no URL) queues the top search result.
- [ ] `/play <youtu.be short link>` and `/play <.../shorts/... link>` both
      resolve correctly (not just the full `/watch?v=` form).
- [ ] While something is playing, `/play` a second track — it queues
      rather than interrupting; `/queue` shows both the now-playing track
      and the queued one.
- [ ] `/pause` then `/resume` — audio actually stops and restarts, not
      just the command replying successfully.
- [ ] `/skip` — the queued track starts playing next automatically (no
      manual `/play` needed).
- [ ] `/stop` — audio stops and `/queue` shows nothing playing.
- [ ] **Auto-disconnect**: queue a track, let it finish with nothing else
      queued, and wait out the idle timeout (~2.5 minutes) — the bot should
      leave the voice channel on its own. (Long wait — worth doing once,
      not on every test pass.)
- [ ] `/radio` toggles radio mode on; once the queue drains, it keeps
      queuing similar tracks on its own (seeded from whatever last played)
      rather than going idle.

## 3. Player panel

`/player` posts a single persistent, self-updating panel per guild. This
is the one area with no live-Discord substitute for manual testing —
everything here depends on real message edits landing in real time.

- [ ] `/player` with nothing playing posts a panel: an informational
      "search, `/play`, or `/playlist_play` to get started" message,
      playback buttons (Pause/Skip/Stop/Shuffle) disabled, and
      Search/Volume/Radio buttons enabled.
- [ ] Panel's **Search** button opens a modal; submitting a query shows an
      ephemeral result picker (only visible to you); picking a result
      starts playback (auto-joining your voice channel) and the panel
      updates in place to show it — title, thumbnail, channel, requester,
      progress.
- [ ] Panel's own Pause/Resume/Skip/Stop/Shuffle/Volume/Radio buttons work
      and update the panel in place immediately.
- [ ] Run `/skip`, `/pause`, `/stop`, `/shuffle`, or `/volume` as **slash
      commands** (not panel buttons) while a panel is live — the panel
      message updates on its own within a second or two, with nobody
      touching its buttons.
- [ ] Let a track play to its natural end with another queued behind it —
      the panel advances to the next track on its own.
- [ ] Let the queue drain and the idle-timeout auto-disconnect fire (see
      the "Auto-disconnect" step above) — the panel updates to the
      "nothing is playing" empty state rather than freezing on the last
      track that played.
- [ ] Run `/player` again in the same guild — the previous panel message is
      deleted (check the channel), and only the new one remains and keeps
      updating.
- [ ] Manually delete the panel message yourself, then trigger a state
      change (e.g. `/play` something) — the bot doesn't error or hang; it
      just has no panel to update until `/player` is run again.

## 4. Failure modes

- [ ] `/play` an age-restricted video — replies with a clear
      "age-restricted" message, not a raw error dump or a hang.
- [ ] `/play` a deleted/private video ID — replies "unavailable", same
      standard.
- [ ] Run any command that queues a track while **not** in a voice channel
      and the bot isn't already connected — replies "join a voice channel
      first, or use `/join`", doesn't silently fail.
- [ ] If `YT_DLP_COOKIES_FILE` is unset (or stale) and requests start
      failing with "Sign in to confirm you're not a bot" — set it to a
      fresh `cookies.txt` and confirm search/playback recover.

## 5. Multi-guild sanity (skip if you only have one test server)

- [ ] Playing in two different guilds at once doesn't cross-contaminate
      queues — `/queue` in guild A never shows guild B's tracks.
- [ ] `/volume` set in guild A doesn't affect guild B's volume, and
      persists across a bot restart (`Ctrl+C`, `cargo run` again) for
      whichever guild it was set in.
