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

use rill_chain::stale::classify_stale_object;
use rill_chain::ChainError;

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
}
