//! Gas that is read, not assumed: the live half, against real nodes.
//!
//!   cargo test -p rill-chain --test gas_read_live -- --ignored --nocapture
//!
//! # The busy address
//!
//! `ListOwnedObjects` pages at fifty, and an address holding fewer than that never exercises the
//! page token. So a fixture address holds sixty SUI coins of 1_000_000 mist each, funded once from
//! the owner key in a single transaction (digest `36EatwMU5empUjsehhuXQ2v9rne9Wao9jhuQ1Xh8WwdZ`,
//! testnet). Its address is the SHA-256 of a sentence, and no key exists for it: coins sent there
//! stay, which is what makes the fixture durable. Sending them to the owner instead would have
//! undone it on the owner's next signed transaction, because every command here puts all of the
//! sender's SUI coins in the gas payment and the node smashes them into one.
//!
//! Set `RILL_BUSY_ADDRESS` to point these at another address that owns more than fifty objects.

use rill_chain::grpc::{GrpcSui, OWNED_OBJECTS_PAGE_SIZE};
use rill_chain::stale::{classify_stale_object, Staleness};
use rill_chain::{ChainError, SuiRead, Verification};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const MAINNET: &str = "https://fullnode.mainnet.sui.io:443";

/// `sha256("rill: gas pagination fixture, no key exists for this address")`.
const BUSY_ADDRESS: &str = "0xf2289b0dc93819ce00baacc1c0b5686f539d41a28cb05aa6eb426e83ef424c5a";

const SUI_COIN_TYPE: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

fn busy_address() -> String {
    std::env::var("RILL_BUSY_ADDRESS").unwrap_or_else(|_| BUSY_ADDRESS.to_owned())
}

fn is_sui_coin(o: &rill_chain::ObjectSummary) -> bool {
    o.object_type.as_deref() == Some(SUI_COIN_TYPE)
}

/// The scenario from the plan: an address with more than fifty owned objects yields a complete
/// gas set. Checked twice, at the production page size and at a page size of seven, so the token
/// path is exercised by the second even if a node ever grew its page above the first.
#[tokio::test]
#[ignore = "requires network access"]
async fn an_address_with_more_than_fifty_objects_yields_a_complete_gas_set() {
    let owner = busy_address();
    let chain = GrpcSui::new(TESTNET).expect("client");

    let through_the_trait = chain.list_owned_objects(&owner).await.expect("list");
    let coins = through_the_trait.iter().filter(|o| is_sui_coin(o)).count();
    println!(
        "{owner}\n  objects   : {} (page size {OWNED_OBJECTS_PAGE_SIZE})\n  SUI coins : {coins}",
        through_the_trait.len()
    );
    assert!(
        through_the_trait.len() > 50,
        "the fixture must hold more than one page; see the module note on funding it"
    );
    assert!(coins > 50, "and more than a page of them must be gas");

    let in_sevens = chain
        .list_owned_objects_paged(&owner, 7)
        .await
        .expect("list in pages of 7");
    println!("  in sevens : {} objects", in_sevens.len());
    assert_eq!(
        in_sevens.len(),
        through_the_trait.len(),
        "a different page size must not change what is owned"
    );
    let mut a: Vec<&str> = through_the_trait
        .iter()
        .map(|o| o.reference.id.as_str())
        .collect();
    let mut b: Vec<&str> = in_sevens.iter().map(|o| o.reference.id.as_str()).collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "the same objects, whichever way they were paged");
    a.dedup();
    assert_eq!(a.len(), through_the_trait.len(), "no object read twice");
}

