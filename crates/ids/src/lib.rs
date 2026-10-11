//! Single source of truth for on-chain addresses and instruction
//! discriminators shared across crates.
//!
//! A program upgrade or discriminator change must be edited exactly once,
//! here — the compiler links every consumer, so drift becomes a build
//! break instead of silent mainnet misbehavior.

pub mod pump;
pub mod raydium;
pub mod system;

pub use pump::*;
pub use raydium::*;
pub use system::*;
