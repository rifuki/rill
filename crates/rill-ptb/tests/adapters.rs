//! Cetus and Haedal, the two protocols beyond the DeepBook hero path.

use rill_ptb::cetus::{expected_swap_targets, swap, CetusError, Swap};
use rill_ptb::haedal::{expected_stake_targets, request_stake, HaedalError, Stake, MIN_STAKE_MIST};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

fn addr(n: u8) -> Address {
    format!("0x{:064x}", n).parse().unwrap()
}

/// Every shared object these fixtures reference, at a plausible non-zero initial version.
///
/// Deliberately never 0: a test that entered zero would pass while re-encoding the exact defect
/// `SharedObjects` exists to stop.
fn resolved() -> SharedObjects {
    let mut shared = SharedObjects::new();
    for n in 0x20u8..=0x80 {
        shared.insert(addr(n), 400_000 + n as u64);
    }
    shared
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
        // The bound that leaves the swap open, which is direction-dependent: a floor of zero going
        // A to B, the maximum going B to A. This fixture used the maximum in both directions, which
        // is the value that made the first real swap abort in the pool, so the fixture was building
        // a swap no chain would run.
        sqrt_price_limit: if a2b {
            0
        } else {
            rill_ptb::cetus::MAX_SQRT_PRICE
        },
    }
}

#[test]
fn a_swap_builds_into_a_real_transaction() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000);
    let out = swap(&mut tx, &a_swap(true, 1_000_000), coin, &resolved()).expect("should build");
    let recipient = tx.pure(&addr(9));
    tx.transfer_objects(out.both().to_vec(), recipient);
    tx.try_build().expect("valid transaction");
}

/// Both directions must build — the funded side moves, and only one zero coin is ever made.
#[test]
fn both_swap_directions_build() {
    for a2b in [true, false] {
        let mut tx = funded();
        let coin = a_coin(&mut tx, 1_000_000);
        let out = swap(&mut tx, &a_swap(a2b, 1_000_000), coin, &resolved())
            .unwrap_or_else(|e| panic!("a2b={a2b}: {e}"));
        let recipient = tx.pure(&addr(9));
        tx.transfer_objects(out.both().to_vec(), recipient);
        tx.try_build().unwrap_or_else(|e| panic!("a2b={a2b}: {e}"));
    }
}

#[test]
fn a_swap_of_zero_is_refused() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1);
    assert!(matches!(
        swap(&mut tx, &a_swap(true, 0), coin, &resolved()),
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
        swap(&mut tx, &bad, coin, &resolved()),
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
    request_stake(&mut tx, &a_stake(MIN_STAKE_MIST), coin, &resolved())
        .expect("exactly one SUI is allowed");
    tx.try_build().expect("valid transaction");
}

/// Refused before anything is emitted: a transaction certain to abort wastes gas and reports its
/// cause as a Move abort code rather than anywhere a user is looking.
#[test]
fn a_stake_below_the_minimum_is_refused_before_any_command_is_emitted() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1);
    assert!(matches!(
        request_stake(&mut tx, &a_stake(MIN_STAKE_MIST - 1), coin, &resolved()),
        Err(HaedalError::BelowMinimum { .. })
    ));
}

#[test]
fn the_refusal_names_both_the_amount_and_the_floor() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1);
    let message = request_stake(&mut tx, &a_stake(500_000_000), coin, &resolved())
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
///
/// And the residual has to go somewhere. The swap returns both sides, the stake takes the one that
/// was bought, and the funded side comes back holding whatever the swap did not spend. `Coin` has no
/// `drop`, so leaving it aborts with `UnusedValueWithoutDrop`: the composition is only legal once
/// both are placed, which is the thing returning a single argument hid.
#[test]
fn a_swap_can_fund_a_stake_in_one_transaction() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 2_000_000_000);
    let swapped = swap(&mut tx, &a_swap(false, 2_000_000_000), coin, &resolved()).expect("swap");
    request_stake(
        &mut tx,
        &a_stake(MIN_STAKE_MIST),
        swapped.output(),
        &resolved(),
    )
    .expect("stake");
    let to = tx.pure(&addr(9));
    tx.transfer_objects(vec![swapped.residual()], to);
    tx.try_build().expect("the composed flow must build");
}

