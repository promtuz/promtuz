//! Thin wrapper binary for the headless e2e client driver.
//!
//! All logic lives in the `core::e2e` lib module (feature `e2e-client`) so
//! it can reach crate-internals (the `RelayDhtClient` explicit-signer seam,
//! the MLS stash/provider). Build: `cargo build -p core --bin e2e-client
//! --features e2e-client`. Driven by the `testnet` orchestrator.

fn main() {
    core::e2e::run();
}
