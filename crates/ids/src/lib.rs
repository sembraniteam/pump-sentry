//! On-chain addresses and discriminators. Edit once here.


pub mod pump;
pub mod pumpswap;
pub mod raydium;
pub mod system;

pub use pump::*;
pub use pumpswap::*;
pub use raydium::*;
pub use system::*;

// Intentional discriminator collision, never merge — disambiguated by
// program-id (Pump `sell_v2` vs PumpSwap AMM `sell_v2`). The assert
// pins the equality so a one-side edit breaks loudly instead of
// silently forking the two venues.
const _: () = assert!(
    u64::from_le_bytes(PUMP_SELL_V2_DISCRIMINATOR)
        == u64::from_le_bytes(PUMP_AMM_SELL_V2_DISCRIMINATOR)
);

/// Reduce an endpoint URL to `scheme://host` for logs.
///
/// Providers hide keys in different places — Helius in `?api-key=`,
/// Chainstack in the path (`.../core.chainstack.com/<key>`), dRPC in
/// the path too. Stripping only the query (the old rule) still leaks
/// path keys, as one live log proved. Scheme + host keeps the
/// debuggable part (which provider) and drops everything else.
/// Unparseable input yields a fixed placeholder, never the original.
pub fn redact_endpoint(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => return "<unparseable-endpoint>".to_string(),
    };
    let scheme = &url[..url.len() - rest.len() - 3];
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if scheme.is_empty() || host.is_empty() {
        return "<unparseable-endpoint>".to_string();
    }
    format!("{scheme}://{host}")
}

/// Strip `for url (...)` spans from rendered transport errors.
///
/// `solana-client`/`reqwest` errors embed the full request URL, query
/// string included — and our RPC URLs carry `?api-key=`. Every
/// `warn!(error = %e, ...)` on a transport error would otherwise write
/// the key into bot.log. Lives here because `ids` is the one crate
/// every error-logging crate already depends on; one implementation,
/// no per-crate copies.
///
/// Returns the message unchanged when no such span exists. Balanced
/// parens are scanned so trailing context after the URL survives.
pub fn strip_error_url(msg: &str) -> String {
    const MARKER: &str = " for url (";
    let Some(start) = msg.find(MARKER) else {
        return msg.to_string();
    };
    let url_start = start + MARKER.len();
    let mut depth = 1usize;
    let mut end = None;
    for (i, c) in msg[url_start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(url_start + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return msg.to_string();
    };
    let mut out = String::with_capacity(msg.len());
    out.push_str(&msg[..start]);
    out.push_str(" for url (<redacted>)");
    out.push_str(&msg[end + 1..]);
    out
}

#[cfg(test)]
mod test {
    use super::{redact_endpoint, strip_error_url};

    #[test]
    fn endpoint_keeps_host_only() {
        // Helius-style (key in query) and Chainstack-style (key in
        // path) both collapse to scheme://host.
        assert_eq!(
            redact_endpoint("https://mainnet.helius-rpc.com/?api-key=K1"),
            "https://mainnet.helius-rpc.com"
        );
        assert_eq!(
            redact_endpoint("wss://solana-mainnet.core.chainstack.com/eeb0e908secret"),
            "wss://solana-mainnet.core.chainstack.com"
        );
        assert_eq!(
            redact_endpoint("https://lb.drpc.live/solana/Amp_secret"),
            "https://lb.drpc.live"
        );
        assert_eq!(
            redact_endpoint("http://127.0.0.1:11434/v1/chat/completions"),
            "http://127.0.0.1:11434"
        );
        for bad in ["", "not a url", ":///x", "https://"] {
            assert_eq!(redact_endpoint(bad), "<unparseable-endpoint>", "{bad}");
        }
    }

    #[test]
    fn strips_url_span_and_keeps_context() {
        let msg = "HTTP status client error (400 Bad Request) for url (https://lb.drpc.live/solana/SECRETKEY) extra";
        assert_eq!(
            strip_error_url(msg),
            "HTTP status client error (400 Bad Request) for url (<redacted>) extra"
        );
    }

    #[test]
    fn leaves_clean_messages_alone() {
        for msg in ["timeout", "", "rpc error: connection closed"] {
            assert_eq!(strip_error_url(msg), msg);
        }
    }

    #[test]
    fn unbalanced_parens_are_left_alone() {
        let msg = "weird for url (https://x never closed";
        assert_eq!(strip_error_url(msg), msg);
    }
}
