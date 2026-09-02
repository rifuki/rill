//! Ground-truth spike for the three gas questions the audit raised.
//!
//! Nothing here asserts a value that a network can change under it. It prints what the fullnode
//! actually answers, so a claim about the gRPC surface is made from a run rather than from a
//! reading of the proto.
//!
//! ```sh
//! cargo test -p rill-chain --test gas_spike -- --ignored --nocapture
//! ```

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::{
    simulate_transaction_request::TransactionChecks, GetEpochRequest, ListOwnedObjectsRequest,
    SimulateTransactionRequest,
};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const MAINNET: &str = "https://fullnode.mainnet.sui.io:443";

const SUI_COIN_TYPE: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// Q1 — where the reference gas price actually comes from over gRPC.
#[tokio::test]
#[ignore = "requires network access"]
async fn reference_gas_price_from_get_epoch() {
    for endpoint in [TESTNET, MAINNET] {
        let mut client = Client::new(endpoint).expect("client");
        let mut request = GetEpochRequest::default();
        // epoch left None = the current epoch.
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec!["epoch".into(), "reference_gas_price".into()],
        });
        let response = client
            .ledger_client()
            .get_epoch(request)
            .await
            .expect("get_epoch")
            .into_inner();
        let epoch = response.epoch.expect("an epoch");
        println!(
            "{endpoint}\n  epoch               : {:?}\n  reference_gas_price : {:?}",
            epoch.epoch, epoch.reference_gas_price
        );
        assert!(
            epoch.reference_gas_price.is_some(),
            "the mask asked for it, so it must come back"
        );
    }
}

/// Q2 — the pagination and server-side type filter on ListOwnedObjects.
#[tokio::test]
#[ignore = "requires network access"]
async fn list_owned_objects_paginates_and_filters_server_side() {
    let sender = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let mut client = Client::new(TESTNET).expect("client");

    let mut page_token = None;
    let mut pages = 0;
    let mut coins = 0;
    loop {
        let mut request = ListOwnedObjectsRequest::default();
        request.owner = Some(sender.clone());
        request.page_size = Some(1); // deliberately tiny, to force the token path
        request.page_token = page_token.clone();
        request.object_type = Some(SUI_COIN_TYPE.to_owned());
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec![
                "object_id".into(),
                "version".into(),
                "digest".into(),
                "object_type".into(),
                "balance".into(),
            ],
        });
        let response = client
            .state_client()
            .list_owned_objects(request)
            .await
            .expect("list_owned_objects")
            .into_inner();
        pages += 1;
        for object in &response.objects {
            coins += 1;
            println!(
                "  page {pages}: {} v{} balance={} type={:?}",
                object.object_id(),
                object.version(),
                object.balance(),
                object.object_type
            );
            assert_eq!(
                object.object_type.as_deref(),
                Some(SUI_COIN_TYPE),
                "the server-side object_type filter must be exact"
            );
        }
        page_token = response.next_page_token;
        if page_token.is_none() || pages >= 5 {
            break;
        }
    }
    println!("pages walked: {pages}, coins seen: {coins}");
    assert!(coins > 0, "the spike address should own SUI");
}

/// Q3 — what `do_gas_selection` returns, and whether it removes the need to pick coins.
#[tokio::test]
#[ignore = "requires network access"]
async fn do_gas_selection_reports_the_payment_it_chose() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    // A transaction with an EMPTY gas payment: built against a placeholder, then cleared, which is
    // the same trick `policy_read::policy_rules_transaction` uses.
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000); // deliberately possibly-wrong, to see what comes back
    tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let mut built = tx.try_build().expect("build");
    built.gas_payment.objects.clear();

    let bytes = bcs::to_bytes(&built).expect("bcs");
    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(bytes.into());

    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(transaction);
    request.checks = Some(TransactionChecks::Enabled as i32);
    request.do_gas_selection = Some(true);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "transaction".into(),
            "command_outputs".into(),
            "suggested_gas_price".into(),
        ],
    });

    let response = client
        .execution_client()
        .simulate_transaction(request)
        .await
        .expect("simulate_transaction")
        .into_inner();

    println!("suggested_gas_price: {:?}", response.suggested_gas_price);
    let executed = response.transaction.as_ref().expect("a transaction back");
    println!("digest             : {:?}", executed.digest);
    match executed
        .transaction
        .as_ref()
        .and_then(|t| t.gas_payment.as_ref())
    {
        Some(payment) => {
            println!("gas_payment.owner  : {:?}", payment.owner);
            println!("gas_payment.price  : {:?}", payment.price);
            println!("gas_payment.budget : {:?}", payment.budget);
            println!("gas_payment.objects: {}", payment.objects.len());
            for object in &payment.objects {
                println!(
                    "  {} v{:?} {:?}",
                    object.object_id(),
                    object.version,
                    object.digest
                );
            }
        }
        None => println!("gas_payment        : ABSENT — do_gas_selection returned no payment"),
    }
    let status = executed.effects.as_ref().and_then(|e| e.status.as_ref());
    println!("status             : {status:?}");
}

