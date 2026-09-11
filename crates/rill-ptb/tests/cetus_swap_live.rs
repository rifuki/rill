//! A Cetus swap, against a real node, because nothing else catches the defect this covers.
//!
//! `router::swap` returns two coins. `TransactionBuilder::move_call` hands back one `Argument`
//! standing for the whole return tuple, and the adapter returned it as though it were the output
//! coin. Result arity is enforced by the VM and not by the builder, so `try_build` accepted that and
//! every offline test passed while a node answered
//! `CommandArgumentError { arg_idx: 0, kind: InvalidResultArity { result_idx: 2 } }`. The swap had
//! never executed.
//!
//! The first test below reproduces that, and the second shows the fix. Both are simulations: they
//! need a live node and no funds, which is the cheapest place this property can be observed at all.
//!
//! Addresses come from the Cetus SDK's own testnet configuration and are checked against the chain
//! rather than trusted: `router::swap`'s signature is read back in the first assertion.

use rill_chain::{grpc::GrpcSui, SuiRead};
use rill_ptb::cetus::{swap, Swap};
use rill_ptb::shared::SharedObjects;
use sui_crypto::SuiSigner;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{Argument, Function, ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// Cetus on testnet, from `@cetusprotocol/sui-clmm-sdk`'s own config.
const CLMM: &str = "0x5372d555ac734e272659136c2a0cd3227f9b92de67c80dc11250307268af2db8";
const INTEGRATE: &str = "0xab2d58dd28ff0dc19b18ab2c634397b785a38c342a8f5065ade5f53f9dbffa1c";
const GLOBAL_CONFIG: &str = "0xc6273f844b4bc258952c4e477697aa12c918c8e08106fac6b934811298c9820a";

/// A live H/SUI pool, from the same API that serves the Cetus front end.
const POOL: &str = "0xf9e61536e895436c70b583d96fe4928b991ba37d84c73905f568dbc0f8c78565";
const COIN_A: &str = "0xbcd2c79828a21415197804dc5d720e0cfadaef302c361ba0db572f257d6d6408::h::H";
const COIN_B: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";

/// Cetus's price bounds. A swap names the price it refuses to cross, and which end of the range that
/// is depends on the direction: up for B to A, down for A to B.
const MAX_SQRT_PRICE: u128 = 79_226_673_515_401_279_992_447_579_055;
#[allow(dead_code)]
const MIN_SQRT_PRICE: u128 = 4_295_048_016;

/// The key this reads the address of. Nothing here signs: every test is a simulation.
const SENDER: &str = "0xb649a075e07c7cf0baebeaa82150416218c63943e2e767fe93a24aa5c7ce64a9";

fn addr(s: &str) -> Address {
    s.parse().expect("a constant address")
}

/// The shared objects a swap touches, each at the initial version the node reports.
async fn shared_for(chain: &GrpcSui) -> SharedObjects {
    let mut shared = SharedObjects::new();
    for id in [GLOBAL_CONFIG, POOL, "0x6"] {
        let summary = chain
            .get_object(id)
            .await
            .unwrap_or_else(|e| panic!("reading {id}: {e}"));
        let initial = summary
            .shared_initial_version
            .unwrap_or_else(|| panic!("{id} is not shared"));
        shared.insert(addr(&summary.reference.id), initial);
    }
    shared
}

/// A transaction funded with the sender's largest SUI coin, and one split off it to swap.
async fn funded(chain: &GrpcSui, amount: u64) -> (TransactionBuilder, Argument) {
    let owned = chain
        .list_owned_objects(SENDER)
        .await
        .expect("the sender's objects");
    let sui_coin = "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";
    let mut coins: Vec<_> = owned
        .iter()
        .filter(|o| o.object_type.as_deref() == Some(sui_coin))
        .collect();
    assert!(!coins.is_empty(), "the sender holds no SUI");
    coins.sort_by_key(|o| o.reference.version);

    let mut tx = TransactionBuilder::new();
    tx.set_sender(addr(SENDER));
    tx.set_gas_budget(100_000_000);
    tx.set_gas_price(
        chain
            .reference_gas_price()
            .await
            .expect("the node's reference price"),
    );
    tx.add_gas_objects(
        coins
            .iter()
            .map(|o| {
                ObjectInput::owned(
                    addr(&o.reference.id),
                    o.reference.version,
                    o.reference.digest.parse::<Digest>().expect("a digest"),
                )
            })
            .collect::<Vec<_>>(),
    );
    let amount_arg = tx.pure(&amount);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount_arg]);
    let funded = split.into_iter().next().expect("one split coin");
    (tx, funded)
}

