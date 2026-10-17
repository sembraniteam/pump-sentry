//! Pump.fun `Create` instruction parser.

use borsh::BorshDeserialize;
use solana_sdk::pubkey::Pubkey;
#[cfg(test)]
use std::str::FromStr;
use thiserror::Error;

/// Canonical addresses live in [`ids`]. Re-exported here.
pub use ids::{
    PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_BUY_V3_DISCRIMINATOR,
    PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR, PUMP_CREATE_DISCRIMINATOR,
    PUMP_CREATE_V2_DISCRIMINATOR, PUMP_MIGRATE_DISCRIMINATOR, PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR,
    PUMP_SELL_DISCRIMINATOR, PUMP_SELL_V2_DISCRIMINATOR, PUMP_SELL_V3_DISCRIMINATOR,
};

/// Min accounts on a `create` ix.
pub const PUMP_CREATE_MIN_ACCOUNTS: usize = 8;
/// Max Borsh args bytes after the discriminator (DoS cap).
pub const MAX_CREATE_ARGS_LEN: usize = 4096;

/// Account slots in a `create` ix.
pub const IDX_MINT: usize = 0;
pub const IDX_BONDING_CURVE: usize = 2;
pub const IDX_ASSOCIATED_BONDING_CURVE: usize = 3;
pub const IDX_USER: usize = 7;

/// Account slots in a `create_v2` ix (14-account layout).
pub const IDX_V2_MINT: usize = 0;
pub const IDX_V2_BONDING_CURVE: usize = 2;
pub const IDX_V2_ASSOCIATED_BONDING_CURVE: usize = 3;
/// Payer in `create_v2`. Named `CREATE`: `screen` owns the v2 swap index.
pub const IDX_CREATE_V2_USER: usize = 5;
/// Min accounts on a `create_v2` ix.
pub const PUMP_CREATE_V2_MIN_ACCOUNTS: usize = 14;

/// Parse errors. No panics on hot paths.
#[derive(Error, Debug)]
pub enum PumpParseError {
    /// Data shorter than the 8-byte discriminator.
    #[error("Instruction data too short")]
    DataTooShort,
    /// First 8 bytes are not the `create` discriminator.
    #[error("Discriminator mismatch")]
    InvalidDiscriminator,
    /// Borsh decoding of `name/symbol/uri` failed.
    #[error("Failed to decode Borsh data: {0}")]
    Borsh(#[from] std::io::Error),
    /// Args exceed [`MAX_CREATE_ARGS_LEN`].
    #[error("Create args too long: {0} bytes")]
    ArgsTooLong(usize),
    /// Fewer than 8 accounts were supplied.
    #[error("Insufficient accounts: required at least {required}, found {found}")]
    InsufficientAccounts { required: usize, found: usize },
}

/// Borsh args of `create`: `name`, `symbol`, `uri`.
#[derive(borsh::BorshDeserialize, borsh::BorshSerialize, Debug, Clone, PartialEq, Eq)]
pub struct PumpCreateInstructionArgs {
    /// Token name.
    pub name: String,
    /// Token symbol/ticker.
    pub symbol: String,
    /// Off-chain metadata URI.
    pub uri: String,
}

/// Borsh args of `create_v2`. Test encoding only.
///
/// WARNING: not a wire mirror. Live bytes carry the 32-byte creator
/// after the triple. Never use for wire decoding.
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

/// Token-launch event (signature plus slot attached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpTokenLaunchEvent {
    /// Newly created mint.
    pub mint: Pubkey,
    /// Bonding curve PDA.
    pub bonding_curve: Pubkey,
    /// Associated curve account (curve inventory).
    pub associated_bonding_curve: Pubkey,
    /// Creator / payer.
    pub user: Pubkey,
    /// Creator. `Some` for `create_v2`, `None` for `create`.
    pub creator: Option<Pubkey>,
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Metadata URI.
    pub uri: String,
    /// Transaction signature (base58).
    pub signature: String,
    /// Slot the tx was confirmed in.
    pub slot: u64,
}

/// Fast pre-filter for `create`. Safe on short slices.
#[inline]
pub fn is_pump_create_ix(data: &[u8]) -> bool {
    data.starts_with(&PUMP_CREATE_DISCRIMINATOR)
}

