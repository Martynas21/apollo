#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Apollo is a Discord bot that streams audio from YouTube (search, direct
//! URL, or playlist) into a voice channel, controlled entirely through its
//! own web dashboard rather than Discord slash commands. All YouTube access
//! goes through a `yt-dlp` subprocess — no Google API, no OAuth.
//!
//! This crate is the Discord-facing process: it owns the gateway connection,
//! the database, and the web dashboard, but has no direct voice connection —
//! playback itself is driven by the separate `apollo-audio-worker` process
//! over IPC (see `voice::ipc_backend`).

pub mod config;
pub mod db;
pub mod model;
pub mod voice;
pub mod web;
pub mod youtube;
