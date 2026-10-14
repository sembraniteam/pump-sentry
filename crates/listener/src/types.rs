//! Listener config, events, stats.

use crate::parser::PumpTokenLaunchEvent;
use solana_commitment_config::CommitmentConfig;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// Bounded event queue capacity.
pub const EVENT_CHANNEL_CAPACITY: usize = 10_000;

/// Listener config.
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    /// WebSocket RPC URL, e.g. `wss://api.mainnet-beta.solana.com`.
    pub ws_url: String,
    /// HTTP RPC URL, e.g. `https://api.mainnet-beta.solana.com`.
    pub http_url: String,
    /// Commitment as string: `processed`, `confirmed`, or `finalized`.
    pub commitment: String,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://api.mainnet-beta.solana.com".to_string(),
            http_url: "https://api.mainnet-beta.solana.com".to_string(),
            commitment: "confirmed".to_string(),
        }
    }
}

/// Config validation errors.
#[derive(Error, Debug)]
pub enum ConfigError {
    /// WebSocket URL is not a valid `ws`/`wss` URL.
    #[error("invalid ws_url: {0}")]
    BadWsUrl(String),
    /// HTTP URL is not a valid `http`/`https` URL.
    #[error("invalid http_url: {0}")]
    BadHttpUrl(String),
    /// Commitment string is unknown.
    #[error("invalid commitment '{0}': expected processed|confirmed|finalized")]
    BadCommitment(String),
}

impl ListenerConfig {
    /// Validate URLs and commitment. No I/O.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let ws = match url::Url::parse(&self.ws_url) {
            Ok(u) if u.scheme() == "ws" || u.scheme() == "wss" => u,
            _ => return Err(ConfigError::BadWsUrl(self.ws_url.clone())),
        };
        let http = match url::Url::parse(&self.http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u,
            _ => return Err(ConfigError::BadHttpUrl(self.http_url.clone())),
        };
        // Hostless URLs mis-resolve.
        if ws.host_str().is_none_or(|h| h.is_empty()) {
            return Err(ConfigError::BadWsUrl(self.ws_url.clone()));
        }
        if http.host_str().is_none_or(|h| h.is_empty()) {
            return Err(ConfigError::BadHttpUrl(self.http_url.clone()));
        }
        // Split hosts warn only: providers may differ.
        if ws.host_str() != http.host_str() {
            tracing::warn!(
                ws = %self.ws_url,
                http = %self.http_url,
                "listener WS and HTTP hosts differ; ensure both serve the same cluster"
            );
        }
        match self.commitment_config() {
            Some(_) => {}
            None => return Err(ConfigError::BadCommitment(self.commitment.clone())),
        }
        Ok(())
    }

    /// Map commitment string to config. `None` when unknown.
    pub fn commitment_config(&self) -> Option<CommitmentConfig> {
        curve::CurveTicker::parse_commitment(&self.commitment)
    }
}

/// Event sent downstream. `Arc` avoids clones.
pub type ListenerEvent = Arc<PumpTokenLaunchEvent>;

/// Runtime counters (all atomics; cheap to share across tasks).
#[derive(Debug, Default)]
pub struct ListenerStats {
    /// Raw log notifications seen on the socket.
    pub logs_seen: AtomicU64,
    /// Logs that passed success + `Instruction: Create` filter.
    pub candidates: AtomicU64,
    /// `getTransaction` RPCs that returned a decodable transaction.
    pub fetch_ok: AtomicU64,
    /// Fetches that failed or returned `None`.
    pub fetch_err: AtomicU64,
    /// Instructions successfully parsed as `create`.
    pub parse_ok: AtomicU64,
    /// Instructions that failed to parse.
    pub parse_err: AtomicU64,
    /// Events emitted into the channel.
    pub events_emitted: AtomicU64,
    /// Events dropped on a full channel.
    pub dropped_full: AtomicU64,
    /// Candidates dropped with no free fetch permit.
    pub dropped_throttled: AtomicU64,
}

