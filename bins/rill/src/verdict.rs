//! Rendering a chain error for the person who has to act on it.
//!
//! Three things come back from a simulation or a submission, and each calls for a different
//! response. A transport failure means nothing was learned: try again, or check the endpoint. A
//! refusal means the node read the transaction and would not run it. The most common refusal, an
//! object whose version moved between being read and being used, is fixed by running the command
//! again, because every command reads its objects afresh. The two must not share a sentence:
//! "the node did not answer", said of a refusal, sends whoever reads it to check a network that
//! is fine, while the transaction they hold will never work.
//!
//! # Why this is not the node's text
//!
//! The node's own words for a stale gas coin are `Transaction needs to be rebuilt because object
//! 0x... version 0x3bb012c3 (...) is unavailable for consumption, current version: 0x3bb012c4`.
//! That is accurate and reads like a bug in the builder. It is not one: nothing here re-uses a
//! reference across commands, so the only way to hold a stale one is for something else to have
//! spent the coin in between, and the answer is to run again.

use rill_chain::aborts::{classify_rule_abort, RuleRefusal};
use rill_chain::stale::classify_stale_object;
use rill_chain::ChainError;

/// Why a command did not happen: a rule refused it, by name, or something else went wrong.
///
/// # A refusal and a failure are different answers
///
/// A rule that refused is the wallet working, and the rule's name is the whole of what the caller
/// needs in order to decide what to do next: spend less, or stop asking. Anything else is a
/// failure whose words belong to the node.
///
/// Carried as a type rather than as one formatted string because the MCP layer has to put the
/// rule's name in a field. A response that said `"code": "refused"` and buried `per_tx` in prose
/// left an agent nothing to act on but a substring match, and an agent that cannot tell "the cap
/// stopped you" from "the node is down" retries the same amount forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A rule attached to the wallet refused it. Named, and carrying its own advice.
    Refused(RuleRefusal),
    /// Anything else: a bad argument, a node that did not answer, a submission that failed.
    Failed(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The rule first, then what to do about it. Byte for byte what the one call site that
            // did this by hand produced, because a person reads this line too, not only a caller
            // matching on its opening words.
            Self::Refused(refusal) => write!(f, "{refusal}.\n\n{}", refusal.advice()),
            Self::Failed(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Failure {}

/// Every `?` inside a command path hands back a sentence, and a sentence is a plain failure.
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

/// A simulation the chain ran and said would fail, with the rule named when a rule is what refused.
///
/// Used by every build path before it signs anything. The alternative, which each path used to do
/// for itself, was to quote the abort: `the chain says this would fail: MoveAbort(MoveLocation {
/// module: ModuleId { ... name: Identifier("per_tx") ... }, 1)`. That is the same information and
/// nobody reads it.
pub fn would_fail(error: Option<String>) -> Failure {
    let error = error.unwrap_or_else(|| "no reason given".into());
    named_or(&error, format!("the chain says this would fail: {error}"))
}

/// A transaction that was submitted and then failed on chain.
///
/// Rare by construction, because nothing is submitted that the chain has not already agreed would
/// execute. Not impossible: a wallet's budget can be consumed by another transaction in between,
/// and then the abort is a rule's and has to be named here as it would have been before signing.
pub fn did_fail(error: &str) -> Failure {
    named_or(error, format!("the transaction failed on chain: {error}"))
}

fn named_or(error: &str, sentence: String) -> Failure {
    match classify_rule_abort(error) {
        Some(refusal) => Failure::Refused(refusal),
        None => Failure::Failed(sentence),
    }
}

/// A simulation that produced no verdict: the node could not be reached, or it refused to run
/// the transaction at all.
pub fn no_verdict(error: ChainError) -> String {
    match error {
        ChainError::Rejected(message) => refused(&message, "before it ran"),
        other => format!("the node did not answer, so there is no verdict: {other}"),
    }
}

/// A submission the node did not take, or may have taken without saying so.
///
/// The transport case is not a refusal and must not read like one. A submission whose response was
/// lost may already be on chain, so the advice is to look before sending it again: a blind retry is
/// how one intended spend becomes two.
pub fn submit_failed(error: ChainError) -> String {
    match error {
        ChainError::Rejected(message) => refused(&message, "at submission"),
        other => format!(
            "the node did not answer the submission, so whether it landed is unknown: {other}. \
             Look for the transaction on chain before sending it again."
        ),
    }
}

fn refused(message: &str, when: &str) -> String {
    match classify_stale_object(message) {
        Some(stale) => format!(
            "{stale}. Run the command again: every object it uses is read afresh on each run."
        ),
        None => format!("the node refused it {when}: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE: &str = "Error checking transaction input objects: Transaction needs to be \
         rebuilt because object 0xe863 version 0x3bb012c3 (8GLu) is unavailable for consumption, \
         current version: 0x3bb012c4";

    /// The failure this module exists for: a refusal that read as an outage.
    #[test]
    fn a_stale_gas_coin_is_told_to_run_again_not_to_check_the_network() {
        let text = no_verdict(ChainError::Rejected(STALE.into()));
        assert!(text.contains("changed since it was read"), "{text}");
        assert!(text.contains("Run the command again"), "{text}");
        assert!(!text.contains("did not answer"), "{text}");
    }

    #[test]
    fn a_dropped_connection_is_still_an_absent_verdict() {
        let text = no_verdict(ChainError::Transport("connection reset".into()));
        assert!(text.starts_with("the node did not answer"), "{text}");
    }

    /// A refusal that is not staleness keeps the node's words, because inventing a cause for it
    /// would be worse than none.
    #[test]
    fn an_unrecognised_refusal_keeps_the_nodes_words() {
        let text = no_verdict(ChainError::Rejected(
            "Gas price 999 under reference gas price (RGP) 1000".into(),
        ));
        assert!(text.contains("refused it before it ran"), "{text}");
        assert!(text.contains("RGP"), "{text}");
    }

    /// The refusal the whole product turns on, rendered for the person reading it.
    ///
    /// An owner signing the agent's spend is stopped by Sui while it checks input objects, because
    /// the `AgentCap` belongs to the agent, so no Move code runs and there is no abort code to
    /// name. Calling that "the node did not answer" would send the reader to check a network that
    /// is fine, and hide the first of the two lines the delegation rests on.
    #[test]
    fn an_owner_signing_the_agents_spend_reads_as_a_refusal_not_an_outage() {
        let ownership = "Error checking transaction input objects: Transaction was not signed by              the correct sender: Object 0xea3f is owned by account address 0xb93c, but given              owner/signer address is 0xb649";
        let text = no_verdict(ChainError::Rejected(ownership.into()));
        assert!(text.contains("refused it before it ran"), "{text}");
        assert!(text.contains("is owned by account address"), "{text}");
        assert!(!text.contains("did not answer"), "{text}");
    }

    #[test]
    fn a_stale_coin_at_submission_gets_the_same_advice() {
        let text = submit_failed(ChainError::Rejected(STALE.into()));
        assert!(text.contains("Run the command again"), "{text}");
    }

    /// The exact text a testnet node returned when a 0.06 SUI spend hit a 0.05 cap.
    const PER_TX: &str = "MoveAbort(MoveLocation { module: ModuleId { address: b02f39d6, \
         name: Identifier(\"per_tx\") }, function: 2, instruction: 21, \
         function_name: Some(\"prove\") }, 1) in command 2";

    /// The refusal R3 turns on: the rule survives as a rule, not as quoted bytecode.
    #[test]
    fn a_rule_abort_in_a_simulation_is_named_rather_than_quoted() {
        let failure = would_fail(Some(PER_TX.into()));
        let Failure::Refused(refusal) = &failure else {
            panic!("a per_tx abort must be a refusal, not a failure: {failure}");
        };
        assert_eq!(refusal.module, "per_tx");
        assert_eq!(refusal.code, 1);
        assert!(
            failure.to_string().starts_with("per_tx refused it"),
            "{failure}"
        );
        assert!(
            failure.to_string().contains("Spend less"),
            "a named refusal carries its own advice: {failure}"
        );
    }

    /// Dressing an unrelated failure up as a policy decision tells someone their limits are
    /// working when something else is broken.
    #[test]
    fn a_simulation_failure_that_is_not_a_rule_keeps_the_nodes_words() {
        let failure = would_fail(Some("InsufficientGas".into()));
        assert_eq!(
            failure,
            Failure::Failed("the chain says this would fail: InsufficientGas".into())
        );
    }

    #[test]
    fn a_simulation_that_failed_for_no_stated_reason_says_that_rather_than_inventing_one() {
        assert_eq!(
            would_fail(None),
            Failure::Failed("the chain says this would fail: no reason given".into())
        );
    }

    /// A rule can refuse after the gate, when the wallet's state moved in between. It is still a
    /// rule, and still named.
    #[test]
    fn a_rule_abort_at_submission_is_named_too() {
        let failure = did_fail(PER_TX);
        assert!(matches!(failure, Failure::Refused(_)), "{failure}");
    }

    #[test]
    fn a_failure_at_submission_that_is_not_a_rule_says_it_failed_on_chain() {
        let failure = did_fail("UnusedValueWithoutDrop");
        assert_eq!(
            failure,
            Failure::Failed("the transaction failed on chain: UnusedValueWithoutDrop".into())
        );
    }
}