fn encode(tx: &sui_sdk_types::Transaction) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(tx).expect("bcs"))
}

/// The signature the whole defect turns on, read from the chain rather than assumed.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_pool_and_the_router_are_what_this_test_thinks_they_are() {
    let chain = GrpcSui::new(TESTNET).expect("a client");
    let pool = chain.get_object(POOL).await.expect("the pool");
    let object_type = pool.object_type.clone().expect("the pool has a type");
    assert!(
        object_type.starts_with(&format!("{CLMM}::pool::Pool<")),
        "the pool is not from the lineage this test configures: {object_type}"
    );
    assert!(
        object_type.contains(COIN_A) && object_type.contains(COIN_B),
        "the pool is not the pair this test swaps: {object_type}"
    );
    let signature = rill_chain::describe::describe_function(TESTNET, INTEGRATE, "router", "swap")
        .await
        .expect("router::swap must exist on the configured integrate package");
    let returns = format!("{signature:?}");
    assert_eq!(
        returns.matches("coin::Coin").count(),
        4,
        "router::swap takes two coins and returns two; if that changed, the arity this test is \
         about changed with it: {returns}"
    );
}

/// The defect, reproduced: one result where the call returned two.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn handing_a_swaps_whole_result_tuple_to_a_transfer_is_refused_by_the_node() {
    let chain = GrpcSui::new(TESTNET).expect("a client");
    let shared = shared_for(&chain).await;
    let (mut tx, funded_coin) = funded(&chain, 1_000_000).await;

    // Built by hand, the way the adapter did before it returned both coins: the bare result of the
    // move call, treated as one object.
    let zero = tx.move_call(
        Function::new(
            addr("0x2"),
            "coin".parse().expect("ident"),
            "zero".parse().expect("ident"),
        )
        .with_type_args(vec![COIN_A.parse().expect("a type tag")]),
        vec![],
    );
    let config = tx.object(shared.input(addr(GLOBAL_CONFIG), false).expect("config"));
    let pool = tx.object(shared.input(addr(POOL), true).expect("pool"));
    let clock = tx.object(shared.input(addr("0x6"), false).expect("clock"));
    let args = vec![
        config,
        pool,
        zero,
        funded_coin,
        tx.pure(&false),
        tx.pure(&true),
        tx.pure(&1_000_000u64),
        tx.pure(&0u128),
        tx.pure(&false),
        clock,
    ];
    let whole_tuple = tx.move_call(
        Function::new(
            addr(INTEGRATE),
            "router".parse().expect("ident"),
            "swap".parse().expect("ident"),
        )
        .with_type_args(vec![
            COIN_A.parse().expect("a type tag"),
            COIN_B.parse().expect("a type tag"),
        ]),
        args,
    );
    let to = tx.pure(&addr(SENDER));
    tx.transfer_objects(vec![whole_tuple], to);

    // The builder accepts it. That is the point: the check that matters is not here.
    let built = tx.try_build().expect("the builder accepts a single result");
    let outcome = chain
        .simulate(&encode(&built))
        .await
        .expect("the node answers");
    assert!(
        !outcome.ok,
        "a single argument standing for two results must be refused; if the node now accepts it, \
         the premise of this whole test file is gone"
    );
    let error = outcome.error.unwrap_or_default();
    assert!(
        error.contains("InvalidResultArity") || error.contains("arity"),
        "the refusal must be about arity rather than something incidental: {error}"
    );
    println!("  the old shape is refused: {error}");
}

