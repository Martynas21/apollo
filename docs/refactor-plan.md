# Refactor plan: conventional Rust layout

A phased plan to bring Apollo's structure in line with ordinary Rust project
conventions, and to split the four modules that currently hold 72% of the code.

Every phase is independently shippable. `cargo fmt --check`, `cargo clippy
--workspace --all-targets --all-features -- -D warnings` and `cargo test
--workspace --all-features` must pass at the end of each one. Phases 1, 2, 3,
5, 6 and 7 are pure moves with no behavior change; only phase 4 rewrites
behavior-carrying code, so that is where review effort belongs.

## Starting point

| File | Lines | Tests | Concerns in one file |
|---|---|---|---|
| `src/voice/player.rs` | 3118 | 60 | 4 traits, 2 error types, state structs, registry impl, test fakes, tests |
| `src/db.rs` | 1724 | 35 | connect, settings, playlists, sessions, queue, stats, users |
| `src/web/api.rs` | 765 | 0 | login, guilds, transport, queue, search, playlists, favourites |
| `src/youtube/api.rs` | 680 | 41 | subprocess runner, parsers, domain type, error type |

Two things deliberately stay as they are: the four-trait `VoiceBackend`
abstraction in `player.rs` (it is what makes the 60 fake-backend tests
possible), and the two-process split (load-bearing for audio packet timing —
see CLAUDE.md).

---

## Phase 1 — Library targets

`apollo` and `apollo-audio-worker` are binary-only. `src/main.rs` declares
`mod config; mod db; mod voice; mod web; mod youtube;`, so there is no library
target and `tests/` integration tests are structurally impossible. All 200
tests are inline `#[cfg(test)] mod tests`. That is why `src/web/api.rs` and
`src/web/users.rs` have 1061 lines and zero tests between them: there is no
seam from which to exercise an axum `Router`.

- Add `src/lib.rs` with `pub mod config; pub mod db; pub mod voice; pub mod
  web; pub mod youtube;` plus a `//!` crate doc comment.
- Reduce `src/main.rs` to config load, wiring, serve. No `mod` declarations.
- Same split for `audio-worker/`: `lib.rs` exposing `rpc` and `session`,
  `main.rs` down to bind/accept/shutdown.
- Move `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used,
  clippy::panic))]` to each new `lib.rs`; keep it on the `main.rs` roots too.
- Add `#![forbid(unsafe_code)]` to every crate root.
- Add `[dev-dependencies]` to the `apollo` manifest (currently none at all):
  `tower` (for `ServiceExt::oneshot`) and `http-body-util`.

Visibility widens as needed for `tests/` to reach items. Keep pure-function
tests inline where they are — `volume_multiplier`, `pick_radio_seed`,
`parse_flat_playlist_ids`, `classify_ytdlp_stderr`, the `truncate_*` family.
Only tests that drive a public API move out.

Once this lands, delete the `#[cfg(test)] pub async fn queue_push_back` hack at
`src/db.rs:514` — a test-only function living in production code because there
was nowhere else to put it.

**Done when:** both crates have `lib.rs`, `main.rs` files carry no `mod`
declarations, and the full test suite still passes unchanged.

---

## Phase 2 — Workspace hygiene

**Hoist shared dependency versions.** `tokio 1.53`, `anyhow`, `tracing`,
`tracing-subscriber 0.3.23`, `uuid`, `serde`, `dotenvy 0.15.7`, `async-trait`
and `songbird 0.6.0` are pinned separately in two or three manifests. Move to
`[workspace.dependencies]` in the root `Cargo.toml`; members use
`tokio = { workspace = true, features = [...] }`. Feature sets stay per-member
— `apollo` and `apollo-audio-worker` deliberately use different songbird
features, and the comment in the root manifest explaining why must be kept.

**Move the dashboard asset out of `src/`.** `src/web/dashboard.html` is a
non-Rust asset in the source tree. Move to `assets/dashboard.html` and update
the `include_str!` in `src/web/mod.rs`. Compile-time embedding is preserved.

> Line endings: `dashboard.html` is LF in the repo. Confirm the moved file is
> still LF before committing — a git round-trip can flip it to CRLF and produce
> a whole-file diff.

**Add `--locked`** to the `cargo build` and `cargo test` steps in
`.github/workflows/ci.yml` so `Cargo.lock` is actually enforced.

