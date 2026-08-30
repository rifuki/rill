//! FlowGraph to one unsigned PTB, and PTB reconstruction for the signer.
//!
//! Every Move call is written directly against `sui-transaction-builder`. No protocol SDK
//! sits on the money path: a DeepBook order is one `pool::place_limit_order` call plus a
//! trade proof, and the only Rust DeepBook SDK is third-party and unpublished.
//!
//! Funding flows through the v3 contract shape — `request_spend`, one `prove` per attached
//! rule, then `confirm_spend`, which releases the coin only once the hot potato carries a
//! receipt for every rule. The retired `spend()` entry point does not exist here.
