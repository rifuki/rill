//! Reconciling a wallet's rules to a manifest.
//!
//! The operation that did not exist: `add_rule` aborts on a rule already attached, so an attach
//! could only ever run once, and nothing could change or remove a limit.

use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::rules::{build_reconcile_rules, RuleTarget};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

fn addr(n: u8) -> Address {
    format!("0x{n:064x}").parse().unwrap()
}

fn resolved() -> SharedObjects {
    let mut shared = SharedObjects::new();
    shared.insert(addr(0x20), 400_020);
    shared.insert(addr(3), 400_003);
    shared
}

fn builder() -> TransactionBuilder {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(addr(9));
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(addr(0x0a), 1, Digest::ZERO)]);
    tx
}

fn target(rules: Vec<CapabilityRule>) -> RuleTarget {
    RuleTarget {
        package_id: addr(0xca),
        wallet_id: addr(0x20),
        version_id: addr(3),
        coin_type: "0x2::sui::SUI".into(),
        manifest: CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules,
        },
    }
}

fn budget(total: &str) -> CapabilityRule {
    CapabilityRule::Budget {
        total_mist: total.into(),
    }
}

fn per_tx(max: &str) -> CapabilityRule {
    CapabilityRule::PerTx {
        max_mist: max.into(),
    }
}

/// An empty wallet gets adds and nothing else.
#[test]
fn a_wallet_with_no_rules_only_gains_them() {
    let mut tx = builder();
    let result = build_reconcile_rules(
        &mut tx,
        &target(vec![budget("5000000000")]),
        &[],
        &resolved(),
    )
    .expect("should build");
    assert!(result.removed.is_empty());
    assert!(result.orphaned.is_empty());
    assert_eq!(result.added, vec!["budget"]);
    tx.try_build().expect("valid transaction");
}

/// Re-running an attach used to abort E_RULE_ALREADY_SET. Now it removes and re-adds, so the second
/// run succeeds — which is what makes the command safe to repeat after an ambiguous submit.
#[test]
fn re_running_an_attach_removes_before_it_adds() {
    let mut tx = builder();
    let result = build_reconcile_rules(
        &mut tx,
        &target(vec![budget("5000000000"), per_tx("1000000000")]),
        &["budget", "per_tx"],
        &resolved(),
    )
    .expect("should build");
    assert_eq!(result.removed, vec!["budget", "per_tx"]);
    assert_eq!(result.added, vec!["budget", "per_tx"]);
    assert!(result.is_no_change());
    tx.try_build().expect("valid transaction");
}

/// A rule the manifest no longer names is detached and not restored.
#[test]
fn a_rule_dropped_from_the_manifest_is_removed_and_not_re_added() {
    let mut tx = builder();
    let result = build_reconcile_rules(
        &mut tx,
        &target(vec![budget("5000000000")]),
        &["budget", "per_tx"],
        &resolved(),
    )
    .expect("should build");
    assert_eq!(result.orphaned, vec!["per_tx"]);
    assert_eq!(result.added, vec!["budget"]);
    assert!(!result.is_no_change());
}

/// Adding a rule to a wallet that already has others must not abort on the ones already there.
#[test]
fn adding_a_third_rule_does_not_trip_over_the_first_two() {
    let mut tx = builder();
    let result = build_reconcile_rules(
        &mut tx,
        &target(vec![
            budget("5000000000"),
            per_tx("1000000000"),
            CapabilityRule::RateLimit {
                window_ms: "3600000".into(),
                max_mist: "2000000000".into(),
            },
        ]),
        &["budget", "per_tx"],
        &resolved(),
    )
    .expect("should build");
    assert_eq!(result.removed, vec!["budget", "per_tx"]);
    assert_eq!(result.added, vec!["budget", "per_tx", "rate_limit"]);
    tx.try_build().expect("valid transaction");
}

/// An empty manifest is refused here as it is everywhere else: a wallet with no rules has no limits.
#[test]
fn an_empty_manifest_is_refused() {
    let mut tx = builder();
    assert!(build_reconcile_rules(&mut tx, &target(vec![]), &["budget"], &resolved()).is_err());
}

/// The removes must all precede the adds, or replacing a rule aborts E_RULE_ALREADY_SET with a
/// cause that points at the add rather than at the ordering.
#[test]
fn every_remove_is_emitted_before_every_add() {
    let mut tx = builder();
    build_reconcile_rules(
        &mut tx,
        &target(vec![budget("1"), per_tx("1")]),
        &["budget", "per_tx"],
        &resolved(),
    )
    .expect("should build");

    let built = tx.try_build().expect("valid transaction");
    let commands = format!("{:?}", built.kind);
    let last_remove = commands.rfind("remove").expect("removes were emitted");
    let first_add = commands.find("\"add\"").expect("adds were emitted");
    assert!(
        last_remove < first_add,
        "a replace aborts unless every remove precedes every add"
    );
}
