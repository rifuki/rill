//! Pure domain logic. **This crate performs no I/O and must never gain the ability to.**
//!
//! It holds the money path (integer base units only — no constructor from `f64` exists),
//! the capability manifest and the single `to_declaration` producer, the ExecutionEnvelope
//! types and their digest, and FlowGraph validation.
//!
//! The invariant is checked mechanically rather than by review: `cargo tree -p rill-core`
//! must never show `tokio`, `axum`, `sui-rpc`, or `reqwest`. Two thirds of the reference
//! implementation was already logic of this kind; here the dependency graph is what keeps
//! it honest, so the majority of the system tests with no network and no mocks.

pub mod amounts;
pub mod envelope;
pub mod flow;
