//! Shared types for the `apollo` \<-\> `apollo-audio-worker` IPC boundary —
//! see the crate's sibling `audio-worker/` for the worker binary and
//! `apollo`'s `src/voice/ipc_backend.rs` for the client side.

pub mod dto;
pub mod framing;
pub mod proto;

pub use dto::{ConnectionInfoDto, TrackStatusDto};
pub use framing::{read_frame, write_frame, FramingError};
pub use proto::{Envelope, Event, Request, Response};
