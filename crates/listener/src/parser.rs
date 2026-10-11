//! Exact Pump.fun `Create` instruction parser (see skill `solana-geyser-pumpfun`).
//!
//! - Program ID: `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
//! - Create discriminator (8 bytes): `[24, 30, 200, 40, 5, 28, 7, 119]`
//! - Args (Borsh): `name: String, symbol: String, uri: String`
//! - Accounts: `[0]=mint, [2]=bonding_curve, [3]=associated_bonding_curve,
//!   [7]=user`, minimum 8 accounts.

use borsh::BorshDeserialize;
use solana_sdk::pubkey::Pubkey;
#[cfg(test)]
use std::str::FromStr;
use thiserror::Error;

/// Canonical addresses and discriminators live in [`ids`]; re-exported
/// here so the whole workspace shares one definition (a program upgrade
/// edits exactly one file).
pub use ids::{
    PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_CREATE_DISCRIMINATOR,
    PUMP_CREATE_V2_DISCRIMINATOR, PUMP_MIGRATE_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR,
    PUMP_SELL_DISCRIMINATOR, PUMP_SELL_V2_DISCRIMINATOR,
};

/// Minimum number of accounts expected on a `create` instruction.
pub const PUMP_CREATE_MIN_ACCOUNTS: usize = 8;
/// Maximum Borsh args bytes after the discriminator (name/symbol/uri).
/// Caps hostile-prefix allocations across concurrent fetch workers.
pub const MAX_CREATE_ARGS_LEN: usize = 4096;

/// Account indices inside the `create` instruction accounts list.
pub const IDX_MINT: usize = 0;
pub const IDX_BONDING_CURVE: usize = 2;
pub const IDX_ASSOCIATED_BONDING_CURVE: usize = 3;
pub const IDX_USER: usize = 7;

/// Account indices inside the `create_v2` instruction accounts list
/// (verified against the canonical Pump.fun IDL: 14 accounts —
/// mint, mintAuthority, bondingCurve, associatedBondingCurve, global,
/// user, systemProgram, tokenProgram, associatedTokenProgram,
/// mayhemProgram, globalParams, solVault, mayhemState, mayhemTokenVault).
pub const IDX_V2_MINT: usize = 0;
pub const IDX_V2_BONDING_CURVE: usize = 2;
pub const IDX_V2_ASSOCIATED_BONDING_CURVE: usize = 3;
pub const IDX_V2_USER: usize = 5;
/// Minimum number of accounts expected on a `create_v2` instruction.
pub const PUMP_CREATE_V2_MIN_ACCOUNTS: usize = 14;

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
    /// Args section exceeds [`MAX_CREATE_ARGS_LEN`] bytes (hostile prefix).
    #[error("Create args too long: {0} bytes")]
    ArgsTooLong(usize),
    /// Name/symbol/uri is not valid UTF-8.
    #[error("Create string field is not valid UTF-8")]
    BadString,
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

/// Borsh args of Pump.fun `create_v2` as a TEST-ENCODING helper only.
///
/// WARNING — not a wire mirror: live `create_v2` bytes carry the 32-byte
/// creator IMMEDIATELY after the string triple (vault-PDA verified), so
/// a bool in this position would misattribute the creator by one byte.
/// The bool's true wire position is unmapped. Use
/// [`decode_name_symbol_uri`] + explicit tail reads for wire decoding
/// (see [`parse_pump_create_v2`]); this struct exists so tests can
/// encode IDL-shaped payloads.
#[derive(borsh::BorshDeserialize, borsh::BorshSerialize, Debug, Clone, PartialEq, Eq)]
pub struct PumpCreateV2InstructionArgs {
    /// Token name.
    pub name: String,
    /// Token symbol/ticker.
    pub symbol: String,
    /// Off-chain metadata URI.
    pub uri: String,
    /// Whether the launch uses mayhem mode.
    pub is_mayhem_mode: bool,
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
    /// Token creator: `Some` for `create_v2` (32-byte chunk after the
    /// name/symbol/uri strings — vault-PDA match verified against a real
    /// mayhem buy), `None` for legacy `create` (no creator field; the
    /// launcher-equals-creator assumption is unverified, so v1 stays on
    /// the legacy swap path rather than a guessed vault).
    pub creator: Option<Pubkey>,
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
    data.starts_with(&PUMP_CREATE_DISCRIMINATOR)
}