/// The output is the side that was not funded, in both directions.
///
/// Reading the wrong one would stake the change rather than what was bought, and the types are the
/// same shape so nothing would complain until the amounts were looked at.
#[test]
fn the_output_is_the_side_that_was_not_funded() {
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000);
    let a2b = swap(&mut tx, &a_swap(true, 1_000_000), coin, &resolved()).expect("swap");
    // `Argument` is not PartialEq, so the debug rendering stands in. It carries the nested index,
    // which is the whole of what distinguishes the two results.
    assert_eq!(
        format!("{:?}", a2b.output()),
        format!("{:?}", a2b.coin_b),
        "funding A buys B, so B is the output"
    );
    assert_eq!(format!("{:?}", a2b.residual()), format!("{:?}", a2b.coin_a));

    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000);
    let b2a = swap(&mut tx, &a_swap(false, 1_000_000), coin, &resolved()).expect("swap");
    assert_eq!(
        format!("{:?}", b2a.output()),
        format!("{:?}", b2a.coin_a),
        "funding B buys A, so A is the output"
    );
    assert_eq!(format!("{:?}", b2a.residual()), format!("{:?}", b2a.coin_b));
    assert_ne!(
        format!("{:?}", b2a.coin_a),
        format!("{:?}", b2a.coin_b),
        "the two results must be distinct arguments, or this is one result read twice"
    );
}

/// A price bound on the wrong side of the direction is refused here, not by the pool.
///
/// The first swap this adapter ever built for real failed in the pool with
/// `MoveAbort(... flash_swap_internal, 11)`, which names neither the value nor the field. The value
/// was a zero bound on a B to A swap, where the price moves up and zero is already breached. A
/// refusal that names both numbers is the difference between a one-line fix and reading Cetus's
/// source.
#[test]
fn a_price_bound_on_the_wrong_side_is_refused_by_name() {
    use rill_ptb::cetus::{MAX_SQRT_PRICE, MIN_SQRT_PRICE};

    // Funding B buys A and pushes the price up, so the bound is a ceiling and zero is breached.
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000);
    let mut spec = a_swap(false, 1_000_000);
    spec.sqrt_price_limit = 0;
    let err = swap(&mut tx, &spec, coin, &resolved()).expect_err("zero is the wrong side here");
    let said = err.to_string();
    assert!(said.contains("pushes the price up"), "{said}");
    assert!(
        said.contains(&MAX_SQRT_PRICE.to_string()),
        "the refusal must name the value that would leave it open: {said}"
    );
    assert!(
        said.contains("flash_swap_internal"),
        "and the abort it prevents, so the next reader connects the two: {said}"
    );

    // And the mirror: funding A pushes the price down, so a ceiling is the wrong side.
    let mut tx = funded();
    let coin = a_coin(&mut tx, 1_000_000);
    let mut spec = a_swap(true, 1_000_000);
    spec.sqrt_price_limit = MAX_SQRT_PRICE;
    let said = swap(&mut tx, &spec, coin, &resolved())
        .expect_err("a ceiling is the wrong side for a2b")
        .to_string();
    assert!(said.contains("pushes the price down"), "{said}");
    assert!(said.contains("Use 0"), "{said}");

    // The open values in each direction build.
    for (a2b, limit) in [
        (true, 0u128),
        (false, MAX_SQRT_PRICE),
        (false, MIN_SQRT_PRICE),
    ] {
        let mut tx = funded();
        let coin = a_coin(&mut tx, 1_000_000);
        let mut spec = a_swap(a2b, 1_000_000);
        spec.sqrt_price_limit = limit;
        swap(&mut tx, &spec, coin, &resolved())
            .unwrap_or_else(|e| panic!("a2b={a2b} limit={limit} must build: {e}"));
    }
}
