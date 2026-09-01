//! Creating an agent wallet: the refusals, then the real thing on testnet.

use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::create::{build_create_wallet, expected_create_targets, CreateError, NewWallet};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const NOW: u64 = 1_756_600_000_000;
const SUI: &str = "0x2::sui::SUI";

fn addr(n: u8) -> Address {
    format!("0x{n:064x}").parse().unwrap()
}

fn resolved() -> SharedObjects {
    let mut shared = SharedObjects::new();
    shared.insert(addr(3), 400_003);
    shared
}

fn manifest(rules: Vec<CapabilityRule>) -> CapabilityManifest {
    CapabilityManifest {
        wallet_coin_type: SUI.into(),
        rules,
    }
}

fn new_wallet(manifest: CapabilityManifest) -> NewWallet {
    NewWallet {
        package_id: addr(0xca),
        version_id: addr(3),
        agent: addr(9),
        expires_at_ms: NOW + 86_400_000,
        coin_type: SUI.into(),
        manifest,
    }
}

fn funded() -> TransactionBuilder {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(addr(9));
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(addr(0x0a), 1, Digest::ZERO)]);
    tx
}

fn a_coin(tx: &mut TransactionBuilder, amount: u64) -> sui_transaction_builder::Argument {
    let value = tx.pure(&amount);
    let gas = tx.gas();
    tx.split_coins(gas, vec![value]).into_iter().next().unwrap()
}

fn budget() -> Vec<CapabilityRule> {
    vec![CapabilityRule::Budget {
        total_mist: "5000000000".into(),
    }]
}

#[test]
fn a_wallet_with_rules_builds() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000_000);
    build_create_wallet(
        &mut tx,
        &new_wallet(manifest(budget())),
        coin,
        &resolved(),
        NOW,
    )
    .expect("a governed wallet must build");
    tx.try_build().expect("valid transaction");
}

/// The contract accepts an empty policy and `confirm_spend` then requires zero receipts — so a
/// wallet created that way holds real funds under no limits at all. Refused here.
#[test]
fn a_wallet_with_no_rules_is_refused_before_any_command_is_emitted() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000_000);
    let result = build_create_wallet(
        &mut tx,
        &new_wallet(manifest(vec![])),
        coin,
        &resolved(),
        NOW,
    );
    assert!(
        matches!(result, Err(CreateError::Manifest(_))),
        "an empty manifest must not reach the chain: {result:?}"
    );
}

#[test]
fn a_manifest_governing_a_different_coin_is_refused() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000_000);
    let mut wallet = new_wallet(manifest(budget()));
    wallet.coin_type = "0x2::usdc::USDC".into();
    assert!(matches!(
        build_create_wallet(&mut tx, &wallet, coin, &resolved(), NOW),
        Err(CreateError::CoinTypeMismatch { .. })
    ));
}

#[test]
fn an_expiry_already_past_is_refused_rather_than_minted() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000_000);
    let mut wallet = new_wallet(manifest(budget()));
    wallet.expires_at_ms = NOW - 1;
    let Err(CreateError::AlreadyExpired { .. }) =
        build_create_wallet(&mut tx, &wallet, coin, &resolved(), NOW)
    else {
        panic!("a wallet that cannot be used must not be funded");
    };
}

#[test]
fn the_expected_target_names_the_call_that_is_emitted() {
    assert_eq!(
        expected_create_targets(addr(0xca)),
        vec![format!("{}::agent_wallet::create_wallet", addr(0xca))]
    );
}

#[test]
fn an_unresolved_version_object_is_refused() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000_000);
    assert!(matches!(
        build_create_wallet(
            &mut tx,
            &new_wallet(manifest(budget())),
            coin,
            &SharedObjects::new(),
            NOW
        ),
        Err(CreateError::UnknownShared(_))
    ));
}
