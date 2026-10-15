//! On-chain addresses and discriminators. Edit once here.


pub mod pump;
pub mod pumpswap;
pub mod raydium;
pub mod system;

pub use pump::*;
pub use pumpswap::*;
pub use raydium::*;
pub use system::*;

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
    use super::strip_error_url;

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
