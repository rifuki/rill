//! Structural inspection of the transaction itself — the checks Move cannot make.
//!
//! Move sees a `Coin<T>` leave the wallet and nothing about where it goes next. `allowed_packages`
//! is recorded on-chain and never asserted in `confirm_spend`, and it cannot be: by the time the
//! rule runs, the commands that spend the coin have not executed. That gap is the entire reason a
//! local signer exists, and this module is that gap's answer.
//!
//! The comparison is against an exact **sequence**, not a set. A transaction that calls every
//! approved target in a different order is a different transaction — and a set comparison would
//! wave through one that called `place_limit_order` before `confirm_spend`.

use crate::Rejection;

/// An address in the one spelling the chain uses.
///
/// Anything that does not parse as an address is left exactly as written — this is a comparison
/// helper, not a validator, and silently rewriting a string it does not understand would hide the
/// mismatch rather than resolve it.
fn normalise(id: &str) -> String {
    id.parse::<sui_sdk_types::Address>()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| id.to_owned())
}

/// Compare the transaction's Move call targets against the approved sequence.
///
/// Two failures are reported differently on purpose. An **off-scope** target names a call nobody
/// approved, which is the alarming case. A **sequence mismatch** means the calls were all approved
/// but arrived in a shape that was not — which is usually a compiler bug, not an attack, and an
/// operator reading the message should be able to tell those apart immediately.
pub fn check_target_sequence(expected: &[String], found: &[String]) -> Result<(), Rejection> {
    if let Some(off_scope) = found.iter().find(|t| !expected.contains(t)) {
        return Err(Rejection::OffScopeTarget(off_scope.clone()));
    }
    if expected != found {
        return Err(Rejection::TargetSequenceMismatch {
            expected: expected.to_vec(),
            found: found.to_vec(),
        });
    }
    Ok(())
}

/// Every object the transaction touches must have been approved.
///
/// One-directional by design: an approved object the transaction did not use is fine — the run-set
/// lists what *may* be touched, not what must be. An object that was not approved is not.
pub fn check_object_scope(approved: &[String], touched: &[String]) -> Result<(), Rejection> {
    // `0x6` and `0x0000…0006` are the same object and different strings. The chain always writes
    // the expanded form; a human, a config file and a constant write the short one. Comparing them
    // raw rejects a transaction for touching an object that was approved under its other spelling.
    let approved: Vec<String> = approved.iter().map(|o| normalise(o)).collect();
    let unexpected: Vec<String> = touched
        .iter()
        .filter(|o| !approved.contains(&normalise(o)))
        .cloned()
        .collect();
    if unexpected.is_empty() {
        Ok(())
    } else {
        Err(Rejection::ObjectSetMismatch { unexpected })
    }
}

/// The guards the envelope promised must be exactly the guards the transaction contains.
///
/// Equality in both directions, which is the only version that means anything. A missing guard is
/// an unprotected swap. An extra guard the envelope did not declare is a call nobody reviewed,
/// sitting in the middle of a money path.
pub fn check_guard_set(expected: &[String], found: &[String]) -> Result<(), Rejection> {
    let mut a = expected.to_vec();
    let mut b = found.to_vec();
    a.sort();
    b.sort();
    if a == b {
        Ok(())
    } else {
        Err(Rejection::GuardSetMismatch {
            expected: a,
            found: b,
        })
    }
}

/// After this spend, the wallet must still hold at least the reserve.
///
/// No on-chain rule expresses this. `budget` caps the total ever spent; nothing says "always leave
/// enough for the owner to get their funds out". It is the one balance check here that is not
/// duplicating something Move already does.
pub fn check_reserve(
    wallet_balance: u64,
    spend: u64,
    minimum_remaining: u64,
) -> Result<(), Rejection> {
    let remaining = wallet_balance.saturating_sub(spend);
    if remaining < minimum_remaining {
        return Err(Rejection::ReserveBreached {
            remaining,
            minimum: minimum_remaining,
        });
    }
    Ok(())
}

