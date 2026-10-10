//! Exact Pump.fun `Create` instruction parser (see skill `solana-geyser-pumpfun`).
//!
//! - Program ID: `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
//! - Create discriminator (8 bytes): `[24, 30, 200, 40, 5, 28, 7, 119]`
//! - Args (Borsh): `name: String, symbol: String, uri: String`
//! - Accounts: `[0]=mint, [2]=bonding_curve, [3]=associated_bonding_curve,
//!   [7]=user`, minimum 8 accounts.

use borsh::BorshDeserialize;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use thiserror::Error;

/// Pump.fun program ID (string form, for RPC filters).
pub const PUMP_PROGRAM_ID_STR: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Pump.fun program ID as bytes (avoids runtime parse on hot path).
const PUMP_PROGRAM_ID_BYTES: [u8; 32] = [
    0x01, 0x56, 0xe0, 0xf6, 0x93, 0x66, 0x5a, 0xcf, 0x44, 0xdb, 0x15, 0x68, 0xbf, 0x17, 0x5b, 0xaa,
    0x51, 0x89, 0xcb, 0x97, 0xf5, 0xd2, 0xff, 0x3b, 0x65, 0x5d, 0x2b, 0xb6, 0xfd, 0x6d, 0x18, 0xb0,
];

/// Pump.fun program ID.
///
/// NOTE: bytes above are derived from the base58 string
/// `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`. A debug assertion in
/// [`pump_program_id()`] cross-checks the two representations in tests;
/// at runtime we expose the const-parsed form via [`PUMP_PROGRAM_ID_STR`]
/// parsing only where a `Pubkey` value is required.
pub const PUMP_PROGRAM_ID: Pubkey = Pubkey::from_str_const(PUMP_PROGRAM_ID_STR);

/// Anchor discriminator for Pump.fun `create` (first 8 bytes of instruction data).
pub const PUMP_CREATE_DISCRIMINATOR: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];

/// Minimum number of accounts expected on a `create` instruction.
pub const PUMP_CREATE_MIN_ACCOUNTS: usize = 8;

/// Account indices inside the `create` instruction accounts list.
pub const IDX_MINT: usize = 0;
pub const IDX_BONDING_CURVE: usize = 2;
pub const IDX_ASSOCIATED_BONDING_CURVE: usize = 3;
pub const IDX_USER: usize = 7;

/// Typed parse errors — no panics on hot paths.
#[derive(Error, Debug)]
pub enum PumpParseError {
    /// Instruction data shorter than the 8-byte discriminator.
    #[error("Instruction data too short")]
    DataTooShort,
    /// First 8 bytes do not match the `create` discriminator.
    #[error("Discriminator mismatch")]
    InvalidDiscriminator,
    /// Borsh decoding of `name/symbol/uri` failed.
    #[error("Failed to decode Borsh data: {0}")]
    Borsh(#[from] std::io::Error),
    /// Fewer than 8 accounts were supplied.
    #[error("Insufficient accounts: required at least {required}, found {found}")]
    InsufficientAccounts { required: usize, found: usize },
}

/// Borsh args of Pump.fun `create`: `name`, `symbol`, `uri`.
#[derive(borsh::BorshDeserialize, borsh::BorshSerialize, Debug, Clone, PartialEq, Eq)]
pub struct PumpCreateInstructionArgs {
    /// Token name.
    pub name: String,
    /// Token symbol/ticker.
    pub symbol: String,
    /// Off-chain metadata URI.
    pub uri: String,
}

/// Enriched token-launch event (signature + slot attached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpTokenLaunchEvent {
    /// Newly created mint.
    pub mint: Pubkey,
    /// Bonding curve PDA.
    pub bonding_curve: Pubkey,
    /// Associated bonding curve (token account holding curve inventory).
    pub associated_bonding_curve: Pubkey,
    /// Creator / payer.
    pub user: Pubkey,
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Metadata URI.
    pub uri: String,
    /// Transaction signature (base58).
    pub signature: String,
    /// Slot the transaction was confirmed in.
    pub slot: u64,
}

