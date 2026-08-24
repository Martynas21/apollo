//! YouTube integration: Google OAuth2 login, the local OAuth callback
//! server, and YouTube Data API v3 access.
//!
//! The API client itself (YouTube Data API v3) lands in a later phase —
//! this module currently only covers account linking (OAuth2).

pub mod oauth;
pub mod server;
