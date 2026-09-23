//! The player state machine: `PlayerRegistry`/`GuildState` and the five
//! derived playback states they encode. Read `docs/player-states.md` before
//! touching anything under `registry/` or `state.rs` — it documents the
//! state model and the action × state compatibility matrix that
//! `registry::every_action_is_panic_free_and_keeps_the_handle_track_id_invariant_in_every_state`
//! exercises directly.

pub mod backend;
pub mod error;
pub mod ipc_backend;
pub mod presence;
pub mod radio;
mod registry;
pub mod resolve;
mod state;
#[cfg(test)]
mod testing;

pub use crate::model::QueuedTrack;
pub use error::PlayerError;
pub use registry::PlayerRegistry;
pub use state::QueueSnapshot;
