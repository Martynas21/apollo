# Player state model

Reference documentation for `src/voice/player.rs`'s `PlayerRegistry`/`GuildState`:
what the code actually does, not an aspirational design.

## The five observable playback states

Everything the web dashboard sees is one of five states, derived from
`PlayerRegistry::queue_snapshot` (`now_playing`/`loading`/`last_played`) plus
`is_paused()`. They are keyed off `GuildState.now_playing`,
`current_track_id`, `last_played`, and the current track handle's pause
status — never a dedicated "state" field.

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
| `stop` | `NothingPlaying` | clears `now_playing`/queue, disables radio | stops the handle, clears queue/radio state, bumps `epoch` | same as Playing | `NothingPlaying` |
| `shuffle` | `NothingToShuffle` (queue has < 2) | same | shuffles the upcoming queue if it has ≥ 2 tracks, restarts the prefetch | same as Playing | `NothingToShuffle` unless a queue survived |
| `toggle_radio` | flips the flag; refills if turning on with an empty queue | same | same | same | same |
| `clear_queue` | `QueueEmpty` | `QueueEmpty` unless upcoming tracks exist | drops upcoming, leaves `now_playing` alone; refills if radio is on | same as Playing | `QueueEmpty` |
| `set_volume` | persists the setting; no current track to apply it to | persists; no handle yet | persists and applies to the current handle | same as Playing | persists |
| `remove_queue_track` | `InvalidQueueIndex` | `InvalidQueueIndex` unless upcoming tracks exist | removes the track at that queue position, leaves `now_playing` alone, restarts the prefetch if the first upcoming track changed | same as Playing | `InvalidQueueIndex` |
| `move_queue_track` | `InvalidQueueIndex` | `InvalidQueueIndex` unless upcoming tracks exist | moves an upcoming track from one queue position to another, shifting the tracks in between, leaves `now_playing` alone, restarts the prefetch if the first upcoming track changed | same as Playing | `InvalidQueueIndex` |
| `play_queue_track` | `NothingPlaying` | `InvalidQueueIndex` unless upcoming tracks exist | pulls the chosen upcoming track out of the queue, stops the current handle without requeuing it, and starts the chosen track immediately; every other upcoming track keeps its relative order | same as Playing | `NothingPlaying` |

All of these are reached only through `src/web/api.rs`'s HTTP handlers — there
is no longer a Discord-side command or panel driving them.

In every case, the action either succeeds, silently no-ops, or returns a
well-typed `PlayerError` — never panics — and the invariant
`current_handle.is_some() == current_track_id.is_some()` holds afterward.
This table is exercised directly by the table-driven test
`every_action_is_panic_free_and_keeps_the_handle_track_id_invariant_in_every_state`
in `src/voice/player.rs`.