/// The agent cap held must be the wallet's currently active cap.
///
/// The reference does not check this, and the chain does. After `rotate_agent` a stale cap passes
/// every local check the reference makes and then aborts on-chain — burning gas and producing a
/// failure whose cause is nowhere in the local logs. This is the one place the reference's local
/// verification is weaker than Move's, so it is checked here.
pub fn check_active_cap(held_cap_id: &str, wallet_active_cap_id: &str) -> Result<(), Rejection> {
    if held_cap_id == wallet_active_cap_id {
        Ok(())
    } else {
        Err(Rejection::StaleCap {
            held: held_cap_id.to_owned(),
            active: wallet_active_cap_id.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_exact_sequence_passes() {
        assert!(check_target_sequence(&v(&["a", "b", "c"]), &v(&["a", "b", "c"])).is_ok());
    }

    /// The check a set comparison would miss.
    #[test]
    fn the_same_calls_in_a_different_order_are_refused() {
        assert!(matches!(
            check_target_sequence(&v(&["a", "b", "c"]), &v(&["a", "c", "b"])),
            Err(Rejection::TargetSequenceMismatch { .. })
        ));
    }

    #[test]
    fn an_unapproved_call_is_reported_as_off_scope_not_as_a_sequence_problem() {
        assert!(
            matches!(
                check_target_sequence(&v(&["a", "b"]), &v(&["a", "evil"])),
                Err(Rejection::OffScopeTarget(t)) if t == "evil"
            ),
            "an operator must be able to tell an unapproved call from a reordered one at a glance"
        );
    }

    #[test]
    fn a_truncated_sequence_is_refused() {
        assert!(check_target_sequence(&v(&["a", "b", "c"]), &v(&["a", "b"])).is_err());
    }

    #[test]
    fn an_unapproved_object_is_refused() {
        assert!(matches!(
            check_object_scope(&v(&["0x1"]), &v(&["0x1", "0x2"])),
            Err(Rejection::ObjectSetMismatch { .. })
        ));
    }

    #[test]
    fn an_unused_approved_object_is_fine() {
        assert!(
            check_object_scope(&v(&["0x1", "0x2"]), &v(&["0x1"])).is_ok(),
            "the run-set lists what may be touched, not what must be"
        );
    }

    #[test]
    fn a_missing_guard_is_refused() {
        assert!(check_guard_set(&v(&["g"]), &v(&[])).is_err());
    }

    #[test]
    fn an_extra_undeclared_guard_is_also_refused() {
        assert!(
            check_guard_set(&v(&[]), &v(&["surprise"])).is_err(),
            "a call nobody reviewed sitting in a money path is not made acceptable by being a guard"
        );
    }

    #[test]
    fn guard_order_does_not_matter() {
        assert!(check_guard_set(&v(&["a", "b"]), &v(&["b", "a"])).is_ok());
    }

    #[test]
    fn a_spend_that_breaches_the_reserve_is_refused() {
        assert!(matches!(
            check_reserve(1_000, 900, 200),
            Err(Rejection::ReserveBreached {
                remaining: 100,
                minimum: 200
            })
        ));
    }

    #[test]
    fn a_spend_that_leaves_exactly_the_reserve_is_allowed() {
        assert!(check_reserve(1_000, 800, 200).is_ok());
    }

    #[test]
    fn a_spend_larger_than_the_balance_does_not_wrap() {
        assert!(
            matches!(check_reserve(100, 500, 0), Ok(())),
            "saturating to zero is correct; wrapping to a huge remainder would pass the check"
        );
        assert!(check_reserve(100, 500, 1).is_err());
    }

    #[test]
    fn a_rotated_cap_is_refused() {
        assert!(matches!(
            check_active_cap("0xold", "0xnew"),
            Err(Rejection::StaleCap { .. })
        ));
    }

    #[test]
    fn the_active_cap_passes() {
        assert!(check_active_cap("0xsame", "0xsame").is_ok());
    }
}

#[cfg(test)]
mod address_form_tests {
    use super::*;

    /// The clock is written `0x6` in a constant and `0x0000…0006` by the chain.
    #[test]
    fn the_same_object_in_two_spellings_is_one_object() {
        let short = vec!["0x6".to_string()];
        let expanded =
            vec!["0x0000000000000000000000000000000000000000000000000000000000000006".to_string()];
        assert!(check_object_scope(&short, &expanded).is_ok());
        assert!(check_object_scope(&expanded, &short).is_ok());
    }

    /// Normalising must not turn two different objects into one.
    #[test]
    fn two_different_objects_stay_different() {
        let approved = vec!["0x6".to_string()];
        let touched =
            vec!["0x0000000000000000000000000000000000000000000000000000000000000007".to_string()];
        assert!(check_object_scope(&approved, &touched).is_err());
    }

    /// A string that is not an address is compared as written rather than quietly rewritten.
    #[test]
    fn something_that_is_not_an_address_is_left_alone() {
        let approved = vec!["not-an-address".to_string()];
        assert!(check_object_scope(&approved, &["not-an-address".to_string()]).is_ok());
        assert!(check_object_scope(&approved, &["other".to_string()]).is_err());
    }
}
