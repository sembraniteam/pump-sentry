---
description: Execute compiler checks, unit tests, and clippy linter across all workspace crates.
---

Execute full compilation and test validation of the Rust workspace:

Current cargo check output:
!`cargo check --all-targets`

Unit test results (live probes stay `#[ignore]`d behind env gates — never run casually):
!`cargo test --workspace`

Clippy linter diagnostics (must be zero warnings):
!`cargo clippy --all-targets --tests -- -D warnings`

### Verification Tasks:

1. Resolve any compilation errors, test failures, or clippy warnings. Frozen mainnet fixtures
   (`crates/listener/tests/fixtures/`) are the gate that must stay green.
2. Fix all lifetime mismatches, synchronization bugs, or unhandled errors. Known footgun: `StreamExt::map`
   with an async block over borrowed items trips E0631 at distant spawn sites — use owned items or `.then()`.
3. `cargo fmt --all -- --check` has PRE-EXISTING diffs: fix only your own lines, never whole-tree fmt.
