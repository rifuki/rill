//! What a pool will actually accept, read from the pool: tick size, lot size, minimum size.
//!
//! # Why two pools, and why nothing here is a copied number
//!
//! `BookParams` carries three numbers per pool and they differ by orders of magnitude: on testnet
//! DEEP/SUI takes nothing under 10 DEEP while SUI/DBUSDC takes nothing under 1 SUI, a hundredfold
//! gap. A single hardcoded set would be right for one pool and silently wrong for the other, which
//! is the reason these are read rather than declared.
//!
//! So this file asserts no literal parameter value. It reads both pools, then checks that
//! [`BookParams::check`] agrees with whatever the node said: the pool's own minimum is accepted, one
//! base unit under it is refused, one unit off its own lot grid is refused, and the neighbouring
//! values the refusal names are themselves accepted. Those hold whatever DeepBook relists, and they
//! fail the moment the checker and the pool disagree.
//!
//! The transaction and the decoding both come from `rill_ptb::book`, which is what `rill order`
//! uses. An earlier version of this test built its own copy of the PTB, so it could pass while the
//! command's copy was wrong.
//!
//!   cargo test -p rill-ptb --test book_params -- --ignored --nocapture

use rill_chain::grpc::GrpcSui;
use rill_chain::SuiRead;
use rill_ptb::book::{book_params_transaction, parse_book_params};
use rill_ptb::book_params::{BookParams, OrderConstraintError};
use rill_ptb::deepbook::PoolSpec;
use rill_ptb::registry::{pool_spec, DeepBookNetwork, TESTNET_PACKAGE_ID};
use rill_ptb::shared::SharedObjects;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// Ask the pool, through the same builder the command ships.
async fn read_params(chain: &GrpcSui, key: &str) -> (PoolSpec, BookParams) {
    let pool =
        pool_spec(DeepBookNetwork::Testnet, key).unwrap_or_else(|| panic!("{key} is listed"));

    let summary = chain
        .get_object(&pool.pool_id.to_string())
        .await
        .unwrap_or_else(|e| panic!("the {key} pool must exist on testnet: {e}"));
    let mut shared = SharedObjects::new();
    shared.insert(
        pool.pool_id,
        summary
            .shared_initial_version
            .expect("a DeepBook pool is shared"),
    );

    // Read, as production does: the node refuses a read priced below its reference.
    let gas_price = chain
        .reference_gas_price()
        .await
        .expect("the node reports its reference gas price");

    let tx = book_params_transaction(
        TESTNET_PACKAGE_ID.parse().expect("a package id"),
        &pool,
        &shared,
        gas_price,
    )
    .expect("build the parameter read");
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&tx).unwrap())
    };

    let outcome = chain.simulate_read(&b64).await.expect("the node answers");
    assert!(
        outcome.ok,
        "reading {key}'s parameters failed: {:?}",
        outcome.error
    );
    let returned: Vec<&[u8]> = outcome
        .command_returns
        .iter()
        .flatten()
        .map(Vec::as_slice)
        .collect();
    let params = parse_book_params(&returned).expect("three u64s");

    println!("\n{key}  {}", pool.pool_id);
    println!(
        "  scales   : base {} quote {}",
        pool.base_scalar, pool.quote_scalar
    );
    println!("  tick_size: {}", params.tick_size);
    println!("  lot_size : {}", params.lot_size);
    println!("  min_size : {}", params.min_size);

    (pool, params)
}

/// A price on the pool's grid, at least one tick, derived from the pool's own tick size.
fn price_on_grid(params: &BookParams) -> u64 {
    params.tick_size
}

/// The smallest quantity the pool accepts: its minimum, rounded up onto its own lot grid.
fn smallest_legal_quantity(params: &BookParams) -> u64 {
    let remainder = params.min_size % params.lot_size;
    if remainder == 0 {
        params.min_size
    } else {
        params.min_size + (params.lot_size - remainder)
    }
}

