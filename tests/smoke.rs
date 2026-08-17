//! Smoke test for the test infrastructure (Wave 1 todo 1).
//!
//! Purpose: prove the test harness wires up alongside the existing 202 tests
//! in `src/`. This single test failing would mean the harness itself is broken,
//! not any production code. It is intentionally trivial — `assert!(true)`.
//!
//! Given: the test infrastructure (cargo-nextest + cargo-llvm-cov + insta) is
//!        configured in `Cargo.toml`, `.config/nextest.toml`, `scripts/coverage.sh`.
//! When:  `cargo nextest run` (or `cargo test`) executes the test binary.
//! Then:  this test passes, proving the harness runs and reports results.

#[test]
fn smoke_test_infrastructure_wires_up() {
    // The assertion is deliberately trivial: the point is that the *binary*
    // containing it builds, links, and executes under nextest. If this fails,
    // the infrastructure is broken, not the production code.
    assert!(
        true,
        "smoke test must pass to prove the test harness wires up"
    );
}
