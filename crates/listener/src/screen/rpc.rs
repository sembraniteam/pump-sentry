use super::*;
use super::swaps::{for_each_compiled_ix, resolve_keys};

pub async fn screen_developer(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    dev: &Pubkey,
    now_secs: u64,
    cfg: &DevScreenConfig,
) -> Result<DevVerdict, ClientError> {
    let sigs = rpc
        .get_signatures_for_address_with_config(
            dev,
            GetConfirmedSignaturesForAddress2Config {
                before: None,
                until: None,
                limit: Some(cfg.sig_limit),
                commitment: Some(commitment),
            },
        )
        .await?;
    if sigs.is_empty() {
        return Ok(judge_developer(None, 0, cfg));
    }
    // Newest-first: the last entry is the oldest observed activity.
    let age_secs = sigs
        .last()
        .and_then(|s| s.block_time)
        .map(|t| now_secs.saturating_sub(t.max(0) as u64));
    // Early reject before the expensive fan-out: a wallet that already
    // fails the age gate (or has no timestamp at all) can never pass
    // `judge_developer` regardless of its prior-launch count, so
    // sampling 5-20 full transactions for it only burns RPC quota
    // (fresh throwaway wallets are 85-90% of Pump.fun devs and 429
    // directly). Prior count is reported as 0 — irrelevant on a reject.
    if age_secs.is_none_or(|age| age < cfg.min_age_secs) {
        return Ok(judge_developer(age_secs, 0, cfg));
    }

    let tx_config = json_tx_config(commitment);
    // Bounded fan-out (5-wide): 20 sequential fetches could consume the
    // whole 15s budget, and 20 concurrent ones hammer the node. Failed
    // transactions are skipped (a reverted `create` never launched);
    // unreadable ones are counted separately below — a mostly-unreadable
    // sample must not undercount serial launchers into a pass.
    let candidates: Vec<(Signature, bool)> = sigs
        .iter()
        .take(cfg.sample_txs)
        .filter_map(|s| {
            Signature::from_str(&s.signature)
                .ok()
                .map(|sig| (sig, s.err.is_some()))
        })
        .collect();
    use futures_util::stream::{self, StreamExt};
    let mut prior_launches: usize = 0;
    let mut fetch_failed: usize = 0;
    // Owned items through `.map()` + `.buffered()`: each future owns its
    // `(Signature, bool)` item, so no item borrows escape into the
    // combinator. (`.map()` over BORROWED items with an async block that
    // also borrows the environment trips rustc's higher-ranked lifetime
    // check (E0631) at distant spawn sites; `.then()` would be sequential
    // and cannot take `buffered` after it.)
    let total = candidates.len();
    let fetched: Vec<(usize, bool)> = stream::iter(candidates)
        .map(|(sig, failed)| async move {
            if failed {
                return (0usize, false);
            }
            let fetched = match tokio::time::timeout(
                TX_FETCH_TIMEOUT,
                rpc.get_transaction_with_config(&sig, tx_config),
            )
            .await
            {
                Ok(Ok(fetched)) => fetched,
                _ => return (0usize, true),
            };
            // Defensive: skip failed transactions even if the status flag was
            // missing (a failed create never launched a token).
            let failed_tx = fetched
                .transaction
                .meta
                .as_ref()
                .is_some_and(|m| m.err.is_some());
            if failed_tx {
                return (0usize, false);
            }
            (count_dev_creates(&fetched, dev), false)
        })
        .buffered(DEV_SCREEN_FANOUT)
        .collect()
        .await;
    for (count, failed) in fetched {
        prior_launches += count;
        fetch_failed += usize::from(failed);
    }
    if !sample_trustworthy(fetch_failed, total) {
        // Most of the sample unreadable: the launch count is a lower
        // bound that would let serial launchers pass — distrust the
        // sample and fail closed with the evidence attached.
        return Ok(DevVerdict {
            pass: false,
            age_secs,
            prior_launches,
            reason: format!(
                "dev history unreadable ({fetch_failed} of {total} fetches failed); failing closed",
            ),
        });
    }
    Ok(judge_developer(age_secs, prior_launches, cfg))
}

/// True when a dev-history sample is trustworthy enough to count:
/// unreadable fetches must stay strictly below half the sample, and an
/// empty sample proves nothing (every signature unparsable or flagged —
/// counting zero launches would launder a decode failure into a pass).
/// Pure and unit-tested (the fail-closed boundary for serial launchers).
pub(crate) fn sample_trustworthy(failed: usize, total: usize) -> bool {
    total > 0 && (failed == 0 || failed * 2 < total)
}

