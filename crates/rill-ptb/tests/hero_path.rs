//! The DeepBook order, the guard, and the two of them composed with the funding sequence.

use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::deepbook::{
    expected_order_targets, place_limit_order, DeepBookError, LimitOrder, PoolSpec,
};
use rill_ptb::guard::{assert_min_value, GuardError, GuardOutcome};
use rill_ptb::spend::{build_manifest_gated_spend, expected_spend_targets, WalletBinding};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const AGENT_WALLET_PKG: &str = "0x000000000000000000000000000000000000000000000000000000000000cafe";
const DEEPBOOK_PKG: &str = "0x000000000000000000000000000000000000000000000000000000000000dee9";
const GUARD_PKG: &str = "0x0000000000000000000000000000000000000000000000000000000000009a1d";
const SUI: &str = "0x2::sui::SUI";

fn addr(hex: &str) -> Address {
    hex.parse().expect("address")
}

fn owned(hex: &str) -> ObjectInput {
    ObjectInput::owned(addr(hex), 1, Digest::ZERO)
}

fn shared_id(n: u8) -> Address {
    addr(&format!("0x{:064x}", n))
}

fn funded_builder() -> TransactionBuilder {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(shared_id(9));
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(shared_id(10), 1, Digest::ZERO)]);
    tx
}

/// `DEEP_SUI` — base 1e6, quote 1e9. The pool shape whose 1e12 multiplier is where the reference's
/// float arithmetic lands a base unit off. It is listed on both testnet and mainnet.
fn deep_sui_pool() -> PoolSpec {
    PoolSpec {
        pool_id: shared_id(0x20),
        base_coin_type: "0xde::deep::DEEP".into(),
        quote_coin_type: SUI.into(),
        base_scalar: 1_000_000,
        quote_scalar: 1_000_000_000,
    }
}

fn order(price: &str, quantity: &str) -> LimitOrder {
    LimitOrder {
        pool: deep_sui_pool(),
        balance_manager_id: shared_id(0x21),
        trade_cap: owned("0x0000000000000000000000000000000000000000000000000000000000000022"),
        client_order_id: 1,
        price: price.into(),
        quantity: quantity.into(),
        is_bid: true,
        pay_with_deep: false,
    }
}

#[test]
fn an_order_builds_into_a_real_transaction() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1_000_000u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    place_limit_order(&mut tx, addr(DEEPBOOK_PKG), &order("2.5", "1.5"), coin)
        .expect("order should build");
    tx.try_build().expect("valid transaction");
}

/// The price the reference gets wrong. Building it must succeed and must use the exact integer —
/// if the conversion were lossy, this would be the transaction that carried the wrong price.
#[test]
fn the_price_the_reference_rounds_wrong_builds_exactly() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1_000_000u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    // 2362.123456 on a 1e12 multiplier: exact 2362123456000000, reference 2362123456000001.
    place_limit_order(
        &mut tx,
        addr(DEEPBOOK_PKG),
        &order("2362.123456", "1"),
        coin,
    )
    .expect("an exactly-representable price must build");
    tx.try_build().expect("valid transaction");
}

#[test]
fn a_price_that_cannot_be_represented_exactly_is_refused() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    // More precision than the 1e12 multiplier can carry.
    let too_precise = order("1.0000000000000001", "1");
    assert!(matches!(
        place_limit_order(&mut tx, addr(DEEPBOOK_PKG), &too_precise, coin),
        Err(DeepBookError::Amount { .. })
    ));
}

#[test]
fn a_float_shaped_price_is_refused_before_it_reaches_the_chain() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    assert!(matches!(
        place_limit_order(&mut tx, addr(DEEPBOOK_PKG), &order("1e-9", "1"), coin),
        Err(DeepBookError::Amount { .. })
    ));
}

// ── guard ──

#[test]
fn a_zero_floor_emits_nothing_and_says_so() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    assert_eq!(
        assert_min_value(&mut tx, Some(addr(GUARD_PKG)), coin, SUI, 0),
        Ok(GuardOutcome::NotRequested),
        "an assertion that can never fail is not protection, so none is emitted"
    );
}

#[test]
fn a_floor_without_a_guard_package_is_refused_not_skipped() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    assert!(
        matches!(
            assert_min_value(&mut tx, None, coin, SUI, 1_000),
            Err(GuardError::NoGuardPackage { .. })
        ),
        "silently dropping the only thing bounding a swap's loss is the worst available answer"
    );
}

#[test]
fn a_real_floor_is_enforced_and_leaves_the_coin_usable() {
    let mut tx = funded_builder();
    let coin = {
        let amount = tx.pure(&1_000_000u64);
        let gas = tx.gas();
        tx.split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap()
    };
    assert_eq!(
        assert_min_value(&mut tx, Some(addr(GUARD_PKG)), coin, SUI, 1_000),
        Ok(GuardOutcome::Enforced)
    );
    // The guard borrows the coin, so it is still spendable afterwards.
    let recipient = tx.pure(&shared_id(9));
    tx.transfer_objects(vec![coin], recipient);
    tx.try_build().expect("the coin survives the assertion");
}

// ── the two composed ──

/// The hero path: fund from the agent wallet through the rule sequence, then place the order.
#[test]
fn the_full_hero_path_builds_and_its_targets_are_the_pinned_sequence() {
    let binding = WalletBinding {
        package_id: addr(AGENT_WALLET_PKG),
        wallet_id: shared_id(1),
        cap: owned("0x0000000000000000000000000000000000000000000000000000000000000002"),
        version_id: shared_id(3),
        coin_type: SUI.into(),
        manifest: CapabilityManifest {
            wallet_coin_type: SUI.into(),
            rules: vec![
                CapabilityRule::Budget {
                    total_mist: "5000000000".into(),
                },
                CapabilityRule::PerTx {
                    max_mist: "2000000000".into(),
                },
            ],
        },
    };

    let mut tx = funded_builder();
    let coin = build_manifest_gated_spend(&mut tx, &binding, 1_000_000_000).expect("spend");
    place_limit_order(&mut tx, addr(DEEPBOOK_PKG), &order("2.5", "1"), coin).expect("order");
    tx.try_build()
        .expect("the hero path must produce a valid transaction");

    let mut targets = expected_spend_targets(&binding).unwrap();
    targets.extend(expected_order_targets(addr(DEEPBOOK_PKG)));
    assert_eq!(
        targets,
        vec![
            format!("{AGENT_WALLET_PKG}::agent_wallet::request_spend"),
            format!("{AGENT_WALLET_PKG}::budget::prove"),
            format!("{AGENT_WALLET_PKG}::per_tx::prove"),
            format!("{AGENT_WALLET_PKG}::agent_wallet::confirm_spend"),
            format!("{DEEPBOOK_PKG}::balance_manager::deposit"),
            format!("{DEEPBOOK_PKG}::balance_manager::generate_proof_as_trader"),
            format!("{DEEPBOOK_PKG}::pool::place_limit_order"),
        ],
        "this exact sequence is what the signer pins the transaction against"
    );
}
