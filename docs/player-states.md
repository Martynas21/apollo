# Player state model

Reference documentation for `PlayerRegistry`/`GuildState`, split across
`src/voice/state.rs` (the structs) and `src/voice/registry/*.rs` (the
`impl PlayerRegistry` blocks that act on them): what the code actually does,
not an aspirational design.

```
voice/backend.rs      VoiceEvents, VoiceBackend, VoiceCall, VoiceTrack traits
                      + AudioSource, TrackStatus
voice/error.rs        PlayerError
voice/state.rs        GuildState, QueueSnapshot, SessionSnapshot,
                      StartOutcome, AdvanceFill + derived-state accessors
                      (`is_paused`, `track_status`, `queue_snapshot`)
voice/registry/mod.rs        struct PlayerRegistry, new(), shared helpers
voice/registry/queue.rs      enqueue, enqueue_next, enqueue_many, remove_queue_track,
                             move_queue_track, play_queue_track, clear_queue, shuffle
voice/registry/playback.rs   spawn/run_start_sequence, try_start_playback,
                             resolve_for_start, commit_started_track, advance,
                             pause, resume, skip, stop, get_volume/set_volume
voice/registry/session.rs    join, leave, leave_if_idle, persist_session,
                             restore_session_if_new, load/apply persisted session
voice/registry/radio.rs      toggle_radio, is_radio_enabled, maybe_spawn_radio_refill,
                             run_radio_refill*, radio_refill_*, push_radio_refill,
                             await_radio_refill_then_repop, kick_off_if_idle
voice/registry/watchdog.rs   spawn/run_stall_watchdog, stop_stalled_track,
                             rebuild_voice_session
voice/registry/recovery.rs   handle_track_error, claim_early_retry, abandon_current
voice/testing.rs      #[cfg(test)] FakeTrack, FakeCall, FakeBackend + test helpers
```

## The five observable playback states

Everything the web dashboard sees is one of five states, derived from
`PlayerRegistry::queue_snapshot` (`now_playing`/`loading`/`last_played`) plus
`is_paused()`. They are keyed off `GuildState.now_playing`,
`current_track_id`, `last_played`, and `GuildState.paused`, which
`pause`/`resume` maintain — never a dedicated "state" field or a worker
round trip.

| State | `now_playing` | `current_track_id` | `last_played` | `is_paused()` | Dashboard shows |
| --- | --- | --- | --- | --- | --- |
| Empty | `None` | `None` | `None` | `None` | "Nothing is playing" |
| Buffering | `Some` | `None` | — | `None` | `"buffering"` |
| Playing | `Some` | `Some` | — | `Some(false)` | `"playing"` |
| Paused | `Some` | `Some` | — | `Some(true)` | `"paused"` |
| Queue-finished | `None` | `None` | `Some` | `None` | `"queue_finished"` |

Buffering is the gap between `enqueue`/`advance` setting `now_playing`
(so a track shows immediately) and `commit_started_track` setting
`current_track_id` once the audio source has actually resolved and playback
has started via `VoiceCall::play`.

Dashboard transport buttons: Pause/Resume, Skip, and Stop are disabled
whenever `now_playing` is `None` (Empty and Queue-finished). Shuffle is
disabled with fewer than two upcoming tracks. Clear Queue is disabled with an
empty queue. Radio, Volume, Search, and Playlists stay enabled in every
state.

## Radio as an orthogonal flag

`GuildState.radio_enabled` doesn't change which of the five states applies —
it changes what happens around the edges:

- **Auto-refill.** Whenever the queue empties out from underneath a playing
  guild (`advance`, `clear_queue`) or `toggle_radio` turns it on with an
  empty queue, and radio is enabled, `maybe_spawn_radio_refill` spawns
  `run_radio_refill`, which picks a weighted-random seed from
  `radio_history`, lists a YouTube mix, hydrates a few unplayed candidates,
  and pushes them onto the queue.
- **`stop()` silently disables it.** It unconditionally clears
  `radio_enabled`, `radio_history`, `radio_requested_by`, and
  `radio_played` — there is no separate "radio survives a stop" mode.
- **Refill lifecycle.** At most one refill runs per guild at a time:
  `radio_refill_snapshot` atomically claims `radio_refill_running` (and
  lazily creates a `radio_refill_notify: Arc<Notify>`) before any work
  starts, and bails immediately if a refill is already running or the mix is
  known to be exhausted (`radio_exhausted`). `finish_radio_refill` always
  clears the running flag and calls `notify_waiters()` on the way out,
  regardless of which branch `run_radio_refill_body` took.
- **The `advance()` refill wait.** When a track ends with an empty queue and
  radio is enabled, if a refill is already running `advance()` waits up to
  `RADIO_ADVANCE_WAIT` (3s) on that guild's notify before finalizing idle,
  then re-pops the DB queue — the fix for skip "flashing idle" for several
  seconds while a refill is still in flight. This is best-effort only:
  `run_radio_refill_body`'s own `kick_off_if_idle` call remains the safety
  net if the wait times out or a wakeup is missed, so no new failure mode is
  introduced, only a shrunk gap in the common case.

## Stall watchdog