/// RPC-backed swap reader for one curve: signatures since `since_sig`
/// (newest-first walk, stop at the watermark), classified records.
/// Returns `(records, newest_signature, truncated)`.
///
/// When `exhaustive` is false (quorum path), `since_sig=None` is the
/// baseline first read and never truncated; a watermarked read that
/// never finds its watermark saw a partial view (high-velocity gap or
/// pruned history) and reports truncated so callers hold instead of
/// advancing quorum or the watermark over the gap. When `exhaustive`
/// is true (bundle forensics, which needs the whole creation-slot
/// history), hitting the page cap on full pages reports truncated even
/// without a watermark — the fail-closed branch it feeds would
/// otherwise be dead code on curves with 75+ transactions.
/// Failed transactions are skipped; undecodable ones simply
/// contribute no records (gate stays `Wait`: fail-closed for entries).
pub async fn read_curve_swaps(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    curve: &Pubkey,
    since_sig: Option<&str>,
    exhaustive: bool,
) -> Result<(Vec<SwapRecord>, Option<String>, bool), ClientError> {
    let tx_config = json_tx_config(commitment);
    let mut records = Vec::new();
    let mut newest: Option<String> = None;
    let mut before: Option<Signature> = None;
    let mut watermark_found = since_sig.is_none();
    let mut pages = 0usize;

    for _ in 0..CURVE_SWAP_MAX_PAGES {
        let sigs = rpc
            .get_signatures_for_address_with_config(
                curve,
                GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(CURVE_SWAP_PAGE_LIMIT),
                    commitment: Some(commitment),
                },
            )
            .await?;
        if sigs.is_empty() {
            break;
        }
        pages += 1;
        let page_full = sigs.len() >= CURVE_SWAP_PAGE_LIMIT;

        for status in &sigs {
            if Some(status.signature.as_str()) == since_sig {
                watermark_found = true;
                break;
            }
            if newest.is_none() {
                newest = Some(status.signature.clone());
            }
            if status.err.is_some() {
                continue;
            }
            let Ok(signature) = Signature::from_str(&status.signature) else {
                continue;
            };
            // Per-tx deadline: one slow fetch must not consume the whole
            // screening budget (outer `with_screen_timeout` is the backstop).
            let fetched = match tokio::time::timeout(
                TX_FETCH_TIMEOUT,
                rpc.get_transaction_with_config(&signature, tx_config),
            )
            .await
            {
                Ok(Ok(fetched)) => fetched,
                _ => continue,
            };
            records.extend(extract_pump_swaps(&fetched, &status.signature));
        }
        if watermark_found {
            break;
        }
        // Watermark not in this page: continue with `before` cursor, unless
        // the page was short (history exhausted).
        if !page_full {
            break;
        }
        before = sigs
            .last()
            .and_then(|s| Signature::from_str(&s.signature).ok());
        let Some(cursor) = before else { break };
        before = Some(cursor);
    }

    let truncated = match (since_sig, exhaustive) {
        // Watermarked read that never found its watermark: partial view.
        (Some(_), _) => !watermark_found,
        // Exhaustive read that hit the page cap on full pages: more
        // history exists than was read.
        (None, true) => pages >= CURVE_SWAP_MAX_PAGES,
        // Baseline first read: complete by definition.
        (None, false) => false,
    };
    if truncated {
        // High-velocity gap: newest 75 returned, intermediates missed.
        // Callers must not advance quorum on this partial view.
        debug!(
            pages,
            fetched = records.len(),
            "curve read truncated at page cap; watermark stale"
        );
    }
    Ok((records, newest, truncated))
}

/// RPC read deadline for screening paths (no unbounded hangs).
///
/// Timeout budget per path (outer → inner, inner must fire first):
/// - screening (`dev_verdict`, `curve_swaps`, `hunt_migration`,
///   `bundle_verdict`): 15s outer / 5s per-tx fetch.
/// - funder tracing: 15s outer / 8s per-buyer walk (page-capped).
/// - curve/vault tickers: 15s per round (`ROUND_TIMEOUT`), 1s poll
///   interval (a slow round skips; misses back off).
/// - executor: 90s send-and-confirm, 15s blockhash/poll, 10s balance
///   reads, 2s Jito HTTP; live confirm window 60s with pending recheck.
pub const SCREEN_RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// Run an RPC future with the screening deadline. `None` on timeout.
async fn with_screen_timeout<F, T>(future: F) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(SCREEN_RPC_TIMEOUT, future).await.ok()
}

/// Detected migration, ready for alerting and swap routing.
///
/// RPC-layer shape: holds typed [`Pubkey`]s and the parsed
/// [`RaydiumV4Pool`]. Convert to [`MigrationReport`] at the crate
/// boundary (see [`RpcScreener::hunt_migration`] callers) — the two types
/// are an intentional layer split, not duplication: typed inside
/// `listener`, plain strings outside it.
#[derive(Debug, Clone)]
pub struct DetectedMigration {
    /// Migration transaction signature (base58).
    pub signature: String,
    /// Where liquidity landed.
    pub destination: MigrationDestination,
    /// New AMM account for Raydium migrations (initialize2 account 4,
    /// [`INITIALIZE2_AMM_INDEX`]).
    pub amm_id: Option<Pubkey>,
    /// Parsed pool for Raydium migrations (swap-ready addresses).
    pub pool: Option<RaydiumV4Pool>,
}

/// Binary-friendly migration report (plain strings for cross-crate use).
/// Produced from [`DetectedMigration::report`]; the pipeline's `Screener`
/// trait speaks only this shape so the binary never depends on RPC-layer
/// key types.
///
/// Orientation: on Pump.fun Raydium migrations the base side is WSOL and
/// the quote side is the migrated token (chain-verified, see
/// `migration::pool` docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Migration transaction signature (base58).
    pub signature: String,
    /// Where liquidity landed.
    pub destination: MigrationDestination,
    /// New AMM account, if resolved.
    pub amm_id: Option<String>,
    /// Pool base mint (WSOL on migrations), if parsed.
    pub base_mint: Option<String>,
    /// Pool quote mint (the migrated token), if parsed.
    pub quote_mint: Option<String>,
    /// Coin-side LP vault (WSOL inventory), if parsed.
    pub base_vault: Option<String>,
    /// Pc-side LP vault (token inventory), if parsed.
    pub quote_vault: Option<String>,
}

