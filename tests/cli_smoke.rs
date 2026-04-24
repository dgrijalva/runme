//! Smoke test that verifies the CLI test harness compiles and the binary exists.
//!
//! Actual CLI integration tests will be added by subsequent tasks.

mod harness;

#[test]
fn harness_compiles() {
    // Verify the binary path resolves (env! would fail at compile time if not set)
    let bin = env!("CARGO_BIN_EXE_rnme");
    assert!(
        std::path::Path::new(bin).exists(),
        "rnme binary should exist at {}",
        bin
    );
}
