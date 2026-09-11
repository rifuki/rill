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

pub mod describe;
pub mod fake;
pub mod grpc;
pub mod settle;

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
        /// The version this object was first shared at, for a shared object; `None` when it is
        /// owned. This is what a transaction must reference a shared object by — not its current
        /// version, and never zero.
        pub shared_initial_version: Option<u64>,
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
        /// The BCS bytes each command returned, command by command.
        ///
        /// Carried rather than merely counted because a Move function with a return value — a
        /// pool's mid price, a quote, a balance — is read by simulating a call to it and taking
        /// the bytes back out. Counting them would say a value arrived without saying what it was.
        pub command_returns: Vec<Vec<Vec<u8>>>,
    }

    /// The result of actually submitting a transaction.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExecutionOutcome {
        pub digest: String,
        pub success: bool,
        pub error: Option<String>,
        pub gas_used_mist: u64,
        pub balance_changes: Vec<BalanceDelta>,
        /// Objects this transaction brought into existence.
        ///
        /// Carried because a multi-step flow cannot continue without them: `create_wallet` shares a
        /// wallet and mints a capability, and the ids of both are knowable only from the effects of
        /// the transaction that made them. Without this the next step has nothing to reference.
        pub created: Vec<CreatedObject>,
    }

    /// An object that did not exist before this transaction.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CreatedObject {
        pub object_id: String,
        pub object_type: Option<String>,
        /// For a shared object, the version it was shared at — which is what any later transaction
        /// must reference it by. `None` when it is owned.
        pub shared_initial_version: Option<u64>,
        /// The address it was transferred to, when it is owned by one.
        pub owner: Option<String>,
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
    /// The node answered about a thing that exists, and the answer was missing the part that was
    /// asked for.
    ///
    /// Distinct from [`NotFound`](Self::NotFound) because the two call for opposite responses and a
    /// caller cannot tell them apart once they share a variant. A digest the node does not have is a
    /// chain condition, and a caller that handles pruning by moving on is right to. A response that
    /// arrives without the bytes the read mask asked for is a fault in the request, and a caller
    /// that moves on has turned a bug in its own code into a silently skipped test: reducing the
    /// mask to `["digest"]` made a live test pass by skipping, which is how this was found.
    Malformed(String),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "could not reach the Sui node: {m}"),
            Self::Rejected(m) => write!(f, "the Sui node refused the request: {m}"),
            Self::NotFound(m) => write!(f, "not found on chain: {m}"),
            Self::Malformed(m) => write!(
                f,
                "the Sui node answered but the answer was not usable, which is a fault in the \
                 request rather than a condition of the chain: {m}"
            ),
        }
    }
}

impl std::error::Error for ChainError {}

pub type ChainResult<T> = Result<T, ChainError>;

