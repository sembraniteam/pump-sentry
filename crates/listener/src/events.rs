//! Pump.fun Anchor CPI events (`TradeEvent`, `CompleteEvent`,
//! `PostCompleteBuyEvent`).
//!
//! Provenance: decoded from real mainnet log lines (`Program data:`
//! base64, also emitted as `emit_cpi` self-CPI ixs carrying the same
//! bytes), frozen in `tests/fixtures/complete_*_mainnet.json` and
//! reconciled against balance deltas: `token_amount` equals buyer
//! credits exactly; `sol_amount` lands within fee legs of curve deltas.
//!
//! Only byte-pinned PREFIXES are decoded; tails stay opaque
//! (unverified fields are not parsed). `PostCompleteBuyEvent` has a
//! single capture (Jupiter-routed buy crossing below the shared token
//! floor): user/mint/curve/timestamp are pinned, the rest is opaque
//! until a second capture confirms roles.
//!
//! Consumer guidance: bundle forensics keeps counting ix args, not
//! event actuals — delivered is always ≤ requested, so ix args are the
//! conservative (reject-biased) figure and switching would bias screens
//! toward PASS. This decoder exists for analytics and for the day a
//! pool-part capture needs reconciling.

use ids::{
    COMPLETE_EVENT_DISCRIMINATOR, POST_COMPLETE_BUY_EVENT_DISCRIMINATOR, TRADE_EVENT_DISCRIMINATOR,
};
use solana_sdk::pubkey::Pubkey;

/// Curve-part fill, byte-pinned prefix: mint[0:32], sol[32:40],
/// tokens[40:48], is_buy[48], user[49:81]. Tail opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeEvent {
    pub mint: Pubkey,
    /// Quote paid (lamports, fees-in basis on the frozen sample).
    pub sol_amount: u64,
    /// Base units credited (equals the buyer balance delta, proven).
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: Pubkey,
}

/// Curve transition, byte-pinned prefix: user[0:32], mint[32:64],
/// curve[64:96], unix timestamp[96:104]. Tail opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompleteEvent {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub curve: Pubkey,
    pub timestamp: u64,
}

/// Post-completion buy, byte-pinned prefix: user[0:32], mint[32:64],
/// curve[64:96], unix timestamp[128:136] (== block time on the frozen
/// sample). Bytes [96:128] and the tail stay opaque (roles unverified).
/// Observed exactly once (Jupiter-routed exact-quote buy that pushed
/// the remainder BELOW the shared floor; the two on-the-floor
/// completions lack it). All bought tokens still came from the curve —
/// no pool leg materialized in-transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostCompleteBuyEvent {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub curve: Pubkey,
    pub timestamp: u64,
}

/// Decoded pump event (known discriminators only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    Trade(TradeEvent),
    Complete(CompleteEvent),
    PostCompleteBuy(PostCompleteBuyEvent),
}