impl DetectedMigration {
    /// Convert to the plain report shape.
    pub fn report(&self) -> MigrationReport {
        MigrationReport {
            signature: self.signature.clone(),
            destination: self.destination,
            amm_id: self.amm_id.map(|k| k.to_string()),
            base_mint: self.pool.as_ref().map(|p| p.base_mint.to_string()),
            quote_mint: self.pool.as_ref().map(|p| p.quote_mint.to_string()),
            base_vault: self.pool.as_ref().map(|p| p.base_vault.to_string()),
            quote_vault: self.pool.as_ref().map(|p| p.quote_vault.to_string()),
        }
    }
}

/// AMM account index inside a Raydium `initialize2` instruction.
/// Verified against `raydium-amm/program/src/instruction.rs::initialize2`:
/// `[0]=SPL Token program, [1]=ATA program, [2]=system program,
/// [3]=rent sysvar, [4]=new AMM pool` (then authority, open-orders, ...).
/// (A prior revision used index 1, confusing `initialize2` with the
/// `Deposit`/`Swap` layouts where index 1 is the AMM; index 1 here is the
/// ATA *program*, so pool resolution never succeeded.)
pub const INITIALIZE2_AMM_INDEX: usize = 4;

/// Resolve the new AMM id from decoded instructions given as
/// `(program_id, resolved account keys)` pairs. Returns the accounts[4]
/// of the first Raydium-program instruction. Pure and unit-tested.
pub fn initialize2_amm_id(instructions: &[(Pubkey, Vec<Pubkey>)]) -> Option<Pubkey> {
    instructions
        .iter()
        .find(|(program, _)| *program == RAYDIUM_V4_PROGRAM_ID)
        .and_then(|(_, accounts)| accounts.get(INITIALIZE2_AMM_INDEX).copied())
}

/// One decoded instruction for migration classification: program,
/// resolved account keys, and raw instruction data.
#[derive(Debug, Clone)]
pub(crate) struct DecodedIx {
    pub(crate) program: Pubkey,
    pub(crate) accounts: Vec<Pubkey>,
    pub(crate) data: Vec<u8>,
}