/// Fast pre-filter for `create_v2`. Safe on short slices.
#[inline]
pub fn is_pump_create_v2_ix(data: &[u8]) -> bool {
    data.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR)
}

/// Decode the shared `(name, symbol, uri)` triple.
/// One path for both versions. Caller reads the tail.
fn decode_name_symbol_uri(
    cursor: &mut &[u8],
) -> Result<PumpCreateInstructionArgs, PumpParseError> {
    PumpCreateInstructionArgs::deserialize(cursor).map_err(PumpParseError::Borsh)
}

/// Parse a `create` ix. No panics, no indexing.
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

    // Copy the 4 pubkeys with checked access.
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

    // Skip the discriminator in place (no full-buffer copy).
    // Cap size first: hostile prefixes can't force big allocs.
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

/// Parse a `create_v2` ix. Same event shape. No panics.
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
    let user = *accounts
        .get(IDX_CREATE_V2_USER)
        .ok_or(insufficient(accounts.len()))?;

    let args_start = PUMP_CREATE_V2_DISCRIMINATOR.len();
    let args_bytes: &[u8] = match data.get(args_start..) {
        Some(rest) => rest,
        None => return Err(PumpParseError::DataTooShort),
    };
    if args_bytes.len() > MAX_CREATE_ARGS_LEN {
        return Err(PumpParseError::ArgsTooLong(args_bytes.len()));
    }
    // Creator is the first 32 bytes after the triple.
    // Short tail gives `None`.
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

/// Decode ix data: base58, then base64, then URL-safe base64.
/// Empty payloads never decode. One home for all decoders.
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

/// True when `data` opens with any known Pump.fun discriminator.
fn starts_with_known_discriminator(data: &[u8]) -> bool {
    data.starts_with(&PUMP_CREATE_DISCRIMINATOR)
        || data.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR)
        || data.starts_with(&PUMP_BUY_DISCRIMINATOR)
        || data.starts_with(&PUMP_SELL_DISCRIMINATOR)
        || data.starts_with(&PUMP_BUY_V2_DISCRIMINATOR)
        || data.starts_with(&PUMP_SELL_V2_DISCRIMINATOR)
        || data.starts_with(&PUMP_BUY_V3_DISCRIMINATOR)
        || data.starts_with(&PUMP_SELL_V3_DISCRIMINATOR)
        || data.starts_with(&PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR)
        || data.starts_with(&PUMP_MIGRATE_DISCRIMINATOR)
}

/// Parse helper without enrichment (signature/slot defaulted). Test-only.
#[cfg(test)]
pub fn parse_pump_create_bare(
    data: &[u8],
    accounts: &[Pubkey],
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    parse_pump_create_inner(data, accounts, String::new(), 0)
}

