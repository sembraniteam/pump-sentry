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
    // Window full: older history hides beyond `sig_limit`. The launch
    // count below is a lower bound; a zero count on a full window proves
    // nothing and must fail closed.
    let history_truncated = sigs.len() >= cfg.sig_limit;
    // Newest first. Last entry is oldest activity.
    let age_secs = sigs
        .last()
        .and_then(|s| s.block_time)
        .map(|t| now_secs.saturating_sub(t.max(0) as u64));
    // Early reject skips the fan-out: failing age can never pass.
    if age_secs.is_none_or(|age| age < cfg.min_age_secs) {
        return Ok(judge_developer(age_secs, 0, cfg));
    }

    let tx_config = json_tx_config(commitment);
    // Bounded fan-out (5-wide). Failed txs skipped. Unreadable counted.
    let candidates: Vec<(Signature, bool)> = sigs
        .iter()
        .take(cfg.sample_txs)
        .filter_map(|s| {
            Signature::from_str(&s.signature)
                .ok()
                .map(|sig| (sig, s.err.is_some()))
        })
        .collect();
    let total = candidates.len();
    let err_skipped = candidates.iter().filter(|(_, e)| *e).count();
    if total > 0 && err_skipped * 2 >= total {
        // Majority failed on-chain: the sample proves no successful
        // history, not a clean one. Fail closed without burning fetches.
        return Ok(DevVerdict {
            pass: false,
            age_secs,
            prior_launches: 0,
            reason: format!(
                "dev history mostly failed txs ({err_skipped} of {total} errored); failing closed",
            ),
        });
    }
    use futures_util::stream::{self, StreamExt};
    let mut prior_launches: usize = 0;
    let mut fetch_failed: usize = 0;
    // Owned items for `.buffered()`: borrowed items trip E0631.
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
            // Failed tx never launched. Skip even when flag missing.
            let failed_tx = fetched
                .transaction
                .meta
                .as_ref()
                .is_some_and(|m| m.err.is_some());
            if failed_tx {
                return (0usize, false);
            }
            // Undecodable keys prove nothing: count unreadable, never zero.
            match count_dev_creates(&fetched, dev) {
                Some(count) => (count, false),
                None => (0usize, true),
            }
        })
        .buffered(DEV_SCREEN_FANOUT)
        .collect()
        .await;
    for (count, failed) in fetched {
        prior_launches += count;
        fetch_failed += usize::from(failed);
    }
    if !sample_trustworthy(fetch_failed, total) {
        // Mostly-unreadable sample: count is a lower bound. Fail closed.
        return Ok(DevVerdict {
            pass: false,
            age_secs,
            prior_launches,
            reason: format!(
                "dev history unreadable ({fetch_failed} of {total} fetches failed); failing closed",
            ),
        });
    }
    if history_truncated && prior_launches == 0 {
        // Full window with zero hits: older launches may hide beyond
        // `sig_limit`. A zero count here is a lower bound, not proof.
        return Ok(DevVerdict {
            pass: false,
            age_secs,
            prior_launches,
            reason: format!(
                "dev history window full ({total} sampled of {} sigs); zero launches unproven, failing closed",
                cfg.sig_limit,
            ),
        });
    }
    Ok(judge_developer(age_secs, prior_launches, cfg))
}

/// True when a dev sample is trusted.
/// Fails stay below half. Empty sample proves nothing.
pub(crate) fn sample_trustworthy(failed: usize, total: usize) -> bool {
    total > 0 && (failed == 0 || failed * 2 < total)
}