/// Decode one fetched transaction for migration classification:
/// full account keys, [`DecodedIx`] per instruction (top-level plus
/// inner compiled), and program logs. `None` on any undecodable key.
/// Pure and unit-tested via downstream asserts.
pub(crate) fn decode_migration_tx(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
) -> Option<(Vec<Pubkey>, Vec<DecodedIx>, Vec<String>)> {
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return None;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return None;
    };
    let keys = resolve_keys(raw, &tx_with_meta.meta)?;

    let mut decoded: Vec<DecodedIx> = Vec::new();
    let mut skipped = 0usize;
    let mut push = |program_id_index: u8, accounts: &[u8], data_str: &str| {
        let program = keys.get(usize::from(program_id_index)).copied()?;
        let mut resolved = Vec::with_capacity(accounts.len());
        for idx in accounts {
            resolved.push(keys.get(usize::from(*idx)).copied()?);
        }
        // Single decode path shared with `decode_ix_data` semantics:
        // bs58 first (Pump/Raydium wire), base64 fallback (RPC JSON).
        let raw = decode_ix_data(data_str)?;
        decoded.push(DecodedIx { program, accounts: resolved, data: raw });
        Some(())
    };
    let mut visit = |program_id_index: u8, accounts: &[u8], data_str: &str| {
        if push(program_id_index, accounts, data_str).is_none() {
            skipped += 1;
        }
    };
    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut visit);
    if skipped > 0 {
        debug!(skipped, decoded = decoded.len(), "migration tx had undecodable ixs; continuing with partial view");
    }

    let logs = match &tx_with_meta.meta {
        Some(meta) => match &meta.log_messages {
            OptionSerializer::Some(logs) => logs.clone(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    Some((keys, decoded, logs))
}

/// RPC-backed screening client: developer verdicts plus incremental
/// curve-swap reads with a per-mint signature watermark.
#[derive(Clone)]
pub struct RpcScreener {
    rpc: Arc<RpcClient>,
    commitment: CommitmentConfig,
    dev_cfg: DevScreenConfig,
    funder_cache: Arc<tokio::sync::Mutex<TimedCache<Option<String>>>>,
    dev_cache: Arc<tokio::sync::Mutex<TimedCache<DevVerdict>>>,
}

impl RpcScreener {
    /// Build against an HTTP RPC URL with explicit commitment and dev policy.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        dev_cfg: DevScreenConfig,
    ) -> Result<Self, ScreenError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(ScreenError::BadUrl(http_url.to_string())),
        }
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            commitment,
            dev_cfg,
            funder_cache: Arc::new(tokio::sync::Mutex::new(TimedCache::new(
                FUNDER_CACHE_TTL_SECS,
            ))),
            dev_cache: Arc::new(tokio::sync::Mutex::new(TimedCache::new(DEV_CACHE_TTL_SECS))),
        })
    }

    /// Developer history verdict (fail-closed on transport errors).
    /// Repeat wallets are served from a TTL cache (history barely moves)
    /// — except early age-rejects (no-history or below-minimum-age with
    /// an unsampled prior count): age is time-sensitive, and rechecking
    /// costs one cheap signature page (the fan-out stays skipped), so a
    /// wallet that ages past the gate is trusted on its next launch,
    /// not an hour later.
    pub async fn dev_verdict(
        &self,
        dev: &Pubkey,
        now_secs: u64,
    ) -> Result<DevVerdict, ScreenError> {
        if let Some(hit) = self.dev_cache.lock().await.get(&dev.to_string(), now_secs) {
            return Ok(hit);
        }
        let verdict = match with_screen_timeout(screen_developer(
            &self.rpc,
            self.commitment,
            dev,
            now_secs,
            &self.dev_cfg,
        ))
        .await
        {
            None => return Err(ScreenError::Timeout("dev_verdict")),
            Some(result) => result?,
        };
        // Only full-sample verdicts are cacheable: early age-rejects
        // (`!pass` with no sampled priors) would freeze a young wallet
        // as rejected past its aging-out, with a fabricated `priors: 0`.
        // (Unreachable otherwise: the full path only rejects on priors.)
        if verdict.pass || verdict.prior_launches > 0 {
            self.dev_cache
                .lock()
                .await
                .put(dev.to_string(), verdict.clone(), now_secs);
        }
        Ok(verdict)
    }

    /// Swap records for a curve since `since_sig`, plus the new watermark
    /// and a truncation flag (see [`read_curve_swaps`]: a truncated round
    /// must not advance quorum).
    pub async fn curve_swaps(
        &self,
        curve: &Pubkey,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>, bool), ScreenError> {
        match with_screen_timeout(read_curve_swaps(
            &self.rpc,
            self.commitment,
            curve,
            since_sig,
            false,
        ))
        .await
        {
            None => Err(ScreenError::Timeout("curve_swaps")),
            Some(result) => result.map_err(ScreenError::Rpc),
        }
    }

    /// Hunt the migration transaction for a graduated curve: scan recent
    /// signatures, classify each candidate, and on a Raydium hit resolve
    /// the new AMM id (initialize2 account 4, [`INITIALIZE2_AMM_INDEX`])
    /// plus its parsed pool.
    /// The pool is trusted only after the full gate: Raydium-owned
    /// account, WSOL base, and quote matching `expected_token_mint` —
    /// anything else keeps scanning. Returns `None` when no migration is
    /// found. Runs once per mint upstream — never in a hot loop.
    /// Bounded by [`SCREEN_RPC_TIMEOUT`] like every other screening path
    /// (previously this was the only unbounded RPC path).
    pub async fn hunt_migration(
        &self,
        curve: &Pubkey,
        expected_token_mint: &Pubkey,
    ) -> Result<Option<DetectedMigration>, ScreenError> {
        match with_screen_timeout(self.hunt_migration_inner(curve, expected_token_mint)).await {
            None => Err(ScreenError::Timeout("hunt_migration")),
            Some(result) => result.map_err(ScreenError::Rpc),
        }
    }

    async fn hunt_migration_inner(
        &self,
        curve: &Pubkey,
        expected_token_mint: &Pubkey,
    ) -> Result<Option<DetectedMigration>, ClientError> {
        let monitor = MigrationMonitor::new();
        let tx_config = json_tx_config(self.commitment);
        // First Raydium detection whose pool cannot be resolved yet
        // (transient RPC or unparseable account): kept as a fallback and
        // returned only after every candidate is scanned, so one bad
        // account never hides the real migration behind it.
        let mut fallback: Option<(String, Option<Pubkey>)> = None;
        let mut before: Option<Signature> = None;
        let mut fetches: usize = 0;
        for _ in 0..HUNT_MAX_PAGES {
            let sigs = self
                .rpc
                .get_signatures_for_address_with_config(
                    curve,
                    GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: None,
                        limit: Some(HUNT_PAGE_LIMIT),
                        commitment: Some(self.commitment),
                    },
                )
                .await?;
            if sigs.is_empty() {
                break;
            }
            let page_full = sigs.len() >= HUNT_PAGE_LIMIT;
            for status in &sigs {
                if fetches >= HUNT_MAX_FETCHES {
                    debug!("migration hunt hit fetch budget; stopping");
                    break;
                }
                if status.err.is_some() {
                    continue;
                }
                let Ok(signature) = Signature::from_str(&status.signature) else {
                    continue;
                };
                fetches += 1;
                let fetched = match tokio::time::timeout(
                    TX_FETCH_TIMEOUT,
                    self.rpc.get_transaction_with_config(&signature, tx_config),
                )
                .await
                {
                    Ok(Ok(fetched)) => fetched,
                    _ => continue,
                };
                let Some((keys, decoded, logs)) = decode_migration_tx(&fetched) else {
                    continue;
                };
                let mut ix_datas: Vec<Vec<u8>> = Vec::with_capacity(decoded.len());
                let mut pairs: Vec<(Pubkey, Vec<Pubkey>)> = Vec::with_capacity(decoded.len());
                for ix in decoded {
                    ix_datas.push(ix.data);
                    pairs.push((ix.program, ix.accounts));
                }
                match monitor.classify_migration(
                    &keys,
                    &ix_datas,
                    &logs,
                    curve,
                    expected_token_mint,
                ) {
                    Some(MigrationDestination::RaydiumV4) => {
                        let amm_id = initialize2_amm_id(&pairs);
                        let pool = match amm_id {
                            Some(amm) => {
                                let account = match tokio::time::timeout(
                                    TX_FETCH_TIMEOUT,
                                    self.rpc.get_account(&amm),
                                )
                                .await
                                {
                                    Ok(Ok(account)) => account,
                                    _ => {
                                        debug!(%amm, "pool account fetch failed; keeping as fallback, continuing hunt");
                                        if fallback.is_none() {
                                            fallback = Some((status.signature.clone(), amm_id));
                                        }
                                        continue;
                                    }
                                };
                                match RaydiumV4Pool::parse_validated(
                                    &amm,
                                    &account.data,
                                    &account.owner,
                                    expected_token_mint,
                                ) {
                                    Some(pool) => Some(pool),
                                    None => {
                                        debug!(%amm, "pool failed trust gate; keeping as fallback, continuing hunt");
                                        if fallback.is_none() {
                                            fallback = Some((status.signature.clone(), amm_id));
                                        }
                                        continue;
                                    }
                                }
                            }
                            None => {
                                debug!("raydium migration without resolvable AMM; continuing hunt");
                                if fallback.is_none() {
                                    fallback = Some((status.signature.clone(), None));
                                }
                                continue;
                            }
                        };
                        return Ok(Some(DetectedMigration {
                            signature: status.signature.clone(),
                            destination: MigrationDestination::RaydiumV4,
                            amm_id,
                            pool,
                        }));
                    }
                    Some(MigrationDestination::PumpSwap) => {
                        return Ok(Some(DetectedMigration {
                            signature: status.signature.clone(),
                            destination: MigrationDestination::PumpSwap,
                            amm_id: None,
                            pool: None,
                        }));
                    }
                    None => continue,
                }
            }
            if fetches >= HUNT_MAX_FETCHES {
                break;
            }
            if !page_full {
                break;
            }
            before = sigs
                .last()
                .and_then(|s| Signature::from_str(&s.signature).ok());
            if before.is_none() {
                break;
            }
        }
        if let Some((signature, amm_id)) = fallback {
            warn!("raydium migration detected but pool unresolvable; reporting without pool");
            return Ok(Some(DetectedMigration {
                signature,
                destination: MigrationDestination::RaydiumV4,
                amm_id,
                pool: None,
            }));
        }
        Ok(None)
    }

    /// Full bundle forensics for a curve at confirm time: recent buy
    /// swaps with slots and supply shares, genesis funders for the first
    /// few distinct buyers, evaluated against `creation_slot`.
    /// Transport errors propagate (fail-closed upstream).
    pub async fn bundle_verdict(
        &self,
        curve: &Pubkey,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ScreenError> {
        match with_screen_timeout(self.bundle_verdict_tiered(curve, creation_slot)).await {
            None => Err(ScreenError::Timeout("bundle_verdict")),
            Some(result) => result.map(|(analysis, _)| analysis).map_err(ScreenError::Rpc),
        }
    }

    /// Two-tier shortcut is applied by the caller shape below: when the
    /// slot rule alone already proves a bundle, funders cannot change the
    /// verdict, so [`Self::bundle_verdict`] skips their walks entirely.
    /// Returns `(analysis, walked_funders)`.
    ///
    /// Slot attribution uses the buyer's **earliest** slot (`min`), and the
    /// supply share counts **creation-slot buys only** — previously the
    /// newest-first order plus all-slot sums overcounted the monopoly rule
    /// into false Rejects. When funder tracing is mostly untraceable the
    /// verdict is escalated to `SuspectLowVolume` (fail-closed: unknown is
    /// not organic).
    async fn bundle_verdict_tiered(
        &self,
        curve: &Pubkey,
        creation_slot: u64,
    ) -> Result<(BundleAnalysis, bool), ClientError> {
        let (records, _, truncated) =
            read_curve_swaps(&self.rpc, self.commitment, curve, None, true).await?;
        if truncated {
            // Partial newest-75 view: early creation-slot buys may be
            // missing, so the monopoly share undercounts toward a false
            // SafeOrganic. Fail closed to paper-only suspicion.
            return Ok((
                BundleAnalysis {
                    is_bundled: true,
                    creation_slot_buys: 0,
                    shared_funder_count: 0,
                    total_cluster_supply_pct: 0.0,
                    verdict: BundleVerdict::SuspectLowVolume,
                },
                false,
            ));
        }
        // Fail closed on unreadable supply: a zero fallback would neuter
        // the >15% monopoly rule into a permanent Suspect.
        let total_supply = self.curve_total_supply(curve).await?;

        // Distinct buyers, first-seen order, bounded (RPC budget).
        // Only proven `Buy` sides count: `Unknown` records (future
        // unmapped layouts) carry no trustworthy amount or side, so the
        // monopoly math ignores them rather than guessing.
        let mut seen: HashSet<Pubkey> = HashSet::new();
        let mut buyers: Vec<Pubkey> = Vec::new();
        for record in records.iter().filter(|r| r.side == SwapSide::Buy) {
            if seen.insert(record.wallet) {
                buyers.push(record.wallet);
            }
            if buyers.len() >= MAX_TRACED_BUYERS {
                break;
            }
        }

        let mut swaps: Vec<(u64, Pubkey, f64)> = Vec::new();
        for buyer in &buyers {
            // Earliest slot across this buyer's buys (records are
            // newest-first from RPC; `find` would return the newest).
            let slot = records
                .iter()
                .filter(|r| r.side == SwapSide::Buy && r.wallet == *buyer)
                .map(|r| r.slot)
                .min()
                .unwrap_or(u64::MAX);
            // Creation-slot amount only: later accumulation must not
            // inflate the monopoly share attributed to the launch slot.
            let creation_amount: u64 = records
                .iter()
                .filter(|r| {
                    r.side == SwapSide::Buy && r.wallet == *buyer && r.slot == creation_slot
                })
                .fold(0u64, |acc, r| acc.saturating_add(r.amount_base_units));
            let pct = if total_supply > 0 {
                creation_amount as f64 / total_supply as f64 * 100.0
            } else {
                0.0
            };
            swaps.push((slot, *buyer, pct));
        }

        let slot_buys = swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .count();
        let walked = slot_buys < 3;
        let now = unix_now();
        let funding_parents = if walked {
            self.trace_funders(&buyers, now).await
        } else {
            Vec::new()
        };

        let mut analysis =
            BundleDetector::evaluate_launch_slot(creation_slot, &swaps, &funding_parents);
        // Fail-closed on untraceable funders: when we walked but resolved
        // fewer than half the buyers (cache + fresh), unknown ancestry must
        // not read as organic. Escalate Safe → Suspect.
        if walked && analysis.verdict == BundleVerdict::SafeOrganic {
            let traced: HashSet<Pubkey> =
                funding_parents.iter().map(|(b, _)| *b).collect();
            let resolved = buyers.iter().filter(|b| traced.contains(b)).count();
            // `+1` avoids divide noise on tiny sets; threshold = <50%.
            if !buyers.is_empty() && resolved * 2 < buyers.len() {
                analysis.verdict = BundleVerdict::SuspectLowVolume;
                analysis.is_bundled = true;
            }
        }

        Ok((analysis, walked))
    }

    /// Cached, concurrent, deadline-bound funder tracing.
    /// `now` is the caller's wall-clock snapshot (single source per round;
    /// see [`unix_now`]) so TTL edges are deterministic within one verdict.
    async fn trace_funders(&self, buyers: &[Pubkey], now: u64) -> Vec<(Pubkey, Pubkey)> {
        let mut pairs: Vec<(Pubkey, Pubkey)> = Vec::new();
        let mut missing: Vec<Pubkey> = Vec::new();
        {
            let cache = self.funder_cache.lock().await;
            for buyer in buyers {
                match cache.get(&buyer.to_string(), now) {
                    Some(Some(funder)) => {
                        if let Ok(funder) = Pubkey::from_str(&funder) {
                            pairs.push((*buyer, funder));
                        }
                    }
                    Some(None) => {}
                    None => missing.push(*buyer),
                }
            }
        }
        if missing.is_empty() {
            return pairs;
        }
        use futures_util::stream::{self, StreamExt};
        // Outer `Err` (deadline or transport) is transient: never
        // cached, retried next round. `Ok` (found or conclusively
        // untraceable) is stable for 24h.
        let fresh: Vec<(Pubkey, Result<Option<String>, ()>)> =
            stream::iter(missing)
                .map(|buyer| async move {
                    let found = match tokio::time::timeout(
                        FUNDER_TRACE_TIMEOUT,
                        self.genesis_funder(&buyer),
                    )
                    .await
                    {
                        Err(_) | Ok(Err(())) => Err(()),
                        Ok(Ok(found)) => Ok(found.map(|funder| funder.to_string())),
                    };
                    (buyer, found)
                })
                .buffered(FUNDER_TRACE_FANOUT)
                .collect()
                .await;
        {
            let mut cache = self.funder_cache.lock().await;
            for (buyer, found) in &fresh {
                let Ok(funder) = found else { continue };
                cache.put(buyer.to_string(), funder.clone(), now);
                if let Some(funder) = funder
                    && let Ok(funder) = Pubkey::from_str(funder)
                {
                    pairs.push((*buyer, funder));
                }
            }
        }
        pairs
    }

    /// Total token supply from the curve account (`token_total_supply`
    /// u64 at offset 40). Fail-closed: any unreadable supply (transport
    /// error, timeout, short data) propagates as `Err` so the >15%
    /// monopoly rule can never silently degrade to `Suspect` on a zero
    /// fallback. Bounded by [`TX_FETCH_TIMEOUT`] so one slow account
    /// fetch cannot stall the bundle verdict.
    async fn curve_total_supply(&self, curve: &Pubkey) -> Result<u64, ClientError> {
        let account = match tokio::time::timeout(TX_FETCH_TIMEOUT, self.rpc.get_account(curve)).await
        {
            Ok(Ok(account)) => account,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(solana_client::client_error::ClientErrorKind::Custom(
                    "curve_total_supply timed out".to_string(),
                )
                .into());
            }
        };
        let supply = account
            .data
            .get(curve::TOKEN_SUPPLY_OFFSET..curve::TOKEN_SUPPLY_OFFSET + curve::TOKEN_SUPPLY_LEN)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
            .filter(|s| *s > 0)
            .ok_or_else(|| {
                solana_client::client_error::ClientErrorKind::Custom(
                    "curve account missing token_total_supply".to_string(),
                )
            })?;
        Ok(supply)
    }

    /// Genesis funder: walk the wallet's signature history newest-first
    /// (pages of 1000, capped at [`MAX_FUNDER_PAGES`]) down to its first
    /// transaction, then return the source of the first SOL funding
    /// found there (oldest few tried in order). `Ok(None)` when the walk
    /// conclusively ends (no history, page cap, nothing parses) —
    /// stable enough to cache. `Err(())` on transport/timeout trouble —
    /// the caller must NOT cache it (a blip would freeze as
    /// "untraceable" for 24h). The caller deadline-bounds each trace
    /// via [`FUNDER_TRACE_TIMEOUT`].
    async fn genesis_funder(&self, buyer: &Pubkey) -> Result<Option<Pubkey>, ()> {
        let mut before: Option<Signature> = None;
        let mut oldest_page: Vec<(String, bool)> = Vec::new();
        let mut pages = 0u32;
        loop {
            if pages >= MAX_FUNDER_PAGES {
                debug!(%buyer, pages, "funder walk hit page cap; treating as untraceable");
                break;
            }
            let page = match tokio::time::timeout(
                FUNDER_OP_TIMEOUT,
                self.rpc.get_signatures_for_address_with_config(
                    buyer,
                    GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: None,
                        limit: Some(1000),
                        commitment: Some(self.commitment),
                    },
                ),
            )
            .await
            {
                Ok(Ok(page)) => page,
                _ => return Err(()),
            };
            if page.is_empty() {
                break;
            }
            pages += 1;
            let exhausted = page.len() < 1000;
            before = page
                .last()
                .and_then(|s| Signature::from_str(&s.signature).ok());
            oldest_page = page
                .iter()
                .map(|s| (s.signature.clone(), s.err.is_some()))
                .collect();
            if exhausted {
                break;
            }
        }
        if oldest_page.is_empty() {
            return Ok(None);
        }
        let tx_config = json_tx_config(self.commitment);
        // Oldest first; a few fallbacks in case the very first
        // transaction carries no parseable funding. Failed
        // transactions never fund anyone. A fetch that fails here only
        // skips that candidate (the next trail slot may still resolve);
        // total unreadability falls through to `Ok(None)` — the trail
        // was walked, just unresolvable.
        for (sig, failed) in oldest_page.iter().rev().take(FUNDER_TRAIL) {
            if *failed {
                continue;
            }
            let Ok(signature) = Signature::from_str(sig) else {
                continue;
            };
            let fetched = match tokio::time::timeout(
                FUNDER_OP_TIMEOUT,
                self.rpc.get_transaction_with_config(&signature, tx_config),
            )
            .await
            {
                Ok(Ok(fetched)) => fetched,
                _ => continue,
            };
            if let Some(funder) = find_funding_source(&fetched, buyer) {
                return Ok(Some(funder));
            }
        }
        Ok(None)
    }
}