`commit_started_track` also spawns `run_stall_watchdog`
(`voice/registry/watchdog.rs`) for every track it commits. Every 5 s it asks
the track handle for its status; a track that is not paused and whose
reported position has not changed for 30 s is stopped, which ends it like any
other track and lets `advance()` promote the next one. If the audio worker
has not reported the stopped track as ended within a further 10 s, the
watchdog drops the track (`now_playing` and the handle are cleared without
starting another) and rebuilds the guild's voice session (`leave` then
`join` on the same channel, which replaces the worker's driver; the rejoin
restores the persisted session and starts the rest of the queue exactly
once), since a worker that ignores a stop is not going to play the next
track either. The watchdog exits as soon as its track stops being
`current_track_id`, and its polling loop is bounded by the track's own length
plus the stall allowance. The audio worker's own stream reads time out after
15 s, after which songbird resumes the stream from the byte offset it
reached, so the watchdog only fires for a track that genuinely cannot recover
on its own.

## Start failures and early errors

`run_start_sequence` walks the queue past tracks whose stream cannot be
resolved, but gives up after three consecutive failures: the failed
candidates are dropped, `now_playing` is cleared, and the rest of the queue
is left in place. Joining the guild again (any `join`, including one to the
channel it is already in) starts that queue via `kick_off_if_idle`, and so
does `play_queue_track`; `stop` clears it. A guild with nothing playing or
loading counts as idle for `leave_if_idle` whether or not such a queue
exists, as does one whose current track is paused. A `join` that moves the
bot to another channel replaces the worker's driver, so the track that was
playing is started again on the new one.

A track the worker reports as errored within its first 5 s of playback is
retried once with a freshly resolved stream URL (`handle_track_error` in
`voice/registry/recovery.rs`): its handle is dropped so the guild reads as
Buffering, `now_playing` stays put, and `GuildState.retry_used` marks the
attempt so a second early failure moves on; any later non-retry start clears
the mark. The retry keeps the
prefetch its first start armed and adds neither a second history entry nor a
second play count. A report that arrives before `commit_started_track` has
run is parked in `GuildState.uncommitted_outcome` and applied by the commit.
Three tracks in a row that fail early even after their retry
(`consecutive_early_failures`) make `abandon_current` drop the current track
and leave the rest of the queue in place, the same outcome as three start
failures; a track that finishes or fails later on resets the count. Errors
later in a track always advance. Prefetched stream
URLs older than an hour are resolved again before use, since they expire.

## Action × state compatibility matrix

One row per player-affecting action, one column per state. "radio" columns
only apply where the action's behavior actually differs with radio on vs.
off; where it doesn't, a single cell covers both.

| Action | Empty | Buffering | Playing | Paused | Queue-finished |
| --- | --- | --- | --- | --- | --- |
| `enqueue`/`enqueue_many` | starts immediately | appends behind the loading track | appends behind the current track | appends behind the current track | starts immediately |
| `enqueue_next` | starts immediately | inserts ahead of the upcoming queue, behind the loading track | inserts as the very next upcoming track, restarts the prefetch | same as Playing | starts immediately |
| `pause` | `NothingPlaying` | `NothingPlaying` (no handle yet) | pauses, arms idle-disconnect timer | no-op (already paused) | `NothingPlaying` |
| `resume` | `NothingPlaying` | `NothingPlaying` | no-op (already playing) | resumes | `NothingPlaying` |
| `skip` | `NothingPlaying` | `NothingPlaying` | stops the handle; `advance()` promotes the next track (or refill-waits/idles if radio) | same as Playing | `NothingPlaying` |
| `stop` | `NothingPlaying`, or clears a queue left behind by an abandoned start | clears `now_playing`/queue, disables radio | stops the handle, clears queue/radio state, bumps `epoch`; the idle timer and persisted session are settled even if the worker does not answer | same as Playing | `NothingPlaying`, or clears a queue left behind |
| `shuffle` | `NothingToShuffle` (queue has < 2) | same | shuffles the upcoming queue if it has ≥ 2 tracks, restarts the prefetch | same as Playing | `NothingToShuffle` unless a queue survived |
| `toggle_radio` | flips the flag; refills if turning on with an empty queue | same | same | same | same |
| `clear_queue` | `QueueEmpty` | `QueueEmpty` unless upcoming tracks exist | drops upcoming, leaves `now_playing` alone; refills if radio is on | same as Playing | `QueueEmpty` |
| `set_volume` | persists the setting; no current track to apply it to | persists; no handle yet | persists and applies to the current handle | same as Playing | persists |
| `remove_queue_track` | `InvalidQueueIndex` | `InvalidQueueIndex` unless upcoming tracks exist | removes the track at that queue position, leaves `now_playing` alone, restarts the prefetch if the first upcoming track changed | same as Playing | `InvalidQueueIndex` |
| `move_queue_track` | `InvalidQueueIndex` | `InvalidQueueIndex` unless upcoming tracks exist | moves an upcoming track from one queue position to another, shifting the tracks in between, leaves `now_playing` alone, restarts the prefetch if the first upcoming track changed | same as Playing | `InvalidQueueIndex` |
| `play_queue_track` | `InvalidQueueIndex`, or starts the chosen track of a queue left behind | `InvalidQueueIndex` unless upcoming tracks exist | pulls the chosen upcoming track out of the queue, stops the current handle without requeuing it, and starts the chosen track immediately; every other upcoming track keeps its relative order | same as Playing | same as Empty |

All of these are reached only through `src/web/api.rs`'s HTTP handlers — there
is no longer a Discord-side command or panel driving them.

In every case, the action either succeeds, silently no-ops, or returns a
well-typed `PlayerError` — never panics — and the invariant
`current_handle.is_some() == current_track_id.is_some()` holds afterward.
This table is exercised directly by the table-driven test
`every_action_is_panic_free_and_keeps_the_handle_track_id_invariant_in_every_state`
in `src/voice/registry/mod.rs`.