/// Fast pre-filter for `create_v2` launches (14-account IDL layout;
/// parsed by `parse_pump_create_v2`, never by the v1 parser).
///
/// Never panics: returns `false` for short slices without indexing.
#[inline]
pub fn is_pump_create_v2_ix(data: &[u8]) -> bool {
    data.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR)
}

/// Decode the leading Borsh `(name, symbol, uri)` triple shared by
/// `create` and `create_v2` args, advancing `cursor` past it and leaving
/// any versioned tail for the caller. One decode path for both versions:
/// Borsh `&[u8]` decoding never requires exact consumption (no
/// `try_from_slice` tail breakage), and allocation is slice-bounded on
/// top of borsh's cautious 1MB cap — a hostile length prefix fails
/// without giant allocations.
fn decode_name_symbol_uri(
    cursor: &mut &[u8],
) -> Result<PumpCreateInstructionArgs, PumpParseError> {
    PumpCreateInstructionArgs::deserialize(cursor).map_err(PumpParseError::Borsh)
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
    // Upper bound before Borsh: a hostile prefix length cannot force a
    // giant allocation across 32 concurrent workers (name/symbol/uri are
    // tiny on legitimate creates; 4 KiB is generous headroom).
    let args_start = PUMP_CREATE_DISCRIMINATOR.len();
    let args_bytes: &[u8] = match data.get(args_start..) {
        Some(rest) => rest,
        None => return Err(PumpParseError::DataTooShort),
    };
    if args_bytes.len() > MAX_CREATE_ARGS_LEN {
        return Err(PumpParseError::ArgsTooLong(args_bytes.len()));
    }
    let mut args_slice: &[u8] = args_bytes;
    let args = decode_name_symbol_uri(&mut args_slice)?;

    Ok(PumpTokenLaunchEvent {
        mint,
        bonding_curve,
        associated_bonding_curve,
        user,
        creator: None,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        signature,
        slot,
    })
}

/// Parse a Pump.fun `create_v2` instruction (14-account IDL layout; see
/// `IDX_V2_*`). Emits the same [`PumpTokenLaunchEvent`] shape — downstream
/// screening treats v1 and v2 launches identically.
///
/// All failures map to [`PumpParseError`]; no panics, no indexing.
pub fn parse_pump_create_v2(
    data: &[u8],
    accounts: &[Pubkey],
    signature: String,
    slot: u64,
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    if data.len() < PUMP_CREATE_V2_DISCRIMINATOR.len() {
        return Err(PumpParseError::DataTooShort);
    }
    if !is_pump_create_v2_ix(data) {
        return Err(PumpParseError::InvalidDiscriminator);
    }
    if accounts.len() < PUMP_CREATE_V2_MIN_ACCOUNTS {
        return Err(PumpParseError::InsufficientAccounts {
            required: PUMP_CREATE_V2_MIN_ACCOUNTS,
            found: accounts.len(),
        });
    }
    let insufficient = |found| PumpParseError::InsufficientAccounts {
        required: PUMP_CREATE_V2_MIN_ACCOUNTS,
        found,
    };
    let mint = *accounts.get(IDX_V2_MINT).ok_or(insufficient(accounts.len()))?;
    let bonding_curve = *accounts
        .get(IDX_V2_BONDING_CURVE)
        .ok_or(insufficient(accounts.len()))?;
    let associated_bonding_curve = *accounts
        .get(IDX_V2_ASSOCIATED_BONDING_CURVE)
        .ok_or(insufficient(accounts.len()))?;
    let user = *accounts.get(IDX_V2_USER).ok_or(insufficient(accounts.len()))?;

    let args_start = PUMP_CREATE_V2_DISCRIMINATOR.len();
    let args_bytes: &[u8] = match data.get(args_start..) {
        Some(rest) => rest,
        None => return Err(PumpParseError::DataTooShort),
    };
    if args_bytes.len() > MAX_CREATE_ARGS_LEN {
        return Err(PumpParseError::ArgsTooLong(args_bytes.len()));
    }
    // Decode the leading (name, symbol, uri) triple with the shared
    // helper and read the creator from the versioned tail: the FIRST 32
    // bytes after the triple. Wire-verified, not IDL-assumed — a frozen
    // mainnet create_v2 carries triple + creator[32] + 11 unknown tail
    // bytes, and the creator-vault PDA derived from exactly these bytes
    // matched a live mayhem buy's vault account. (The IDL-shaped
    // `PumpCreateV2InstructionArgs` puts a bool right after the triple
    // and must NOT be used for wire decoding — it would misattribute by
    // one byte.) Short tail → None (event stays valid, swaps fall back
    // to the legacy shape).
    let mut cursor: &[u8] = args_bytes;
    let args = decode_name_symbol_uri(&mut cursor)?;
    let name = args.name;
    let symbol = args.symbol;
    let uri = args.uri;
    let creator = cursor
        .get(..32)
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .map(Pubkey::new_from_array);

    Ok(PumpTokenLaunchEvent {
        mint,
        bonding_curve,
        associated_bonding_curve,
        user,
        creator,
        name,
        symbol,
        uri,
        signature,
        slot,
    })
}

