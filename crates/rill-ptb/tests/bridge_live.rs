//! The refusal, against the bridge that is actually deployed.
//!
//! The offline tests use the five token types read from testnet on the day this was written. This one
//! reads the registry from the chain at run time, so the refusal is checked against what the bridge
//! says now rather than against a fixture. If the bridge registers SUI, this fails and the refusal in
//! `rill_ptb::bridge` needs revisiting rather than quietly becoming wrong.
//!
//!   cargo test -p rill-ptb --test bridge_live -- --ignored --nocapture

use rill_chain::{grpc::GrpcSui, SuiRead};
use rill_ptb::bridge::{carries, chain, send_token, Bridge, BridgeError, BRIDGE_OBJECT};
use rill_ptb::shared::SharedObjects;
use sui_transaction_builder::TransactionBuilder;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const BRIDGE_INNER: &str = "0x7e1cbb5e18bf371232f9efe1e954a0f80bd72533a9da06a347087c434e6224b9";
const SUI: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";

/// The bridge's token registry, read from its treasury.
async fn registry(chain: &impl SuiRead) -> Vec<String> {
    let fields = chain
        .list_dynamic_fields(BRIDGE_INNER)
        .await
        .expect("the bridge's inner object reads");
    let inner = fields
        .first()
        .and_then(|f| f.fields.clone())
        .expect("a BridgeInner field");
    let inner = inner.get("value").cloned().unwrap_or(inner);
    inner["treasury"]["id_token_type_map"]["contents"]
        .as_array()
        .expect("the token map")
        .iter()
        .filter_map(|e| e["value"].as_str().map(str::to_owned))
        .collect()
}

/// A SUI transfer is refused against the live registry, and the refusal lists what is carried.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn a_sui_transfer_is_refused_against_the_registry_as_it_is_now() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let carried = registry(&chain).await;
    println!("the bridge carries {} token type(s):", carried.len());
    for t in &carried {
        println!("  {t}");
    }

    let bridge = chain_bridge(SUI);
    let mut tx = TransactionBuilder::new();
    let amount = tx.pure(&1_000_000u64);
    let gas = tx.gas();
    let coin = tx
        .split_coins(gas, vec![amount])
        .into_iter()
        .next()
        .expect("a coin");

    let err = send_token(&mut tx, &bridge, &carried, coin, &shared(&chain).await)
        .expect_err("SUI is not carried, so this must be refused");
    assert!(
        matches!(err, BridgeError::TokenNotCarried { .. }),
        "refused for the wrong reason: {err:?}"
    );
    println!("\nrefusal:\n  {err}");
}

/// And a token the live registry does list builds, so the refusal is not refusing everything.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn a_token_the_live_registry_lists_builds() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let carried = registry(&chain).await;
    let usdc = carried
        .iter()
        .find(|t| t.ends_with("::usdc::USDC"))
        .expect("the bridge carries USDC")
        .clone();
    assert!(carries(&carried, &usdc));

    let mut tx = TransactionBuilder::new();
    let amount = tx.pure(&1_000_000u64);
    let gas = tx.gas();
    let coin = tx
        .split_coins(gas, vec![amount])
        .into_iter()
        .next()
        .expect("a coin");

    send_token(
        &mut tx,
        &chain_bridge(&usdc),
        &carried,
        coin,
        &shared(&chain).await,
    )
    .expect("a carried token to a real Ethereum address builds");
    println!("built a USDC transfer to Sepolia against the live registry");
}

fn chain_bridge(coin_type: &str) -> Bridge {
    Bridge {
        target_chain: chain::ETH_SEPOLIA,
        // A real-shaped Ethereum address. Nothing is submitted, so it goes nowhere.
        target_address: vec![0xab; 20],
        coin_type: coin_type.to_string(),
    }
}

/// The Bridge object's shared version, read rather than assumed.
async fn shared(chain: &impl SuiRead) -> SharedObjects {
    let bridge = chain
        .get_object(BRIDGE_OBJECT)
        .await
        .expect("the Bridge object reads");
    let initial = bridge
        .shared_initial_version
        .expect("the Bridge is a shared object");
    let mut shared = SharedObjects::new();
    shared.insert(BRIDGE_OBJECT.parse().expect("an address"), initial);
    shared
}
