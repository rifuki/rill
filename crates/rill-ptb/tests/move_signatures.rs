//! The Rust builder against the Move source it calls.
//!
//! This is the drift that killed the reference implementation: the signer required an entry point
//! the contract no longer had, and 746 passing tests said nothing about it, because both halves
//! were tested against their own idea of the contract rather than against each other.
//!
//! So these read `move/agent_wallet/sources/**` — the source in this repo, the one that matches the
//! deployed package — and check the emitted calls against it. A signature change that the builder
//! does not follow fails here, at the arity, rather than on chain as an abort code.

use std::path::PathBuf;

fn move_source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../move/agent_wallet/sources")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("the Move source must be carried in this repo: {path:?}: {e}"))
}

/// Everything between `public fun <name>(` and the matching `)`, with whitespace collapsed.
fn parameters_of(source: &str, function: &str) -> String {
    let needle = format!("public fun {function}");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("{function} must exist in the Move source"));
    let open = start + source[start..].find('(').expect("a parameter list");
    let mut depth = 0usize;
    let mut end = open;
    for (i, c) in source[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    source[open + 1..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `ctx` is supplied by the runtime, never by the caller, so it is not one of the arguments a PTB
/// command carries.
fn caller_supplied_arity(params: &str) -> usize {
    params
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| !p.starts_with("ctx:") && !p.starts_with("_ctx:"))
        .count()
}

#[test]
fn request_spend_takes_the_five_arguments_the_builder_emits() {
    let source = move_source("agent_wallet.move");
    let params = parameters_of(&source, "request_spend");
    assert_eq!(
        caller_supplied_arity(&params),
        5,
        "the builder emits [wallet, cap, version, amount, clock]; Move says: {params}"
    );
    // Order matters as much as count, and the builder's order is positional.
    for (position, expected) in ["wallet:", "cap:", "version:", "amount:", "clock:"]
        .iter()
        .enumerate()
    {
        let actual = params.split(',').nth(position).unwrap().trim();
        assert!(
            actual.starts_with(expected),
            "argument {position} of request_spend is {actual}, not {expected}"
        );
    }
}

#[test]
fn confirm_spend_takes_the_four_arguments_the_builder_emits() {
    let source = move_source("agent_wallet.move");
    let params = parameters_of(&source, "confirm_spend");
    assert_eq!(
        caller_supplied_arity(&params),
        4,
        "the builder emits [wallet, request, version, clock]; Move says: {params}"
    );
    for (position, expected) in ["wallet:", "req:", "version:", "clock:"].iter().enumerate() {
        let actual = params.split(',').nth(position).unwrap().trim();
        assert!(
            actual.starts_with(expected),
            "argument {position} of confirm_spend is {actual}, not {expected}"
        );
    }
}

/// The bug this file was written for.
///
/// `budget` and `per_tx` take three arguments; `rate_limit` and `time_window` take four, because
/// both decide against the current time. The builder emitted three for all of them, so any manifest
/// carrying a rate limit or a time window — exactly what a cautious owner writes — produced a call
/// with the wrong arity.
#[test]
fn each_rules_prove_takes_the_clock_exactly_when_the_manifest_says_it_does() {
    use rill_core::manifest::RuleKind;

    for (kind, module) in [
        (RuleKind::Budget, "budget"),
        (RuleKind::PerTx, "per_tx"),
        (RuleKind::RateLimit, "rate_limit"),
        (RuleKind::TimeWindow, "time_window"),
    ] {
        let source = move_source(&format!("rules/{module}.move"));
        let params = parameters_of(&source, "prove");
        let move_takes_clock = params.contains("clock:");
        assert_eq!(
            kind.prove_takes_clock(),
            move_takes_clock,
            "{module}::prove is ({params}), but the manifest says prove_takes_clock = {}",
            kind.prove_takes_clock()
        );

        let expected_arity = if move_takes_clock { 4 } else { 3 };
        assert_eq!(
            caller_supplied_arity(&params),
            expected_arity,
            "{module}::prove arity changed: {params}"
        );
    }
}

/// The reference's docs describe `spend()`. The deployed package this repo binds to does not have
/// it, and neither does this source — so if it ever reappears, that is a deployment change worth
/// stopping on rather than absorbing.
#[test]
fn the_superseded_spend_entry_point_is_absent() {
    let source = move_source("agent_wallet.move");
    assert!(
        !source.contains("public fun spend"),
        "this source is supposed to be the hot-potato generation, which has no spend()"
    );
    assert!(source.contains("public fun request_spend"));
    assert!(source.contains("public fun confirm_spend"));
}
