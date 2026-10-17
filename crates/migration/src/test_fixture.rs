//! Typed access to the frozen pool fixtures (tests only).
//!
//! Fixtures are recorded `getAccountInfo` truth: tests decode them
//! through this DTO instead of hand-walking untyped JSON, so a shape
//! mismatch breaks loudly at the fixture boundary.

/// Frozen pool account fixture `{amm, owner, data: [u8], via?}`.
/// `via` names the RPC method the snapshot came from (provenance pin).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PoolFixture {
    pub(crate) amm: String,
    pub(crate) owner: String,
    pub(crate) data: Vec<u8>,
    #[serde(default)]
    pub(crate) via: Option<String>,
}

/// Decode a frozen pool fixture; panics on drift.
pub(crate) fn pool_fixture(fixture: &str) -> PoolFixture {
    serde_json::from_str(fixture).expect("pool fixture decodes")
}