/// Reads. Nothing here can change chain state, so a caller holding only this cannot spend.
#[allow(async_fn_in_trait)]
pub trait SuiRead {
    async fn get_object(&self, id: &str) -> ChainResult<ObjectSummary>;
    /// Every object `owner` holds. The whole set, not the first page of it: a caller picking gas
    /// coins out of this list cannot tell a short list from a truncated one, so the truncation
    /// must never happen here.
    async fn list_owned_objects(&self, owner: &str) -> ChainResult<Vec<ObjectSummary>>;
    async fn get_balance(&self, owner: &str, coin_type: &str) -> ChainResult<u64>;
    /// Evaluate an unsigned transaction. Takes base64 BCS so this trait stays independent of the
    /// transaction-builder crate.
    async fn simulate(&self, unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome>;

    /// Evaluate a transaction purely to read a Move function's return value.
    ///
    /// # This is not the gate, and must never be used as one
    ///
    /// Input checks are turned off, which is what makes a read possible at all: a call to
    /// `pool::mid_price` has no sender who owns anything and no gas coin to pay with, and with
    /// checks on the node refuses it before reaching the function. Off, it runs the code and hands
    /// back what it returned.
    ///
    /// That is also exactly why it cannot stand in for [`simulate`]: a transaction that passes here
    /// has not been shown to be payable, authorised, or executable — only that its Move code does
    /// not abort. The strict gate before signing asks the other question, and asks it with checks
    /// on.
    ///
    /// [`simulate`]: SuiRead::simulate
    async fn simulate_read(&self, unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome>;

    /// The current epoch's reference gas price, in mist per gas unit.
    ///
    /// # It is not the same number on every network
    ///
    /// Testnet answers 1000 and mainnet answers 100, so a literal that happens to be right on the
    /// network you developed against is ten times the price on the one you ship to. It also moves:
    /// it is a per-epoch value the validators set, and a transaction built below it is rejected
    /// outright rather than merely running slowly.
    ///
    /// So it is read, per build, from the chain the transaction is going to.
    async fn reference_gas_price(&self) -> ChainResult<u64>;
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
/// than a real failure.
///
/// Curated, because matching the abort text alone would let any package that happens to use the
/// same assertion name be waved through — and this is the one hole in the simulation gate, so
/// widening it by accident is the expensive mistake.
///
/// The list mixes networks deliberately: an error string is matched by substring, and which network
/// a package lives on has no bearing on whether its id appears in one. Marked here so the mix is
/// not later "tidied" into a single-network list that then fails to match half the aborts.
///
/// Every entry is checked against the chain by `tests/cetus_ids.rs` — a curated list nobody
/// verifies is folklore, and this one guards a gate.
pub const CETUS_PACKAGE_IDS: &[(&str, &str)] = &[
    (
        "mainnet",
        "0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb",
    ),
    (
        "testnet",
        "0x0868b71c0cba55bf0faf6c40df8c179c67a4d0ba0e79965b68b3d72d7dfbf666",
    ),
    (
        "mainnet",
        "0x70968826ad1b4ba895753f634b0aea68d0672908ca1075a2abdf0fc9e0b2fc6a",
    ),
];

/// Decide whether a failed simulation tells us anything about execution.
///
/// The only case that does not is Cetus's package-version assertion, which aborts under simulation
/// regardless of whether the swap would succeed. The check is package-scoped rather than a bare
/// substring match: `checked_package_version` alone appears in code that is not Cetus, and treating
/// an unrelated package's abort as "inconclusive" would quietly widen the one hole in the gate.
pub fn classify_failure(error: &str) -> Verification {
    let mentions_assertion = error.contains("checked_package_version");
    let mentions_cetus = CETUS_PACKAGE_IDS
        .iter()
        .any(|(_network, id)| error.contains(id));
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
            CETUS_PACKAGE_IDS[0].1
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
            CETUS_PACKAGE_IDS[1].1
        );
        assert_eq!(
            classify_failure(&err),
            Verification::Verified,
            "a genuine Cetus failure is still a genuine failure"
        );
    }
}

/// Which rule module aborted, and why — read out of a Move abort.
///
/// # A rule refusing is the system working
///
/// The chain reports a policy refusal as `MoveAbort(MoveLocation { module: ..., function_name:
/// Some("prove") }, 1)`, which is indistinguishable at a glance from a bug. It is the opposite: the
/// wallet's own limits stopped a spend that exceeded them, on chain, where no client can talk it
/// out of the answer.
///
/// Presenting that as a raw abort teaches whoever reads it that refusals look like crashes. So it
/// is named.
pub mod aborts {
    /// A refusal traced back to the rule that made it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RuleRefusal {
        pub module: String,
        pub code: u64,
        /// What the rule is for, in the words a person would use.
        pub meaning: &'static str,
    }

