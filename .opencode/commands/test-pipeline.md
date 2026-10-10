---
description: Execute compiler checks, unit tests, and clippy linter across all workspace crates.
agent: build
subtask: false
---

Execute full compilation and test validation of the Rust workspace:

Current cargo check output:
!`cargo check --all-targets`

Unit test results:
!`cargo test --all -- --nocapture`

Clippy linter diagnostics:
!`cargo clippy --all-targets -- -D warnings`

### Verification Tasks:

1. Resolve any compilation errors, test failures, or clippy warnings.
2. Fix all lifetime mismatches, synchronization bugs, or unhandled errors.
