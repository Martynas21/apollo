#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod dto;
pub mod framing;
pub mod proto;

pub use dto::{ConnectionInfoDto, TrackStatusDto};
pub use framing::{FramingError, read_frame, write_frame};
pub use proto::{Envelope, Event, Request, Response};

pub const DEFAULT_SOCKET_PATH: &str = "/run/apollo-ipc/audio.sock";

pub fn optional_env_var(
    lookup: &impl Fn(&str) -> Result<String, std::env::VarError>,
    key: &str,
) -> Option<String> {
    lookup(key).ok().filter(|value| !value.trim().is_empty())
}
