//! Read-path RPC redundancy, single construction site.
//!
//! [`RpcPool`] wraps one or more HTTP endpoints inside ONE shared
//! `Arc<RpcClient>` (`pool.client(..)`): a read that fails (or hangs)
//! on a chosen endpoint is retried on the next healthy one, latency is
//! tracked per endpoint, and a background sampler doubles as the
//! health check and the staleness detector (a primary whose slot lags
//! the freshest peer is marked stale; endpoints disagreeing on slot by
//! more than a lag multiple are flagged inconsistent).
//!
//! ## What this is NOT
//!
//! **No dispatch fan-out, ever.** Every logical read is issued exactly
//! once to exactly one endpoint at a time — a request is re-attempted
//! on the NEXT endpoint only after an attempt failed (failover
//! semantics, never an all-endpoints broadcast: broadcasting would
//! double-rate-limit every carrier and, worse, would make a
//! non-idempotent call execute twice). Order execution stays
//! single-path by design: dispatch is Jito-bundles-only through its
//! own client and never rides this pool; executed-once trade semantics
//! are owned by the send/confirm guards, not by redundancy. This
//! module only hardens READS (balances, confirms, blockhashes, quotes).
//!
//! A read is only as fresh as its commitment: the sampler compares
//! slots ACROSS endpoints and never invents freshness for a stale one
//! — the gate/preflight fail-closed rules still own the decision.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use solana_client::client_error::{ClientError, ClientErrorKind};
use solana_client::rpc_client::RpcClientConfig;
use solana_client::rpc_sender::RpcSender;
use solana_client::rpc_sender::RpcTransportStats;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_request::RpcRequest};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client::http_sender::HttpSender;
use tracing::{debug, info, warn};

/// Background sampler cadence (also the recovery-probe cadence).
pub const POOL_PROBE_SECS: u64 = 10;
/// Per-attempt read deadline for light calls. This MUST sit under the
/// callers' outer timeouts (executor wraps reads at 10s): a hung
/// primary must fail over with room left for the fallback attempt.
/// Heavy calls (blocks, transactions, signature pages) get their own
/// budget via [`attempt_timeout`].
pub const POOL_READ_TIMEOUT: Duration = Duration::from_secs(6);
/// Per-attempt deadline for heavy calls (blocks, transactions,
/// signature pages, sims). This MUST sit under the tightest caller
/// outer (executor wraps reads at 10s, screen at 15s, backfill blocks
/// at 20s): a hung primary must fail over with room left for the
/// fallback attempt. A spurious failover is only an extra attempt on
/// the next endpoint — never a failure — so this favors availability
/// over waiting out a hung primary.
pub const POOL_HEAVY_TIMEOUT: Duration = Duration::from_secs(7);
/// Endpoint cooldown after a failed attempt; a successful sampler
/// probe clears it earlier.
const EP_COOLDOWN_MS: u64 = 30_000;
/// Allowed slot lag behind the freshest peer before an endpoint reads
/// as stale (~20s of slots at ~400ms/slot).
const STALE_SLOT_LAG: u64 = 50;
/// Freshest-to-stalest slot spread that reads as inconsistent views.
const INCONSISTENT_SLOT_SPREAD: u64 = 2 * STALE_SLOT_LAG;

/// One endpoint: its sender plus mutable view state.
struct Endpoint {
    url: String,
    sender: BoxSender,
    /// Cooldown deadline (monotonic ms); 0 = eligible now.
    cooldown_until_ms: AtomicU64,
    /// Sampler-set: slot lags the freshest peer past [`STALE_SLOT_LAG`].
    stale: AtomicBool,
    /// SMA(α=1/2) of successful latencies, µs; 0 = unknown.
    ewma_us: AtomicU64,
    /// Sampler-observed slot; 0 = never probed.
    last_slot: AtomicU64,
    /// Set while the endpoint is failing (transition-only warns).
    failing: AtomicBool,
}

/// Sender object bound: the pool (and thus the shared client's future)
/// must stay `Send + Sync` for the sampler task and shared client.
type BoxSender = Box<dyn RpcSender + Send + Sync>;

