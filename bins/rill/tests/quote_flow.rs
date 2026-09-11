//! What the quote reads from the pool, derives, and refuses.
//!
//! The figure itself is not computed here and cannot be checked here: it comes from the node running
//! Cetus's own code, and the fake's simulation reports whatever a test puts in it. What this file
//! covers is everything around that figure, which is where a quote can be wrong while still looking
//! right: the coin types, the direction, the floor, and the refusals.
//!
//! The figure's accuracy is established on chain instead. Quoted 759142891525 and filled
//! 759142891525 on testnet, exactly, in `Apeu7gyrqNic4C78hHYWQVdyWGygsMfYiaBVrJuGefJ1`. An earlier
//! version of this path computed the figure here from the pool's square-root price, and that was 11%
//! high against a real fill, then 36% high once the pool moved.

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_chain::{BalanceDelta, ObjectRef, ObjectSummary};
use rill_cli::keystore::Keystore;
use rill_cli::quote_cmd::{quote_json_on, QuoteArgs};
use serde_json::{json, Value};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Digest;

const PACKAGE: &str = "0x000000000000000000000000000000000000000000000000000000000000caf0";
const VERSION: &str = "0x0000000000000000000000000000000000000000000000000000000000000fff";
const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
const CAP: &str = "0x0000000000000000000000000000000000000000000000000000000000000cab";
const COIN: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";
const INTEGRATE: &str = "0x00000000000000000000000000000000000000000000000000000000000000ce";
const GLOBAL_CONFIG: &str = "0x0000000000000000000000000000000000000000000000000000000000000030";
const POOL: &str = "0x0000000000000000000000000000000000000000000000000000000000000031";
const CLOCK: &str = "0x6";
const CLMM: &str = "0x00000000000000000000000000000000000000000000000000000000000000c1";
const H: &str = "0x00000000000000000000000000000000000000000000000000000000000000cd::h::H";
const SUI: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

fn key(seed: u8) -> Keystore {
    let encoded = Ed25519PrivateKey::new([seed; 32])
        .to_suiprivkey()
        .expect("a key encodes");
    Keystore::from_suiprivkey(&encoded).expect("a keystore")
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

/// The rule type names a `policy_rules` read returns, BCS-encoded as a vector of strings.
fn type_names(modules: &[&str]) -> Vec<u8> {
    let mut out = vec![modules.len() as u8];
    for module in modules {
        let name = format!("{}::{module}::Rule", &PACKAGE[2..]);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }
    out
}

fn shared(id: &str, initial: u64, object_type: &str, fields: Option<Value>) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: initial + 1,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(object_type.to_owned()),
        fields,
        shared_initial_version: Some(initial),
    }
}

fn owned(id: &str, object_type: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: 9,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(object_type.to_owned()),
        fields: None,
        shared_initial_version: None,
    }
}

fn pool_fields() -> Value {
    json!({
        "current_sqrt_price": "16565176178191172",
        "fee_rate": "2500",
        "liquidity": "10000000000",
        "is_pause": false
    })
}

/// A pool whose type names the two coins in the pool's own order.
fn pool_object(coin_a: &str, coin_b: &str, fields: Option<Value>) -> ObjectSummary {
    shared(
        POOL,
        6,
        &format!("{CLMM}::pool::Pool<{coin_a}, {coin_b}>"),
        fields,
    )
}

/// A chain that answers every read, and whose simulation reports `out` of the bought coin.
fn chain(agent: &Keystore, pool: ObjectSummary, out: i128, bought: &str) -> FakeSui {
    FakeSui::new()
        .with_object(
            None,
            shared(
                WALLET,
                4,
                &format!("{PACKAGE}::agent_wallet::AgentWallet"),
                None,
            ),
        )
        .with_object(None, shared(VERSION, 3, "0x2::package::Version", None))
        .with_object(
            None,
            shared(GLOBAL_CONFIG, 5, "0xc1::config::GlobalConfig", None),
        )
        .with_object(None, pool)
        .with_object(None, shared(CLOCK, 1, "0x2::clock::Clock", None))
        .with_object(
            Some(&agent.address().to_string()),
            owned(CAP, &format!("{PACKAGE}::agent_wallet::AgentCap")),
        )
        .with_object(
            Some(&agent.address().to_string()),
            owned(COIN, SUI_COIN_TYPE),
        )
        .with_read_return(type_names(&["budget", "per_tx"]))
        .with_reference_gas_price(1_000)
        .with_simulation(SimulationBehavior::SucceedsWithBalances {
            gas_used_mist: 2_434_956,
            balance_changes: vec![
                BalanceDelta {
                    address: agent.address().to_string(),
                    coin_type: bought.to_owned(),
                    amount: out.to_string(),
                },
                BalanceDelta {
                    address: agent.address().to_string(),
                    coin_type: SUI.to_owned(),
                    amount: "-1002434".to_string(),
                },
            ],
        })
}