fn pubkey_at(data: &[u8], off: usize) -> Option<Pubkey> {
    let bytes: [u8; 32] = data.get(off..off + 32)?.try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

fn u64_at(data: &[u8], off: usize) -> Option<u64> {
    data.get(off..off + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
}

/// Decode one raw event payload (discriminator + body).
pub fn decode_event(data: &[u8]) -> Option<PumpEvent> {
    let (disc, body) = data.split_first_chunk::<8>()?;
    if *disc == TRADE_EVENT_DISCRIMINATOR {
        // Pinned prefix is 81 bytes; longer bodies keep an opaque tail.
        if body.len() < 81 {
            return None;
        }
        Some(PumpEvent::Trade(TradeEvent {
            mint: pubkey_at(body, 0)?,
            sol_amount: u64_at(body, 32)?,
            token_amount: u64_at(body, 40)?,
            is_buy: body.get(48) == Some(&1),
            user: pubkey_at(body, 49)?,
        }))
    } else if *disc == COMPLETE_EVENT_DISCRIMINATOR {
        // Pinned prefix is 104 bytes; longer bodies keep an opaque tail.
        if body.len() < 104 {
            return None;
        }
        Some(PumpEvent::Complete(CompleteEvent {
            user: pubkey_at(body, 0)?,
            mint: pubkey_at(body, 32)?,
            curve: pubkey_at(body, 64)?,
            timestamp: u64_at(body, 96)?,
        }))
    } else if *disc == POST_COMPLETE_BUY_EVENT_DISCRIMINATOR {
        // Pinned prefix reaches the timestamp at 128; bytes [96:128]
        // and the tail stay opaque (single capture — roles unverified
        // beyond user/mint/curve/timestamp).
        if body.len() < 136 {
            return None;
        }
        Some(PumpEvent::PostCompleteBuy(PostCompleteBuyEvent {
            user: pubkey_at(body, 0)?,
            mint: pubkey_at(body, 32)?,
            curve: pubkey_at(body, 64)?,
            timestamp: u64_at(body, 128)?,
        }))
    } else {
        None
    }
}

/// Scan transaction log lines for pump events (`Program data:` and
/// `Program log:` base64). Unknown discriminators are skipped, never
/// guessed — including `PostCompleteBuyEvent` until a capture exists.
pub fn pump_events(logs: &[String]) -> Vec<PumpEvent> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
    let mut out = Vec::new();
    for line in logs {
        let raw = line
            .strip_prefix("Program data: ")
            .or_else(|| line.strip_prefix("Program log: "));
        let Some(raw) = raw else { continue };
        let Ok(data) = BASE64_STANDARD.decode(raw.trim()) else {
            continue;
        };
        if let Some(event) = decode_event(&data) {
            out.push(event);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn frozen_logs() -> Vec<String> {
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/complete_mayhem_buy_mainnet.json"
        ))
        .expect("fixture parses");
        logs_of(&raw)
    }

    fn logs_of(raw: &serde_json::Value) -> Vec<String> {
        raw["meta"]["logMessages"]
            .as_array()
            .expect("logs array")
            .iter()
            .map(|v| v.as_str().expect("log line").to_string())
            .collect()
    }

    fn std_logs() -> Vec<String> {
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/complete_std_buy_mainnet.json"
        ))
        .expect("fixture parses");
        logs_of(&raw)
    }

    #[test]
    fn trade_event_pins_curve_part() {
        // Balance-reconciled on the frozen sample: tokens equal the
        // buyer credit exactly (26616181758971), sol within fee legs.
        let events = pump_events(&frozen_logs());
        let trade = events.iter().find_map(|e| match e {
            PumpEvent::Trade(t) => Some(*t),
            _ => None,
        });
        assert_eq!(
            trade,
            Some(TradeEvent {
                mint: Pubkey::from_str("21UZa6qk9jQHgnU61ijKHWUoac7aEwzu7jM7RUzmpump").unwrap(),
                sol_amount: 4_743_474,
                token_amount: 26_616_181_758_971,
                is_buy: true,
                user: Pubkey::from_str("ANrVDsew28co4isEY9Bc4eJKxeG9D2vesuKefCsqWwDA").unwrap(),
            })
        );
    }

    #[test]
    fn complete_event_pins_transition() {
        let events = pump_events(&frozen_logs());
        let complete = events.iter().find_map(|e| match e {
            PumpEvent::Complete(c) => Some(*c),
            _ => None,
        });
        assert_eq!(
            complete,
            Some(CompleteEvent {
                user: Pubkey::from_str("ANrVDsew28co4isEY9Bc4eJKxeG9D2vesuKefCsqWwDA").unwrap(),
                mint: Pubkey::from_str("21UZa6qk9jQHgnU61ijKHWUoac7aEwzu7jM7RUzmpump").unwrap(),
                curve: Pubkey::from_str("GSaNN6wrmpuRXTYkuiXq6BrcGtN6kiM8cA8iNqK9a2Ho").unwrap(),
                timestamp: 1_792_401_800,
            })
        );
        // Sequence on the frozen sample: curve fill first, transition
        // second, and NO pool part in the same transaction.
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                PumpEvent::Trade(_) => "trade",
                PumpEvent::Complete(_) => "complete",
                PumpEvent::PostCompleteBuy(_) => "post",
            })
            .collect();
        assert_eq!(kinds, vec!["trade", "complete"]);
    }

    #[test]
    fn truncated_and_foreign_payloads_refuse() {
        assert!(decode_event(&TRADE_EVENT_DISCRIMINATOR).is_none());
        assert!(decode_event(&COMPLETE_EVENT_DISCRIMINATOR).is_none());
        assert!(decode_event(&POST_COMPLETE_BUY_EVENT_DISCRIMINATOR).is_none());
        let unknown = [0u8; 16];
        assert!(decode_event(&unknown).is_none());
    }

    #[test]
    fn post_complete_buy_pins_floor_crossing() {
        // Single capture (slot 458801131): Jupiter-routed exact-quote
        // buy that pushed the remainder BELOW the shared floor — the
        // two on-the-floor completions lack this event. All bought
        // tokens still came from the curve (curve debit == user credit
        // exactly): no pool leg materialized in-transaction.
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/post_complete_buy_mainnet.json"
        ))
        .expect("fixture parses");
        let events = pump_events(&logs_of(&raw));
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                PumpEvent::Trade(_) => "trade",
                PumpEvent::Complete(_) => "complete",
                PumpEvent::PostCompleteBuy(_) => "post",
            })
            .collect();
        assert_eq!(kinds, vec!["trade", "complete", "post"]);
        let post = events.iter().find_map(|e| match e {
            PumpEvent::PostCompleteBuy(p) => Some(*p),
            _ => None,
        });
        assert_eq!(
            post,
            Some(PostCompleteBuyEvent {
                user: Pubkey::from_str("7UZoia2YqDAYfRdgm2DmdvdB8t5D5pnk2Wx6CDsNEtFN").unwrap(),
                mint: Pubkey::from_str("68xW3Bpf5ax6ML91yLbAHCRNjxPAo4R5XTyLqA5bNPtM").unwrap(),
                curve: Pubkey::from_str("5yyr5UnWvsBENvBbjBT8ZUvHSNG1chyQzJZJ7MSc5ume").unwrap(),
                timestamp: 1_792_405_916,
            })
        );
    }

    #[test]
    fn standard_completion_pins_same_codecs() {
        // Second capture, standard (non-mayhem) curve, same slot-era:
        // the codecs are not mayhem-specific. Sequence is again
        // trade-then-complete with no pool part in-transaction.
        let events = pump_events(&std_logs());
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                PumpEvent::Trade(_) => "trade",
                PumpEvent::Complete(_) => "complete",
                PumpEvent::PostCompleteBuy(_) => "post",
            })
            .collect();
        assert_eq!(kinds, vec!["trade", "complete"]);
        let trade = events.iter().find_map(|e| match e {
            PumpEvent::Trade(t) => Some(*t),
            _ => None,
        });
        assert_eq!(
            trade,
            Some(TradeEvent {
                mint: Pubkey::from_str("EBQRGRKjw61DShkLkA3aexAD2eET7hXQjKuFjzT3pump").unwrap(),
                sol_amount: 7_818_151_612,
                token_amount: 20_415_688_021_638,
                is_buy: true,
                user: Pubkey::from_str("YARSciE3tFureJbF9YmeBSxY946V24DmeKhuV3AoyXW").unwrap(),
            })
        );
        let complete = events.iter().find_map(|e| match e {
            PumpEvent::Complete(c) => Some(*c),
            _ => None,
        });
        assert_eq!(
            complete,
            Some(CompleteEvent {
                user: Pubkey::from_str("YARSciE3tFureJbF9YmeBSxY946V24DmeKhuV3AoyXW").unwrap(),
                mint: Pubkey::from_str("EBQRGRKjw61DShkLkA3aexAD2eET7hXQjKuFjzT3pump").unwrap(),
                curve: Pubkey::from_str("3jwbG3djWyyAWtzZJjtjR5WHYBjeYidkJgNTBPgKw3Qi").unwrap(),
                timestamp: 1_792_404_866,
            })
        );
    }
}
