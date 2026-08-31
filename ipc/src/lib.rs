//! Shared types for the `apollo` \<-\> `apollo-audio-worker` IPC boundary —
//! see the crate's sibling `audio-worker/` for the worker binary and
//! `apollo`'s `src/voice/ipc_backend.rs` for the client side.

pub mod dto;
pub mod framing;
pub mod proto;

pub use dto::{ConnectionInfoDto, TrackStatusDto};
pub use framing::{read_frame, write_frame, FramingError};
pub use proto::{Envelope, Event, Request, Response};

/// Default Unix domain socket path for the `apollo` <-> `apollo-audio-worker`
/// IPC connection — matches `compose.yaml`'s volume mount. Both `apollo`
/// (`Config::audio_worker_socket`) and `apollo-audio-worker`
/// (`AUDIO_WORKER_SOCKET`) fall back to this if the env var is unset; defined
/// once here so the two can't drift out of sync with each other.
pub const DEFAULT_SOCKET_PATH: &str = "/run/apollo-ipc/audio.sock";