/// Point-in-time counters for logs and metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListenerStatsSnapshot {
    /// See [`ListenerStats::logs_seen`].
    pub logs_seen: u64,
    /// See [`ListenerStats::candidates`].
    pub candidates: u64,
    /// See [`ListenerStats::fetch_ok`].
    pub fetch_ok: u64,
    /// See [`ListenerStats::fetch_err`].
    pub fetch_err: u64,
    /// See [`ListenerStats::parse_ok`].
    pub parse_ok: u64,
    /// See [`ListenerStats::parse_err`].
    pub parse_err: u64,
    /// See [`ListenerStats::events_emitted`].
    pub events_emitted: u64,
    /// See [`ListenerStats::dropped_full`].
    pub dropped_full: u64,
    /// See [`ListenerStats::dropped_throttled`].
    pub dropped_throttled: u64,
}

impl ListenerStats {
    /// Create zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load all counters.
    pub fn snapshot(&self) -> ListenerStatsSnapshot {
        ListenerStatsSnapshot {
            logs_seen: self.logs_seen.load(Ordering::Relaxed),
            candidates: self.candidates.load(Ordering::Relaxed),
            fetch_ok: self.fetch_ok.load(Ordering::Relaxed),
            fetch_err: self.fetch_err.load(Ordering::Relaxed),
            parse_ok: self.parse_ok.load(Ordering::Relaxed),
            parse_err: self.parse_err.load(Ordering::Relaxed),
            events_emitted: self.events_emitted.load(Ordering::Relaxed),
            dropped_throttled: self.dropped_throttled.load(Ordering::Relaxed),
            dropped_full: self.dropped_full.load(Ordering::Relaxed),
        }
    }

    /// Increment helpers (each a single atomic op).
    pub fn inc_logs_seen(&self) {
        self.logs_seen.fetch_add(1, Ordering::Relaxed);
    }
    /// Read logs-seen count.
    pub fn logs_seen(&self) -> u64 {
        self.logs_seen.load(Ordering::Relaxed)
    }
    /// Increment candidate counter.
    pub fn inc_candidates(&self) {
        self.candidates.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment fetch-ok counter.
    pub fn inc_fetch_ok(&self) {
        self.fetch_ok.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment fetch-err counter.
    pub fn inc_fetch_err(&self) {
        self.fetch_err.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment parse-ok counter.
    pub fn inc_parse_ok(&self) {
        self.parse_ok.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment parse-err counter.
    pub fn inc_parse_err(&self) {
        self.parse_err.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment emitted counter.
    pub fn inc_events_emitted(&self) {
        self.events_emitted.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment dropped-full counter.
    pub fn inc_dropped_throttled(&self) {
        self.dropped_throttled.fetch_add(1, Ordering::Relaxed);
    }
    /// Increment dropped-full counter.
    pub fn inc_dropped_full(&self) {
        self.dropped_full.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        let cfg = ListenerConfig::default();
        cfg.validate().expect("default config must validate");
        assert_eq!(cfg.commitment_config(), Some(CommitmentConfig::confirmed()));
        assert_eq!(
            ListenerConfig {
                commitment: "bogus".to_string(),
                ..ListenerConfig::default()
            }
            .commitment_config(),
            None
        );
    }

    #[test]
    fn bad_urls_rejected() {
        let cfg = ListenerConfig {
            ws_url: "http://nope".to_string(),
            http_url: "https://api.mainnet-beta.solana.com".to_string(),
            commitment: "confirmed".to_string(),
        };
        assert!(cfg.validate().is_err());

        let cfg2 = ListenerConfig {
            ws_url: "wss://api.mainnet-beta.solana.com".to_string(),
            http_url: "notaurl".to_string(),
            commitment: "confirmed".to_string(),
        };
        assert!(cfg2.validate().is_err());

        let cfg3 = ListenerConfig {
            ws_url: "wss://api.mainnet-beta.solana.com".to_string(),
            http_url: "https://api.mainnet-beta.solana.com".to_string(),
            commitment: "bogus".to_string(),
        };
        assert!(cfg3.validate().is_err());
    }

    #[test]
    fn stats_snapshot_roundtrip() {
        let s = ListenerStats::new();
        s.inc_logs_seen();
        s.inc_candidates();
        s.inc_dropped_full();
        let snap = s.snapshot();
        assert_eq!(snap.logs_seen, 1);
        assert_eq!(snap.candidates, 1);
        assert_eq!(snap.dropped_full, 1);
    }
}
