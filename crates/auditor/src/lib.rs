//! RugCheck.xyz token risk evaluation (see skill `solana-rugcheck-api`).
//!
//! [`RugCheckClient`] fetches a token report over HTTPS with timeout +
//! bounded retries, then [`assess`] enforces the security invariants:
//! score cap, no `danger` risks, no mint/freeze authority.

pub mod client;
pub mod policy;
pub mod types;

pub use client::{RugCheckClient, RugCheckClientBuilder};
pub use policy::{AuditPolicy, Verdict, assess};
pub use types::{RiskItem, RugCheckError, RugCheckReport, TokenInfo};
