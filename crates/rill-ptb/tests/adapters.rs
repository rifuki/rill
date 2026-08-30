//! Cetus and Haedal, the two protocols beyond the DeepBook hero path.

use rill_ptb::cetus::{expected_swap_targets, swap, CetusError, Swap};
use rill_ptb::haedal::{expected_stake_targets, request_stake, HaedalError, Stake, MIN_STAKE_MIST};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

fn addr(n: u8) -> Address {
    format!("0x{:064x}", n).parse().unwrap()
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

fn a_swap(a2b: bool, amount: u64) -> Swap {
    Swap {
        integrate_package_id: addr(0xce),
        global_config_id: addr(0x30),
        pool_id: addr(0x31),
        coin_type_a: "0x2::sui::SUI".into(),
        coin_type_b:
            "0x00000000000000000000000000000000000000000000000000000000000000cd::usdc::USDC".into(),
        a2b,
        by_amount_in: true,
        amount,
        sqrt_price_limit: 79_226_673_515_401_279_992_447_579_055,
    }
}

#[test]
fn a_swap_builds_into_a_real_transaction() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000);
    let out = swap(&mut tx, &a_swap(true, 1_000_000), coin).expect("should build");
    let recipient = tx.pure(&addr(9));
    tx.transfer_objects(vec![out], recipient);
    tx.try_build().expect("valid transaction");
}

/// Both directions must build — the funded side moves, and only one zero coin is ever made.
#[test]
fn both_swap_directions_build() {
    for a2b in [true, false] {
        let mut tx = funded();
        let coin = a_coin(&mut tx, 1_000_000);
        let out = swap(&mut tx, &a_swap(a2b, 1_000_000), coin)
            .unwrap_or_else(|e| panic!("a2b={a2b}: {e}"));
        let recipient = tx.pure(&addr(9));
        tx.transfer_objects(vec![out], recipient);
        tx.try_build().unwrap_or_else(|e| panic!("a2b={a2b}: {e}"));
    }
}

#[test]
fn a_swap_of_zero_is_refused() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1);
    assert!(matches!(
        swap(&mut tx, &a_swap(true, 0), coin),
        Err(CetusError::ZeroAmount)
    ));
}

#[test]
fn a_swap_with_an_unparseable_coin_type_is_refused() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000);
    let mut bad = a_swap(true, 1_000);
    bad.coin_type_b = "not a type".into();
    assert!(matches!(
        swap(&mut tx, &bad, coin),
        Err(CetusError::BadIdentifier(_))
    ));
}

#[test]
fn the_swap_sequence_names_the_zero_coin_it_creates() {
    let targets = expected_swap_targets(addr(0xce));
    assert!(targets[0].ends_with("::coin::zero"));
    assert!(targets[1].ends_with("::router::swap"));
}

// ── haedal ──

fn a_stake(amount: u64) -> Stake {
    Stake {
        package_id: addr(0xad),
        staking_object_id: addr(0x40),
        validator: addr(0x41),
        amount_mist: amount,
    }
}

#[test]
fn a_stake_at_the_minimum_builds() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, MIN_STAKE_MIST);
    request_stake(&mut tx, &a_stake(MIN_STAKE_MIST), coin).expect("exactly one SUI is allowed");
    tx.try_build().expect("valid transaction");
}

/// Refused before anything is emitted: a transaction certain to abort wastes gas and reports its
/// cause as a Move abort code rather than anywhere a user is looking.
#[test]
fn a_stake_below_the_minimum_is_refused_before_any_command_is_emitted() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1);
    assert!(matches!(
        request_stake(&mut tx, &a_stake(MIN_STAKE_MIST - 1), coin),
        Err(HaedalError::BelowMinimum { .. })
    ));
}

#[test]
fn the_refusal_names_both_the_amount_and_the_floor() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1);
    let message = request_stake(&mut tx, &a_stake(500_000_000), coin)
        .unwrap_err()
        .to_string();
    assert!(message.contains("500000000"));
    assert!(message.contains(&MIN_STAKE_MIST.to_string()));
}

#[test]
fn the_stake_sequence_is_one_call() {
    assert_eq!(expected_stake_targets(addr(0xad)).len(), 1);
}

/// The composed flow the reference supports: swap output funds the stake.
#[test]
fn a_swap_can_fund_a_stake_in_one_transaction() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 2_000_000_000);
    let swapped = swap(&mut tx, &a_swap(false, 2_000_000_000), coin).expect("swap");
    request_stake(&mut tx, &a_stake(MIN_STAKE_MIST), swapped).expect("stake");
    tx.try_build().expect("the composed flow must build");
}