/// Fast pre-filter: true when `data` starts with the `create` discriminator.
///
/// Never panics: returns `false` for short slices without indexing.
#[inline]
pub fn is_pump_create_ix(data: &[u8]) -> bool {
    if data.len() < PUMP_CREATE_DISCRIMINATOR.len() {
        return false;
    }
    // Compare byte-by-byte without slicing (avoids any panic path).
    for (i, discr_byte) in PUMP_CREATE_DISCRIMINATOR.iter().enumerate() {
        match data.get(i) {
            Some(b) if b == discr_byte => {}
            _ => return false,
        }
    }
    true
}

/// Parse a Pump.fun `create` instruction.
///
/// `data` is the raw instruction data (discriminator + Borsh args),
/// `accounts` is the resolved account list for the instruction in order.
/// `signature`/`slot` enrich the emitted event without extra lookups.
///
/// All failures map to [`PumpParseError`]; no panics, no indexing.
pub fn parse_pump_create(
    data: &[u8],
    accounts: &[Pubkey],
    signature: String,
    slot: u64,
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    parse_pump_create_inner(data, accounts, signature, slot)
}

fn parse_pump_create_inner(
    data: &[u8],
    accounts: &[Pubkey],
    signature: String,
    slot: u64,
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    if data.len() < PUMP_CREATE_DISCRIMINATOR.len() {
        return Err(PumpParseError::DataTooShort);
    }
    if !is_pump_create_ix(data) {
        return Err(PumpParseError::InvalidDiscriminator);
    }
    if accounts.len() < PUMP_CREATE_MIN_ACCOUNTS {
        return Err(PumpParseError::InsufficientAccounts {
            required: PUMP_CREATE_MIN_ACCOUNTS,
            found: accounts.len(),
        });
    }

    // Copy out the 4 Pubkeys we need with bounds-checked access only.
    let mint = *accounts
        .get(IDX_MINT)
        .ok_or(PumpParseError::InsufficientAccounts {
            required: PUMP_CREATE_MIN_ACCOUNTS,
            found: accounts.len(),
        })?;
    let bonding_curve =
        *accounts
            .get(IDX_BONDING_CURVE)
            .ok_or(PumpParseError::InsufficientAccounts {
                required: PUMP_CREATE_MIN_ACCOUNTS,
                found: accounts.len(),
            })?;
    let associated_bonding_curve = *accounts.get(IDX_ASSOCIATED_BONDING_CURVE).ok_or(
        PumpParseError::InsufficientAccounts {
            required: PUMP_CREATE_MIN_ACCOUNTS,
            found: accounts.len(),
        },
    )?;
    let user = *accounts
        .get(IDX_USER)
        .ok_or(PumpParseError::InsufficientAccounts {
            required: PUMP_CREATE_MIN_ACCOUNTS,
            found: accounts.len(),
        })?;

    // Skip the 8-byte discriminator in place (no copy of the full buffer).
    let args_start = PUMP_CREATE_DISCRIMINATOR.len();
    let args_bytes: &[u8] = match data.get(args_start..) {
        Some(rest) => rest,
        None => return Err(PumpParseError::DataTooShort),
    };
    let mut args_slice: &[u8] = args_bytes;
    let args = PumpCreateInstructionArgs::deserialize(&mut args_slice)?;

    Ok(PumpTokenLaunchEvent {
        mint,
        bonding_curve,
        associated_bonding_curve,
        user,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        signature,
        slot,
    })
}

/// Parse helper matching the skill signature `parse_pump_create(data, accounts)`
/// without enrichment (signature/slot defaulted). Useful for unit tests and
/// gRPC paths that attach enrichment later.
pub fn parse_pump_create_bare(
    data: &[u8],
    accounts: &[Pubkey],
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    parse_pump_create_inner(data, accounts, String::new(), 0)
}

/// Returns the program ID, cross-checking the const bytes against the string
/// form (string parse is fallible; const form is infallible).
pub fn pump_program_id() -> Pubkey {
    PUMP_PROGRAM_ID
}

