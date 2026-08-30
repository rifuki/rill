//! The funding sequence: shape, ordering, and which rules produce a proof.

use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{
    build_manifest_gated_spend, expected_spend_targets, SpendError, WalletBinding,
};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const PKG: &str = "0x000000000000000000000000000000000000000000000000000000000000cafe";

fn addr(hex: &str) -> Address {
    hex.parse().expect("address")
}

/// Every shared object these fixtures reference, at a plausible non-zero initial version.
///
/// Never 0: a test that entered zero would pass while re-encoding the defect `SharedObjects`
/// exists to stop.
fn resolved() -> SharedObjects {
    let mut shared = SharedObjects::new();
    for n in 0x01u32..=0xffff {
        shared.insert(addr(&format!("0x{n:064x}")), 400_000 + n as u64);
    }
    shared
}

fn binding(rules: Vec<CapabilityRule>) -> WalletBinding {
    WalletBinding {
        package_id: addr(PKG),
        wallet_id: addr("0x0000000000000000000000000000000000000000000000000000000000000001"),
        cap: ObjectInput::owned(
            addr("0x0000000000000000000000000000000000000000000000000000000000000002"),
            1,
            Digest::ZERO,
        ),
        version_id: addr("0x0000000000000000000000000000000000000000000000000000000000000003"),
        coin_type: "0x2::sui::SUI".into(),
        manifest: CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules,
        },
    }
}

fn all_rule_kinds() -> Vec<CapabilityRule> {
    vec![
        CapabilityRule::Budget {
            total_mist: "5000000000".into(),
        },
        CapabilityRule::PerTx {
            max_mist: "1000000000".into(),
        },
        CapabilityRule::RateLimit {
            window_ms: "3600000".into(),
            max_mist: "2000000000".into(),
        },
        CapabilityRule::TimeWindow {
            not_before_ms: "1".into(),
            not_after_ms: "2".into(),
        },
        // Pre-flight kinds — these must produce no `prove` call.
        CapabilityRule::ProtocolScope {
            allowed_packages: vec![PKG.into()],
        },
        CapabilityRule::SlippageFloor {
            min_out_mist: "1".into(),
        },
        CapabilityRule::AssetScope {
            allowed_coin_types: vec!["0x2::sui::SUI".into()],
        },
        CapabilityRule::RecipientAllowlist {
            addresses: vec![PKG.into()],
        },
    ]
}

/// The sequence the signer pins against. Only the four on-chain rules contribute a proof; the
/// pre-flight ones appear nowhere, because a `prove` for a rule the chain does not hold would be
/// a call to a function that does not exist.
#[test]
fn only_on_chain_rules_contribute_a_proof_and_the_order_is_stable() {
    let targets = expected_spend_targets(&binding(all_rule_kinds())).unwrap();
    assert_eq!(
        targets,
        vec![
            format!("{PKG}::agent_wallet::request_spend"),
            format!("{PKG}::budget::prove"),
            format!("{PKG}::per_tx::prove"),
            format!("{PKG}::rate_limit::prove"),
            format!("{PKG}::time_window::prove"),
            format!("{PKG}::agent_wallet::confirm_spend"),
        ],
        "eight rules attached, four proofs emitted"
    );
}

/// Nothing here may emit the retired v2 entry point. That is the exact call the reference's signer
/// still demands, and the reason its generated run-sets can never validate.
#[test]
fn the_retired_spend_entry_point_is_never_emitted() {
    let targets = expected_spend_targets(&binding(all_rule_kinds())).unwrap();
    assert!(
        !targets.iter().any(|t| t.ends_with("::agent_wallet::spend")),
        "agent_wallet::spend was replaced by request_spend/confirm_spend and no longer exists"
    );
}

#[test]
fn a_single_rule_wallet_emits_three_calls() {
    let targets = expected_spend_targets(&binding(vec![CapabilityRule::Budget {
        total_mist: "1".into(),
    }]))
    .unwrap();
    assert_eq!(targets.len(), 3, "request, one proof, confirm");
}

#[test]
fn the_sequence_builds_into_a_real_transaction() {
    let b = binding(all_rule_kinds());
    let mut tx = TransactionBuilder::new();
    tx.set_sender(addr(
        "0x0000000000000000000000000000000000000000000000000000000000000009",
    ));
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(
        addr("0x000000000000000000000000000000000000000000000000000000000000000a"),
        1,
        Digest::ZERO,
    )]);

    let coin = build_manifest_gated_spend(&mut tx, &b, 1_000_000, &resolved())
        .expect("spend should build");
    // The released coin must be fully consumed, or execution aborts with UnusedValueWithoutDrop.
    let recipient = tx.pure(&b.wallet_id);
    tx.transfer_objects(vec![coin], recipient);

    tx.try_build()
        .expect("the sequence must produce a valid transaction");
}

#[test]
fn a_zero_spend_is_refused() {
    let b = binding(vec![CapabilityRule::Budget {
        total_mist: "1".into(),
    }]);
    let mut tx = TransactionBuilder::new();
    assert!(matches!(
        build_manifest_gated_spend(&mut tx, &b, 0, &resolved()),
        Err(SpendError::ZeroAmount)
    ));
}

/// A manifest with no rules would mean a spend nothing gates. The manifest layer refuses it, and
/// the refusal reaches here rather than producing a transaction with no proofs in it.
#[test]
fn a_manifest_with_no_rules_cannot_produce_a_spend() {
    let b = binding(vec![]);
    let mut tx = TransactionBuilder::new();
    assert!(matches!(
        build_manifest_gated_spend(&mut tx, &b, 1, &resolved()),
        Err(SpendError::Manifest(_))
    ));
    assert!(expected_spend_targets(&b).is_err());
}
