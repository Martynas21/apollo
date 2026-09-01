//! Shared types for the `apollo` \<-\> `apollo-audio-worker` IPC boundary —
//! see the crate's sibling `audio-worker/` for the worker binary and
//! `apollo`'s `src/voice/ipc_backend.rs` for the client side.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod dto;
pub mod framing;
pub mod proto;

pub use dto::{ConnectionInfoDto, TrackStatusDto};
pub use framing::{FramingError, read_frame, write_frame};
pub use proto::{Envelope, Event, Request, Response};

/// Default Unix domain socket path for the `apollo` <-> `apollo-audio-worker`
/// IPC connection — matches `compose.yaml`'s volume mount. Both `apollo`
/// (`Config::audio_worker_socket`) and `apollo-audio-worker`
/// (`AUDIO_WORKER_SOCKET`) fall back to this if the env var is unset; defined
/// once here so the two can't drift out of sync with each other.
pub const DEFAULT_SOCKET_PATH: &str = "/run/apollo-ipc/audio.sock";

/// Reads an environment variable via an arbitrary key lookup (matching
/// [`std::env::var`]'s signature), treating both "unset" and "set but
/// empty" as absent. Shared by `apollo`'s `Config::from_env` and
/// `apollo-audio-worker`'s `main` so that e.g. `AUDIO_WORKER_SOCKET=""`
/// falls back to [`DEFAULT_SOCKET_PATH`] identically on both sides of the
/// IPC boundary, rather than the two processes disagreeing on the fallback.
/// The generic lookup (rather than calling `std::env::var` directly) lets
/// callers substitute an in-memory map in tests.
pub fn optional_env_var(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
) -> Option<String> {
    lookup(key).ok().filter(|value| !value.trim().is_empty())
}
