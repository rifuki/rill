//! The only crate permitted to talk to Sui.
//!
//! The entire reference implementation used just nine distinct client methods, so this is
//! a small surface behind `SuiRead` / `SuiWrite` traits plus an in-memory fake. Everything
//! else in the workspace takes the trait, and therefore tests without a network.
//!
//! Simulation goes through `execution_client().simulate_transaction()`.
//! `SimulateTransactionRequest` carries no `signatures` field, while
//! `ExecuteTransactionRequest` does — simulating an unsigned transaction without a key is
//! the designed API, which is what makes Rill's keyless build possible at all.