fn args(slippage_bps: u64) -> QuoteArgs {
    QuoteArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        wallet_id: WALLET.into(),
        cap_id: CAP.into(),
        integrate_package_id: INTEGRATE.into(),
        global_config_id: GLOBAL_CONFIG.into(),
        pool_id: POOL.into(),
        spend: "0.001".into(),
        slippage_bps,
        gas_budget: 50_000_000,
    }
}

/// The coin types and the direction come from the pool, and the floor from the simulation.
///
/// A caller passes a pool id and nothing about the pool. Before this, an agent had to be told both
/// coin types and which way round SUI was, and a transposed pair aborts inside Cetus with a type
/// mismatch that names neither coin.
#[test]
fn the_pool_supplies_the_types_and_the_direction() {
    let agent = key(3);
    let chain = chain(
        &agent,
        pool_object(H, SUI, Some(pool_fields())),
        1_000_000,
        H,
    );
    let out = run(quote_json_on(&chain, &agent, &args(100))).expect("a quote");

    assert_eq!(out["coinTypeA"], H);
    assert_eq!(out["coinTypeB"], SUI);
    assert_eq!(
        out["a2b"], false,
        "SUI is side B here, so the wallet spends B and a2b is false"
    );
    assert_eq!(out["boughtCoinType"], H);
    assert_eq!(out["expectedOut"], "1000000");
    assert_eq!(out["minOut"], "990000", "one percent below the simulation");
}

/// With SUI on side A the direction flips, and so does the coin the floor applies to.
///
/// One test with the pool built one way round would pass with the direction hardcoded.
#[test]
fn a_pool_with_sui_on_the_other_side_reverses_the_direction() {
    let agent = key(3);
    let chain = chain(
        &agent,
        pool_object(SUI, H, Some(pool_fields())),
        2_000_000,
        H,
    );
    let out = run(quote_json_on(&chain, &agent, &args(100))).expect("a quote");

    assert_eq!(out["a2b"], true, "SUI is side A, so the wallet spends A");
    assert_eq!(out["boughtCoinType"], H, "and buys the other side");
    assert_eq!(out["expectedOut"], "2000000");
}

/// The floor is the simulated figure less the slippage asked for, rounded down.
#[test]
fn the_floor_sits_below_the_simulated_figure_by_the_slippage_given() {
    let agent = key(3);
    for (bps, expected) in [(0u64, "1000000"), (100, "990000"), (5_000, "500000")] {
        let chain = chain(
            &agent,
            pool_object(H, SUI, Some(pool_fields())),
            1_000_000,
            H,
        );
        let out = run(quote_json_on(&chain, &agent, &args(bps))).expect("a quote");
        assert_eq!(out["minOut"], expected, "at {bps} bps");
    }
    // A nonsense tolerance is clamped rather than underflowing into an enormous floor.
    let chain = chain(
        &agent,
        pool_object(H, SUI, Some(pool_fields())),
        1_000_000,
        H,
    );
    let out = run(quote_json_on(&chain, &agent, &args(99_999))).expect("a quote");
    assert_eq!(out["minOut"], "0");
}