/// Decode instruction data tolerant to RPC encoding: base58, then
/// standard base64, then URL-safe base64. Empty payloads never decode.
/// Single home for every decoder in the crate (stream, screen).
///
/// A base64 payload using only bs58-alphabet characters would mis-decode
/// as base58, so when several decodings succeed the one opening with a
/// known Pump.fun discriminator wins; otherwise the first success is
/// kept (previous behavior).
pub fn decode_ix_data(data_str: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let decodings = [
        bs58::decode(data_str).into_vec().ok(),
        base64::engine::general_purpose::STANDARD
            .decode(data_str)
            .ok(),
        base64::engine::general_purpose::URL_SAFE
            .decode(data_str)
            .ok(),
    ];
    let mut fallback: Option<Vec<u8>> = None;
    for decoded in decodings.into_iter().flatten() {
        if decoded.is_empty() {
            continue;
        }
        if starts_with_known_discriminator(&decoded) {
            return Some(decoded);
        }
        if fallback.is_none() {
            fallback = Some(decoded);
        }
    }
    fallback
}

/// True when `data` opens with any known Pump.fun discriminator
/// (create/create_v2/buy/sell/buy_v2/sell_v2/migrate).
fn starts_with_known_discriminator(data: &[u8]) -> bool {
    data.starts_with(&PUMP_CREATE_DISCRIMINATOR)
        || data.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR)
        || data.starts_with(&PUMP_BUY_DISCRIMINATOR)
        || data.starts_with(&PUMP_SELL_DISCRIMINATOR)
        || data.starts_with(&PUMP_BUY_V2_DISCRIMINATOR)
        || data.starts_with(&PUMP_SELL_V2_DISCRIMINATOR)
        || data.starts_with(&PUMP_MIGRATE_DISCRIMINATOR)
}

/// Parse helper matching the skill signature `parse_pump_create(data, accounts)`
/// without enrichment (signature/slot defaulted). Test-only.
#[cfg(test)]
pub fn parse_pump_create_bare(
    data: &[u8],
    accounts: &[Pubkey],
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    parse_pump_create_inner(data, accounts, String::new(), 0)
}