/// Migration-hunt depth: 3 pages × 10 signatures with a 10-fetch
/// budget. A migration deeper than 30 signatures is missed (returns
/// `None`, and the pipeline retries the hunt on later ticks) — depth is
/// capped for RPC budget, and per-tx fetches stay under the outer 15s
/// screening deadline instead of ballooning to 10 × 5s unsupervised.
pub const HUNT_MAX_PAGES: usize = 3;
/// See [`HUNT_MAX_PAGES`].
pub const HUNT_PAGE_LIMIT: usize = 10;
/// See [`HUNT_MAX_PAGES`].
pub const HUNT_MAX_FETCHES: usize = 10;
/// Maximum distinct buyers traced for funders per evaluation.
pub const MAX_TRACED_BUYERS: usize = 8;
/// Per-buyer genesis-trace deadline: bounds the stall when one wallet
/// has a pathological history (the page walk itself is page-capped).
/// Must stay below [`SCREEN_RPC_TIMEOUT`] so the inner deadline fires
/// first and the outer timeout remains a backstop, not the primary path.
pub const FUNDER_TRACE_TIMEOUT: Duration = Duration::from_secs(8);
/// Per-transaction fetch deadline inside fan-out reads (dev sampling,
/// curve pages). Keeps one slow RPC from consuming the whole 15s budget.
pub const TX_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-operation deadline inside the genesis walk (signature pages and
/// single transaction fetches): one slow page must not eat the whole
/// 8s [`FUNDER_TRACE_TIMEOUT`] budget. The outer timeout stays the
/// backstop, not the primary path.
pub const FUNDER_OP_TIMEOUT: Duration = Duration::from_secs(3);
/// Dev-history fan-out width: 20 sequential fetches could consume the
/// whole screening budget; 20 concurrent ones hammer the node.
pub const DEV_SCREEN_FANOUT: usize = 5;
/// Concurrent genesis-trace walks: one confirm may trace up to
/// [`MAX_TRACED_BUYERS`] buyers, each walking [`MAX_FUNDER_PAGES`] pages.
/// Bounded like every other fan-out in this module (4 keeps one confirm
/// from opening 8 concurrent page-walks against the node).
pub const FUNDER_TRACE_FANOUT: usize = 4;
/// Curve-swap signature page size and page cap: 3 pages × 25 = 75 newest
/// signatures per round. Beyond that the watermark is stale and the
/// caller is warned via `truncated` handling upstream.
pub const CURVE_SWAP_PAGE_LIMIT: usize = 25;
pub const CURVE_SWAP_MAX_PAGES: usize = 3;
/// Genesis funder page-walk cap: 10 pages × 1000 signatures. Beyond this
/// the wallet is treated as untraceable (RPC budget over completeness).
pub const MAX_FUNDER_PAGES: u32 = 10;
/// Funder ancestry cache TTL: sybil infrastructure is reused across
/// launches, so repeat buyers cost zero RPC.
pub const FUNDER_CACHE_TTL_SECS: u64 = 86_400;
/// Dev-verdict cache TTL: history barely moves within an hour.
pub const DEV_CACHE_TTL_SECS: u64 = 3_600;
/// Signatures kept from the oldest page for funding fallbacks.
const FUNDER_TRAIL: usize = 4;

/// Local unix seconds for cache TTLs.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Find the source of a native SOL transfer into `beneficiary` inside a
/// fetched transaction (top-level and inner instructions). Handles the
/// system funding shapes: `transfer` (u32 LE 2, 12 bytes, accounts
/// `[from, to]`), `createAccount` (u32 LE 0, 52 bytes, accounts
/// `[funder, new]`), `createAccountWithSeed` (u32 LE 3, ≥92 bytes,
/// accounts `[from, new, base]`), and `transferWithSeed` (u32 LE 11,
/// ≥52 bytes, accounts `[from, base, to]`). Multisig/CPI-routed funding
/// is covered whenever it executes one of these as an inner instruction
/// (the traversal below visits them). Also attributes ATA creation paid
/// by someone else: a `createAssociatedTokenAccount`/`CreateIdempotent`
/// instruction (`[payer, ata, owner, ...]`) whose `owner` is the
/// beneficiary names the SPL onboarding funder. Pure; `None` when no
/// such transfer exists.
///
/// Best-effort by construction: raw SPL-token `Transfer`/`TransferChecked`
/// amounts carry no ATA→owner map without account fetches, and exchange
/// payouts outside these shapes, do not match any arm and read as
/// untraceable (callers escalate low trace rates to `Suspect`, never to
/// organic).
pub fn find_funding_source(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    beneficiary: &Pubkey,
) -> Option<Pubkey> {
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return None;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return None;
    };
    let keys = resolve_keys(raw, &tx_with_meta.meta)?;

    let scan = |program_id_index: u8, accounts: &[u8], data_b58: &str| -> Option<Pubkey> {
        let program = keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default();
        if program == SYSTEM_PROGRAM_ID {
            scan_system_funding(&keys, accounts, data_b58, beneficiary)
        } else if program == ASSOCIATED_TOKEN_PROGRAM_ID {
            scan_ata_funding(&keys, accounts, data_b58, beneficiary)
        } else {
            None
        }
    };

    let mut found: Option<Pubkey> = None;
    let mut visit = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        if found.is_some() {
            return;
        }
        if let Some(funder) = scan(program_id_index, accounts, data_b58) {
            found = Some(funder);
        }
    };
    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut visit);
    found
}

