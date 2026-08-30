//! Reading a real DeepBook order book.
//!
//! Ignored by default. Run deliberately:
//!   cargo test -p rill-ptb --test book_live -- --ignored --nocapture

use rill_chain::grpc::GrpcSui;
use rill_chain::SuiRead;
use rill_ptb::book::{mid_price_transaction, parse_u64_return, MidPrice};
use rill_ptb::registry::{pool_spec, DeepBookNetwork, MAINNET_PACKAGE_ID, TESTNET_PACKAGE_ID};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::Address;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const MAINNET: &str = "https://fullnode.mainnet.sui.io:443";

/// Read the version an object was actually shared at.
///
/// The point of the whole exercise: this number cannot be assumed, and the previous code assumed
/// zero for every shared object in the system.
async fn resolve_shared(chain: &GrpcSui, id: Address) -> SharedObjects {
    let summary = chain
        .get_object(&id.to_string())
        .await
        .expect("the pool object must exist on this network");
    let version = summary
        .shared_initial_version
        .expect("a DeepBook pool is a shared object and must report an initial shared version");
    println!("shared: {id} first shared at version {version}");
    let mut shared = SharedObjects::new();
    shared.insert(id, version);
    shared
}

async fn read_mid_price(endpoint: &str, package: &str, network: DeepBookNetwork, key: &str) {
    let pool = pool_spec(network, key).unwrap_or_else(|| panic!("{key} is listed"));
    println!("pool  : {}", pool.pool_id);
    println!(
        "scales: base {} / quote {}",
        pool.base_scalar, pool.quote_scalar
    );

    let chain = GrpcSui::new(endpoint).expect("connect");
    let shared = resolve_shared(&chain, pool.pool_id).await;

    let tx = mid_price_transaction(
        package.parse().unwrap(),
        &pool,
        "0x6".parse().unwrap(),
        &shared,
    )
    .expect("build the read");

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&tx).unwrap())
    };

    // A read, not the gate: no payer and no owned inputs. See `SuiRead::simulate_read`.
    let outcome = chain.simulate_read(&b64).await.expect("simulate the read");
    println!(
        "ok={} outputs={} err={:?}",
        outcome.ok, outcome.command_output_count, outcome.error
    );
    assert!(outcome.ok, "reading a mid price must not fail");

    let bytes = outcome
        .command_returns
        .iter()
        .flatten()
        .next()
        .expect("a mid-price read must carry a return value");
    let raw = parse_u64_return(bytes).expect("mid_price returns a u64");

    let rendered = MidPrice {
        raw,
        base_scalar: pool.base_scalar,
        quote_scalar: pool.quote_scalar,
    }
    .to_decimal_string()
    .unwrap();

    println!("\n{key} mid price");
    println!("  raw u64  : {raw}");
    println!("  rendered : {rendered}");
    assert!(raw > 0, "a live pool must quote a non-zero mid price");
    println!("\nPASS: a live DeepBook mid price, read as an exact integer.");
}

#[tokio::test]
#[ignore = "requires network access to a Sui mainnet fullnode"]
async fn a_mainnet_mid_price_reads_back_as_an_exact_integer() {
    read_mid_price(
        MAINNET,
        MAINNET_PACKAGE_ID,
        DeepBookNetwork::Mainnet,
        "DEEP_SUI",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn a_testnet_mid_price_reads_back_as_an_exact_integer() {
    read_mid_price(
        TESTNET,
        TESTNET_PACKAGE_ID,
        DeepBookNetwork::Testnet,
        "SUI_DBUSDC",
    )
    .await;
}