/// Swap reader for one curve since `since_sig`.
/// Returns `(records, newest_sig, truncated)`.
/// Truncated rounds must not advance quorum.
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

        // Watermark cutoff first: sigs at/after it never fetch.
        if since_sig.is_some()
            && sigs
                .iter()
                .any(|s| Some(s.signature.as_str()) == since_sig)
        {
            watermark_found = true;
        }
        let end = since_sig
            .and_then(|w| sigs.iter().position(|s| s.signature.as_str() == w))
            .unwrap_or(sigs.len());
        if newest.is_none() {
            newest = sigs
                .first()
                .filter(|s| Some(s.signature.as_str()) != since_sig)
                .map(|s| s.signature.clone());
        }
        // Bounded concurrency: serial page reads stall confirms (75
        // serial fetches blow the 15s screen budget). Jobs are owned
        // up front (Send-safe); order restored by index; 429s degrade
        // to skips (fail-closed), never errors.
        use futures_util::stream::{self, StreamExt};
        let jobs: Vec<(usize, Signature, String)> = sigs[..end]
            .iter()
            .enumerate()
            .filter_map(|(i, status)| {
                if status.err.is_some() {
                    return None;
                }
                let signature = Signature::from_str(&status.signature).ok()?;
                Some((i, signature, status.signature.clone()))
            })
            .collect();
        let mut fetched: Vec<(usize, Vec<SwapRecord>)> = stream::iter(jobs)
            .map(|(i, signature, sig_str)| async move {
                let records = match tokio::time::timeout(
                    TX_FETCH_TIMEOUT,
                    rpc.get_transaction_with_config(&signature, tx_config),
                )
                .await
                {
                    Ok(Ok(fetched)) => extract_pump_swaps(&fetched, &sig_str),
                    _ => Vec::new(),
                };
                (i, records)
            })
            .buffered(CURVE_SWAP_FETCH_FANOUT)
            .collect()
            .await;
        fetched.sort_by_key(|(i, _)| *i);
        for (_, recs) in fetched {
            records.extend(recs);
        }
        if watermark_found {
            break;
        }
        // Watermark missed: keep paging unless history ran out.
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
        // Watermark missed: partial view.
        (Some(_), _) => !watermark_found,
        // Exhaustive read hitting the cap: more history exists.
        (None, true) => pages >= CURVE_SWAP_MAX_PAGES,
        // First read is complete by definition.
        (None, false) => false,
    };
    if truncated {
        // Callers must not advance quorum on this partial view.
        debug!(
            pages,
            fetched = records.len(),
            "curve read truncated at page cap; watermark stale"
        );
    }
    Ok((records, newest, truncated))
}

/// Screening deadline. No unbounded hangs.
///
/// Budget per path (outer first, inner fires first):
/// screen 15s / 5s fetch; funder 15s / 8s walk;
/// tickers 15s; executor 90s.
pub const SCREEN_RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// Run a future with the screening deadline. `None` on timeout.
async fn with_screen_timeout<F, T>(future: F) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(SCREEN_RPC_TIMEOUT, future).await.ok()
}

/// Found migration, ready for alerts and routing.
///
/// Typed shape. Pipeline speaks only the plain-string report.
#[derive(Debug, Clone)]
pub struct DetectedMigration {
    /// Migration transaction signature (base58).
    pub signature: String,
    /// Where liquidity landed.
    pub destination: MigrationDestination,
    /// New pool account for resolved hits.
    pub amm_id: Option<Pubkey>,
    /// Parsed pool for resolved hits.
    pub pool: Option<MigratedPool>,
}

/// Plain-string migration report for cross-crate use.
/// Venue orientation differs: Raydium pools are WSOL-based, PumpSwap
/// pools are WSOL-quoted (see `MigratedPool` vaults, the source of
/// truth — never infer orientation from these mint labels).
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
    /// Convert to the plain report.
    pub fn report(&self) -> MigrationReport {
        MigrationReport {
            signature: self.signature.clone(),
            destination: self.destination,
            amm_id: self.amm_id.map(|k| k.to_string()),
            base_mint: self.pool.as_ref().map(|p| p.base_mint().to_string()),
            quote_mint: self.pool.as_ref().map(|p| p.quote_mint().to_string()),
            base_vault: self.pool.as_ref().map(|p| p.base_vault().to_string()),
            quote_vault: self.pool.as_ref().map(|p| p.quote_vault().to_string()),
        }
    }
}

