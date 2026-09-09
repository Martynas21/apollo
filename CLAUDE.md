# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Apollo is a Discord bot that streams audio from YouTube (search, direct URL,
or playlist) into a voice channel. All YouTube access goes through a `yt-dlp`
subprocess — no Google API, no OAuth. Rust, edition 2024, tokio async
throughout. It's meant to run locally/single-user, not as a multi-tenant
service — see README.md "Running it" before suggesting deployment-style
hardening (secrets managers, metrics backends, etc.) that this setup doesn't need.

## Commands

```sh
cargo fmt --check                                          # formatting (CI)
cargo fmt                                                   # apply formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings   # lint (CI)
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-features                       # unit tests (CI), no live Discord/YouTube needed
cargo test <test_name>                                       # run a single test
cargo llvm-cov --all-features --workspace --summary-only     # coverage (requires cargo-llvm-cov + llvm-tools-preview)
```

Run fmt/clippy/test locally before pushing — CI (`.github/workflows/ci.yml`) enforces all of them.

Running the bot requires **two processes** (see Architecture): `apollo` needs
`apollo-audio-worker` reachable via `AUDIO_WORKER_SOCKET` or it can't play
anything.

```sh
AUDIO_WORKER_SOCKET=127.0.0.1:7878 cargo run --bin apollo-audio-worker &
AUDIO_WORKER_SOCKET=127.0.0.1:7878 cargo run --bin apollo
```

`yt-dlp` and a JS runtime (Deno) must be on `PATH` for `apollo`
(not `apollo-audio-worker`) — `yt-dlp` is checked at startup, fails fast if missing.
`docker compose up -d --build` runs both processes via `compose.yaml`/`Dockerfile`.

## Architecture

Three-crate Cargo workspace:

- **`apollo`** (root `src/`) — the Discord-facing process: serenity (gateway/REST)
  + poise (slash commands) + sqlx (SQLite), on tokio. Owns all yt-dlp calls and
  the database; has no direct voice connection.
- **`audio-worker/`** (`apollo-audio-worker`) — a separate OS process holding
  the actual `songbird::Driver`/voice connection, isolated so gateway traffic,
  DB writes, and `yt-dlp` spawns in `apollo` can't starve the mixer's packet
  timing and cause audible stutter (this was a real, previously-misdiagnosed
  problem — see the "Discord replies", "IPC" and "stream directly into songbird"
  entries in `git log`). Streams a resolved media URL straight into songbird
  via `songbird::input::HttpRequest` + symphonia — no download, no local file.
  Much smaller image than `apollo`: no `yt-dlp`/Deno, no Discord
  token, no DB access.
- **`ipc/`** (`apollo-ipc`) — the wire protocol shared by both: length-prefixed
  framing (`framing.rs`) over a plain TCP socket, `Envelope`/`Request`/
  `Response`/`Event` enums (`proto.rs`) and DTOs (`dto.rs`). `apollo` is the
  IPC client (`src/voice/ipc_backend.rs`'s `VoiceBackend`), `apollo-audio-worker`
  the server (`audio-worker/src/rpc.rs`). Requests are `Join`/`Leave`/`Play`/
  `Pause`/`Resume`/`Stop`/`SetVolume`/`Status`, all keyed by `guild_id` +
  `track_id` (a `Uuid`, minted per-track so stale responses/events for an
  already-superseded track can be detected and dropped); worker→apollo
  `Event`s (`TrackFinished`/`TrackErrored`/`ConnectionLost`) drive queue
  advancement. TCP was chosen over a Unix socket specifically so both
  processes can run natively (including cross-platform, e.g. one side in
  Docker) without sharing a filesystem — see `c99ee73` and the `AUDIO_WORKER_SOCKET`
  env var (`host:port`, defaults to Docker's `audio-worker:7878`).

### Player state machine (`src/voice/player.rs`, ~3000 lines)

The core of the bot. `PlayerRegistry`/`GuildState` hold **no explicit "state"
field** — every observable state is derived from `now_playing`,
`current_track_id`, `last_played`, and the current handle's pause status. Full
reference: **`docs/player-states.md`** — read it before touching player logic.
Key points:

- Five derived states: Empty, Buffering, Playing, Paused, Queue-finished.
  Buffering is the gap between `enqueue`/`advance` (sets `now_playing`
  immediately) and `commit_started_track` (sets `current_track_id` once audio
  has actually started).
- Radio mode (`radio_enabled`) is an orthogonal flag, not a sixth state — it
  changes auto-refill behavior at the edges (queue-empty refill, `/stop`
  always clears it, at-most-one-refill-per-guild via `radio_refill_running`
  + `Notify`).
  `/radio`'s implementation lives in `src/voice/radio.rs`.
- Invariant that must hold after every action: `current_handle.is_some() ==
  current_track_id.is_some()`, and no action panics — it succeeds, no-ops, or
  returns a typed `PlayerError`. This is exercised by a table-driven test in
  `player.rs` (`every_action_is_panic_free_and_keeps_the_handle_track_id_invariant_in_every_state`)
  cross-referencing the action×state matrix in `docs/player-states.md` — extend
  both together when adding a new action or state transition.

### Other modules

- `src/commands/` — poise slash commands: `playback.rs`, `library.rs`
  (search/queue/saved playlists), `radio.rs`.
- `src/youtube/api.rs` — the `yt-dlp` subprocess client (search,
  single-video metadata, playlist listing — all via `yt-dlp -j`).
- `src/voice/resolve.rs` — resolves a track to a direct streamable URL
  (metadata-only `yt-dlp -j`, no download). `src/voice/mod.rs` does the
  `yt-dlp` startup dependency check.
- `src/voice/panel.rs` — renders/updates the persistent `/player` panel.
- `src/db.rs` — sqlx/SQLite: per-guild settings (volume), saved playlists,
  guild sessions. Migrations in `migrations/` are embedded into the binary at
  compile time (sqlx `migrate!`).

## Code style (enforced by `clippy.toml` + workspace `[lints]`, not just convention)

- **No recursion** — use iteration.
- **No unbounded loops** outside an explicitly-allowed list: `apollo-audio-worker`'s
  driver loop, the IPC accept/read loops (`ipc_backend.rs`, `audio-worker/src/rpc.rs`),
  and the DB migration-retry loop (`db.rs`). A new unbounded loop anywhere else
  needs explicit justification in review.
- **Functions stay under ~60 lines** (`clippy::too_many_lines`, threshold set
  in `clippy.toml`) — split by sub-step, not arbitrary truncation.
- **No `unwrap()`/`expect()`/`panic!()` outside `#[cfg(test)]`** — propagate
  `anyhow::Result` instead. The one sanctioned exception: recovering a
  poisoned `Mutex` via `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)`
  rather than propagating, since poisoning here doesn't indicate corrupted
  state worth crashing over.
- No AI/task-referencing comments (e.g. "added for the X fix", "handles the
  case from issue #123") — describe current behavior only, never the history
  or motivation behind a change.

## Commit messages

Short — a single concise line (imperative mood, like the existing `git log`).
No body/footer, and specifically **no `Co-Authored-By`/`Claude-Session`
trailer** — the user has opted out of that attribution in this repo,
regardless of any default attribution instructions elsewhere.