/// Verify the shared [`ids`] program ID matches its canonical string.
/// Returns `true` on match; used by tests.
#[cfg(test)]
pub fn pump_program_id_bytes_match() -> bool {
    use ids::PUMP_PROGRAM_ID as IDS_PROGRAM_ID;
    match Pubkey::from_str(PUMP_PROGRAM_ID_STR) {
        Ok(parsed) => parsed == PUMP_PROGRAM_ID && parsed == IDS_PROGRAM_ID,
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

    fn encode_create_v2(name: &str, symbol: &str, uri: &str, mayhem: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 64);
        out.extend_from_slice(&PUMP_CREATE_V2_DISCRIMINATOR);
        let args = PumpCreateV2InstructionArgs {
            name: name.to_string(),
            symbol: symbol.to_string(),
            uri: uri.to_string(),
            is_mayhem_mode: mayhem,
        };
        borsh::to_vec(&args).map(|v| out.extend_from_slice(&v)).ok();
        out
    }

    #[test]
    fn parse_v2_ok_with_idl_layout() {
        // 14-account IDL order: mint, mintAuthority, bondingCurve,
        // assocCurve, global, user, system, token, ata, mayhem, params,
        // solVault, mayhemState, mayhemVault.
        let data = encode_create_v2("Pepe", "PEPE", "https://x/y.json", false);
        assert!(is_pump_create_v2_ix(&data));
        assert!(!is_pump_create_ix(&data));
        let accounts: Vec<Pubkey> = (10u8..24).map(test_pubkey).collect();
        let ev = parse_pump_create_v2(&data, &accounts, "sig123".to_string(), 42)
            .expect("v2 parse ok");
        assert_eq!(ev.mint, test_pubkey(10));
        assert_eq!(ev.bonding_curve, test_pubkey(12));
        assert_eq!(ev.associated_bonding_curve, test_pubkey(13));
        assert_eq!(ev.user, test_pubkey(15));
        assert_eq!(ev.name, "Pepe");
        assert_eq!(ev.symbol, "PEPE");
        assert_eq!(ev.signature, "sig123");
        assert_eq!(ev.slot, 42);
    }

    #[test]
    fn parse_v2_pins_live_wire_tail() {
        // Frozen mainnet create_v2 (146-byte data): string triple, then
        // creator[32], then an 11-byte versioned tail of unknown layout.
        // The creator bytes are the same ones whose vault PDA matched a
        // live mayhem buy — first-32 attribution is evidence-backed. If
        // anyone "fixes" the parser to the IDL-struct order (bool right
        // after the triple), this test breaks loudly.
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/create_v2_mainnet.json"))
                .expect("fixture");
        let msg = &raw["result"]["transaction"]["message"];
        let keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        let pump = PUMP_PROGRAM_ID.to_string();
        for ix in msg["instructions"].as_array().expect("ixs") {
            let pi = ix["programIdIndex"].as_u64().expect("pi") as usize;
            if keys.get(pi) != Some(&pump) {
                continue;
            }
            let data = decode_ix_data(ix["data"].as_str().expect("data")).expect("decode");
            if !is_pump_create_v2_ix(&data) {
                continue;
            }
            let idx: Vec<usize> =
                serde_json::from_value(ix["accounts"].clone()).expect("accts");
            // 16-account live instruction (MIN tolerance is 14).
            assert_eq!(idx.len(), 16);
            let accounts: Vec<Pubkey> = idx
                .iter()
                .map(|i| Pubkey::from_str(&keys[*i]).expect("key"))
                .collect();
            let ev =
                parse_pump_create_v2(&data, &accounts, "sig".to_string(), 77).expect("parse");
            assert_eq!(ev.name, "makemerich");
            assert_eq!(ev.symbol, "mmrich");
            assert_eq!(
                ev.uri,
                "https://ipfs.io/ipfs/QmNsGCZiXUFWrGfDUZgs9MCV7Q8JEZxQGjUbj4Tj2oNMjh"
            );
            assert_eq!(ev.mint.to_string(), keys[idx[0]]);
            assert_eq!(ev.bonding_curve.to_string(), keys[idx[2]]);
            assert_eq!(ev.associated_bonding_curve.to_string(), keys[idx[3]]);
            assert_eq!(ev.user.to_string(), keys[idx[5]]);
            assert_eq!(
                ev.creator,
                Some(Pubkey::new_from_array([
                    181, 33, 156, 169, 71, 74, 115, 4, 142, 47, 129, 175, 116, 186, 124, 26,
                    181, 141, 124, 30, 200, 103, 36, 11, 1, 87, 83, 217, 77, 172, 67, 38,
                ]))
            );
            assert_eq!(ev.slot, 77);
            return;
        }
        panic!("no create_v2 instruction in fixture");
    }

    #[test]
    fn parse_v2_rejects_short_accounts_and_v1_data() {
        let data = encode_create_v2("a", "b", "c", true);
        let few = vec![test_pubkey(1); 13];
        assert!(matches!(
            parse_pump_create_v2(&data, &few, String::new(), 0),
            Err(PumpParseError::InsufficientAccounts { .. })
        ));
        // v1 data is not v2 (and vice versa): no cross-talk.
        let v1 = encode_create("a", "b", "c");
        let accounts = vec![test_pubkey(1); 14];
        assert!(matches!(
            parse_pump_create_v2(&v1, &accounts, String::new(), 0),
            Err(PumpParseError::InvalidDiscriminator)
        ));
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
