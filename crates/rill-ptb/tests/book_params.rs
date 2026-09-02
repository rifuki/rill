//! What a pool will actually accept: tick size, lot size, minimum size.
//!
//!   cargo test -p rill-ptb --test book_params -- --ignored --nocapture

use rill_chain::grpc::GrpcSui;
use rill_chain::SuiRead;
use rill_ptb::book::parse_u64_return;
use rill_ptb::registry::{pool_spec, DeepBookNetwork, TESTNET_PACKAGE_ID};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn the_pool_reports_what_it_will_accept() {
    let pool = pool_spec(DeepBookNetwork::Testnet, "DEEP_SUI").expect("listed");
    let chain = GrpcSui::new(TESTNET).expect("connect");

    let summary = chain
        .get_object(&pool.pool_id.to_string())
        .await
        .expect("the pool exists");
    let mut shared = SharedObjects::new();
    shared.insert(pool.pool_id, summary.shared_initial_version.unwrap());

    let mut tx = TransactionBuilder::new();
    tx.set_sender(Address::ZERO);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    let pool_arg = tx.object(shared.input(pool.pool_id, false).unwrap());
    tx.move_call(
        Function::new(
            TESTNET_PACKAGE_ID.parse().unwrap(),
            Identifier::new("pool").unwrap(),
            Identifier::new("pool_book_params").unwrap(),
        )
        .with_type_args(vec![
            pool.base_coin_type.parse().unwrap(),
            pool.quote_coin_type.parse().unwrap(),
        ]),
        vec![pool_arg],
    );
    tx.add_gas_objects([sui_transaction_builder::ObjectInput::owned(
        "0x1".parse().unwrap(),
        1,
        sui_sdk_types::Digest::ZERO,
    )]);
    let mut built = tx.try_build().expect("build");
    built.gas_payment.objects.clear();

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&built).unwrap())
    };
    let outcome = chain.simulate_read(&b64).await.expect("read");
    assert!(outcome.ok, "err={:?}", outcome.error);

    let values: Vec<u64> = outcome
        .command_returns
        .iter()
        .flatten()
        .map(|b| parse_u64_return(b).expect("u64"))
        .collect();
    println!("pool  : {}", pool.pool_id);
    println!(
        "scales: base {} quote {}",
        pool.base_scalar, pool.quote_scalar
    );
    println!("\npool_book_params -> {values:?}");
    println!("  tick_size : {}", values[0]);
    println!("  lot_size  : {}", values[1]);
    println!("  min_size  : {}", values[2]);
    println!(
        "\nSo the smallest order is {} base units = {} SUI, and quantities must be a multiple of {}.",
        values[2],
        values[2] as f64 / pool.base_scalar as f64,
        values[1]
    );
}
