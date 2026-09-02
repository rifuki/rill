//! Follow-up spike: the questions `gas_spike.rs` leaves open.
//!
//! A) Does `do_gas_selection` REPAIR a stale gas object ref, or only fill an empty one?
//! B) With `do_gas_selection` on, is a price below RGP still a hard gRPC error?
//! C) Does the returned bcs differ from the sent bytes ONLY in `gas_payment`?
//! D) Is the returned bcs actually signable and executable?
//!
//! ```sh
//! RILL_SPIKE_ADDRESS=0x... cargo test -p rill-chain --test gas_spike2 -- --ignored --nocapture
//! ```

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::{
    simulate_transaction_request::TransactionChecks, ListOwnedObjectsRequest,
    SimulateTransactionRequest,
};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const SUI_COIN_TYPE: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

fn spike_address() -> (String, Address) {
    let s = std::env::var("RILL_SPIKE_ADDRESS").expect("set RILL_SPIKE_ADDRESS");
    let a = s.parse().expect("address");
    (s, a)
}

async fn a_real_coin(client: &mut Client, owner: &str) -> (Address, u64, Digest) {
    let mut list = ListOwnedObjectsRequest::default();
    list.owner = Some(owner.to_owned());
    list.page_size = Some(10);
    list.object_type = Some(SUI_COIN_TYPE.to_owned());
    list.read_mask = Some(prost_types::FieldMask {
        paths: vec!["object_id".into(), "version".into(), "digest".into()],
    });
    let owned = client
        .state_client()
        .list_owned_objects(list)
        .await
        .expect("list")
        .into_inner();
    let coin = owned.objects.first().expect("a SUI coin");
    (
        coin.object_id().parse().unwrap(),
        coin.version(),
        coin.digest().parse().unwrap(),
    )
}

fn a_transfer(sender: Address, price: u64, budget: u64, gas: Vec<ObjectInput>) -> Vec<u8> {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(budget);
    tx.set_gas_price(price);
    let empty = gas.is_empty();
    if empty {
        tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
    } else {
        tx.add_gas_objects(gas);
    }
    let amount = tx.pure(&1_000u64);
    let g = tx.gas();
    let split = tx.split_coins(g, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let mut built = tx.try_build().expect("build");
    if empty {
        built.gas_payment.objects.clear();
    }
    bcs::to_bytes(&built).expect("bcs")
}

async fn simulate(
    client: &mut Client,
    bytes: Vec<u8>,
    selection: bool,
) -> Result<sui_rpc::proto::sui::rpc::v2::SimulateTransactionResponse, tonic::Status> {
    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(bytes.into());
    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(transaction);
    request.checks = Some(TransactionChecks::Enabled as i32);
    if selection {
        request.do_gas_selection = Some(true);
    }
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "transaction.transaction".into(),
            "transaction.effects".into(),
            "suggested_gas_price".into(),
        ],
    });
    client
        .execution_client()
        .simulate_transaction(request)
        .await
        .map(|r| r.into_inner())
}

/// A — a STALE gas ref. This is the exact failure the three-transaction sequence produces: the
/// version and digest were current when listed, and are not by the time the third transaction is
/// built.
#[tokio::test]
#[ignore = "requires network access"]
async fn does_gas_selection_repair_a_stale_gas_ref() {
    let (owner, sender) = spike_address();
    let mut client = Client::new(TESTNET).expect("client");
    let (id, version, digest) = a_real_coin(&mut client, &owner).await;
    println!("current ref: {id} v{version} {digest}");

    // The same coin, one version behind — what a client holds after any other transaction has
    // touched it.
    let stale = vec![ObjectInput::owned(id, version - 1, digest)];
    for selection in [false, true] {
        let bytes = a_transfer(sender, 1_000, 10_000_000, stale.clone());
        match simulate(&mut client, bytes, selection).await {
            Ok(response) => {
                let executed = response.transaction.as_ref();
                let payment = executed
                    .and_then(|e| e.transaction.as_ref())
                    .and_then(|t| t.gas_payment.as_ref());
                let status = executed
                    .and_then(|e| e.effects.as_ref())
                    .and_then(|e| e.status.as_ref())
                    .map(|s| (s.success, s.error.as_ref().and_then(|e| e.description.clone())));
                println!(
                    "stale ref, do_gas_selection={selection:<5} Ok  status={status:?}\n  \
                     payment objects={:?}",
                    payment.map(|p| p
                        .objects
                        .iter()
                        .map(|o| format!("{} v{:?}", o.object_id(), o.version))
                        .collect::<Vec<_>>())
                );
            }
            Err(s) => println!(
                "stale ref, do_gas_selection={selection:<5} ERR {:?}: {}",
                s.code(),
                s.message()
            ),
        }
    }
}