impl Endpoint {
    fn eligible(&self, start: Instant) -> bool {
        mono_ms(start) >= self.cooldown_until_ms.load(Ordering::Relaxed)
            && !self.stale.load(Ordering::Relaxed)
    }
    fn record_ok(&self, elapsed: Duration) {
        self.failing.store(false, Ordering::Relaxed);
        self.cooldown_until_ms.store(0, Ordering::Relaxed);
        self.record_latency(elapsed);
    }
    fn record_latency(&self, elapsed: Duration) {
        let sample_us = elapsed.as_micros() as u64;
        let prev = self.ewma_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            sample_us
        } else {
            prev / 2 + sample_us / 2
        };
        self.ewma_us.store(next, Ordering::Relaxed);
    }
    /// Set the cooldown; returns true on the failing transition (warn
    /// once per episode, deduplicated even under burst-failure loads).
    fn record_err(&self, start: Instant) -> bool {
        self.cooldown_until_ms.store(
            mono_ms(start).saturating_add(EP_COOLDOWN_MS),
            Ordering::Relaxed,
        );
        !self.failing.swap(true, Ordering::Relaxed)
    }
}

/// Monotonic-ms source relative to the pool's construction.
fn mono_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// Shared counters (sampler and sender both touch them).
#[derive(Default)]
struct PoolCounters {
    failover_reads: AtomicU64,
    sampler_runs: AtomicU64,
    /// Endpoints the last probe round reached (any slot answer).
    last_probe_reachable: AtomicU64,
    /// Monotonic SECONDS of the last full probe round.
    last_probe_secs: AtomicU64,
    inconsistent: AtomicBool,
}

/// Per-attempt deadline by method weight (pure, pinned by tests).
/// Light reads fail over fast (under the callers' 10s outers);
/// block-weight reads get slightly more room (backfill blocks are
/// large) but still sit under the tightest 10s caller outer so a hung
/// primary actually fails over instead of burning the whole budget.
fn attempt_timeout(request: RpcRequest) -> Duration {
    match request {
        RpcRequest::GetBlock
        | RpcRequest::GetTransaction
        | RpcRequest::GetSignaturesForAddress
        | RpcRequest::GetBlockTime
        | RpcRequest::SimulateTransaction => POOL_HEAVY_TIMEOUT,
        _ => POOL_READ_TIMEOUT,
    }
}

/// Read-path RPC pool (see module docs).
pub struct RpcPool {
    start: Instant,
    nodes: Vec<Endpoint>,
    counters: PoolCounters,
}

impl RpcPool {
    /// Build from non-empty, deduplicated URLs (first = primary).
    pub fn build(urls: Vec<String>) -> Arc<Self> {
        let mut seen = std::collections::HashSet::new();
        let mut nodes = Vec::with_capacity(urls.len());
        for url in urls {
            let url = url.trim().to_string();
            if url.is_empty() || !seen.insert(url.clone()) {
                continue;
            }
            nodes.push(Endpoint {
                sender: Box::new(HttpSender::new(url.clone())),
                url,
                cooldown_until_ms: AtomicU64::new(0),
                stale: AtomicBool::new(false),
                ewma_us: AtomicU64::new(0),
                last_slot: AtomicU64::new(0),
                failing: AtomicBool::new(false),
            });
        }
        assert!(!nodes.is_empty(), "rpc pool needs at least one endpoint");
        Arc::new(Self {
            start: Instant::now(),
            nodes,
            counters: PoolCounters::default(),
        })
    }

    /// Test-only: build over scripted senders (no network).
    #[cfg(test)]
    fn build_with(senders: Vec<(String, BoxSender)>) -> Arc<Self> {
        assert!(!senders.is_empty(), "rpc pool needs at least one endpoint");
        let nodes = senders
            .into_iter()
            .map(|(url, sender)| Endpoint {
                sender,
                url,
                cooldown_until_ms: AtomicU64::new(0),
                stale: AtomicBool::new(false),
                ewma_us: AtomicU64::new(0),
                last_slot: AtomicU64::new(0),
                failing: AtomicBool::new(false),
            })
            .collect();
        Arc::new(Self {
            start: Instant::now(),
            nodes,
            counters: PoolCounters::default(),
        })
    }

