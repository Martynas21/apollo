#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod dto;
pub mod framing;
pub mod proto;

pub use dto::{ConnectionInfoDto, TrackStatusDto};
pub use framing::{FramingError, read_frame, write_frame};
pub use proto::{Envelope, Event, Request, Response};
