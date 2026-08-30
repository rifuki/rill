//! U7 spike — the one assumption the plan could not close from documentation.
//!
//! Rill's whole model depends on simulating a transaction that **nobody has signed**, from a
//! server that holds no key. This test proves the Rust path does that against a real fullnode:
//! read a real coin, build an unsigned PTB spending it, hand that to `simulate_transaction`,
//! and get a classifiable result back — with no keypair anywhere in the process.
//!
//! It also exercises two of the nine Sui methods the whole system needs
//! (`list_owned_objects` and `simulate_transaction`), so a pass here de-risks `rill-chain`
//! well beyond the simulation question itself.
//!
//! Ignored by default so CI needs no network. Run it deliberately:
//!
//! ```sh
//! cargo test -p rill-chain --test simulate_spike -- --ignored --nocapture
//! ```

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::{
    simulate_transaction_request::TransactionChecks, ListOwnedObjectsRequest,
    SimulateTransactionRequest,
};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// A funded testnet address carried over from the reference deployment's public records.
/// Only its public object ids are used — nothing here needs, or has, its key.
const FUNDED_SENDER: &str = "0xf73e2dea746d9a7071ec5c49bfc2a75f73be5efd02212632e849217234e7ab46";

#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn simulates_an_unsigned_ptb_without_a_key() {
    let mut client = Client::new(TESTNET).expect("construct gRPC client");
    let sender: Address = FUNDED_SENDER.parse().expect("parse sender address");

    // ── 1. Read: find a SUI coin the sender owns, to pay for gas. ──────────────────
    let mut list = ListOwnedObjectsRequest::default();
    list.owner = Some(FUNDED_SENDER.to_string());
    list.page_size = Some(50);
    list.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "object_id".into(),
            "version".into(),
            "digest".into(),
            "object_type".into(),
        ],
    });

    let owned = client
        .state_client()
        .list_owned_objects(list)
        .await
        .expect("list_owned_objects should reach the fullnode")
        .into_inner();

    println!("owned objects returned: {}", owned.objects.len());

    let coin = owned
        .objects
        .iter()
        .find(|o| {
            o.object_type
                .as_deref()
                .is_some_and(|t| t.ends_with("::sui::SUI>") && t.contains("::coin::Coin<"))
        })
        .expect("sender should own at least one SUI coin on testnet");

    let coin_id: Address = coin.object_id().parse().expect("parse coin id");
    let coin_version = coin.version();
    let coin_digest: Digest = coin.digest().parse().expect("parse coin digest");
    println!("gas coin: {coin_id} v{coin_version}");

    // ── 2. Build an unsigned PTB. No keypair is constructed anywhere. ──────────────
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(coin_id, coin_version, coin_digest)]);

    // Split a coin and send it back to the sender. The transfer matters: a split result
    // left unused aborts with `UnusedValueWithoutDrop` — the same trap the reference
    // implementation documents for Cetus's zero-coin pattern. Worth noting that this
    // simulation catches it, which the old devInspect path did not.
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);

    let built = tx.try_build().expect("build an unsigned transaction");

    // ── 3. Simulate it. The request type has no `signatures` field at all. ─────────
    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(built.into());
    // Checks stay ENABLED — we want the fullnode's real verdict, not a bypass.
    request.checks = Some(TransactionChecks::Enabled as i32);

    let response = client
        .execution_client()
        .simulate_transaction(request)
        .await;

    match response {
        Ok(ok) => {
            let body = ok.into_inner();
            println!("\n--- simulate_transaction returned Ok ---");
            println!("command_outputs    : {}", body.command_outputs.len());
            println!("suggested_gas_price: {:?}", body.suggested_gas_price);
            if let Some(executed) = &body.transaction {
                if let Some(effects) = &executed.effects {
                    println!("status             : {:?}", effects.status);
                }
                println!("balance_changes    : {}", executed.balance_changes.len());
            }
            let status = body
                .transaction
                .as_ref()
                .and_then(|t| t.effects.as_ref())
                .and_then(|e| e.status.as_ref())
                .expect("simulation should report an execution status");
            assert_eq!(
                status.success,
                Some(true),
                "a well-formed unsigned PTB should simulate successfully: {:?}",
                status.error
            );
            println!("\nPASS: an unsigned PTB was simulated successfully with no key present.");
        }
        Err(status) => {
            // A gRPC status is still an answer from the fullnode: the unsigned request was
            // transported and understood. Only a transport failure would threaten the plan's
            // assumption, so print enough to tell the two apart.
            println!("\n--- simulate_transaction returned a gRPC status ---");
            println!("code   : {:?}", status.code());
            println!("message: {}", status.message());
            panic!("simulate_transaction did not return Ok; see the status above");
        }
    }
}