/// Everything the checker promises, checked against the numbers this pool actually reported.
fn the_checker_agrees_with(key: &str, params: &BookParams) {
    assert!(
        params.tick_size > 1 && params.lot_size > 1 && params.min_size > 0,
        "{key} reported {params:?}; a zero tick or lot turns the corresponding check off entirely, \
         and a pool that reports one has either been relisted or is not the pool this names"
    );

    let price = price_on_grid(params);
    let quantity = smallest_legal_quantity(params);
    params.check(price, quantity).unwrap_or_else(|e| {
        panic!("{key} rejects the smallest order it says it takes ({quantity} at {price}): {e}")
    });

    // One base unit under the floor. Refused, and the message names the floor the pool reported.
    let err = params.check(price, params.min_size - 1).unwrap_err();
    assert!(
        matches!(err, OrderConstraintError::BelowMinimum { .. }),
        "{key}: one unit under the floor must be refused as below the minimum, got {err}"
    );
    assert!(
        err.to_string().contains(&params.min_size.to_string()),
        "{key}: the refusal must name the pool's own minimum: {err}"
    );

    // One base unit off the quantity grid. Refused, and both neighbours it names are legal.
    let off_lot = quantity + 1;
    let err = params.check(price, off_lot).unwrap_err();
    assert!(
        matches!(err, OrderConstraintError::OffLot { .. }),
        "{key}: a quantity off the lot grid must be refused as off-lot, got {err}"
    );
    let message = err.to_string();
    let below = off_lot - (off_lot % params.lot_size);
    let above = below + params.lot_size;
    assert!(
        message.contains(&below.to_string()) && message.contains(&above.to_string()),
        "{key}: the refusal must name both neighbours {below} and {above}: {message}"
    );
    params
        .check(price, below)
        .unwrap_or_else(|e| panic!("{key}: the lower neighbour {below} must be legal: {e}"));
    params
        .check(price, above)
        .unwrap_or_else(|e| panic!("{key}: the upper neighbour {above} must be legal: {e}"));

    // One unit off the price grid.
    let err = params.check(price + 1, quantity).unwrap_err();
    assert!(
        matches!(err, OrderConstraintError::OffTick { .. }),
        "{key}: a price off the tick grid must be refused as off-tick, got {err}"
    );
    assert!(
        err.to_string().contains(&params.tick_size.to_string()),
        "{key}: the refusal must name the pool's own tick size: {err}"
    );

    println!("  checker agrees with {key} on all four of its own numbers.");
}

/// The two pools, read live, each checked against its own answer and then against the other's.
#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn two_pools_report_different_constraints_and_the_checker_follows_each() {
    let chain = GrpcSui::new(TESTNET).expect("connect");

    let (deep_sui, deep_sui_params) = read_params(&chain, "DEEP_SUI").await;
    let (sui_dbusdc, sui_dbusdc_params) = read_params(&chain, "SUI_DBUSDC").await;

    the_checker_agrees_with("DEEP_SUI", &deep_sui_params);
    the_checker_agrees_with("SUI_DBUSDC", &sui_dbusdc_params);

    assert_ne!(
        deep_sui.pool_id, sui_dbusdc.pool_id,
        "two pools, or this proves nothing about per-pool parameters"
    );
    assert_ne!(
        deep_sui_params, sui_dbusdc_params,
        "these two pools differ in every parameter; if the node now reports them identical, the \
         reason book_params is read per pool instead of hardcoded needs re-checking"
    );

    // The property that makes a constant impossible: an order that one pool accepts, the other
    // refuses outright. Which pool is the larger is read, not assumed.
    let (small, small_key, large, large_key) =
        if deep_sui_params.min_size < sui_dbusdc_params.min_size {
            (
                &deep_sui_params,
                "DEEP_SUI",
                &sui_dbusdc_params,
                "SUI_DBUSDC",
            )
        } else {
            (
                &sui_dbusdc_params,
                "SUI_DBUSDC",
                &deep_sui_params,
                "DEEP_SUI",
            )
        };
    let smallest = smallest_legal_quantity(small);
    small
        .check(price_on_grid(small), smallest)
        .unwrap_or_else(|e| panic!("{small_key} must accept its own smallest order: {e}"));
    let err = large
        .check(price_on_grid(large), smallest)
        .expect_err("the larger pool must refuse the smaller pool's smallest order");
    println!("\n{large_key} refuses {smallest}, which {small_key} accepts:\n  {err}");

    println!(
        "\nPASS: {} and {} disagree by a factor of {}, read from the chain, and the checker follows \
         each.",
        small_key,
        large_key,
        large.min_size / small.min_size.max(1)
    );
}

/// The order that landed is still legal by the numbers the pool reports today.
///
/// Digest `dxzyeAfW5eRdGUobBNUGeu2mnmaN4xyzY7J8dZxL5fZ` placed 10 DEEP at 0.004 SUI: quantity
/// 10_000_000 base units, price 4_000_000_000 in DeepBook's scaled form. If this ever fails, the
/// recorded order is no longer reproducible and `deepbook_order_live` will fail for the same reason.
#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn the_recorded_orders_numbers_are_still_on_the_pools_grid() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let (_, params) = read_params(&chain, "DEEP_SUI").await;

    params.check(4_000_000_000, 10_000_000).unwrap_or_else(|e| {
        panic!("the recorded order would be refused by DEEP_SUI as it stands today: {e}")
    });
    println!("\nPASS: the recorded order is still on this pool's grid.");
}