    impl std::fmt::Display for RuleRefusal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} refused it: {}", self.module, self.meaning)
        }
    }

    impl RuleRefusal {
        /// What to actually do about it.
        ///
        /// Generic advice is worse than none at the moment somebody is confused: telling the holder
        /// of a revoked wallet to "spend less" sends them to try smaller amounts forever, when
        /// nothing they can do to the amount will help. So the advice follows the refusal.
        pub fn advice(&self) -> &'static str {
            match (self.module.as_str(), self.code) {
                // Amount limits. A smaller number is a real answer here.
                ("budget", _) | ("per_tx", _) | ("rate_limit", _) => {
                    "The limit is on chain, not in this client — raising it here changes nothing, \
                     and neither will retrying with the same amount. Spend less, or have the \
                     wallet's owner attach different rules."
                }
                ("time_window", _) => {
                    "This is a time bound, not an amount. Retrying now will fail the same way; \
                     either wait for the window, or have the owner change it."
                }
                ("agent_wallet", 2) => {
                    "A revoked wallet cannot be un-revoked, and no capability still held will \
                     work on it. The funds went back to the owner when it was revoked."
                }
                ("agent_wallet", 3) => {
                    "The wallet has expired. Only its owner can move the expiry forward; nothing \
                     the agent does will."
                }
                ("agent_wallet", 1) => {
                    "This call is owner-only. Sign it with the owner's key — `--as <owner>`."
                }
                ("agent_wallet", 7) => {
                    "This call is the agent's. Sign it with the agent's key — `--as <agent>`."
                }
                ("agent_wallet", 10) => {
                    "The prove calls did not match the wallet's live policy. Read what it actually \
                     carries rather than assuming."
                }
                _ => "Nothing about the amount will change this one.",
            }
        }
    }

    /// Recognise a rule abort in a simulation or execution error.
    ///
    /// Returns `None` for anything that is not one — a gas failure, a missing object, a bug — so a
    /// caller never reports an unrelated failure as a policy decision.
    pub fn classify_rule_abort(error: &str) -> Option<RuleRefusal> {
        if !error.contains("MoveAbort") {
            return None;
        }
        let module = [
            "budget",
            "per_tx",
            "rate_limit",
            "time_window",
            "agent_wallet",
        ]
        .into_iter()
        .find(|m| error.contains(&format!("Identifier(\"{m}\")")))?;

        // The trailing `, N) in command` is the abort code.
        let code = error
            .rsplit_once("}, ")
            .and_then(|(_, tail)| tail.split(')').next())
            .and_then(|n| n.trim().parse::<u64>().ok())
            .unwrap_or(0);

        let meaning = match (module, code) {
            // Each rule module numbers its own aborts from 1, independently of agent_wallet's.
            ("budget", 1) => "this spend would exceed the wallet's total budget",
            ("per_tx", 1) => "this spend is larger than the per-transaction cap",
            ("rate_limit", 1) => {
                "this spend would exceed what may be spent in the current rolling window"
            }
            ("time_window", 1) => "the wallet is outside the window it is permitted to spend in",
            ("time_window", 2) => {
                "the time window itself is invalid — not_before is not before \
                                   not_after"
            }
            // agent_wallet's codes, transcribed from its own source and pinned by a test that
            // reads that source. An earlier version of this table was shifted by one across every
            // entry, which turned "you signed with the wrong key" into "your capability is wrong"
            // — sending whoever read it to inspect the one thing that was correct.
            ("agent_wallet", 1) => {
                "the sender is not this wallet's owner; owner-only calls must \
                                    be signed by the key that created it"
            }
            ("agent_wallet", 2) => "the wallet has been revoked",
            ("agent_wallet", 3) => "the wallet has expired",
            ("agent_wallet", 4) => "the wallet does not hold that much",
            ("agent_wallet", 5) => "that capability does not belong to this wallet",
            ("agent_wallet", 6) => "a spend of zero was requested",
            ("agent_wallet", 7) => {
                "the sender is not this wallet's agent; the spend path must be \
                                    signed by the agent's key, not the owner's"
            }
            ("agent_wallet", 8) => "an expiry may only be moved forward",
            ("agent_wallet", 9) => {
                "a rule was proved against a different wallet than the one \
                                    being spent from"
            }
            ("agent_wallet", 10) => {
                "the spend did not satisfy every rule attached to the wallet; \
                                     the prove calls must match the wallet's live policy exactly"
            }
            ("agent_wallet", 11) => "that rule is already attached; adding rules is not idempotent",
            _ => "a rule refused it",
        };

        Some(RuleRefusal {
            module: module.to_owned(),
            code,
            meaning,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The exact text a testnet node returned when a 0.06 SUI spend hit a 0.05 cap.
        const PER_TX: &str = "MoveAbort(MoveLocation { module: ModuleId { address: b02f39d6, \
             name: Identifier(\"per_tx\") }, function: 2, instruction: 21, \
             function_name: Some(\"prove\") }, 1) in command 2";

        #[test]
        fn a_per_tx_abort_is_named_as_the_cap_it_is() {
            let refusal = classify_rule_abort(PER_TX).expect("this is a rule abort");
            assert_eq!(refusal.module, "per_tx");
            assert_eq!(refusal.code, 1);
            assert!(refusal.to_string().contains("per-transaction cap"));
        }

        #[test]
        fn a_budget_abort_names_the_budget() {
            let error = PER_TX.replace("per_tx", "budget");
            let refusal = classify_rule_abort(&error).unwrap();
            assert_eq!(refusal.module, "budget");
            assert!(refusal.to_string().contains("total budget"));
        }

        /// Reporting an unrelated failure as a policy decision would be worse than saying nothing:
        /// it tells someone their limits are working when something else is broken.
        #[test]
        fn a_failure_that_is_not_a_rule_abort_is_not_dressed_up_as_one() {
            assert_eq!(classify_rule_abort("InsufficientGas"), None);
            assert_eq!(
                classify_rule_abort("Could not find the referenced object 0x20 at version None"),
                None
            );
            assert_eq!(
                classify_rule_abort(
                    "MoveAbort(MoveLocation { module: ModuleId { name: \
                     Identifier(\"pool\") } }, 7) in command 1"
                ),
                None,
                "an abort in someone else's module is not this wallet's policy"
            );
        }
    }
}

/// An object reference the node would not accept because the object has moved on.
///
/// # The node does not repair it, so it has to be named
///
/// Gas coins are read, then a transaction is built, simulated, signed and submitted. Anything
/// that touches a coin in between leaves the transaction naming a version that no longer exists,
/// and the node refuses it before execution. Two things were checked against testnet before this
/// module was written, because either would have made it unnecessary:
///
/// - `do_gas_selection` on the simulation request does **not** repair a stale reference. With
///   it on or off, the node answers the same `InvalidArgument`.
/// - It does not correct a price either: a price below the reference is refused with
///   `Gas price 999 under reference gas price (RGP) 1000`, and a price above it is echoed back.
///
/// So the fix is to read again, and the refusal has to say so. The verbatim text, from a testnet
/// node: `Error checking transaction input objects: Transaction needs to be rebuilt because
/// object 0xe863...e06e version 0x3bb012c3 (8GLu...1x2) is unavailable for consumption, current
/// version: 0x3bb012c4`. Left as it is, that reads like a bug in the builder. It is not: it is
/// the node saying "read this object again", and this module turns it into those words.
pub mod stale {
    /// Why an object reference was refused.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Staleness {
        /// The object exists at a newer version than the transaction named.
        VersionMoved {
            named: Option<u64>,
            current: Option<u64>,
        },
        /// The node could not find the object at all: consumed, merged away, or deleted.
        Gone,
    }

    /// One object the node would not take as an input.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct StaleObject {
        pub id: String,
        pub staleness: Staleness,
    }

    impl std::fmt::Display for StaleObject {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match &self.staleness {
                Staleness::VersionMoved {
                    named: Some(named),
                    current: Some(current),
                } => write!(
                    f,
                    "object {} changed since it was read: the transaction names version {named} \
                     and the chain is at version {current}, so another transaction spent or \
                     touched it in between",
                    self.id
                ),
                Staleness::VersionMoved { .. } => write!(
                    f,
                    "object {} changed since it was read: the transaction names a version the \
                     chain has moved past, so another transaction spent or touched it in between",
                    self.id
                ),
                Staleness::Gone => write!(
                    f,
                    "object {} could not be found at any version the node knows: it was consumed \
                     since it was read (a gas coin merged into another, an object deleted), or it \
                     never existed on this network",
                    self.id
                ),
            }
        }
    }

    /// Recognise a stale-reference refusal in a node's error text.
    ///
    /// Returns `None` for anything else, so a caller never dresses an unrelated refusal up as
    /// "read it again".
    pub fn classify_stale_object(error: &str) -> Option<StaleObject> {
        if let Some(after) = error.split_once("because object ").map(|(_, tail)| tail) {
            if error.contains("unavailable for consumption") {
                let (id, rest) = after.split_once(' ')?;
                let named = rest
                    .strip_prefix("version ")
                    .and_then(|r| r.split_whitespace().next())
                    .and_then(parse_version);
                let current = error
                    .rsplit_once("current version: ")
                    .and_then(|(_, tail)| tail.split_whitespace().next())
                    .and_then(parse_version);
                return Some(StaleObject {
                    id: id.to_owned(),
                    staleness: Staleness::VersionMoved { named, current },
                });
            }
        }
        if let Some(after) = error
            .split_once("Could not find the referenced object ")
            .map(|(_, tail)| tail)
        {
            let id = after.split_whitespace().next()?;
            return Some(StaleObject {
                id: id.to_owned(),
                staleness: Staleness::Gone,
            });
        }
        None
    }

    /// The node prints versions in hex (`0x3bb012c3`); a person expects decimal.
    fn parse_version(text: &str) -> Option<u64> {
        let cleaned = text.trim_end_matches(|c: char| !c.is_ascii_hexdigit());
        match cleaned.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => cleaned.parse().ok(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Verbatim from a testnet node, answering a simulation whose gas coin was one version
        /// behind. The same text came back with `do_gas_selection` on and off.
        const MOVED: &str = "Error checking transaction input objects: Transaction needs to be \
             rebuilt because object \
             0xe863baa3e00fceb19cfc1752118a61670f1a531fee3e42a10959e02bb317e06e version \
             0x3bb012c3 (8GLuzu8jr6WZZ45bwnGbA8euwEHabLtHgUR516uZ1x2) is unavailable for \
             consumption, current version: 0x3bb012c4";

        #[test]
        fn a_moved_version_is_named_with_both_versions_in_decimal() {
            let stale = classify_stale_object(MOVED).expect("this is a stale reference");
            assert_eq!(
                stale.id,
                "0xe863baa3e00fceb19cfc1752118a61670f1a531fee3e42a10959e02bb317e06e"
            );
            assert_eq!(
                stale.staleness,
                Staleness::VersionMoved {
                    named: Some(0x3bb0_12c3),
                    current: Some(0x3bb0_12c4),
                }
            );
            let text = stale.to_string();
            assert!(text.contains("1001394883"), "{text}");
            assert!(text.contains("1001394884"), "{text}");
            assert!(text.contains("changed since it was read"), "{text}");
        }

        #[test]
        fn a_missing_object_is_named_as_gone() {
            let stale =
                classify_stale_object("Could not find the referenced object 0x20 at version None")
                    .expect("a missing input is stale too");
            assert_eq!(stale.id, "0x20");
            assert_eq!(stale.staleness, Staleness::Gone);
            assert!(stale.to_string().contains("could not be found"));
        }

        /// A rule abort, a gas shortfall, or a price below the reference is not "read it again",
        /// and telling someone to re-run would send them in a loop.
        #[test]
        fn an_unrelated_refusal_is_not_dressed_up_as_stale() {
            assert_eq!(classify_stale_object("InsufficientGas"), None);
            assert_eq!(
                classify_stale_object(
                    "Error checking transaction input objects: Gas price 999 under reference \
                     gas price (RGP) 1000"
                ),
                None
            );
            assert_eq!(
                classify_stale_object(
                    "MoveAbort(MoveLocation { module: ModuleId { name: Identifier(\"per_tx\") } \
                     }, 1) in command 2"
                ),
                None
            );
        }
    }
}