/// Verify the shared [`ids`] program ID matches its canonical string.
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
    use solana_transaction::versioned::TransactionVersion;

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
    fn parse_v2_ok_with_synth_encoding() {
        // Synthetic encoding (bool directly after the triple), NOT the
        // IDL layout (creator pubkey there): do not "fix" this toward
        // the bool shape; mainnet parsing is pinned below.
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
        // Frozen mainnet create_v2: triple + creator[32] + 11 tail bytes.
        // Decoding via the IDL struct (bool after the triple) would
        // misattribute by one byte — this test breaks loudly if tried.
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../tests/fixtures/create_v2_mainnet.json"
        ));
        let msg = crate::test_fixture::raw_message(&fetched);
        let keys = crate::test_fixture::key_table(&fetched);
        let pump = PUMP_PROGRAM_ID.to_string();
        for ix in &msg.instructions {
            let pi = usize::from(ix.program_id_index);
            if keys.get(pi) != Some(&pump) {
                continue;
            }
            let data = decode_ix_data(&ix.data).expect("decode");
            if !is_pump_create_v2_ix(&data) {
                continue;
            }
            let idx: Vec<usize> = ix.accounts.iter().map(|i| usize::from(*i)).collect();
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
    fn parse_v2_v0_envelope_recent_launch() {
        // Fresh mainnet create_v2 in a version-0 envelope (slot 457429472,
        // sig 2qSAjZs...NPzVPiBGkcHhx): launches ship BOTH v0 and v1
        // envelopes, so the fetch config must allow both
        // (`maxSupportedTransactionVersion: 1` covers 0 and 1). This test
        // breaks if parsing ever assumes v1-only. v0 transactions use
        // address lookup tables, so the key table includes loaded
        // addresses (static-only indexing would dangle).
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../tests/fixtures/create_v2_v0_recent_mainnet.json"
        ));
        assert_eq!(
            fetched.transaction.version,
            Some(TransactionVersion::Number(0))
        );
        assert_eq!(fetched.slot, 457429472);
        let msg = crate::test_fixture::raw_message(&fetched);
        // v0 transactions use address lookup tables; `key_table` includes
        // the loaded addresses (static-only indexing would dangle).
        let keys = crate::test_fixture::key_table(&fetched);
        let pump = PUMP_PROGRAM_ID.to_string();
        for ix in &msg.instructions {
            let pi = usize::from(ix.program_id_index);
            if keys.get(pi) != Some(&pump) {
                continue;
            }
            let data = decode_ix_data(&ix.data).expect("decode");
            if !is_pump_create_v2_ix(&data) {
                continue;
            }
            let idx: Vec<usize> = ix.accounts.iter().map(|i| usize::from(*i)).collect();
            assert_eq!(idx.len(), 16);
            let accounts: Vec<Pubkey> = idx
                .iter()
                .map(|i| Pubkey::from_str(&keys[*i]).expect("key"))
                .collect();
            let ev = parse_pump_create_v2(&data, &accounts, "sig".to_string(), 77)
                .expect("parse");
            assert_eq!(ev.name, "BYD AUTO");
            assert_eq!(ev.symbol, "$BYD");
            assert_eq!(
                ev.mint.to_string(),
                "7zxewCto2AFBbaaw7i1vKdmuDQ2gqvGYVfw8okf9pump"
            );
            assert_eq!(
                ev.bonding_curve.to_string(),
                "9LxGz5fnvUqW1jkKUxfAMPAsAMtCE6oa6LTyVDYVDGZs"
            );
            assert_eq!(ev.slot, 77);
            assert!(ev.creator.is_some(), "v2 launch carries a creator");
            return;
        }
        panic!("no create_v2 instruction in fixture");
    }

    #[test]
    fn parse_v2_v1_envelope_recent_launch() {
        // Fresh mainnet create_v2 in a version-1 envelope (slot 457431003,
        // sig 4oZKh9Jg...): the v1 counterpart to the v0 fixture above.
        // Together they pin that launches ship both envelopes and the
        // fetch config must keep allowing both.
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../tests/fixtures/create_v2_v1_recent_mainnet.json"
        ));
        assert_eq!(
            fetched.transaction.version,
            Some(TransactionVersion::Number(1))
        );
        assert!(fetched.slot > 455573900);
        let msg = crate::test_fixture::raw_message(&fetched);
        let keys = crate::test_fixture::key_table(&fetched);
        let pump = PUMP_PROGRAM_ID.to_string();
        for ix in &msg.instructions {
            let pi = usize::from(ix.program_id_index);
            if keys.get(pi) != Some(&pump) {
                continue;
            }
            let data = decode_ix_data(&ix.data).expect("decode");
            if !is_pump_create_v2_ix(&data) {
                continue;
            }
            let idx: Vec<usize> = ix.accounts.iter().map(|i| usize::from(*i)).collect();
            assert_eq!(idx.len(), 16);
            let accounts: Vec<Pubkey> = idx
                .iter()
                .map(|i| Pubkey::from_str(&keys[*i]).expect("key"))
                .collect();
            let ev = parse_pump_create_v2(&data, &accounts, "sig".to_string(), 77)
                .expect("parse");
            // Trailing space is on-chain data (attacker-controlled
            // name), not a typo — assert exact.
            assert_eq!(ev.name, "TRUMP STREAM ");
            assert_eq!(ev.symbol, "TRUMPGOLD");
            assert_eq!(
                ev.mint.to_string(),
                "E8c671gi75KXEkHWruLQAZFjBLXitqzj5HAfke4cpump"
            );
            assert_eq!(
                ev.bonding_curve.to_string(),
                "ByQMGnxewzv5sXDXyuC2ioq64BU4nCHL3PZsZZCcBpc9"
            );
            assert_eq!(ev.slot, 77);
            assert!(ev.creator.is_some(), "v2 launch carries a creator");
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
