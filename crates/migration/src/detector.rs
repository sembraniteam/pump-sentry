//! Graduation detection and migration classification (skill
//! `solana-raydium-migration`).

use curve::{BONDING_CURVE_MIN_LEN, COMPLETE_FLAG_OFFSET};
use solana_pubkey::Pubkey;
use thiserror::Error;

pub use ids::{
    PUMP_MIGRATE_DISCRIMINATOR, PUMP_MIGRATION_AUTHORITY, PUMP_PROGRAM_ID, RAYDIUM_V4_PROGRAM_ID,
};

/// PumpSwap migration marker in program logs (current canonical flow;
/// verified live: `Instruction: MigrateV2` with no authority account).
/// Matched case-insensitively: log capitalisation varies across RPC
/// nodes and program versions, and an exact-case miss would drop a real
/// migration. Note this is a *weak* signal on its own (any program can
/// print the word); it only counts together with the pump program or
/// migration authority among account keys (see `classify_migration`).
pub const PUMP_MIGRATE_V2_LOG: &str = "migratev2";
/// Raydium pool-init marker in program logs (matched case-insensitively
/// for the same reason; likewise gated on the Raydium program key).
pub const RAYDIUM_INITIALIZE2_LOG: &str = "initialize2";
/// Real SOL collected at graduation (~84 SOL, skill constant).
pub const GRADUATION_REAL_SOL_LAMPORTS: u64 = 84_000_000_000;

/// Migration failures.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum MigrationError {
    /// Account data shorter than expected (`{0}` bytes).
    #[error("account data too short: {0} bytes")]
    DataTooShort(usize),
    /// Caller handed a non-migration transaction to the pool parser.
    #[error("no migration detected for this transaction")]
    NotAMigration,
}

/// Where migrated liquidity lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationDestination {
    /// Raydium AMM V4 pool.
    RaydiumV4,
    /// PumpSwap pool via the `migrate` instruction.
    PumpSwap,
}

/// Graduation + migration detector. Stateless and cheap to construct.
#[derive(Debug, Clone)]
pub struct MigrationMonitor {
    pump_program: Pubkey,
    pump_migration_authority: Pubkey,
    raydium_v4: Pubkey,
}

impl Default for MigrationMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrationMonitor {
    /// Monitor with the canonical program addresses.
    pub fn new() -> Self {
        Self {
            pump_program: PUMP_PROGRAM_ID,
            pump_migration_authority: PUMP_MIGRATION_AUTHORITY,
            raydium_v4: RAYDIUM_V4_PROGRAM_ID,
        }
    }

    /// True when the bonding-curve `complete` flag (byte 48) is set.
    /// The discriminator is checked first: byte 48 on a non-curve
    /// account (pool, token) must never read as graduation. Any nonzero
    /// byte counts (matches the curve parser); short buffers safely
    /// report `false`. Never panics.
    pub fn is_curve_graduated(account_data: &[u8]) -> bool {
        if account_data.len() < BONDING_CURVE_MIN_LEN {
            return false;
        }
        if account_data[..8] != curve::BONDING_CURVE_DISCRIMINATOR {
            return false;
        }
        account_data[COMPLETE_FLAG_OFFSET] != 0
    }

    /// Graduation progress in percent from real SOL reserves,
    /// clamped to `0.0..=100.0`. Non-finite inputs map to `0.0`.
    pub fn calculate_progress(real_sol_lamports: u64) -> f64 {
        let progress = real_sol_lamports as f64 / GRADUATION_REAL_SOL_LAMPORTS as f64 * 100.0;
        if progress.is_finite() {
            progress.clamp(0.0, 100.0)
        } else {
            0.0
        }
    }