/// The plan's verification: a build on a busy address selects gas correctly. Every SUI coin the
/// address holds goes into the payment, exactly as the CLI does it, the price is the one the node
/// reports, and the node (checks on, no signature) says the transaction would run.
#[tokio::test]
#[ignore = "requires network access"]
async fn a_build_on_a_busy_address_selects_gas_correctly() {
    let owner = busy_address();
    let sender: Address = owner.parse().expect("an address");
    let chain = GrpcSui::new(TESTNET).expect("client");

    let coins: Vec<_> = chain
        .list_owned_objects(&owner)
        .await
        .expect("list")
        .into_iter()
        .filter(is_sui_coin)
        .collect();
    assert!(
        coins.len() > 50,
        "the fixture must hold more than a page of gas"
    );
    let price = chain
        .reference_gas_price()
        .await
        .expect("the reference price");

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(price);
    tx.add_gas_objects(coins.iter().map(|c| {
        ObjectInput::owned(
            c.reference.id.parse().expect("an id from the chain"),
            c.reference.version,
            c.reference.digest.parse::<Digest>().expect("a digest"),
        )
    }));
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let built = tx.try_build().expect("build");
    assert_eq!(built.gas_payment.objects.len(), coins.len());
    assert_eq!(built.gas_payment.price, price);

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&built).expect("bcs"))
    };
    let outcome = chain.simulate(&b64).await.expect("the node answers");
    println!(
        "gas objects: {}  price: {price}  ok={} verification={:?} gas={} error={:?}",
        coins.len(),
        outcome.ok,
        outcome.verification,
        outcome.gas_used_mist,
        outcome.error
    );
    assert!(outcome.ok, "a transaction paying with every coin must run");
    assert_eq!(outcome.verification, Verification::Verified);
}

/// The scenario from the plan: a stale gas reference surfaces as a named error. The experiments
/// in `gas_selection_live.rs` showed the node does not repair one, so the production path must
/// classify it, and must not report it as a transport failure: the node was reached, and said
/// something definite.
#[tokio::test]
#[ignore = "requires network access"]
async fn a_stale_gas_reference_surfaces_as_a_named_error_not_a_transport_failure() {
    let owner = busy_address();
    let sender: Address = owner.parse().expect("an address");
    let chain = GrpcSui::new(TESTNET).expect("client");

    let coin = chain
        .list_owned_objects(&owner)
        .await
        .expect("list")
        .into_iter()
        .find(is_sui_coin)
        .expect("a SUI coin");
    let price = chain
        .reference_gas_price()
        .await
        .expect("the reference price");

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(price);
    // The coin as it was one version ago: what a client holds after anything else touched it.
    tx.add_gas_objects([ObjectInput::owned(
        coin.reference.id.parse().expect("an id"),
        coin.reference.version - 1,
        coin.reference.digest.parse::<Digest>().expect("a digest"),
    )]);
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let built = tx.try_build().expect("build");
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&built).expect("bcs"))
    };

    let error = match chain.simulate(&b64).await {
        Ok(outcome) => panic!("a stale reference must be refused, got {outcome:?}"),
        Err(e) => e,
    };
    println!("node said: {error}");
    let ChainError::Rejected(message) = &error else {
        panic!("a definite refusal must not be reported as transport: {error:?}");
    };
    let stale = classify_stale_object(message).expect("the refusal is a stale reference");
    println!("named as : {stale}");
    assert_eq!(stale.id, coin.reference.id);
    assert_eq!(
        stale.staleness,
        Staleness::VersionMoved {
            named: Some(coin.reference.version - 1),
            current: Some(coin.reference.version),
        }
    );
}

/// The scenario from the plan: the price used matches the network's reference price on both
/// networks. Read through the trait the build path uses, on each network, and both must answer.
#[tokio::test]
#[ignore = "requires network access"]
async fn the_reference_price_is_read_through_the_trait_on_both_networks() {
    let mut prices = Vec::new();
    for (name, endpoint) in [("testnet", TESTNET), ("mainnet", MAINNET)] {
        let chain = GrpcSui::new(endpoint).expect("client");
        let price = chain
            .reference_gas_price()
            .await
            .expect("the node reports its reference gas price");
        println!("{name}: reference gas price {price}");
        assert!(price > 0, "{name} must report a real price");
        prices.push(price);
    }
    // Not asserted equal or unequal: both are values the networks can change. What matters is
    // that each was read, and the build test on the fake proves the number read is the number
    // put on the transaction.
    println!("prices: {prices:?}");
}