/// AMM slot in `initialize2`: `[0]=token, [1]=ATA, [2]=system,
/// [3]=rent, [4]=new AMM`.
pub const INITIALIZE2_AMM_INDEX: usize = 4;

/// Resolve the new AMM id: accounts[4] of the first Raydium ix.
/// Callers already gate the tx on Raydium program + `initialize2` log +
/// curve/mint linkage (see `MigrationMonitor::classify_migration`), so a
/// co-bundled unrelated Raydium ix can't promote a wrong AMM past the
/// pool trust gate (`parse_validated` still must pass owner/mints).
pub fn initialize2_amm_id(instructions: &[(Pubkey, Vec<Pubkey>)]) -> Option<Pubkey> {
    instructions
        .iter()
        .find(|(program, _)| *program == RAYDIUM_V4_PROGRAM_ID)
        .and_then(|(_, accounts)| accounts.get(INITIALIZE2_AMM_INDEX).copied())
}

/// Resolve the new PumpSwap pool: accounts[9] of `migrate`,
/// accounts[10] of `migrate_v2` (per pump IDL). Matches on the pump
/// program plus the instruction discriminator, so a co-bundled
/// unrelated pump ix can't promote a wrong pool past the pool trust
/// gate (`parse_validated` still must pass owner/mints).
pub fn migrate_pool_id(decoded: &[DecodedIx]) -> Option<Pubkey> {
    decoded.iter().find_map(|ix| {
        if ix.program != PUMP_PROGRAM_ID || ix.data.len() < 8 {
            return None;
        }
        let index = if ix.data[..8] == PUMP_MIGRATE_DISCRIMINATOR {
            MIGRATE_POOL_ACCOUNT_INDEX
        } else if ix.data[..8] == PUMP_MIGRATE_V2_DISCRIMINATOR {
            MIGRATE_V2_POOL_ACCOUNT_INDEX
        } else {
            return None;
        };
        ix.accounts.get(index).copied()
    })
}

/// One decoded ix: program, keys, raw data.
#[derive(Debug, Clone)]
pub struct DecodedIx {
    /// Executing program.
    pub program: Pubkey,
    /// Resolved account keys.
    pub accounts: Vec<Pubkey>,
    /// Raw instruction data.
    pub data: Vec<u8>,
}

/// Decode one tx for migration checks.
/// `None` on any bad key.
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
        // One shared decode: bs58 first, base64 fallback.
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

/// Screening client: dev verdicts plus curve reads.
#[derive(Clone)]
pub struct RpcScreener {
    rpc: Arc<RpcClient>,
    commitment: CommitmentConfig,
    dev_cfg: DevScreenConfig,
    funder_cache: Arc<tokio::sync::Mutex<TimedCache<Option<String>>>>,
    dev_cache: Arc<tokio::sync::Mutex<TimedCache<DevVerdict>>>,
}

