//! The only crate permitted to talk to Sui.
//!
//! The entire reference implementation used nine distinct client methods across backend and
//! signer combined. That is a small enough surface to put behind a trait, and putting it behind
//! one is what lets every other crate test with no network and no mocking framework — they take
//! [`SuiRead`] / [`SuiWrite`] and get [`FakeSui`] in tests.
//!
//! The boundary is deliberately expressed in this crate's own types rather than in the gRPC
//! proto's. A trait that handed callers `sui_rpc::proto::...` values would leak the transport into
//! every crate that touches it, and the point of the trait is that it does not.
//!
//! Keyless simulation is the load-bearing capability here: `SimulateTransactionRequest` carries no
//! `signatures` field, while `ExecuteTransactionRequest` does. Evaluating a transaction nobody has
//! signed is the API's designed use, which is what makes Rill's build step able to hold no key.

pub mod fake;
pub mod grpc;

pub use rill_chain_types::*;

/// Domain types for the boundary — kept in one place so the trait, the real client, and the fake
/// all speak the same language.
pub mod rill_chain_types {
    /// Enough of an object to reference it in a transaction.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ObjectRef {
        pub id: String,
        pub version: u64,
        pub digest: String,
    }

    /// An object as read from chain, with its Move type when the node reported one.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ObjectSummary {
        pub reference: ObjectRef,
        pub object_type: Option<String>,
        /// Raw JSON of the object's fields, when requested. `None` when only the reference was read.
        pub fields: Option<serde_json::Value>,
    }

    /// A coin balance delta observed during simulation or execution.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BalanceDelta {
        pub address: String,
        pub coin_type: String,
        /// Signed base units, as a string. Never a float, and never narrowed to i64 here — the
        /// caller decides what precision its own arithmetic needs.
        pub amount: String,
    }

    /// Whether a simulation's verdict can be trusted as a real answer about execution.
    ///
    /// Two values only. A failure is the boolean `ok`; `Unverified` means something specific and
    /// narrow — the node aborted for a reason that says nothing about whether the transaction
    /// would actually work. There is no third state, and deliberately no way to opt into treating
    /// `Unverified` as good enough.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Verification {
        Verified,
        Unverified,
    }

    /// A classified simulation result.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SimulationOutcome {
        pub ok: bool,
        pub verification: Verification,
        /// The abort or error text, when the simulation did not succeed.
        pub error: Option<String>,
        /// Computation + storage, less rebate, in mist.
        pub gas_used_mist: u64,
        pub balance_changes: Vec<BalanceDelta>,
        /// One entry per PTB command, in order — the devInspect-style return values.
        pub command_output_count: usize,
    }

    /// The result of actually submitting a transaction.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExecutionOutcome {
        pub digest: String,
        pub success: bool,
        pub error: Option<String>,
        pub gas_used_mist: u64,
        pub balance_changes: Vec<BalanceDelta>,
    }
}

/// Everything that can go wrong at the chain boundary.
///
/// A transport failure and a rejected transaction are separate variants on purpose: the first
/// means we learned nothing, the second means we learned something definite. Collapsing them
/// would let a network blip read as a refusal — a simulation gate that fails open on a dropped
/// connection is worse than one that does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The node could not be reached, or answered in a way we could not parse.
    Transport(String),
    /// The node answered, and the answer was a refusal.
    Rejected(String),
    NotFound(String),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "could not reach the Sui node: {m}"),
            Self::Rejected(m) => write!(f, "the Sui node refused the request: {m}"),
            Self::NotFound(m) => write!(f, "not found on chain: {m}"),
        }
    }
}

impl std::error::Error for ChainError {}

pub type ChainResult<T> = Result<T, ChainError>;

/// Reads. Nothing here can change chain state, so a caller holding only this cannot spend.
#[allow(async_fn_in_trait)]
pub trait SuiRead {
    async fn get_object(&self, id: &str) -> ChainResult<ObjectSummary>;
    async fn list_owned_objects(&self, owner: &str) -> ChainResult<Vec<ObjectSummary>>;
    async fn get_balance(&self, owner: &str, coin_type: &str) -> ChainResult<u64>;
    /// Evaluate an unsigned transaction. Takes base64 BCS so this trait stays independent of the
    /// transaction-builder crate.
    async fn simulate(&self, unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome>;
}

/// Writes. Separated from [`SuiRead`] so that a component which only needs to read cannot be
/// handed the ability to submit — the type says which half of the boundary it was given.
#[allow(async_fn_in_trait)]
pub trait SuiWrite {
    async fn execute(&self, tx_b64: &str, signatures: &[String]) -> ChainResult<ExecutionOutcome>;
    async fn wait_for(&self, digest: &str) -> ChainResult<ExecutionOutcome>;
}

// ── simulation classification ─────────────────────────────────────────────────────────────────

/// Cetus package ids whose `checked_package_version` abort is a known simulation artefact rather
/// than a real failure. Curated, because matching the abort text alone would let any package that
/// happens to use the same assertion name be waved through.
pub const CETUS_PACKAGE_IDS: &[&str] = &[
    "0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb",
    "0x0868b71c0cba55bf0faf6c40df8c179c67a4d0ba0e79965b68b3d72d7dfbf666",
    "0x70968826ad1b4ba895753f634b0aea68d0672908ca1075a2abdf0fc9e0b2fc6a",
];

/// Decide whether a failed simulation tells us anything about execution.
///
/// The only case that does not is Cetus's package-version assertion, which aborts under simulation
/// regardless of whether the swap would succeed. The check is package-scoped rather than a bare
/// substring match: `checked_package_version` alone appears in code that is not Cetus, and treating
/// an unrelated package's abort as "inconclusive" would quietly widen the one hole in the gate.
pub fn classify_failure(error: &str) -> Verification {
    let mentions_assertion = error.contains("checked_package_version");
    let mentions_cetus = CETUS_PACKAGE_IDS.iter().any(|id| error.contains(id));
    if mentions_assertion && mentions_cetus {
        Verification::Unverified
    } else {
        Verification::Verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_failure_is_a_verified_answer() {
        assert_eq!(
            classify_failure("MoveAbort(.., 5)"),
            Verification::Verified,
            "a real abort is a real answer — the simulation worked, the transaction would not"
        );
    }

    #[test]
    fn the_cetus_version_abort_is_inconclusive() {
        let err = format!(
            "MoveAbort in {}::config: checked_package_version",
            CETUS_PACKAGE_IDS[0]
        );
        assert_eq!(classify_failure(&err), Verification::Unverified);
    }

    #[test]
    fn the_assertion_name_alone_is_not_enough() {
        assert_eq!(
            classify_failure("MoveAbort in 0xdeadbeef::thing: checked_package_version"),
            Verification::Verified,
            "an unrelated package borrowing the name must not inherit the exemption"
        );
    }

    #[test]
    fn a_cetus_package_id_alone_is_not_enough() {
        let err = format!(
            "MoveAbort in {}::pool: EInsufficientLiquidity",
            CETUS_PACKAGE_IDS[1]
        );
        assert_eq!(
            classify_failure(&err),
            Verification::Verified,
            "a genuine Cetus failure is still a genuine failure"
        );
    }
}