    /// The ONE shared read client. Every `Arc<RpcClient>` consumer in
    /// the process (executor, tickers, screener) gets failover by
    /// construction; order dispatch never runs through this client.
    pub fn client(self: &Arc<Self>, commitment: CommitmentConfig) -> Arc<RpcClient> {
        Arc::new(RpcClient::new_sender(
            FailoverSender { pool: self.clone() },
            RpcClientConfig::with_commitment(commitment),
        ))
    }

    /// Health/staleness report for heartbeats and degradation.
    pub fn snapshot(&self) -> RpcPoolSnapshot {
        let now_secs = mono_ms(self.start) / 1000;
        let healthy = self.nodes.iter().filter(|n| n.eligible(self.start)).count();
        let primary_stale = self
            .nodes
            .first()
            .is_some_and(|n| n.stale.load(Ordering::Relaxed));
        let latencies = self
            .nodes
            .iter()
            .map(|n| n.ewma_us.load(Ordering::Relaxed))
            .collect();
        RpcPoolSnapshot {
            endpoints: self.nodes.len(),
            healthy,
            primary_stale,
            inconsistent: self.counters.inconsistent.load(Ordering::Relaxed),
            failover_reads: self.counters.failover_reads.load(Ordering::Relaxed),
            last_probe_age_secs: now_secs
                .saturating_sub(self.counters.last_probe_secs.load(Ordering::Relaxed)),
            latencies_us: latencies,
        }
    }

    /// True when NO endpoint is eligible: reads would still try (the
    /// caller's error is the right failure), but the pool carries no
    /// trustworthy view — entry decisions must not be made on it.
    ///
    /// Hysteresis by construction: a single failed read only cools its
    /// endpoint down (that read already fails closed on its own); the
    /// pool degrades only on sampler-confirmed evidence — nobody
    /// reachable, or everybody reached-but-stale. A transient blip
    /// must never page the operator or halt entries by itself.
    pub fn degraded(&self) -> bool {
        if self.counters.sampler_runs.load(Ordering::Relaxed) == 0 {
            return false; // boot grace: no probe evidence yet
        }
        if self.counters.last_probe_reachable.load(Ordering::Relaxed) == 0 {
            return true; // sampler reached nobody: confirmed outage
        }
        // All reached but none eligible ⟺ every endpoint stale. A mere
        // cooldown (blip) without staleness is not degradation.
        self.snapshot().healthy == 0 && self.nodes.iter().all(|n| n.stale.load(Ordering::Relaxed))
    }