    /// Classify a migration transaction.
    ///
    /// Live-data correction (verified against 25 real authority
    /// transactions): modern migrations omit the migration authority, so
    /// it is a supporting signal, not a gate. Rules, in order:
    /// 1. Raydium V4: Raydium program + `initialize2` log, with the pump
    ///    program or the migration authority also present, AND the watched
    ///    curve or expected base mint among the keys (a shared tx carrying
    ///    an unrelated Raydium init must not false-positive).
    /// 2. PumpSwap: `migrate` discriminator or `MigrateV2` log, with the
    ///    pump program or the migration authority also present, AND the
    ///    same key linkage.
    ///    `None` otherwise (ordinary pump swaps never match).
    pub fn classify_migration(
        &self,
        account_keys: &[Pubkey],
        instructions_data: &[Vec<u8>],
        log_messages: &[String],
        curve: &Pubkey,
        expected_base_mint: &Pubkey,
    ) -> Option<MigrationDestination> {
        let mentions_authority = account_keys
            .iter()
            .any(|k| k == &self.pump_migration_authority);
        let mentions_pump = account_keys.iter().any(|k| k == &self.pump_program);
        if !mentions_authority && !mentions_pump {
            return None;
        }
        // Linkage: the migration must touch what we watch.
        let linked = account_keys
            .iter()
            .any(|k| k == curve || k == expected_base_mint);
        if !linked {
            return None;
        }

        // 1. Raydium AMM V4 initialization check.
        let mentions_raydium = account_keys.iter().any(|k| k == &self.raydium_v4);
        let has_raydium_log = log_messages
            .iter()
            .any(|msg| msg.to_ascii_lowercase().contains(RAYDIUM_INITIALIZE2_LOG));
        if mentions_raydium && has_raydium_log {
            return Some(MigrationDestination::RaydiumV4);
        }

        // 2. PumpSwap migration: legacy instruction or current log marker.
        let has_pump_migrate_ix = instructions_data
            .iter()
            .any(|ix| ix.len() >= 8 && ix[..8] == PUMP_MIGRATE_DISCRIMINATOR);
        let has_migrate_v2_log = log_messages
            .iter()
            .any(|msg| msg.to_ascii_lowercase().contains(PUMP_MIGRATE_V2_LOG));
        if has_pump_migrate_ix || has_migrate_v2_log {
            return Some(MigrationDestination::PumpSwap);
        }

        None
    }

    /// Pump.fun Program ID under observation.
    pub fn pump_program(&self) -> Pubkey {
        self.pump_program
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graduated_data() -> Vec<u8> {
        let mut data = vec![0x17u8, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60];
        data.extend_from_slice(&[0u8; 40]);
        data.push(1);
        data
    }

    fn curve_key() -> Pubkey {
        Pubkey::new_from_array([11; 32])
    }

    fn base_key() -> Pubkey {
        Pubkey::new_from_array([12; 32])
    }

    #[test]
    fn graduation_flag_read() {
        assert!(MigrationMonitor::is_curve_graduated(&graduated_data()));
        let mut live = graduated_data();
        live[COMPLETE_FLAG_OFFSET] = 0;
        assert!(!MigrationMonitor::is_curve_graduated(&live));
        // Any nonzero byte is graduation (matches the curve parser).
        live[COMPLETE_FLAG_OFFSET] = 2;
        assert!(MigrationMonitor::is_curve_graduated(&live));
        assert!(!MigrationMonitor::is_curve_graduated(&[]));
        assert!(!MigrationMonitor::is_curve_graduated(&[0u8; 48]));
    }

    #[test]
    fn graduation_rejects_foreign_discriminator() {
        // Byte 48 set on a non-curve account must not read as graduation.
        let mut foreign = vec![0xFFu8; 49];
        foreign[COMPLETE_FLAG_OFFSET] = 1;
        assert!(!MigrationMonitor::is_curve_graduated(&foreign));
    }

    #[test]
    fn progress_clamps() {
        assert_eq!(MigrationMonitor::calculate_progress(0), 0.0);
        assert_eq!(
            MigrationMonitor::calculate_progress(GRADUATION_REAL_SOL_LAMPORTS / 2),
            50.0
        );
        assert_eq!(
            MigrationMonitor::calculate_progress(GRADUATION_REAL_SOL_LAMPORTS),
            100.0
        );
        assert_eq!(MigrationMonitor::calculate_progress(u64::MAX), 100.0);
    }

    #[test]
    fn classifies_raydium_migration() {
        let monitor = MigrationMonitor::new();
        let keys = vec![
            PUMP_MIGRATION_AUTHORITY,
            RAYDIUM_V4_PROGRAM_ID,
            curve_key(),
        ];
        let logs = vec!["Program log: initialize2: InitializeInstruction2".to_string()];
        assert_eq!(
            monitor.classify_migration(&keys, &[], &logs, &curve_key(), &base_key()),
            Some(MigrationDestination::RaydiumV4)
        );
    }

    #[test]
    fn classifies_pumpswap_migration() {
        let monitor = MigrationMonitor::new();
        let keys = vec![PUMP_MIGRATION_AUTHORITY, PUMP_PROGRAM_ID, base_key()];
        let mut ix = PUMP_MIGRATE_DISCRIMINATOR.to_vec();
        ix.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            monitor.classify_migration(&keys, &[ix], &[], &curve_key(), &base_key()),
            Some(MigrationDestination::PumpSwap)
        );
    }