/// And the fix: both coins taken, both placed.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn a_swap_that_places_both_returned_coins_simulates_cleanly() {
    let chain = GrpcSui::new(TESTNET).expect("a client");
    let shared = shared_for(&chain).await;
    let amount = 1_000_000u64;
    let (mut tx, funded_coin) = funded(&chain, amount).await;

    let out = swap(
        &mut tx,
        &Swap {
            integrate_package_id: addr(INTEGRATE),
            global_config_id: addr(GLOBAL_CONFIG),
            pool_id: addr(POOL),
            coin_type_a: COIN_A.into(),
            coin_type_b: COIN_B.into(),
            // Funding SUI, which is coin B, so the swap runs B to A and buys H.
            a2b: false,
            by_amount_in: true,
            amount,
            // The limit is a bound in the direction the price moves, so zero is only meaningful one
            // way round. Funding B buys A, which pushes the price up, so the bound is the maximum
            // Cetus allows; zero here is a limit already breached and the pool aborts 11 in
            // `flash_swap_internal` before any of the swap's own arithmetic runs. Going the other way
            // the bound is the minimum and zero is the right open value.
            sqrt_price_limit: MAX_SQRT_PRICE,
        },
        funded_coin,
        &shared,
    )
    .expect("the swap builds");

    let to = tx.pure(&addr(SENDER));
    tx.transfer_objects(out.both().to_vec(), to);

    let built = tx.try_build().expect("the swap builds");
    let outcome = chain
        .simulate(&encode(&built))
        .await
        .expect("the node answers");
    println!(
        "  verification={:?} gas={} error={:?}",
        outcome.verification, outcome.gas_used_mist, outcome.error
    );
    assert!(
        outcome.ok,
        "a swap that places both returned coins must simulate: {:?}",
        outcome.error
    );
}

/// A swap that actually lands, because a simulation is not a swap.
///
/// Submits, so it spends testnet SUI and leaves a digest. Everything it asserts is read back from the
/// effects rather than from what the builder intended: the balance changes say the funded side went
/// down and the bought side came up, which is the only statement that distinguishes a swap from a
/// transaction that merely succeeded.
#[tokio::test]
#[ignore = "submits a real transaction and spends testnet SUI"]
async fn a_swap_lands_on_testnet_and_the_balances_move_both_ways() {
    use rill_chain::SuiWrite;

    let chain = GrpcSui::new(TESTNET).expect("a client");
    let shared = shared_for(&chain).await;
    let amount = 1_000_000u64;
    let (mut tx, funded_coin) = funded(&chain, amount).await;

    let out = swap(
        &mut tx,
        &Swap {
            integrate_package_id: addr(INTEGRATE),
            global_config_id: addr(GLOBAL_CONFIG),
            pool_id: addr(POOL),
            coin_type_a: COIN_A.into(),
            coin_type_b: COIN_B.into(),
            a2b: false,
            by_amount_in: true,
            amount,
            sqrt_price_limit: MAX_SQRT_PRICE,
        },
        funded_coin,
        &shared,
    )
    .expect("the swap builds");
    let to = tx.pure(&addr(SENDER));
    tx.transfer_objects(out.both().to_vec(), to);
    let built = tx.try_build().expect("the swap builds");

    // The sui CLI's keystore, read and never written, and the entry selected by the address it
    // derives rather than by position. This is the same shape `delegation_live` uses: an ignored
    // live test already depends on this machine's state, and a key picked by file order is the
    // pattern this project disowns.
    let path = std::path::Path::new(&std::env::var("HOME").expect("HOME"))
        .join(".sui/sui_config/sui.keystore");
    let entries: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the sui keystore"))
            .expect("the keystore is a JSON array");
    let keypair = entries
        .iter()
        .filter_map(|e| sui_crypto::simple::SimpleKeypair::from_base64(e.trim()).ok())
        .find(|k| k.verifying_key().derive_address() == addr(SENDER))
        .unwrap_or_else(|| panic!("{SENDER} is not in {}", path.display()));
    let signature = keypair
        .sign_transaction(&built)
        .expect("the owner signs its own swap");
    let outcome = chain
        .execute(&encode(&built), &[signature.to_base64()])
        .await
        .expect("the node takes the transaction");

    println!("  digest : {}", outcome.digest);
    println!("  success: {}", outcome.success);
    for change in &outcome.balance_changes {
        println!("  {:>22}  {}", change.amount, change.coin_type);
    }
    assert!(outcome.success, "the swap must land: {:?}", outcome.error);

    // Amounts are strings on purpose: never a float, and never narrowed before the caller decides.
    let delta = |needle: &str| -> i128 {
        outcome
            .balance_changes
            .iter()
            .filter(|c| c.address == SENDER)
            .find(|c| c.coin_type.contains(needle))
            .map(|c| c.amount.parse::<i128>().expect("a signed integer"))
            .unwrap_or(0)
    };
    let bought = delta("::h::H");
    let spent = delta("::sui::SUI");
    assert!(
        bought > 0,
        "the bought side must go up, or this was not a swap: {:?}",
        outcome.balance_changes
    );
    assert!(
        spent < 0,
        "and the funded side must go down: {:?}",
        outcome.balance_changes
    );
    println!("  bought {bought} H for {} SUI including gas", -spent);
}