    /// One sampler round (extracted for tests): health probe of every
    /// endpoint, then cross-endpoint staleness/inconsistency marking.
    async fn probe_round(&self) {
        self.counters.sampler_runs.fetch_add(1, Ordering::Relaxed);
        let mut slots: Vec<(usize, u64)> = Vec::with_capacity(self.nodes.len());
        let mut reachable = 0u64;
        for (i, node) in self.nodes.iter().enumerate() {
            let warm = node.last_slot.load(Ordering::Relaxed) != 0;
            match probe_slot(node).await {
                Ok((slot, elapsed)) => {
                    reachable += 1;
                    node.last_slot.store(slot, Ordering::Relaxed);
                    node.record_latency(elapsed);
                    if node.failing.swap(false, Ordering::Relaxed) && warm {
                        info!(url = %ids::redact_endpoint(&node.url), slot, "rpc endpoint recovered (probe ok)");
                    }
                    node.cooldown_until_ms.store(0, Ordering::Relaxed);
                    slots.push((i, slot));
                }
                Err(e) => {
                    if node.record_err(self.start) {
                        warn!(url = %ids::redact_endpoint(&node.url), error = %ids::strip_error_url(&e.to_string()), "rpc endpoint probe failed; cooling down (reads failover)");
                    }
                }
            }
        }
        if slots.len() > 1 {
            let freshest = slots.iter().map(|(_, s)| *s).max().unwrap_or(0);
            let stalest = slots.iter().map(|(_, s)| *s).min().unwrap_or(0);
            let spread = freshest.saturating_sub(stalest);
            let inconsistent = spread > INCONSISTENT_SLOT_SPREAD;
            if inconsistent
                != self
                    .counters
                    .inconsistent
                    .swap(inconsistent, Ordering::Relaxed)
            {
                warn!(
                    "rpc endpoints disagree on slot by {spread} (spread cap {INCONSISTENT_SLOT_SPREAD}); cross-endpoint views inconsistent — balance/custody checks need operator suspicion"
                );
            }
        }
        for (i, node) in self.nodes.iter().enumerate() {
            let Some((_, slot)) = slots.iter().find(|(idx, _)| *idx == i) else {
                continue; // probe failed: staleness undetermined this round
            };
            let freshest = slots.iter().map(|(_, s)| *s).max().unwrap_or(0);
            let stale = (*slot).saturating_add(STALE_SLOT_LAG) < freshest;
            if stale != node.stale.swap(stale, Ordering::Relaxed) {
                if stale {
                    warn!(url = %ids::redact_endpoint(&node.url), slot, freshest, lag_cap = STALE_SLOT_LAG, "rpc endpoint STALE (slot lag beyond cap); reads prefer fresher peers");
                } else if i == 0 {
                    info!(url = %ids::redact_endpoint(&node.url), slot, "primary rpc endpoint no longer stale");
                }
            }
        }
        self.counters
            .last_probe_secs
            .store(mono_ms(self.start) / 1000, Ordering::Relaxed);
        self.counters
            .last_probe_reachable
            .store(reachable, Ordering::Relaxed);
    }

    /// Spawn the background sampler (health check + staleness detector).
    pub fn spawn_sampler(self: &Arc<Self>) {
        let pool = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(POOL_PROBE_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                pool.probe_round().await;
                if pool.degraded() {
                    debug!(
                        "rpc pool: no eligible endpoint (all cooling down or stale); reads fail over, custody decisions fail closed upstream"
                    );
                }
            }
        });
    }
}

/// Health view.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcPoolSnapshot {
    pub endpoints: usize,
    /// Eligible (not cooling down, not stale).
    pub healthy: usize,
    pub primary_stale: bool,
    pub inconsistent: bool,
    pub failover_reads: u64,
    /// Per-endpoint SMA latency, µs (0 = never measured); index order
    /// matches the endpoint order (0 = primary).
    pub latencies_us: Vec<u64>,
    /// Since the last full probe round (0 right after one).
    pub last_probe_age_secs: u64,
}

/// Cheap sampler probe: slot + latency, bypassing eligibility.
async fn probe_slot(node: &Endpoint) -> std::result::Result<(u64, Duration), String> {
    let started = Instant::now();
    let params: serde_json::Value = serde_json::to_value(
        &[CommitmentParam {
            commitment: "confirmed",
        }][..],
    )
    .expect("probe params serialize");
    let raw = tokio::time::timeout(
        POOL_READ_TIMEOUT,
        node.sender.send(RpcRequest::GetSlot, params),
    )
    .await
    .map_err(|_| "probe timed out".to_string())?
    .map_err(|e| e.to_string())?;
    let slot = raw
        .as_u64()
        .or_else(|| raw.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| format!("slot response not a number: {raw}"))?;
    Ok((slot, started.elapsed()))
}

/// Sampler params via typed DTO (never an ad-hoc JSON object).
#[derive(Serialize)]
struct CommitmentParam {
    commitment: &'static str,
}

/// The [`RpcSender`] the shared client rides. One logical request →
/// one in-flight attempt; the next endpoint is used only after an
/// attempt failed (failover, not fan-out — see module docs).
struct FailoverSender {
    pool: Arc<RpcPool>,
}

