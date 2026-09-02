//! The DeepBook path against the capability model it actually has.
//!
//! # One PTB has one sender
//!
//! `agent_wallet::request_spend` asserts `ctx.sender() == wallet.agent`. So a transaction that
//! releases coins from an agent wallet *and* funds a DeepBook BalanceManager is signed by the agent
//! — which means every DeepBook call in it must be one an agent can make without the manager
//! owner's key.
//!
//! DeepBook's answer is a pair of capabilities, and the shape of its own API is the evidence:
//! `deposit` takes no capability, and `deposit_with_cap` exists beside it taking one. A function
//! anyone could call would not need a delegated twin. The builder used the first for a long time,
//! which would have produced a transaction no single signer could sign.
//!
//!   cargo test -p rill-ptb --test deepbook_delegation -- --ignored --nocapture

use rill_chain::describe::describe_function;
use rill_ptb::registry::{MAINNET_PACKAGE_ID, TESTNET_PACKAGE_ID};

async fn arity(endpoint: &str, package: &str, function: &str) -> Option<usize> {
    describe_function(endpoint, package, "balance_manager", function)
        .await
        .ok()
        .map(|s| s.arity())
}

/// Every delegated call the order path needs exists, and takes the capability it is named for.
#[tokio::test]
#[ignore = "requires network access to Sui fullnodes"]
async fn the_delegated_calls_the_order_path_needs_all_exist() {
    for (network, package) in [
        ("testnet", TESTNET_PACKAGE_ID),
        ("mainnet", MAINNET_PACKAGE_ID),
    ] {
        let endpoint = format!("https://fullnode.{network}.sui.io:443");
        println!("\n{network}  {package}");

        // (function, arguments a PTB carries)
        //
        // The owner's forms take one argument fewer than the delegated ones, and that difference is
        // exactly the capability. If they ever take the same count, the distinction this file rests
        // on has gone and the builder needs re-reading.
        for (function, expected) in [
            ("deposit", 2usize),
            ("deposit_with_cap", 3),
            ("generate_proof_as_owner", 1),
            ("generate_proof_as_trader", 2),
            ("mint_deposit_cap", 1),
            ("mint_trade_cap", 1),
        ] {
            let found = arity(&endpoint, package, function)
                .await
                .unwrap_or_else(|| panic!("balance_manager::{function} must exist on {network}"));
            println!("  {function:26} {found} arg(s)");
            assert_eq!(
                found, expected,
                "balance_manager::{function} on {network} takes {found}, expected {expected}"
            );
        }

        let plain = arity(&endpoint, package, "deposit").await.unwrap();
        let delegated = arity(&endpoint, package, "deposit_with_cap").await.unwrap();
        assert_eq!(
            delegated,
            plain + 1,
            "the delegated deposit should differ from the plain one by exactly the capability; if \
             it no longer does, the reason the builder uses deposit_with_cap needs re-checking"
        );
    }
    println!("\nPASS: the delegated forms exist and carry one capability more than the owner's.");
}

/// The builder must not emit the owner's door.
///
/// A grep rather than a behavioural check, because the mistake it guards is a one-word edit that
/// compiles, simulates against a manager the sender happens to own, and fails only once an agent
/// and an owner are finally different addresses.
#[test]
fn the_order_builder_never_calls_the_owner_only_deposit() {
    let source = include_str!("../src/deepbook.rs");
    for line in source.lines() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !code.contains("ident(\"deposit\")"),
            "the builder emits balance_manager::deposit, which needs the manager owner's \
             signature — and request_spend in the same transaction needs the agent's:\n  {line}"
        );
        assert!(
            !code.contains("ident(\"generate_proof_as_owner\")"),
            "the builder emits generate_proof_as_owner, which needs the owner's signature:\n  {line}"
        );
    }
}