/// The control: the same simulation WITHOUT `do_gas_selection`, so the difference is attributable.
#[tokio::test]
#[ignore = "requires network access"]
async fn simulation_without_gas_selection_for_comparison() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let mut built = tx.try_build().expect("build");
    built.gas_payment.objects.clear();

    let bytes = bcs::to_bytes(&built).expect("bcs");
    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(bytes.into());

    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(transaction);
    request.checks = Some(TransactionChecks::Enabled as i32);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec!["transaction".into(), "suggested_gas_price".into()],
    });

    match client
        .execution_client()
        .simulate_transaction(request)
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            println!("suggested_gas_price: {:?}", response.suggested_gas_price);
            let payment = response
                .transaction
                .as_ref()
                .and_then(|e| e.transaction.as_ref())
                .and_then(|t| t.gas_payment.as_ref());
            println!("gas_payment        : {payment:?}");
            let status = response
                .transaction
                .as_ref()
                .and_then(|e| e.effects.as_ref())
                .and_then(|e| e.status.as_ref());
            println!("status             : {status:?}");
        }
        Err(status) => println!("gRPC status: {:?} {}", status.code(), status.message()),
    }
}

/// A gas price BELOW the network's RGP, submitted for simulation, to see what the node says. This
/// is the failure the hardcoded `set_gas_price(1_000)` would produce if RGP ever moved above it.
#[tokio::test]
#[ignore = "requires network access"]
async fn a_gas_price_below_rgp_is_rejected() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    let mut list = ListOwnedObjectsRequest::default();
    list.owner = Some(sender_str.clone());
    list.page_size = Some(10);
    list.object_type = Some(SUI_COIN_TYPE.to_owned());
    list.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "object_id".into(),
            "version".into(),
            "digest".into(),
            "balance".into(),
        ],
    });
    let owned = client
        .state_client()
        .list_owned_objects(list)
        .await
        .expect("list")
        .into_inner();
    let coin = owned.objects.first().expect("a SUI coin");

    for price in [1u64, 999, 1_000] {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(sender);
        tx.set_gas_budget(10_000_000);
        tx.set_gas_price(price);
        tx.add_gas_objects([ObjectInput::owned(
            coin.object_id().parse().unwrap(),
            coin.version(),
            coin.digest().parse().unwrap(),
        )]);
        let amount = tx.pure(&1_000u64);
        let gas = tx.gas();
        let split = tx.split_coins(gas, vec![amount]);
        let recipient = tx.pure(&sender);
        tx.transfer_objects(split, recipient);
        let built = tx.try_build().expect("build");

        let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
        transaction.bcs = Some(bcs::to_bytes(&built).expect("bcs").into());
        let mut request = SimulateTransactionRequest::default();
        request.transaction = Some(transaction);
        request.checks = Some(TransactionChecks::Enabled as i32);

        match client
            .execution_client()
            .simulate_transaction(request)
            .await
        {
            Ok(response) => {
                let response = response.into_inner();
                let status = response
                    .transaction
                    .as_ref()
                    .and_then(|t| t.effects.as_ref())
                    .and_then(|e| e.status.as_ref());
                println!("price {price:5}: Ok, status={status:?}");
            }
            Err(status) => println!(
                "price {price:5}: {:?} — {}",
                status.code(),
                status.message()
            ),
        }
    }
}