**Optional, low priority — virtual workspace root.** The root `Cargo.toml` is
currently both `[workspace]` and `[package]`, with `apollo`'s source at the
repo root. The textbook shape is a virtual manifest with `crates/apollo`,
`crates/apollo-audio-worker`, `crates/apollo-ipc` (directory names matching
package names). Costs: `cargo run` becomes `cargo run -p apollo`, and
`Dockerfile`, `compose.yaml`, `.claude/launch.json` and the `migrations/` path
in `sqlx::migrate!` all need updating. Skip unless the textbook layout is
wanted for its own sake.

**Housekeeping:** `.claude/worktrees/` holds 13 GB across two stale checkouts
with their own `target/` directories. Git ignores them via
`.git/info/exclude`, but rust-analyzer and every filesystem walk does not.
Prune them.

---

## Phase 3 — Fix the layering inversions

Two places where a lower layer is defined inside a higher one.

**`Track` lives in the yt-dlp adapter.** `src/youtube/api.rs:34` defines the
core domain type, and `src/db.rs:11`, `src/voice/player.rs:14`,
`src/voice/ipc_backend.rs:19` and `src/web/api.rs:16` all import it from there.
The database and the IPC backend have no business depending on the YouTube
client module.

- Create `src/model.rs` holding `Track`, `PlaylistListing` and `QueuedTrack`.
- `youtube/api.rs` keeps `YtDlpEntry` and maps it into `Track`.
- Everything else imports from `crate::model`.

**Config parsing lives in the wire-protocol crate.** `ipc/src/lib.rs` exports
`optional_env_var` and hardcodes `DEFAULT_SOCKET_ADDR = "audio-worker:7878"` —
an env-var helper and a Docker Compose service name inside a crate whose job is
`Envelope`/`Request`/`Response`. Both binaries reach into it for env parsing
(`src/config.rs:32`, `audio-worker/src/main.rs:25`).

- Give `audio-worker` its own `config.rs`, mirroring apollo's
  `from_source(lookup)` pattern so it becomes testable the way apollo's already
  is (17 tests).
- Move `optional_env_var` and the socket default out of `apollo-ipc`, leaving
  it as pure protocol. Each binary owns its own default.

**The dashboard reaches YouTube through the player.**
`src/voice/player.rs:1247-1257` has three pass-throughs to `YouTubeClient` —
`search_tracks`, `resolve_video`, `list_playlist` — called only from
`src/web/api.rs` (5 call sites).

- Put `YouTubeClient` on `WebState` and call it directly.
- Delete all three pass-throughs. `PlayerRegistry` keeps its own client for
  radio refill, where it is genuinely used.

---

## Phase 4 — The yt-dlp seam

`Command::new("yt-dlp")` is hand-built in four places, each re-implementing
cookie injection, timeout, `kill_on_drop` and stderr truncation:

- `src/youtube/api.rs` — search, get_video, playlist listing, hydrate
- `src/voice/resolve.rs:92` and `:146` — preflight, resolve
- `src/voice/radio.rs:72` — mix listing
- `src/voice/mod.rs:38` — version probe

`truncate_tail` exists three times verbatim (`youtube/api.rs:98`,
`resolve.rs:55`, `radio.rs:33`), and `STDERR_TRUNCATE_LEN = 200` is declared
three times.

Extract `src/youtube/ytdlp.rs`:

```rust
pub struct YtDlp { cookies_file: Option<String> }

impl YtDlp {
    pub async fn run(&self, args: &[&str], timeout: Duration)
        -> Result<String, YtDlpError>;
    pub async fn probe() -> Result<(), YtDlpError>;
}
```

`run` does spawn, `--cookies` injection, timeout, `kill_on_drop` and stderr
truncation exactly once. `probe` absorbs `check_playback_dependencies` and
`binary_runnable` from `src/voice/mod.rs` — a startup binary probe has nothing
to do with voice.

The payoff is larger than the deduplication: with spawning removed,
`resolve_stream` and `list_mix_video_ids` become pure parsers over a string and
are unit-testable against fixtures. Today their tests only cover the
classification and parsing helpers, never the functions themselves. Add those
tests as part of this phase.

Distinct per-call-site timeouts (`YT_DLP_TIMEOUT` 30s, `YT_DLP_BATCH_TIMEOUT`
90s, `PREFLIGHT_TIMEOUT` 30s, `RESOLVE_TIMEOUT` 30s, `MIX_LISTING_TIMEOUT` 30s,
`DEPENDENCY_PROBE_TIMEOUT` 10s) stay as they are — they are passed to `run`,
not collapsed into one value.

---

## Phase 5 — Split `player.rs` and `db.rs`

### `voice/player.rs` (3118 lines) becomes a directory

