//! The abort table against the Move source that produces the aborts.
//!
//! An abort code is a number, and a table mapping numbers to sentences is exactly the kind of thing
//! that is right when written and wrong after the next contract change. The first version of this
//! table was shifted by one across every `agent_wallet` entry — so "you signed with the wrong key"
//! read as "your capability is wrong", sending whoever hit it to inspect the one thing that was
//! correct.
//!
//! So the constants are read from the source rather than remembered.

use std::collections::BTreeMap;
use std::path::PathBuf;

fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../move/agent_wallet/sources")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"))
}

/// `const E_SOMETHING: u64 = 7;` -> ("E_SOMETHING", 7)
fn abort_codes(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("const E_")?;
            let (name, rest) = rest.split_once(':')?;
            let value = rest.split('=').nth(1)?.trim().trim_end_matches(';');
            Some((format!("E_{name}"), value.parse().ok()?))
        })
        .collect()
}

/// Every `agent_wallet` code the table claims, checked against the constant that defines it.
#[test]
fn the_agent_wallet_abort_table_matches_the_contract() {
    let codes = abort_codes(&source("agent_wallet.move"));

    // What each code means, in the order the source declares them. A change to the contract that
    // renumbers these fails here rather than mislabelling a refusal in front of a user.
    let expected: &[(&str, u64, &str)] = &[
        ("E_NOT_OWNER", 1, "owner"),
        ("E_REVOKED", 2, "revoked"),
        ("E_EXPIRED", 3, "expired"),
        ("E_INSUFFICIENT_FUNDS", 4, "does not hold that much"),
        ("E_BAD_CAP", 5, "capability does not belong"),
        ("E_ZERO_AMOUNT", 6, "zero"),
        ("E_NOT_AGENT", 7, "agent"),
        ("E_EXPIRY_NOT_FORWARD", 8, "forward"),
        ("E_WRONG_WALLET", 9, "different wallet"),
        ("E_RULE_NOT_SATISFIED", 10, "every rule"),
        ("E_RULE_ALREADY_SET", 11, "already attached"),
    ];

    for (name, code, phrase) in expected {
        assert_eq!(
            codes.get(*name).copied(),
            Some(*code),
            "{name} is not {code} in the Move source; the abort table is stale"
        );

        let error = format!(
            "MoveAbort(MoveLocation {{ module: ModuleId {{ address: b02f39d6, \
             name: Identifier(\"agent_wallet\") }}, function: 1, instruction: 1, \
             function_name: Some(\"request_spend\") }}, {code}) in command 0"
        );
        let refusal = rill_chain::aborts::classify_rule_abort(&error)
            .unwrap_or_else(|| panic!("{name} ({code}) must be recognised"));
        assert!(
            refusal.meaning.contains(phrase),
            "{name} ({code}) is explained as {:?}, which does not mention {phrase:?}",
            refusal.meaning
        );
    }
}

/// Each rule module numbers its aborts from 1 independently, so the table must key on both.
#[test]
fn each_rule_module_numbers_its_aborts_from_one() {
    for (module, name) in [
        ("budget", "E_OVER_BUDGET"),
        ("per_tx", "E_OVER_PER_TX"),
        ("rate_limit", "E_OVER_WINDOW"),
        ("time_window", "E_OUTSIDE_TIME_WINDOW"),
    ] {
        let codes = abort_codes(&source(&format!("rules/{module}.move")));
        assert_eq!(
            codes.get(name).copied(),
            Some(1),
            "{module}::{name} is expected to be 1"
        );
    }
}

/// The two tables must not be conflated: `agent_wallet` code 1 is a wrong signer, `budget` code 1
/// is an exceeded budget, and confusing them tells a user to fix the wrong thing.
#[test]
fn the_same_code_in_two_modules_means_two_different_things() {
    let owner = rill_chain::aborts::classify_rule_abort(
        "MoveAbort(MoveLocation { module: ModuleId { name: Identifier(\"agent_wallet\") } }, 1) in command 0",
    )
    .unwrap();
    let budget = rill_chain::aborts::classify_rule_abort(
        "MoveAbort(MoveLocation { module: ModuleId { name: Identifier(\"budget\") } }, 1) in command 1",
    )
    .unwrap();
    assert!(owner.meaning.contains("owner"));
    assert!(budget.meaning.contains("budget"));
    assert_ne!(owner.meaning, budget.meaning);
}
