#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! The audio worker process: holds the `songbird::Driver`/voice connection
//! and streams a resolved media URL straight into songbird, controlled by
//! `apollo` over the IPC protocol defined in `apollo-ipc`.

pub mod config;
pub mod rpc;
pub mod session;
