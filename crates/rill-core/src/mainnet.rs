//! Why this refuses mainnet, in one place, including what it would take to stop refusing.
//!
//! The refusal existed in five copies, four in the signer's tool surface and one in its CLI, each
//! reading "Refusing to sign on mainnet without RILL_ALLOW_MAINNET=true." That named the override
//! and nothing else, so the only thing it told a reader was how to bypass it. An agent that reads
//! that and sets the variable has done exactly what the sentence suggested and exactly what the
//! gate exists to prevent.
//!
//! A refusal is a place to state a precondition, not a place to publish a workaround. So there is
//! one producer, it says what the variable actually asserts, and it points at the checklist where
//! the preconditions are written down and individually checkable.

/// The environment variable that allows a mainnet signature, and asserts a great deal more.
pub const ALLOW_MAINNET_VAR: &str = "RILL_ALLOW_MAINNET";

/// Where the preconditions are written down, each mapped to a check.
pub const CUTOVER_CHECKLIST: &str = "docs/MAINNET.md";

/// The refusal, for every surface that can be asked to sign on mainnet.
///
/// Deliberately longer than a refusal usually should be. The cost of the reader not understanding
/// this one is an unaudited Move contract holding other people's money, which is a different order
/// of mistake from a mistyped flag.
pub fn mainnet_refusal() -> String {
    format!(
        "Refusing to sign on mainnet. Setting {ALLOW_MAINNET_VAR}=true is not a configuration \
         step: it asserts that the contracts holding the money have been audited, that every \
         precondition in {CUTOVER_CHECKLIST} is green, and that a named person authorised the \
         cutover. None of that is true yet, and no test here can make it true. Testnet proves the \
         mechanism and costs nothing; mainnet is where a mistake is somebody's money."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal has to name the variable, because a reader who genuinely needs it must be able
    /// to find it, and must also name what setting it claims. One without the other is either a
    /// dead end or an invitation.
    #[test]
    fn the_refusal_names_the_override_and_what_it_asserts() {
        let said = mainnet_refusal();
        assert!(said.contains(ALLOW_MAINNET_VAR), "{said}");
        assert!(said.contains(CUTOVER_CHECKLIST), "{said}");
        for claim in ["audited", "authorised the cutover", "every precondition"] {
            assert!(
                said.contains(claim),
                "the refusal must say that the variable asserts {claim:?}: {said}"
            );
        }
    }

    /// It must not read as instructions for getting past it.
    #[test]
    fn the_refusal_does_not_read_as_a_workaround() {
        let said = mainnet_refusal();
        assert!(
            !said.contains(&format!("without {ALLOW_MAINNET_VAR}")),
            "\"without X\" is a sentence whose remedy is setting X: {said}"
        );
        assert!(
            said.contains("is not a configuration step"),
            "the reader has to be told the variable is a claim rather than a switch: {said}"
        );
    }
}
