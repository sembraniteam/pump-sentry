//! Non-blocking Discord trade alerts via Hermes CLI (skill
//! `discord-hermes-alerting`).
//!
//! [`Alerter::send`] spawns a detached task per message — it never blocks
//! the pricing or swap paths. Delivery failures only emit
//! `tracing::warn!`; portfolio and trading state are untouched.

pub mod discord;

pub use discord::Alerter;