#[cfg(test)]
mod advice_tests {
    use crate::aborts::classify_rule_abort;

    fn refusal(module: &str, code: u64) -> crate::aborts::RuleRefusal {
        classify_rule_abort(&format!(
            "MoveAbort(MoveLocation {{ module: ModuleId {{ name: Identifier(\"{module}\") }} }}, \
             {code}) in command 0"
        ))
        .expect("recognised")
    }

    /// The bug this exists for: a revoked wallet told its holder to spend less.
    #[test]
    fn a_revoked_wallet_is_not_told_to_spend_less() {
        let advice = refusal("agent_wallet", 2).advice();
        assert!(
            !advice.to_lowercase().contains("spend less"),
            "no amount will help a revoked wallet: {advice}"
        );
        assert!(advice.contains("revoked"));
    }

    /// And an amount limit still gets the advice that does help.
    #[test]
    fn an_amount_limit_is_told_to_spend_less() {
        assert!(refusal("per_tx", 1).advice().contains("Spend less"));
        assert!(refusal("budget", 1).advice().contains("Spend less"));
    }

    /// A wrong-key refusal names which key, because that is the entire fix.
    #[test]
    fn a_wrong_signer_is_told_which_key_to_use() {
        assert!(refusal("agent_wallet", 1).advice().contains("owner's key"));
        assert!(refusal("agent_wallet", 7).advice().contains("agent's key"));
    }