impl RpcScreener {
    /// Build with URL, commitment, and dev policy.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        dev_cfg: DevScreenConfig,
    ) -> Result<Self, ScreenError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(ScreenError::BadUrl(http_url.to_string())),
        }
        Ok(Self::with_rpc(
            Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            commitment,
            dev_cfg,
        ))
    }

    /// Build around a shared RPC client (one connection pool per process).
    pub fn with_rpc(
        rpc: Arc<RpcClient>,
        commitment: CommitmentConfig,
        dev_cfg: DevScreenConfig,
    ) -> Self {
        Self {
            rpc,
            commitment,
            dev_cfg,
            funder_cache: Arc::new(tokio::sync::Mutex::new(TimedCache::new(
                FUNDER_CACHE_TTL_SECS,
            ))),
            dev_cache: Arc::new(tokio::sync::Mutex::new(TimedCache::new(DEV_CACHE_TTL_SECS))),
        }
    }

    /// Dev verdict. Fails closed on transport errors.
    /// Full verdicts cache. Early age-rejects don't:
    /// young wallets age out.
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
        // Only full verdicts cache. Early rejects would freeze youth.
        if verdict.pass || verdict.prior_launches > 0 {
            self.dev_cache
                .lock()
                .await
                .put(dev.to_string(), verdict.clone(), now_secs);
        }
        Ok(verdict)
    }

    /// Swaps for a curve since `since_sig`. Truncated rounds hold quorum.
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

    /// Top-10 holder share of supply, percent. Two bounded reads behind
    /// the screening deadline; any unreadable leg fails closed (`Err`).
    /// Base-unit string math (the ratio needs no decimals): unparseable
    /// amounts or a zero supply refuse rather than price a fiction.
    pub async fn top_holder_pct(&self, mint: &Pubkey) -> Result<f64, ScreenError> {
        let bad = |what: &str| ScreenError::BadAddress(format!("{what} for {mint}"));
        let (largest, supply) = match with_screen_timeout(async {
            let largest = self.rpc.get_token_largest_accounts(mint).await?;
            let supply = self.rpc.get_token_supply(mint).await?;
            Ok::<_, ScreenError>((largest, supply))
        })
        .await
        {
            None => return Err(ScreenError::Timeout("top_holder_pct")),
            Some(result) => result?,
        };
        let supply_units: u128 = supply
            .amount
            .parse()
            .map_err(|_| bad("unparseable supply"))?;
        if supply_units == 0 {
            return Err(bad("zero supply"));
        }
        let mut top: u128 = 0;
        for (i, acc) in largest.iter().take(10).enumerate() {
            let units: u128 = acc
                .amount
                .amount
                .parse()
                .map_err(|_| bad(&format!("unparseable top-{} balance", i + 1)))?;
            top = top.saturating_add(units);
        }
        Ok(top as f64 / supply_units as f64 * 100.0)
    }

    /// Hunt the migration tx for a graduated curve.
    /// Raydium hits need the trust gate (owner, WSOL base, quote).
    /// `None` when nothing found. Bounded by timeout.
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
        // No unvalidated fallback: a Raydium hit that fails the trust
        // gate/fetch/AMM resolve returns None (retry next tick). Promoting
        // it to RaydiumV4 pool:None would mark an unproven pool as migrated.
        let mut before: Option<Signature> = None;
        let mut fetches: usize = 0;
        let mut failed_fetches: usize = 0;
        const MAX_FAILED_FETCHES: usize = 4;
        let mut budget_hit = false;
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
                if fetches >= HUNT_MAX_FETCHES || failed_fetches >= MAX_FAILED_FETCHES {
                    debug!("migration hunt hit fetch budget; stopping");
                    budget_hit = true;
                    break;
                }
                if status.err.is_some() {
                    continue;
                }
                let Ok(signature) = Signature::from_str(&status.signature) else {
                    continue;
                };
                let fetched = match tokio::time::timeout(
                    TX_FETCH_TIMEOUT,
                    self.rpc.get_transaction_with_config(&signature, tx_config),
                )
                .await
                {
                    Ok(Ok(fetched)) => {
                        fetches += 1;
                        fetched
                    }
                    _ => {
                        // Transport blips don't burn the progress budget.
                        failed_fetches += 1;
                        continue;
                    }
                };
                let Some((keys, decoded, logs)) = decode_migration_tx(&fetched) else {
                    continue;
                };
                let mut ix_datas: Vec<Vec<u8>> = Vec::with_capacity(decoded.len());
                let mut pairs: Vec<(Pubkey, Vec<Pubkey>)> = Vec::with_capacity(decoded.len());
                for ix in &decoded {
                    ix_datas.push(ix.data.clone());
                    pairs.push((ix.program, ix.accounts.clone()));
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
                                        debug!(%amm, "pool account fetch failed; skipping unproven hit, continuing hunt");
                                        continue;
                                    }
                                };
                                match RaydiumV4Pool::parse_validated(
                                    &amm,
                                    &account.data,
                                    &account.owner,
                                    expected_token_mint,
                                ) {
                                    Some(pool) => Some(MigratedPool::RaydiumV4(pool)),
                                    None => {
                                        debug!(%amm, "pool failed trust gate; skipping unproven hit, continuing hunt");
                                        continue;
                                    }
                                }
                            }
                            None => {
                                debug!("raydium migration without resolvable AMM; skipping unproven hit, continuing hunt");
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
                        let pool_id = migrate_pool_id(&decoded);
                        let pool = match pool_id {
                            Some(pool_id) => {
                                let account = match tokio::time::timeout(
                                    TX_FETCH_TIMEOUT,
                                    self.rpc.get_account(&pool_id),
                                )
                                .await
                                {
                                    Ok(Ok(account)) => account,
                                    _ => {
                                        debug!(%pool_id, "pool account fetch failed; skipping unproven hit, continuing hunt");
                                        continue;
                                    }
                                };
                                match PumpSwapPool::parse_validated(
                                    &pool_id,
                                    &account.data,
                                    &account.owner,
                                    expected_token_mint,
                                ) {
                                    Some(pool) => Some(MigratedPool::PumpSwap(pool)),
                                    None => {
                                        debug!(%pool_id, "pool failed trust gate (wrong owner/mint, or non-WSOL quote); skipping unproven hit, continuing hunt");
                                        continue;
                                    }
                                }
                            }
                            None => {
                                debug!("pumpswap migration without resolvable pool; skipping unproven hit, continuing hunt");
                                continue;
                            }
                        };
                        return Ok(Some(DetectedMigration {
                            signature: status.signature.clone(),
                            destination: MigrationDestination::PumpSwap,
                            amm_id: pool_id,
                            pool,
                        }));
                    }
                    None => continue,
                }
            }
            if budget_hit || fetches >= HUNT_MAX_FETCHES {
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
        Ok(None)
    }

    /// Bundle check for a curve at confirm time. Fails closed.
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

    /// Slot rule alone can prove a bundle (3+ buys skips funder walks).
    ///
    /// Slot attribution uses each buyer's **earliest** slot; the supply
    /// share counts **creation-slot buys only**. Mostly-untraceable
    /// funders escalate to `SuspectLowVolume` (unknown is not organic).
    async fn bundle_verdict_tiered(
        &self,
        curve: &Pubkey,
        creation_slot: u64,
    ) -> Result<(BundleAnalysis, bool), ClientError> {
        let (records, _, truncated) =
            read_curve_swaps(&self.rpc, self.commitment, curve, None, true).await?;
        if truncated {
            // Partial view: early buys may hide. Fail closed.
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
        // Bad supply can't zero the monopoly math. Propagate.
        let total_supply = self.curve_total_supply(curve).await?;

        // Distinct buyers, earliest-first, bounded. Only Buys count.
        // Records arrive newest-first: iterate oldest-first so the 8
        // traced wallets are the earliest (creation-slot) buyers, not
        // the 8 most recent. Otherwise early bundle buyers fall out of
        // the window and the monopoly math undercounts (fail-open).
        let mut seen: HashSet<Pubkey> = HashSet::new();
        let mut buyers: Vec<Pubkey> = Vec::new();
        for record in records
            .iter()
            .filter(|r| r.side == SwapSide::Buy)
            .rev()
        {
            if seen.insert(record.wallet) {
                buyers.push(record.wallet);
            }
            if buyers.len() >= MAX_TRACED_BUYERS {
                break;
            }
        }

        // Supply share covers ALL creation-slot buys (not just traced
        // buyers): the monopoly denominator must not shrink with the
        // trace cap.
        let total_creation_amount: u64 = records
            .iter()
            .filter(|r| r.side == SwapSide::Buy && r.slot == creation_slot)
            .fold(0u64, |acc, r| acc.saturating_add(r.amount_base_units));
        let total_cluster_all_pct = if total_supply > 0 {
            total_creation_amount as f64 / total_supply as f64 * 100.0
        } else {
            0.0
        };

        let mut swaps: Vec<(u64, Pubkey, f64)> = Vec::new();
        for buyer in &buyers {
            // Earliest slot per buyer (records are newest-first).
            let slot = records
                .iter()
                .filter(|r| r.side == SwapSide::Buy && r.wallet == *buyer)
                .map(|r| r.slot)
                .min()
                .unwrap_or(u64::MAX);
            // Creation-slot amount only. Later buys don't count.
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
        // Trace cap must not shrink the monopoly: raise the cluster
        // share to the all-buyer total when the 8-wallet window hides volume.
        if total_cluster_all_pct > analysis.total_cluster_supply_pct {
            analysis.total_cluster_supply_pct = total_cluster_all_pct;
            if total_cluster_all_pct > 15.0 {
                analysis.is_bundled = true;
                analysis.verdict = BundleVerdict::RejectBundledMonopoly;
            }
        }
        // Few traces resolved: Safe becomes Suspect.
        if walked && analysis.verdict == BundleVerdict::SafeOrganic {
            let traced: HashSet<Pubkey> =
                funding_parents.iter().map(|(b, _)| *b).collect();
            let resolved = buyers.iter().filter(|b| traced.contains(b)).count();
            // Threshold is below half.
            if !buyers.is_empty() && resolved * 2 < buyers.len() {
                analysis.verdict = BundleVerdict::SuspectLowVolume;
                analysis.is_bundled = true;
            }
        }

        Ok((analysis, walked))
    }

    /// Cached funder tracing. `now` fixes TTL edges per verdict.
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
        // Transport errors never cache. Found and untraceable cache 24h.
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

    /// Total supply from the curve account. Errors propagate.
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

    /// Genesis funder: walk history to the first tx, return its funder.
    /// `Ok(None)` ends the walk and caches. `Err` never caches.
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
        // Oldest first. A few fallbacks when the first tx has no funder.
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

/// Hunt depth: 3 pages x 10 sigs, 10-fetch budget.
/// Deeper hits miss (`None`; retried on later ticks).
pub const HUNT_MAX_PAGES: usize = 3;
/// See [`HUNT_MAX_PAGES`].
pub const HUNT_PAGE_LIMIT: usize = 10;
/// See [`HUNT_MAX_PAGES`].
pub const HUNT_MAX_FETCHES: usize = 10;
/// Max buyers traced per check.
pub const MAX_TRACED_BUYERS: usize = 8;
/// Per-buyer trace deadline (inside the screening deadline).
pub const FUNDER_TRACE_TIMEOUT: Duration = Duration::from_secs(8);
/// Per-tx fetch deadline in fan-out reads.
pub const TX_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-op deadline in the genesis walk.
pub const FUNDER_OP_TIMEOUT: Duration = Duration::from_secs(3);
/// Dev-history fan-out width.
pub const DEV_SCREEN_FANOUT: usize = 5;
/// Concurrent genesis-trace walks per confirm.
pub const FUNDER_TRACE_FANOUT: usize = 4;
/// Curve pages: 3 x 25 newest sigs. Past that, watermark is stale.
pub const CURVE_SWAP_PAGE_LIMIT: usize = 25;
/// Page-fetch fanout (see `read_curve_swaps`).
pub const CURVE_SWAP_FETCH_FANOUT: usize = 4;
pub const CURVE_SWAP_MAX_PAGES: usize = 3;
/// Genesis walk cap: 10 x 1000 sigs. Past that, untraceable.
pub const MAX_FUNDER_PAGES: u32 = 10;
/// Funder cache TTL: infra is reused across launches.
pub const FUNDER_CACHE_TTL_SECS: u64 = 86_400;
/// Dev verdict cache TTL: history moves slowly.
pub const DEV_CACHE_TTL_SECS: u64 = 3_600;
/// Signatures kept from the oldest page for funding fallbacks.
const FUNDER_TRAIL: usize = 4;

/// Unix seconds for cache TTLs.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Find the SOL funding source of `beneficiary` in a tx.
/// Covers system transfers, creations, seeded forms, and ATA creation.
/// Other shapes read as untraceable.
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

/// System arm: transfers and creations paying `beneficiary`.
/// Length-checked before slicing.
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

/// ATA arm: payer creating `beneficiary`'s ATA is a funding link.
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