    #[test]
    fn classifies_migrate_v2_without_authority() {
        // Live shape: pump program + MigrateV2 log, authority absent.
        let monitor = MigrationMonitor::new();
        let keys = vec![PUMP_PROGRAM_ID, curve_key()];
        let logs = vec!["Program log: Instruction: MigrateV2".to_string()];
        assert_eq!(
            monitor.classify_migration(&keys, &[], &logs, &curve_key(), &base_key()),
            Some(MigrationDestination::PumpSwap)
        );
    }

    #[test]
    fn unlinked_markers_do_not_match() {
        // All signals present but neither the watched curve nor the base
        // mint is among the keys (shared tx with an unrelated init):
        // no classification, so the hunt keeps scanning.
        let monitor = MigrationMonitor::new();
        let raydium_keys = vec![PUMP_MIGRATION_AUTHORITY, RAYDIUM_V4_PROGRAM_ID];
        let logs = vec!["Program log: initialize2: InitializeInstruction2".to_string()];
        assert_eq!(
            monitor.classify_migration(&raydium_keys, &[], &logs, &curve_key(), &base_key()),
            None
        );
        let mut ix = PUMP_MIGRATE_DISCRIMINATOR.to_vec();
        ix.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            monitor.classify_migration(&raydium_keys, &[ix], &[], &curve_key(), &base_key()),
            None
        );
    }

    #[test]
    fn log_markers_match_case_insensitively() {
        // RPC nodes/program versions vary capitalisation; a real migration
        // must not be dropped over case.
        let monitor = MigrationMonitor::new();
        let raydium_keys = vec![PUMP_MIGRATION_AUTHORITY, RAYDIUM_V4_PROGRAM_ID, curve_key()];
        for log in ["INITIALIZE2", "Initialize2", "initialize2"] {
            assert_eq!(
                monitor.classify_migration(&raydium_keys, &[], &[log.to_string()], &curve_key(), &base_key()),
                Some(MigrationDestination::RaydiumV4),
                "log={log}"
            );
        }
        let pump_keys = vec![PUMP_PROGRAM_ID, base_key()];
        for log in ["MIGRATEV2", "MigrateV2", "migratev2"] {
            assert_eq!(
                monitor.classify_migration(&pump_keys, &[], &[log.to_string()], &curve_key(), &base_key()),
                Some(MigrationDestination::PumpSwap),
                "log={log}"
            );
        }
        // Substring alone is not enough: the program/authority key must
        // also be present (weak-signal gating).
        assert_eq!(
            monitor.classify_migration(&[], &[], &["initialize2".to_string()], &curve_key(), &base_key()),
            None
        );
    }

    #[test]
    fn ordinary_swaps_never_match() {
        // Plain buy: pump program + buy discriminator, no markers.
        let monitor = MigrationMonitor::new();
        let keys = vec![PUMP_PROGRAM_ID, PUMP_MIGRATION_AUTHORITY, curve_key()];
        let mut ix = vec![102u8, 6, 61, 18, 1, 218, 235, 234];
        ix.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            monitor.classify_migration(&keys, &[ix], &[], &curve_key(), &base_key()),
            None
        );
        // Unrelated program entirely.
        assert_eq!(
            monitor.classify_migration(&[], &[], &[], &curve_key(), &base_key()),
            None
        );
    }

    #[test]
    fn rejects_non_migrations() {
        let monitor = MigrationMonitor::new();
        let no_authority = vec![RAYDIUM_V4_PROGRAM_ID, curve_key()];
        let logs = vec!["Program log: initialize2: InitializeInstruction2".to_string()];
        assert_eq!(
            monitor.classify_migration(&no_authority, &[], &logs, &curve_key(), &base_key()),
            None
        );

        // Raydium program without the init log is not a migration.
        let keys = vec![PUMP_MIGRATION_AUTHORITY, RAYDIUM_V4_PROGRAM_ID, curve_key()];
        assert_eq!(
            monitor.classify_migration(&keys, &[], &[], &curve_key(), &base_key()),
            None
        );

        // Init log without the program is not a migration.
        let keys = vec![PUMP_MIGRATION_AUTHORITY, PUMP_PROGRAM_ID, curve_key()];
        assert_eq!(
            monitor.classify_migration(&keys, &[], &logs, &curve_key(), &base_key()),
            None
        );

        // Short instruction data cannot match the discriminator.
        let keys = vec![PUMP_MIGRATION_AUTHORITY, PUMP_PROGRAM_ID, base_key()];
        assert_eq!(
            monitor.classify_migration(&keys, &[vec![155u8; 4]], &[], &curve_key(), &base_key()),
            None
        );
    }
}