    /// A time bound is not an amount, and saying so stops someone retrying smaller forever.
    #[test]
    fn a_time_window_says_the_amount_is_not_the_problem() {
        let advice = refusal("time_window", 1).advice();
        assert!(advice.contains("time bound"));
        assert!(!advice.to_lowercase().contains("spend less"));
    }
}

#[cfg(test)]
mod error_tests {
    use super::ChainError;

    /// A missing digest and a missing field are different conditions and must not share a variant.
    ///
    /// They did. Callers handle pruning by moving on, which is right for a digest the node does not
    /// have and wrong for an answer that arrived without what the read mask asked for: the second is
    /// a fault in the request, and moving on turns it into a test that passes by skipping. Reducing
    /// a read mask to `["digest"]` made a live test skip every digest it was written to check.
    #[test]
    fn an_answer_missing_its_bytes_is_not_the_same_error_as_a_digest_the_node_lacks() {
        let absent = ChainError::NotFound("no such digest".into());
        let empty = ChainError::Malformed("answered without the bytes".into());
        assert_ne!(absent, empty);
        assert!(
            !matches!(empty, ChainError::NotFound(_)),
            "a caller matching NotFound to handle pruning must not also catch this"
        );
    }

    /// And the two must read differently, because the reader decides what to do next from the words.
    #[test]
    fn the_two_say_which_one_happened() {
        let absent = ChainError::NotFound("x".into()).to_string();
        let empty = ChainError::Malformed("x".into()).to_string();
        assert!(absent.contains("not found on chain"), "{absent}");
        assert!(
            empty.contains("fault in the request rather than a condition of the chain"),
            "the message has to place the blame, or a reader retries against a healthy node: {empty}"
        );
        assert!(
            !empty.contains("not found on chain"),
            "and it must not read as absence: {empty}"
        );
    }
}