#[async_trait]
impl RpcSender for FailoverSender {
    async fn send(
        &self,
        request: RpcRequest,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ClientError> {
        // Eligible endpoints first (stable order keeps the primary
        // preferred among equals); ineligible ones still get tried
        // behind them rather than failing an available read outright.
        let mut order: Vec<&Endpoint> = self.pool.nodes.iter().collect();
        order.sort_by_key(|n| usize::from(!n.eligible(self.pool.start)));
        let mut last_err: Option<ClientError> = None;
        for (idx, node) in order.iter().enumerate() {
            let started = Instant::now();
            // `request` is Copy: each attempt hands the SAME logical
            // request to exactly one endpoint (failover, not fan-out).
            // The per-method budget sits under the callers' outers so
            // a hung endpoint actually fails over (not just fast errors).
            let budget = attempt_timeout(request);
            let res = tokio::time::timeout(budget, node.sender.send(request, params.clone())).await;
            let attempted = match res {
                Ok(Ok(value)) => {
                    node.record_ok(started.elapsed());
                    if idx > 0 {
                        self.pool
                            .counters
                            .failover_reads
                            .fetch_add(1, Ordering::Relaxed);
                        debug!(url = %ids::redact_endpoint(&node.url), "read served by a failover endpoint");
                    }
                    return Ok(value);
                }
                Ok(Err(e)) => e,
                Err(_) => ClientError::from(ClientErrorKind::Custom(format!(
                    "rpc read timed out after {}s at {}",
                    budget.as_secs(),
                    ids::redact_endpoint(&node.url)
                ))),
            };
            if node.record_err(self.pool.start) {
                warn!(url = %ids::redact_endpoint(&node.url), error = %ids::strip_error_url(&attempted.to_string()), "rpc read failed; endpoint cooling down (next attempt serves)");
            }
            last_err = Some(attempted);
        }
        Err(last_err
            .expect("rpc pool has at least one endpoint; the loop always records the last error"))
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        let mut total = RpcTransportStats::default();
        for node in &self.pool.nodes {
            let stats = node.sender.get_transport_stats();
            total.request_count = total.request_count.saturating_add(stats.request_count);
            total.elapsed_time = total.elapsed_time.saturating_add(stats.elapsed_time);
            total.rate_limited_time = total
                .rate_limited_time
                .saturating_add(stats.rate_limited_time);
        }
        total
    }

    fn url(&self) -> String {
        self.pool
            .nodes
            .first()
            .map(|n| n.url.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared script state: tests keep handles after the sender is
    /// boxed into the pool.
    #[derive(Default)]
    struct ScriptCount {
        slot: AtomicU64,
        /// Fail non-probe reads (probes answer the slot).
        fail_reads: AtomicBool,
        /// Fail probes too (full outage).
        fail_probes: AtomicBool,
        calls: AtomicU64,
    }
    impl ScriptCount {
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
        fn set_slot(&self, slot: u64) {
            self.slot.store(slot, Ordering::Relaxed);
        }
        fn set_fails(&self, fails: bool) {
            self.fail_reads.store(fails, Ordering::Relaxed);
            self.fail_probes.store(fails, Ordering::Relaxed);
        }
        fn set_read_fails(&self, fails: bool) {
            self.fail_reads.store(fails, Ordering::Relaxed);
        }
    }

    /// Scripted sender: probes and reads return the configured slot or
    /// a configurable failure; per-endpoint call counting pins the
    /// one-attempt-at-a-time invariant.
    struct ScriptSender {
        count: Arc<ScriptCount>,
    }
    impl ScriptSender {
        fn new() -> (Self, Arc<ScriptCount>) {
            let count = Arc::new(ScriptCount::default());
            (
                Self {
                    count: count.clone(),
                },
                count,
            )
        }
    }
    #[async_trait]
    impl RpcSender for ScriptSender {
        async fn send(
            &self,
            _request: RpcRequest,
            _params: serde_json::Value,
        ) -> std::result::Result<serde_json::Value, ClientError> {
            self.count.calls.fetch_add(1, Ordering::Relaxed);
            let probe = matches!(_request, RpcRequest::GetSlot);
            let fails = if probe {
                self.count.fail_probes.load(Ordering::Relaxed)
            } else {
                self.count.fail_reads.load(Ordering::Relaxed)
            };
            if fails {
                Err(ClientError::from(ClientErrorKind::Custom(
                    "scripted failure".to_string(),
                )))
            } else {
                Ok(
                    serde_json::to_value(self.count.slot.load(Ordering::Relaxed))
                        .expect("slot value"),
                )
            }
        }
        fn get_transport_stats(&self) -> RpcTransportStats {
            RpcTransportStats::default()
        }
        fn url(&self) -> String {
            "scripted".to_string()
        }
    }

    fn script_pool(
        primary_fails: bool,
        slots: [u64; 2],
    ) -> (Arc<RpcPool>, Arc<ScriptCount>, Arc<ScriptCount>) {
        let (primary, primary_count) = ScriptSender::new();
        let (fallback, fallback_count) = ScriptSender::new();
        primary_count.set_fails(primary_fails);
        primary_count.set_slot(slots[0]);
        fallback_count.set_slot(slots[1]);
        let pool = RpcPool::build_with(vec![
            ("primary".to_string(), Box::new(primary) as BoxSender),
            ("fallback".to_string(), Box::new(fallback) as BoxSender),
        ]);
        (pool, primary_count, fallback_count)
    }

    #[tokio::test]
    async fn failover_is_sequential_not_broadcast() {
        let (pool, primary, fallback) = script_pool(true, [0, 0]);
        let sender = FailoverSender { pool: pool.clone() };
        let value = sender
            .send(RpcRequest::GetVersion, serde_json::Value::Array(vec![]))
            .await
            .expect("fallback serves the read");
        assert_eq!(value, serde_json::Value::from(0u64));
        // Exactly one attempt per endpoint: the failing primary was
        // tried once, the next healthy endpoint once — never a
        // broadcast to both at once.
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
        assert_eq!(pool.counters.failover_reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cooldown_skips_failing_endpoint_on_next_read() {
        let (pool, primary, fallback) = script_pool(true, [0, 0]);
        let sender = FailoverSender { pool: pool.clone() };
        sender
            .send(RpcRequest::GetVersion, serde_json::Value::Array(vec![]))
            .await
            .expect("first read rides the fallback");
        sender
            .send(RpcRequest::GetVersion, serde_json::Value::Array(vec![]))
            .await
            .expect("second read also served");
        // The failed primary sits in cooldown: both reads hit ONLY the
        // fallback (the primary got its single attempt on read one).
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 2);
    }

    #[tokio::test]
    async fn probe_marks_stale_primary_and_flags_inconsistency() {
        // Primary sits 500 slots behind the fallback: beyond both the
        // staleness lag and the inconsistency spread.
        let (pool, primary, _fallback) = script_pool(false, [1_500, 2_000]);
        pool.probe_round().await;
        let snap = pool.snapshot();
        assert!(snap.primary_stale);
        assert!(snap.inconsistent);
        assert_eq!(snap.healthy, 1, "stale primary is not eligible");
        assert!(!pool.degraded(), "the fallback still carries a live view");

        // Parity on the next round clears both marks.
        primary.set_slot(2_000);
        pool.probe_round().await;
        let snap = pool.snapshot();
        assert!(!snap.primary_stale);
        assert!(!snap.inconsistent);
        assert_eq!(snap.healthy, 2);
    }

    #[tokio::test]
    async fn all_endpoints_failing_is_degraded() {
        let (solo, count) = ScriptSender::new();
        count.set_fails(true);
        let pool = RpcPool::build_with(vec![("solo".to_string(), Box::new(solo) as BoxSender)]);
        pool.probe_round().await;
        assert!(pool.degraded());
        let sender = FailoverSender { pool: pool.clone() };
        // Reads still try (and still fail with the endpoint's error):
        // degradation never silently routes reads to nowhere.
        let err = sender
            .send(RpcRequest::GetVersion, serde_json::Value::Array(vec![]))
            .await
            .expect_err("all endpoints down");
        assert!(err.to_string().contains("scripted failure"));
    }

    #[test]
    fn attempt_timeout_weights_by_method() {
        assert_eq!(attempt_timeout(RpcRequest::GetBalance), POOL_READ_TIMEOUT);
        assert_eq!(attempt_timeout(RpcRequest::GetSlot), POOL_READ_TIMEOUT);
        assert_eq!(attempt_timeout(RpcRequest::GetBlock), POOL_HEAVY_TIMEOUT);
        assert_eq!(
            attempt_timeout(RpcRequest::GetTransaction),
            POOL_HEAVY_TIMEOUT
        );
        assert_eq!(
            attempt_timeout(RpcRequest::GetSignaturesForAddress),
            POOL_HEAVY_TIMEOUT
        );
        assert_eq!(
            attempt_timeout(RpcRequest::SimulateTransaction),
            POOL_HEAVY_TIMEOUT
        );
        // The light budget must sit under the callers' 10s outers, or a
        // hung primary would never fail over in time. The heavy budget
        // must too: screen wraps heavy reads at 15s, backfill blocks at
        // 20s, and executor sims at 10s — a 25s attempt used to burn
        // every one of those budgets before failover could start.
        assert!(POOL_READ_TIMEOUT.as_secs() < 10);
        assert!(POOL_HEAVY_TIMEOUT.as_secs() < 10);
        // Two heavy attempts still fit inside the 15s screen outer.
        assert!(POOL_HEAVY_TIMEOUT.as_secs() * 2 <= 15);
    }

    #[tokio::test]
    async fn single_blip_is_not_degradation() {
        // One failed read cools the only endpoint down, but the sampler
        // still reaches it: transient blip, not an outage — the read
        // fails closed on its own, entries keep running, nobody paged.
        let (solo, count) = ScriptSender::new();
        let pool = RpcPool::build_with(vec![("solo".to_string(), Box::new(solo) as BoxSender)]);
        pool.probe_round().await; // sampler evidence: reachable
        assert!(!pool.degraded());
        count.set_read_fails(true); // reads die, probes answer
        let sender = FailoverSender { pool: pool.clone() };
        sender
            .send(RpcRequest::GetVersion, serde_json::Value::Array(vec![]))
            .await
            .expect_err("solo endpoint failing reads");
        assert_eq!(pool.snapshot().healthy, 0, "cooldown holds the endpoint");
        assert!(
            !pool.degraded(),
            "a lone cooldown without staleness must not degrade the pool"
        );
        // The next probe still reaches it and clears the cooldown.
        pool.probe_round().await;
        assert_eq!(pool.snapshot().healthy, 1);
        assert!(!pool.degraded());
    }

    #[tokio::test]
    async fn boot_grace_withholds_degradation_without_probe_evidence() {
        // No probe round ran yet: even a fully cooled pool is not
        // degraded — the sampler (not a lone read) owns the verdict.
        // The first failing probe then flips it (confirmed outage).
        let (solo, count) = ScriptSender::new();
        count.set_fails(true);
        let pool = RpcPool::build_with(vec![("solo".to_string(), Box::new(solo) as BoxSender)]);
        let sender = FailoverSender { pool: pool.clone() };
        sender
            .send(RpcRequest::GetVersion, serde_json::Value::Array(vec![]))
            .await
            .expect_err("solo endpoint failing");
        assert_eq!(pool.snapshot().healthy, 0);
        assert!(
            !pool.degraded(),
            "no sampler evidence yet: boot grace holds"
        );
        pool.probe_round().await;
        assert!(pool.degraded(), "failing probe confirms the outage");
    }

    #[tokio::test]
    async fn partial_stale_with_healthy_fallback_is_not_degraded() {
        // Primary lags but the fallback carries a live view: failover
        // is working as designed, entries keep running on it.
        let (pool, _primary, _fallback) = script_pool(false, [1_000, 2_000]);
        pool.probe_round().await;
        let snap = pool.snapshot();
        assert!(snap.primary_stale);
        assert_eq!(snap.healthy, 1);
        assert!(!pool.degraded());
    }

    #[tokio::test]
    async fn shared_client_constructs_over_the_pool() {
        let (pool, _primary, _fb) = script_pool(false, [0, 0]);
        // Type-level smoke: the ONE shared client wraps the failover
        // sender; no network happens at construction.
        let _client = pool.client(CommitmentConfig::confirmed());
    }
}
