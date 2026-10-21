//! File configuration: categorized TOML, single source of truth.
//!
//! The file is the ONLY source for tunables — environment variables
//! are not read for them. The sole exception is `INFERENCE_API_KEY`,
//! which stays env-only (a bearer token must never sit in a config
//! file; `SOLANA_PRIVATE_KEY` likewise never enters `Settings`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Env override for the config path (mirrors `PID_FILE`/`LOG_FILE`).
pub const CONFIG_FILE_ENV: &str = "CONFIG_FILE";
/// Config filename inside the state dir.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Config load failures. Parse errors are fatal with file position:
/// a typo'd value must never silently become a default.
#[derive(Error, Debug)]
pub enum ConfigError {
    /// Filesystem trouble (missing dir, permissions).
    #[error("config file error at {0}: {1}")]
    Io(String, #[source] std::io::Error),
    /// TOML syntax trouble (message carries line/column).
    #[error("config parse error at {0}: {1}")]
    Parse(String, String),
}

/// `(toml section, toml key, flat map key)`. The single mapping
/// table: template, overlay, and docs all derive from it. Flat keys
/// reuse the historical `ENV_STYLE` spelling so the parser, warnings,
/// and runbooks stay greppable — but nothing reads the process
/// environment for them anymore: the file is the only source.
pub const KEYMAP: &[(&str, &str, &str)] = &[
    ("network", "mainnet_rpc_url", "MAINNET_RPC_URL"),
    (
        "network",
        "mainnet_rpc_urls_fallback",
        "MAINNET_RPC_URLS_FALLBACK",
    ),
    ("network", "mainnet_wss_rpc_url", "MAINNET_WSS_RPC_URL"),
    ("network", "yellowstone_endpoint", "YELLOWSTONE_ENDPOINT"),
    ("network", "commitment", "COMMITMENT"),
    ("network", "tick_interval_ms", "TICK_INTERVAL_MS"),
    (
        "network",
        "vault_tick_interval_ms",
        "VAULT_TICK_INTERVAL_MS",
    ),
    ("network", "fetch_concurrency", "FETCH_CONCURRENCY"),
    ("network", "backfill_sig_limit", "BACKFILL_SIG_LIMIT"),
    (
        "network",
        "reconnect_backoff_max_ms",
        "RECONNECT_BACKOFF_MAX_MS",
    ),
    ("trading", "start_balance", "START_BALANCE"),
    ("trading", "fee_bps", "FEE_BPS"),
    ("trading", "notional_usd", "NOTIONAL_USD"),
    ("trading", "max_open_positions", "MAX_OPEN_POSITIONS"),
    ("trading", "mint_cooldown_secs", "MINT_COOLDOWN_SECS"),
    ("trading", "max_daily_loss_usd", "MAX_DAILY_LOSS_USD"),
    ("trading", "max_exposure_usd", "MAX_EXPOSURE_USD"),
    (
        "trading",
        "max_expected_entry_gap_usd",
        "MAX_EXPECTED_ENTRY_GAP_USD",
    ),
    ("trading", "max_top_holder_pct", "MAX_TOP_HOLDER_PCT"),
    (
        "trading",
        "max_entry_flip_loss_pct",
        "MAX_ENTRY_FLIP_LOSS_PCT",
    ),
    ("trading", "min_fill_rate_pct", "MIN_FILL_RATE_PCT"),
    ("trading", "max_avg_exit_gap_usd", "MAX_AVG_EXIT_GAP_USD"),
    ("trading", "max_avg_entry_gap_usd", "MAX_AVG_ENTRY_GAP_USD"),
    ("trading", "hot_wallet_max_sol", "HOT_WALLET_MAX_SOL"),
    ("trading", "quote_usd", "QUOTE_USD"),
    ("trading", "liquidity_usd", "LIQUIDITY_USD"),
    ("exits", "trail_percent", "TRAIL_PERCENT"),
    ("exits", "runner_trail_percent", "RUNNER_TRAIL_PERCENT"),
    ("gate", "gate_window_secs", "GATE_WINDOW_SECS"),
    ("gate", "gate_min_buyers", "GATE_MIN_BUYERS"),
    ("gate", "gate_poll_ms", "GATE_POLL_MS"),
    ("gate", "dev_min_age_secs", "DEV_MIN_AGE_SECS"),
    ("gate", "dev_max_prior_launches", "DEV_MAX_PRIOR_LAUNCHES"),
    ("gate", "dev_screen_sample_txs", "DEV_SCREEN_SAMPLE_TXS"),
    ("gate", "dev_sig_limit", "DEV_SIG_LIMIT"),
    ("risk", "strategy_mode", "STRATEGY_MODE"),
    ("risk", "force_exit_regime", "FORCE_EXIT_REGIME"),
    ("risk", "runner_moonbag_percent", "RUNNER_MOONBAG_PERCENT"),
    ("risk", "inference_endpoint", "INFERENCE_ENDPOINT"),
    ("risk", "inference_model", "INFERENCE_MODEL"),
    ("risk", "inference_fallback", "INFERENCE_FALLBACK"),
    ("risk", "inference_defer_max_n", "INFERENCE_DEFER_MAX_N"),
    ("risk", "inference_timeout_ms", "INFERENCE_TIMEOUT_MS"),
    ("risk", "inference_headers", "INFERENCE_HEADERS"),
    ("execution", "slippage_bps", "SLIPPAGE_BPS"),
    (
        "execution",
        "slippage_vol_window_secs",
        "SLIPPAGE_VOL_WINDOW_SECS",
    ),
    (
        "execution",
        "slippage_vol_extra_cap_bps",
        "SLIPPAGE_VOL_EXTRA_CAP_BPS",
    ),
    (
        "execution",
        "priority_fee_micro_lamports",
        "PRIORITY_FEE_MICRO_LAMPORTS",
    ),
    ("execution", "compute_unit_limit", "COMPUTE_UNIT_LIMIT"),
    ("execution", "jito_tip_lamports", "JITO_TIP_LAMPORTS"),
    ("execution", "jito_endpoint", "JITO_ENDPOINT"),
    ("execution", "sim_strict", "SIM_STRICT"),
    ("alerts", "discord_alerts", "DISCORD_ALERTS"),
    ("alerts", "discord_channel", "DISCORD_CHANNEL"),
    ("alerts", "alert_diverged_warn_n", "ALERT_DIVERGED_WARN_N"),
    (
        "alerts",
        "alert_risk_fallback_warn_pct",
        "ALERT_RISK_FALLBACK_WARN_PCT",
    ),
    ("alerts", "alert_giveup_burst_n", "ALERT_GIVEUP_BURST_N"),
    (
        "alerts",
        "alert_giveup_window_secs",
        "ALERT_GIVEUP_WINDOW_SECS",
    ),
];

/// Shipped template: every tunable by category, defaults identical to
/// the built-in ones. Empty strings mean "unset" (public endpoint,
/// host-default channel). Secrets are env-only — see header comments.
pub const DEFAULT_CONFIG_TOML: &str = r#"# pump-sentry config — auto-created on first start (also `pump-sentry init-config`).
# This file is the ONLY source for tunables below; environment variables are not read for them.
# Secrets (SOLANA_PRIVATE_KEY, INFERENCE_API_KEY) stay in env and are NEVER read from here.

[network]
# Dedicated RPC recommended; empty falls back to the public endpoint.
mainnet_rpc_url = ""        # dedicated RPC recommended; empty = public endpoint
mainnet_rpc_urls_fallback = [] # read-path failover endpoints (comma list via MAINNET_RPC_URLS_FALLBACK); READS ONLY — dispatch never fans out; same-cluster https required live (override: ALLOW_FALLBACK_CLUSTER_MISMATCH=1, logged)
mainnet_wss_rpc_url = ""    # empty = public endpoint
yellowstone_endpoint = ""   # Yellowstone gRPC (https URL); non-empty REPLACES ws ingestion (x-token via YELLOWSTONE_TOKEN env, never here)
commitment = "confirmed"    # (confirmed|finalized; processed is refused live)
tick_interval_ms = 1000     # (curve polls)
vault_tick_interval_ms = 5000  # (migration pool polls)
fetch_concurrency = 32      # (concurrent getTransaction fetches; lower on metered RPC)
backfill_sig_limit = 100    # (sigs per reconnect backfill; 0 = disable backfill)
reconnect_backoff_max_ms = 5000  # (reconnect backoff cap; raise on flaky free-tier sockets)

[trading]
start_balance = 100.0       # (virtual paper cash, USD)
fee_bps = 0.0               # (extra commission/side; venue 125bps already in fills)
notional_usd = 10.0         # (paper size per trade, USD)
max_open_positions = 3      # (max concurrent positions: paper + live-held mints)
max_exposure_usd = 0.0      # (0 = off; total open exposure cap: paper basis + one ticket per unpaired live bag)
max_expected_entry_gap_usd = 0.0  # (0 = off; refuse opens whose ticket shape bled worse than this USD in settled live fills)
max_top_holder_pct = 0.0  # (0 = off; refuse opens when the top-10 holders own more than this % of supply)
max_entry_flip_loss_pct = 0.0  # (0 = off; refuse opens whose measured open+sell-back cost at the confirm book exceeds this % of the ticket)
min_fill_rate_pct = 50.0  # (0 = off; pause entries while trailing buy fills run below this %)
max_avg_exit_gap_usd = 0.0  # (0 = off; pause entries while the avg exit gap runs worse than this USD)
max_avg_entry_gap_usd = 0.0  # (0 = off; same for the entry leg)
mint_cooldown_secs = 3600   # (per-mint re-entry cooldown)
max_daily_loss_usd = 0.0    # (0 = off; live requires >0; survives restarts via day-state.json)
hot_wallet_max_sol = 0.0    # (0 = off; live: entries refuse while the wallet holds more SOL than this)
quote_usd = ""              # (TEST-ONLY price override; empty = unset)
liquidity_usd = 0.0         # (reserve-read fallback; 0 = fail-closed)

[exits]
trail_percent = 5.0         # (trailing-stop giveback, %)
runner_trail_percent = 20.0 # (runner wide trail, %)

[gate]
gate_window_secs = 10       # (watchlist window)
gate_min_buyers = 5         # (buyer-quorum threshold)
gate_poll_ms = 2000         # (poll cadence)
dev_min_age_secs = 3600     # (min deployer wallet age)
dev_max_prior_launches = 1  # (tolerated serial launches)
dev_screen_sample_txs = 20  # (wallet history sample)
dev_sig_limit = 100         # (history scan cap)

[risk]
strategy_mode = "adaptive"  # (adaptive|scalp|runner)
force_exit_regime = ""  # (empty = LLM selects; fixed baseline for evaluation, or the LLM kill path: conservative|momentum|liquidity_sensitive)
runner_moonbag_percent = 20.0  # (runner share kept, 0-100)
inference_endpoint = "http://127.0.0.1:11434/v1/chat/completions"  # 
inference_model = "llama3.2:1b"  #
inference_fallback = "heuristic"  # (heuristic|pass|defer; pass skips entries on any inference failure, defer re-watches and retries the model on a later confirm — never a hotter fallback)
inference_defer_max_n = 1  # confirm-level defers per signal with policy=defer; 0 = off, exhausted defers land on heuristic
inference_timeout_ms = 300  # (raising past 300ms costs entry price)
inference_headers = ""  # (optional extra headers, comma-separated when more than one: "Name: value, Other: value", e.g. "x-opencode-session: sess-123, X-Custom-Flag: fast"; empty = none; auth never here — INFERENCE_API_KEY stays in env)
# INFERENCE_API_KEY stays in env (bearer token; secret, never written here).

[execution]
slippage_bps = 200          # (static buy guard / sell floor)
slippage_vol_window_secs = 0     # (0 = adaptive off)
slippage_vol_extra_cap_bps = 0   # (0 = adaptive off)
priority_fee_micro_lamports = 0  # (0 = off; set nonzero for congestion)
compute_unit_limit = 0      # (0 = default)
jito_tip_lamports = 100000  # (per-bundle tip; 0 never lands)
jito_endpoint = "https://mainnet.block-engine.jito.wtf"  # 
sim_strict = false          # (unproven sims refuse; required live)

[alerts]
discord_alerts = true       # (trade cards via Hermes CLI)
discord_channel = ""        # (empty = host default)
alert_diverged_warn_n = 3   # (0 = off; warn at >= 3 paper-without-cover mints; log-only, never cards)
alert_risk_fallback_warn_pct = 50  # (0 = off; warn at >= 50% heuristic decisions; log-only, never cards)
alert_giveup_burst_n = 3    # (0 = off; warn at >= 3 GiveUps per window; log-only, never cards)
alert_giveup_window_secs = 3600  # (trailing window, secs)
"#;

/// Default config path: `~/.pump-sentry/config.toml`.
pub fn default_config_path() -> PathBuf {
    let mut dir = default_state_dir();
    dir.push(CONFIG_FILE_NAME);
    dir
}

/// State dir (`~/.pump-sentry`), `HOME`-rooted like pid/log. When
/// `HOME` is unset there is deliberately NO `/tmp` fallback: a
/// world-writable shared dir is a predictable-path/symlink risk for
/// the config template and everything derived from it. Fall back to
/// the CWD-relative `logs/` dir instead (same as the daemon log
/// fallback in `cli`), or set `CONFIG_FILE` explicitly.
fn default_state_dir() -> PathBuf {
    crate::cli::default_state_dir().unwrap_or_else(|| PathBuf::from("logs"))
}

/// Config path from `CONFIG_FILE`, else the state-dir default.
pub fn resolve_config_path() -> PathBuf {
    if let Ok(v) = std::env::var(CONFIG_FILE_ENV) {
        let v = v.trim();
        if v.is_empty() {
            return default_config_path();
        }
        if let Some(rest) = v.strip_prefix("~/").or_else(|| (v == "~").then_some(""))
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(rest);
        }
        return PathBuf::from(v);
    }
    default_config_path()
}

