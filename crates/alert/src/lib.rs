//! Non-blocking Discord alerts via Hermes CLI.
//!
//! [`Alerter::send`] spawns a detached task per message.
//! Failures only warn.

pub mod discord;

pub use discord::{Alerter, CardCtx, HeartbeatReport};