/// B — below-RGP price with gas selection on. Does the node correct the price, or still refuse?
#[tokio::test]
#[ignore = "requires network access"]
async fn below_rgp_with_gas_selection() {
    let (_, sender) = spike_address();
    let mut client = Client::new(TESTNET).expect("client");
    for price in [1u64, 999, 1_000, 2_000] {
        let bytes = a_transfer(sender, price, 10_000_000, vec![]);
        match simulate(&mut client, bytes, true).await {
            Ok(response) => {
                let payment = response
                    .transaction
                    .as_ref()
                    .and_then(|e| e.transaction.as_ref())
                    .and_then(|t| t.gas_payment.as_ref());
                println!(
                    "price {price:5} + selection: Ok  price back={:?} suggested={:?}",
                    payment.and_then(|p| p.price),
                    response.suggested_gas_price
                );
            }
            Err(s) => println!(
                "price {price:5} + selection: ERR {:?}: {}",
                s.code(),
                s.message()
            ),
        }
    }
}

/// C — the returned bytes, compared field by field against what was sent. Signing bytes a node
/// produced is only acceptable if the only thing it changed is the gas payment.
#[tokio::test]
#[ignore = "requires network access"]
async fn the_returned_bytes_differ_only_in_gas_payment() {
    let (_, sender) = spike_address();
    let mut client = Client::new(TESTNET).expect("client");
    let sent_bytes = a_transfer(sender, 1_000, 10_000_000, vec![]);
    let sent: sui_sdk_types::Transaction = bcs::from_bytes(&sent_bytes).expect("decode sent");

    let response = simulate(&mut client, sent_bytes, true).await.expect("sim");
    let back_bytes = response
        .transaction
        .as_ref()
        .and_then(|e| e.transaction.as_ref())
        .and_then(|t| t.bcs.as_ref())
        .and_then(|b| b.value.as_ref())
        .expect("bcs back");
    let back: sui_sdk_types::Transaction = bcs::from_bytes(back_bytes).expect("decode back");

    println!("sender      same: {}", sent.sender == back.sender);
    println!("expiration  same: {:?} vs {:?}", sent.expiration, back.expiration);
    println!(
        "kind        same: {}",
        bcs::to_bytes(&sent.kind).unwrap() == bcs::to_bytes(&back.kind).unwrap()
    );
    println!(
        "gas owner   {} -> {}",
        sent.gas_payment.owner, back.gas_payment.owner
    );
    println!(
        "gas price   {} -> {}",
        sent.gas_payment.price, back.gas_payment.price
    );
    println!(
        "gas budget  {} -> {}",
        sent.gas_payment.budget, back.gas_payment.budget
    );
    println!(
        "gas objects {} -> {}",
        sent.gas_payment.objects.len(),
        back.gas_payment.objects.len()
    );
    assert_eq!(
        bcs::to_bytes(&sent.kind).unwrap(),
        bcs::to_bytes(&back.kind).unwrap(),
        "the node must not have touched the commands"
    );
    assert_eq!(sent.sender, back.sender);
}

/// D — the whole point. Sign the bytes the node returned and submit them. Nothing short of this
/// proves the gas-selection path actually replaces client-side coin picking.
///
/// Costs a small amount of testnet SUI.
#[tokio::test]
#[ignore = "spends testnet SUI"]
async fn the_returned_bytes_can_be_signed_and_executed() {
    use sui_crypto::SuiSigner;
    let key = std::env::var("RILL_SPIKE_KEY").expect("set RILL_SPIKE_KEY to a suiprivkey1...");
    let keypair = sui_crypto::simple::SimpleKeypair::from_suiprivkey(key.trim()).expect("key");
    let sender = keypair.verifying_key().derive_address();
    println!("sender: {sender}");

    let mut client = Client::new(TESTNET).expect("client");
    let sent_bytes = a_transfer(sender, 1_000, 10_000_000, vec![]);
    let response = simulate(&mut client, sent_bytes, true).await.expect("sim");
    let back_bytes = response
        .transaction
        .as_ref()
        .and_then(|e| e.transaction.as_ref())
        .and_then(|t| t.bcs.as_ref())
        .and_then(|b| b.value.as_ref())
        .expect("bcs back")
        .to_vec();
    let back: sui_sdk_types::Transaction = bcs::from_bytes(&back_bytes).expect("decode");
    println!("gas objects chosen by the node: {}", back.gas_payment.objects.len());

    let signature = keypair.sign_transaction(&back).expect("sign");

    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(back_bytes.into());
    let mut request = sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest::default();
    request.transaction = Some(transaction);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec!["digest".into(), "effects".into()],
    });
    let mut sig = sui_rpc::proto::sui::rpc::v2::UserSignature::default();
    {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(signature.to_base64())
            .unwrap();
        sig.bcs = Some(bytes.into());
    }
    request.signatures = vec![sig];

    match client.execution_client().execute_transaction(request).await {
        Ok(response) => {
            let response = response.into_inner();
            let executed = response.transaction.as_ref();
            println!("digest: {:?}", executed.and_then(|t| t.digest.clone()));
            println!(
                "status: {:?}",
                executed
                    .and_then(|t| t.effects.as_ref())
                    .and_then(|e| e.status.as_ref())
            );
        }
        Err(s) => panic!("execute failed {:?}: {}", s.code(), s.message()),
    }
}