/// Write the categorized template when the file is missing (creating
/// parent dirs). Returns true when it wrote. Existing files are never
/// touched — operator edits are sacred. New files are owner-only
/// (`0600` on unix): the file holds no secrets by construction, but an
/// operator-pasted `?api-key=` RPC URL would be one, so restrictive
/// from birth costs nothing.
pub fn ensure_config_file(path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path)?;
    use std::io::Write;
    file.write_all(DEFAULT_CONFIG_TOML.as_bytes())?;
    Ok(true)
}

/// Parse the file into a TOML table. `None` when missing (the caller
/// ensures first; a missing file simply means env/defaults rule).
pub fn read_table(path: &Path) -> Result<Option<toml::Table>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Io(path.display().to_string(), e))?;
    let table: toml::Table = raw.parse().map_err(|e: toml::de::Error| {
        ConfigError::Parse(path.display().to_string(), e.to_string())
    })?;
    Ok(Some(table))
}

/// Flatten the TOML table onto flat keys. The file is the ONLY
/// source for tunables — no environment merge. Returns the map plus
/// typo/unknown-key notes for `warnings()`: unknown sections/keys
/// and non-scalar values never fail the boot, but they must surface
/// loudly instead of being silently ignored.
pub fn overlay(table: Option<&toml::Table>) -> (HashMap<String, String>, Vec<String>) {
    let mut merged = HashMap::new();
    let mut notes = Vec::new();
    if let Some(table) = table {
        let mut known: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
        for (section, key, flat) in KEYMAP {
            known.insert((section, key));
            let value = table
                .get(*section)
                .and_then(|v| v.as_table())
                .and_then(|t| t.get(*key));
            match cell_string(value) {
                Cell::Absent | Cell::Empty => {} // unset: built-in default rules
                Cell::Set(s) => {
                    merged.insert(flat.to_string(), s);
                }
                Cell::Bad => {
                    notes.push(format!(
                        "config [{section}].{key} ignored (want a single number/string/bool)"
                    ));
                }
            }
        }
        // Unknown sections/keys: operator typos must surface.
        for (section, values) in table.iter() {
            report_unknown_keys(section, values, &known, &mut notes);
        }
    }
    (merged, notes)
}