/// System-program arm of [`find_funding_source`]: SOL transfers and
/// account creations paying into `beneficiary`, including the seeded
/// variants (`TransferWithSeed` tag 11, `CreateAccountWithSeed` tag 3 —
/// account orders verified against the SDK constructors).
/// Length-guarded BEFORE slicing: short payloads are not funding, never
/// a panic.
fn scan_system_funding(
    keys: &[Pubkey],
    accounts: &[u8],
    data_b58: &str,
    beneficiary: &Pubkey,
) -> Option<Pubkey> {
    let raw = decode_ix_data(data_b58)?;
    if raw.len() < 4 {
        return None;
    }
    let tag = u32::from_le_bytes(raw[..4].try_into().ok()?);
    let (from_idx, to_idx) = match (raw.len(), tag) {
        (12, 2) | (52, 0) => (0usize, 1usize),
        // CreateAccountWithSeed: 4 + 32 base + 8 seed-len + seed +
        // 8 lamports + 8 space + 32 owner (≥92); [from, new, base].
        (len, 3) if len >= 92 => (0usize, 1usize),
        // TransferWithSeed: 4 + 8 lamports + 8 seed-len + seed + 32
        // owner (≥52); [from, base, to].
        (len, 11) if len >= 52 => (0usize, 2usize),
        _ => return None,
    };
    let from = keys.get(usize::from(*accounts.get(from_idx)?)).copied()?;
    let to = keys.get(usize::from(*accounts.get(to_idx)?)).copied()?;
    (to == *beneficiary).then_some(from)
}

/// ATA-program arm of [`find_funding_source`]: someone paying the rent
/// to create `beneficiary`'s associated token account is an SPL funding
/// link. Matches `Create` (`[0]`) and `CreateIdempotent` (`[1]`) single
/// bytes with `[payer, ata, owner, ...]` account order.
fn scan_ata_funding(
    keys: &[Pubkey],
    accounts: &[u8],
    data_b58: &str,
    beneficiary: &Pubkey,
) -> Option<Pubkey> {
    let raw = decode_ix_data(data_b58)?;
    if raw.len() != 1 || (raw[0] != 0 && raw[0] != 1) {
        return None;
    }
    if accounts.len() < 3 {
        return None;
    }
    let payer = keys.get(usize::from(accounts[0])).copied()?;
    let owner = keys.get(usize::from(accounts[2])).copied()?;
    (owner == *beneficiary).then_some(payer)
}