/// `swapArguments` is the rest of the swap call, carrying the floor the quote derived.
///
/// This is what makes the pair usable from a prompt: the result of one call is the input of the next,
/// with nothing about the pool retyped in between.
#[test]
fn the_quote_hands_back_the_swap_call_it_implies() {
    let agent = key(3);
    let chain = chain(
        &agent,
        pool_object(H, SUI, Some(pool_fields())),
        1_000_000,
        H,
    );
    let out = run(quote_json_on(&chain, &agent, &args(100))).expect("a quote");

    let swap = &out["swapArguments"];
    assert_eq!(swap["pool"], POOL);
    assert_eq!(swap["coinTypeA"], H);
    assert_eq!(swap["coinTypeB"], SUI);
    assert_eq!(swap["a2b"], false);
    assert_eq!(swap["amount"], "0.001");
    assert_eq!(
        swap["minOut"], out["minOut"],
        "the floor in the hand-off must be the floor that was quoted"
    );
    // Only the caller's own two ids are missing, because only the caller has them.
    for absent in ["wallet", "cap"] {
        assert!(
            swap.get(absent).is_none(),
            "{absent} belongs to the caller, not to the pool"
        );
    }
}

/// A pool with no SUI on either side is refused, naming both coins.
///
/// An agent wallet releases SUI. A quote against a pool it cannot fund would otherwise return a
/// number for a swap that could never be built.
#[test]
fn a_pool_the_wallet_cannot_fund_is_refused_by_name() {
    let agent = key(3);
    let other = "0x00000000000000000000000000000000000000000000000000000000000000ee::k::K";
    let chain = chain(&agent, pool_object(H, other, Some(pool_fields())), 1, H);
    let err = run(quote_json_on(&chain, &agent, &args(100)))
        .expect_err("a pool with no SUI side must be refused");
    assert!(err.contains("neither side of this pool is SUI"), "{err}");
    assert!(err.contains(H) && err.contains(other), "both coins: {err}");
}

/// A pool object that is not a pool, or arrives without its fields, is refused rather than guessed.
#[test]
fn a_pool_that_cannot_be_read_is_refused() {
    let agent = key(3);

    let not_a_pool = shared(
        POOL,
        6,
        "0x2::coin::Coin<0x2::sui::SUI>",
        Some(pool_fields()),
    );
    let err = run(quote_json_on(
        &chain(&agent, not_a_pool, 1, H),
        &agent,
        &args(100),
    ))
    .expect_err("a type with no two coin parameters must be refused");
    assert!(err.contains("not a Cetus pool"), "{err}");

    let fieldless = pool_object(H, SUI, None);
    let err = run(quote_json_on(
        &chain(&agent, fieldless, 1, H),
        &agent,
        &args(100),
    ))
    .expect_err("a pool read without its fields must be refused");
    assert!(err.contains("without its fields"), "{err}");
}

/// A simulation that reports no gain of the bought coin is refused, not quoted as zero.
///
/// Zero would become a floor of zero, which is the unprotected swap the floor exists to prevent, and
/// it would arrive looking like a quote.
#[test]
fn a_simulation_with_no_gain_is_refused_rather_than_quoted_as_zero() {
    let agent = key(3);
    // The bought coin does not appear in the balance changes at all.
    let chain = chain(&agent, pool_object(H, SUI, Some(pool_fields())), 5, SUI);
    let err = run(quote_json_on(&chain, &agent, &args(100)))
        .expect_err("no gain of the bought coin must be refused");
    assert!(err.contains("no gain of"), "{err}");
    assert!(
        err.contains(H),
        "naming the coin that did not arrive: {err}"
    );
}

/// The pool's own context comes back with the quote: fee, liquidity, and whether it is paused.
///
/// None of it is in a simulation, and all of it is what a caller needs to judge whether one percent
/// is a sensible tolerance for the size it is trading.
#[test]
fn the_pools_context_comes_back_with_the_figure() {
    let agent = key(3);
    let chain = chain(
        &agent,
        pool_object(H, SUI, Some(pool_fields())),
        1_000_000,
        H,
    );
    let out = run(quote_json_on(&chain, &agent, &args(100))).expect("a quote");
    assert_eq!(out["feeRateMillionths"], "2500");
    assert_eq!(out["liquidity"], "10000000000");
    assert_eq!(out["poolPaused"], false);
    assert_eq!(
        out["rulesProved"],
        json!(["budget", "per_tx"]),
        "and which rules the simulated spend had to satisfy"
    );
}
