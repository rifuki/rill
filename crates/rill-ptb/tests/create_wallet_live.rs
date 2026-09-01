//! Creating an agent wallet against the deployed testnet contract — keylessly.
//!
//! This is the strict simulation, not a read: checks on, real gas objects owned by a real sender,
//! the same gate the build path runs before anything may be signed. No key is involved, and nothing
//! is submitted — but a success here means the transaction would execute if it were signed.
//!
//!   cargo test -p rill-ptb --test create_wallet_live -- --ignored --nocapture

use rill_chain::grpc::GrpcSui;
use rill_chain::SuiRead;
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::create::{build_create_wallet, NewWallet};
use rill_ptb::deployments::TESTNET_AGENT_WALLET;
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const VERSION_ID: &str = "0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65";
const FUNDED_SENDER: &str = "0xf73e2dea746d9a7071ec5c49bfc2a75f73be5efd02212632e849217234e7ab46";
const SUI: &str = "0x2::sui::SUI";

#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn creating_an_agent_wallet_passes_the_strict_gate() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let sender: Address = FUNDED_SENDER.parse().unwrap();
    let version_id: Address = VERSION_ID.parse().unwrap();

    // The Version object's initial shared version, read rather than assumed.
    let summary = chain
        .get_object(VERSION_ID)
        .await
        .expect("the Version object must exist on testnet");
    let initial = summary
        .shared_initial_version
        .expect("Version is a shared object");
    println!("version : {VERSION_ID}\n          first shared at {initial}");
    let mut shared = SharedObjects::new();
    shared.insert(version_id, initial);

    // A real gas coin the sender owns. Keyless: reading what someone owns needs no key.
    let owned = chain
        .list_owned_objects(FUNDED_SENDER)
        .await
        .expect("list the sender's objects");
    // The chain writes types fully expanded — `0x0000…0002::coin::Coin<0x0000…0002::sui::SUI>`,
    // not the `0x2::` shorthand a human writes. Matching the short form finds nothing.
    let gas_coins: Vec<_> = owned
        .iter()
        .filter(|o| {
            o.object_type
                .as_deref()
                .is_some_and(|t| t.ends_with("::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>"))
        })
        .collect();
    assert!(
        !gas_coins.is_empty(),
        "the sender must hold at least one SUI coin to pay gas"
    );
    // Every SUI coin, not just the first. The gas payment is merged before execution, so a split
    // that one coin alone cannot cover succeeds against the whole balance — and picking one coin
    // fails with `InsufficientCoinBalance` that reads like the account is empty when it is not.
    for coin in &gas_coins {
        println!(
            "gas     : {} v{}",
            coin.reference.id, coin.reference.version
        );
    }

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(100_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects(gas_coins.iter().map(|c| {
        ObjectInput::owned(
            c.reference.id.parse().unwrap(),
            c.reference.version,
            c.reference.digest.parse::<Digest>().unwrap(),
        )
    }));

    // One SUI into the wallet, split off gas.
    let amount = tx.pure(&1_000_000_000u64);
    let gas_arg = tx.gas();
    let funds = tx
        .split_coins(gas_arg, vec![amount])
        .into_iter()
        .next()
        .unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let wallet = NewWallet {
        package_id: TESTNET_AGENT_WALLET.parse().unwrap(),
        version_id,
        // The sender is its own agent here, so the cap lands somewhere reachable.
        agent: sender,
        expires_at_ms: now_ms + 30 * 86_400_000,
        coin_type: SUI.into(),
        manifest: CapabilityManifest {
            wallet_coin_type: SUI.into(),
            rules: vec![
                CapabilityRule::Budget {
                    total_mist: "1000000000".into(),
                },
                CapabilityRule::PerTx {
                    max_mist: "100000000".into(),
                },
            ],
        },
    };

    build_create_wallet(&mut tx, &wallet, funds, &shared, now_ms).expect("the wallet must build");

    let built = tx.try_build().expect("valid transaction");
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&built).unwrap())
    };

    let outcome = chain.simulate(&b64).await.expect("the node must answer");
    println!(
        "\nstrict simulation\n  ok           : {}\n  verification : {:?}\n  gas          : {}",
        outcome.ok, outcome.verification, outcome.gas_used_mist
    );
    if let Some(error) = &outcome.error {
        println!("  error        : {error}");
    }
    for delta in &outcome.balance_changes {
        println!("  balance      : {} {}", delta.amount, delta.coin_type);
    }

    assert!(
        outcome.ok,
        "creating a wallet against the current package must pass the strict gate"
    );
    println!("\nPASS: an unsigned create_wallet would execute. Only a signature is missing.");
}
