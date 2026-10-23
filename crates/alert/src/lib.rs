//! Non-blocking Discord alerts over HTTPS (bot token).
//!
//! [`Alerter::send_card`] and [`Alerter::send_text`] spawn a detached
//! task per message. Failures only warn.

pub mod discord;

pub use discord::{
    Alerter, Card, CardCtx, DISCORD_BOT_TOKEN_ENV, HeartbeatReport, capture_bot_token,
};