/// One TOML cell: absent, empty-unset, set, or non-scalar.
enum Cell {
    Absent,
    Empty,
    Set(String),
    Bad,
}

/// Scalar to string. Empty strings count as unset (the shipped
/// template uses them as placeholders); non-scalars report instead of
/// smuggling a value into the map.
fn cell_string(value: Option<&toml::Value>) -> Cell {
    let v = match value {
        None => return Cell::Absent,
        Some(v) => v,
    };
    match v {
        toml::Value::String(s) if s.trim().is_empty() => Cell::Empty,
        toml::Value::String(s) => Cell::Set(s.clone()),
        toml::Value::Integer(i) => Cell::Set(i.to_string()),
        toml::Value::Float(f) => Cell::Set(float_string(*f)),
        toml::Value::Boolean(b) => Cell::Set(b.to_string()),
        _ => Cell::Bad,
    }
}

/// Shortest float rendering that parses back (`get_float` uses
/// `str::parse`, so Rust `Display` round-trips exactly).
fn float_string(f: f64) -> String {
    format!("{f}")
}

fn report_unknown_keys(
    section: &str,
    values: &toml::Value,
    known: &std::collections::HashSet<(&str, &str)>,
    notes: &mut Vec<String>,
) {
    let table = match values.as_table() {
        Some(t) => t,
        None => {
            notes.push(format!(
                "config [{section}] ignored (want a [section] table, found a single value)"
            ));
            return;
        }
    };
    for key in table.keys() {
        if !known.contains(&(section, key.as_str())) {
            notes.push(format!(
                "config [{section}].{key} unknown; ignoring (check for typos)"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(toml: &str) -> toml::Table {
        toml.parse().expect("test toml parses")
    }

    #[test]
    fn shipped_template_covers_every_key() {
        // Template and KEYMAP must agree: a key missing from either is
        // a silent default or a dead comment.
        let t: toml::Table = DEFAULT_CONFIG_TOML.parse().expect("template parses");
        for (section, key, _) in KEYMAP {
            let v = t
                .get(*section)
                .and_then(|s| s.as_table())
                .and_then(|s| s.get(*key));
            assert!(v.is_some(), "template missing [{section}].{key}");
        }
    }

    #[test]
    fn no_shared_tmp_fallback_without_home() {
        // Without HOME (and no CONFIG_FILE override) the config path
        // must never land in world-writable /tmp: a predictable shared
        // dir is a symlink risk for the template and everything read
        // from it.
        let _guard = crate::test_env::scoped(&[("HOME", None), ("CONFIG_FILE", None)]);
        let path = resolve_config_path();
        assert!(
            !path.starts_with("/tmp"),
            "no /tmp fallback, got {}",
            path.display()
        );
    }

    #[test]
    fn overlay_reads_file_only() {
        let t = table("[trading]\nnotional_usd = 42.0\n");
        let (m, notes) = overlay(Some(&t));
        assert_eq!(m.get("NOTIONAL_USD").map(String::as_str), Some("42"));
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn overlay_skips_empties_and_reports_unknowns() {
        let t = table(
            "[network]\nmainnet_rpc_url = \"\"\n[trading]\nnotional_usd = [1]\n[nope]\nx = 1\n[gate]\ngate_window_sec = 5\n",
        );
        let (m, notes) = overlay(Some(&t));
        assert!(!m.contains_key("MAINNET_RPC_URL"), "empty must stay unset");
        assert!(
            !m.contains_key("NOTIONAL_USD"),
            "non-scalar must not enter the map"
        );
        let joined = notes.join("\n");
        assert!(
            joined.contains("[trading].notional_usd ignored"),
            "{joined}"
        );
        assert!(joined.contains("[nope].x unknown"), "{joined}");
        assert!(
            joined.contains("[gate].gate_window_sec unknown"),
            "{joined}"
        );
    }

    #[test]
    fn overlay_without_file_is_empty() {
        let (m, notes) = overlay(None);
        assert!(m.is_empty());
        assert!(notes.is_empty());
    }

    #[test]
    fn read_table_handles_missing_and_broken() {
        let dir = std::env::temp_dir().join(format!(
            "cfg-read-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let missing = dir.join("nope.toml");
        assert!(read_table(&missing).expect("missing").is_none());
        std::fs::create_dir_all(&dir).expect("mkdir");
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "[trading\nnotional_usd = ").expect("write");
        let err = read_table(&bad).expect_err("must fail");
        assert!(format!("{err}").contains("bad.toml"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_writes_once_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!(
            "cfg-ensure-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("sub").join("config.toml");
        assert!(ensure_config_file(&path).expect("create"));
        assert!(!ensure_config_file(&path).expect("second call"));
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("[trading]"), "template content");
        std::fs::write(&path, "custom = true\n").expect("overwrite");
        assert!(!ensure_config_file(&path).expect("third call"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "custom = true\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn new_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "cfg-perm-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("config.toml");
        assert!(ensure_config_file(&path).expect("create"));
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode={mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