```
voice/backend.rs      VoiceEvents, VoiceBackend, VoiceCall, VoiceTrack traits
                      + AudioSource, TrackStatus        (player.rs:110-153)
voice/error.rs        PlayerError
voice/state.rs        GuildState, QueueSnapshot, SessionSnapshot,
                      StartOutcome, AdvanceFill + derived-state accessors
voice/registry/mod.rs        struct PlayerRegistry, new(), shared helpers
voice/registry/queue.rs      enqueue, remove, move, play, clear, shuffle
voice/registry/playback.rs   start sequence, advance, commit_started_track,
                             pause, resume, skip, stop
voice/registry/session.rs    join, leave, persist, restore
voice/registry/radio.rs      toggle + refill (pairs with voice/radio.rs)
voice/testing.rs      #[cfg(test)] FakeTrack, FakeCall, FakeBackend (~350 lines)
```

`impl PlayerRegistry` blocks split across files is idiomatic and costs nothing.

Two things to preserve carefully:

- The invariant test
  `every_action_is_panic_free_and_keeps_the_handle_track_id_invariant_in_every_state`
  cross-references the action x state matrix in `docs/player-states.md`. Keep
  them in lockstep.
- Update `docs/player-states.md` with the new file layout, and add a `//!` on
  `voice/mod.rs` pointing at it.

### `db.rs` (1724 lines) becomes a directory

```
db/mod.rs        connect + migrate + re-exports
db/settings.rs   guild volume
db/playlists.rs  saved playlists + cached tracks
db/session.rs    guild session persistence
db/queue.rs      queue_push/pop/peek/len/all/clear/replace_all
db/stats.rs      play counts, top tracks, top playlists
db/users.rs      credentials, privileges, CRUD
```

The 35 tests distribute alongside their modules.

Keep runtime `sqlx::query(...)` — 43 call sites, no `query!` macros, so there is
no `.sqlx` offline cache to maintain and CI needs no `DATABASE_URL`. The
compile-time-checked macros would add a build-time database requirement this
project does not need.

---

## Phase 6 — Web layer

`src/web/api.rs` is 765 lines covering seven unrelated surfaces, and the route
table sits in `src/web/mod.rs`, 200 lines from the handlers it names.
`ErrorBody` and `error_response` are byte-identical in `api.rs:21-33` and
`users.rs:17-30`.

```
web/mod.rs        WebState + serve() + merge of route modules
web/response.rs   ErrorBody, error_response, player_error_status,
                  parse_guild_id, parse_channel_id, TrackJson, track_json
web/routes/guilds.rs       list_guilds, list_voice_channels, join_voice_channel
web/routes/playback.rs     toggle_pause, skip, stop, shuffle, toggle_radio,
                           volume, now_playing, now_playing_ws
web/routes/queue.rs        add, remove, move, play, clear
web/routes/search.rs       search
web/routes/playlists.rs    list, import, play, refresh, remove
web/routes/favourites.rs   favourites
web/routes/users.rs        (existing, moved)
web/routes/auth.rs         login (currently orphaned in api.rs)
```

Each route module exports `pub fn routes() -> Router<WebState>`; `mod.rs`
merges them and applies the middleware layers. This deletes the 60-line
`playback_routes()` function and puts each route next to its handler.

The `require_admin`-inside-`require_session` layering in `src/web/mod.rs` and
its explanatory comment must survive unchanged — it is subtle and correct.

Then write the route tests that phase 1 enabled: `tests/web_routes.rs` driving
the router via `tower::ServiceExt::oneshot`. No network, no Discord. This is
the single biggest coverage gap in the repo.

---

## Phase 7 — `thiserror`

Five error enums each carry a hand-written `impl Display` plus
`impl std::error::Error`:

- `PlayerError` (`player.rs:72`)
- `YouTubeApiError` (`youtube/api.rs:42`)
- `PlaybackError` (`resolve.rs:19`)
- `RadioError` (`radio.rs:13`)
- `FramingError` (`ipc/src/framing.rs`)

That is roughly 100 lines of boilerplate. `thiserror` derives collapse it, and
`#[from]` turns the ad-hoc conversion helpers — `better_playback_error`
(`player.rs:52`) and `classify_playback_failure` (`player.rs:650`) — into plain
`?`. `anyhow` is already a dependency and `thiserror` is already in the
lockfile transitively.

Error message text must not change: several tests assert on it.

---

## Order

1. Phase 1 — unblocks everything else, zero behavior change
2. Phase 4 — removes the worst duplication, makes resolve/radio testable
3. Phase 3 — small and mechanical, stops the inversions spreading
4. Phase 5 — the big mechanical move, safest once tests can live outside
5. Phase 6 — split, then write the route tests phase 1 enabled
6. Phase 7, then phase 2 — whenever