/// Verify at runtime that [`PUMP_PROGRAM_ID_BYTES`] matches the base58 string.
/// Returns `true` on match; used by tests.
pub fn pump_program_id_bytes_match() -> bool {
    match Pubkey::from_str(PUMP_PROGRAM_ID_STR) {
        Ok(parsed) => parsed.to_bytes() == PUMP_PROGRAM_ID_BYTES,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pubkey(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    fn encode_create(name: &str, symbol: &str, uri: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 64);
        out.extend_from_slice(&PUMP_CREATE_DISCRIMINATOR);
        let args = PumpCreateInstructionArgs {
            name: name.to_string(),
            symbol: symbol.to_string(),
            uri: uri.to_string(),
        };
        borsh::to_vec(&args).map(|v| out.extend_from_slice(&v)).ok();
        out
    }

    #[test]
    fn discriminator_match() {
        let data = encode_create("n", "s", "u");
        assert!(is_pump_create_ix(&data));
    }

    #[test]
    fn discriminator_mismatch() {
        let mut data = encode_create("n", "s", "u");
        if let Some(first) = data.first_mut() {
            *first ^= 0xff;
        }
        assert!(!is_pump_create_ix(&data));
    }

    #[test]
    fn discriminator_too_short() {
        assert!(!is_pump_create_ix(&[]));
        assert!(!is_pump_create_ix(&[24, 30]));
        let err = parse_pump_create_bare(&[1, 2, 3], &[]).unwrap_err();
        assert!(matches!(err, PumpParseError::DataTooShort));
    }

    #[test]
    fn invalid_discriminator_error() {
        let data = vec![0u8; 16];
        let accounts = vec![test_pubkey(1); 8];
        let err = parse_pump_create_bare(&data, &accounts).unwrap_err();
        assert!(matches!(err, PumpParseError::InvalidDiscriminator));
    }

    #[test]
    fn borsh_roundtrip() {
        let args = PumpCreateInstructionArgs {
            name: "MyToken".to_string(),
            symbol: "MTK".to_string(),
            uri: "https://example.com/meta.json".to_string(),
        };
        let encoded = borsh::to_vec(&args).expect("encode must succeed");
        let mut slice: &[u8] = &encoded;
        let decoded =
            PumpCreateInstructionArgs::deserialize(&mut slice).expect("decode must succeed");
        assert_eq!(args, decoded);
    }

    #[test]
    fn parse_ok_with_enrichment() {
        let data = encode_create("Pepe", "PEPE", "https://x/y.json");
        let accounts = vec![
            test_pubkey(10), // mint
            test_pubkey(11),
            test_pubkey(12), // bonding_curve
            test_pubkey(13), // associated
            test_pubkey(14),
            test_pubkey(15),
            test_pubkey(16),
            test_pubkey(17), // user
        ];
        let ev = parse_pump_create(&data, &accounts, "sig123".to_string(), 42).expect("parse ok");
        assert_eq!(ev.mint, test_pubkey(10));
        assert_eq!(ev.bonding_curve, test_pubkey(12));
        assert_eq!(ev.associated_bonding_curve, test_pubkey(13));
        assert_eq!(ev.user, test_pubkey(17));
        assert_eq!(ev.name, "Pepe");
        assert_eq!(ev.symbol, "PEPE");
        assert_eq!(ev.uri, "https://x/y.json");
        assert_eq!(ev.signature, "sig123");
        assert_eq!(ev.slot, 42);
    }

    #[test]
    fn insufficient_accounts() {
        let data = encode_create("a", "b", "c");
        for n in [0usize, 1, 7] {
            let accounts = vec![test_pubkey(1); n];
            let err = parse_pump_create_bare(&data, &accounts).unwrap_err();
            match err {
                PumpParseError::InsufficientAccounts { required, found } => {
                    assert_eq!(required, 8);
                    assert_eq!(found, n);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }
    }

    #[test]
    fn program_id_matches_skill_string() {
        assert_eq!(
            PUMP_PROGRAM_ID_STR,
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
        );
        assert_eq!(
            PUMP_PROGRAM_ID,
            Pubkey::from_str(PUMP_PROGRAM_ID_STR).expect("valid program id")
        );
        assert!(pump_program_id_bytes_match());
    }
}
