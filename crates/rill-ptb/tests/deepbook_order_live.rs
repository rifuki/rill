//! The gated spend that reaches DeepBook, pinned by the chain rather than by a commit message.
//!
//! # What was wrong with the way this was recorded
//!
//! Two transactions put the whole path on chain in one agent-signed transaction: release a bounded
//! coin from an agent wallet under its own rules, deposit it into a DeepBook BalanceManager with a
//! delegated capability, prove the right to trade, place a limit order. Both digests were written
//! into commit messages, which nothing reads and nothing checks. Every builder underneath them
//! could be reordered, or could go back to the owner-only deposit, and the whole workspace would
//! still be green.
//!
//! So the digests are read back here. The bytes a validator accepted are fetched from testnet and
//! handed to the signer's own decoder, and what comes out is compared against the sequence the
//! builders emit today, call for call and amount for amount. Nothing is submitted and nothing is
//! spent: a transaction that already paid for itself is being read.
//!
//! The last test closes the other half, and it is the one that keeps working. A read-back proves the
//! shape landed once, cannot prove the builders would produce it again, and stops being possible at
//! all once a public fullnode prunes the digest, which one of these two has already been. So the
//! same order is rebuilt from live object versions and simulated with checks on, which is the gate
//! the signer puts in front of a signature.
//!
//!   cargo test -p rill-ptb --test deepbook_order_live -- --ignored --nocapture

use rill_chain::grpc::GrpcSui;
use rill_chain::{SuiRead, SuiWrite};
use rill_policy::decode::decode;
use rill_ptb::book::{book_params_transaction, parse_book_params};
use rill_ptb::deepbook::{expected_order_targets, place_limit_order, LimitOrder};
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::registry::{pool_spec, DeepBookNetwork, TESTNET_PACKAGE_ID};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_gated_spend_for_modules, WalletBinding};
use sui_sdk_types::{Address, Digest, Transaction, TransactionKind};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// The `agent_wallet` package these wallets were created by.
const WALLET_PACKAGE: &str = "0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740";
/// The shared `Version` object every call in the package is gated on.
const VERSION: &str = "0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65";
/// The agent wallet the recorded orders were funded from: budget 0.2 SUI, per-tx 0.05 SUI.
const WALLET: &str = "0x74d0e7b3d0956b08d40834ef19ae0fc9c48f35b09a928c57a517d6a20d8859cf";
/// The owned capability that makes the agent the agent. Held by the agent key, never the owner's.
const AGENT_CAP: &str = "0x564865bc159794e5873d8ef2548cfa110ff50fefe650f1bbd70b08a8d621ef76";
/// The key that signed both recorded orders. Not the wallet owner.
const AGENT: &str = "0xb93cbb8f841a3442e5112c50880f20db9735cb1bb5f1459e745c5f602a2fe29a";

/// The DeepBook BalanceManager the agent trades on without holding its owner's key.
const BALANCE_MANAGER: &str = "0xd817b421de7a65e054160faf3063460ff85d2c76536aa7f6ef8865a7fd12dfe5";
/// Authorises trading on that manager.
const TRADE_CAP: &str = "0xf1e2693c1b5d78c2768faa89c7947320e905f9b34b2fd9ed44295b500608d5ac";
/// Authorises funding it. A separate capability from the trade cap, and separately minted.
const DEPOSIT_CAP: &str = "0x954fbdd985a13f18ebc871e0434becb2c1bc74ae341a2e5066abb578e9774ad7";

const POOL_KEY: &str = "DEEP_SUI";

/// The order that landed, in the chain's own units.
///
/// 10 DEEP bid at 0.004 SUI, funded by releasing 0.04 SUI from the wallet. Quantity is base units
/// against DEEP's 1e6 scalar; price is DeepBook's scaled form, `0.004 * FLOAT_SCALAR * 1e9 / 1e6`.
/// They are written as integers here because integers are what the transaction carries: a decimal
/// converted twice is the defect the whole money path is built to avoid.
const RECORDED_SPEND_MIST: u64 = 40_000_000;
const RECORDED_PRICE_SCALED: u64 = 4_000_000_000;
const RECORDED_QUANTITY_BASE: u64 = 10_000_000;

/// Both recorded runs of the gated-spend-into-order path, newest first.
///
/// The second is commit `4ebe18a`'s, the first the same path driven over MCP. Both are read, because
/// one digest is an anecdote and a shape that reproduced is a claim. The second has since been pruned
/// from the public testnet fullnode, which the test reports rather than fails on.
const RECORDED_ORDERS: &[&str] = &[
    "dxzyeAfW5eRdGUobBNUGeu2mnmaN4xyzY7J8dZxL5fZ",
    "GiL7unaYVnx7TF9QDtpUgc3nFSdWxVgkLb6sMDQfCm77",
];

fn addr(hex: &str) -> Address {
    hex.parse().expect("a literal address in this file")
}

/// The seven Move calls a gated spend into a DeepBook order makes, in order.
///
/// The first four come from the wallet package and the last three from
/// [`expected_order_targets`], which is the same list the server pins a built transaction against.
/// Writing them out here rather than deriving all seven is deliberate: a test that computed its
/// expectation from the same function under test would agree with any reordering.
fn pinned_sequence() -> Vec<String> {
    let mut targets = vec![
        format!("{WALLET_PACKAGE}::agent_wallet::request_spend"),
        format!("{WALLET_PACKAGE}::budget::prove"),
        format!("{WALLET_PACKAGE}::per_tx::prove"),
        format!("{WALLET_PACKAGE}::agent_wallet::confirm_spend"),
    ];
    targets.extend(expected_order_targets(addr(TESTNET_PACKAGE_ID)));
    targets
}

/// Every `u64` the transaction carries as a pure input.
///
/// The amounts that decide what an order costs are pure inputs, and the decoder the signer uses
/// deliberately reports only object inputs and call targets. So the numbers are read here, from the
/// same bytes: a sequence of the right calls carrying the wrong amount is not the transaction that
/// was approved.
fn pure_u64s(transaction: &Transaction) -> Vec<u64> {
    let programmable = match &transaction.kind {
        TransactionKind::ProgrammableTransaction(p) => p,
        _ => panic!("a recorded order is a programmable transaction"),
    };
    programmable
        .inputs
        .iter()
        .filter_map(|input| match input {
            sui_sdk_types::Input::Pure(value) => value.as_slice().try_into().ok(),
            _ => None,
        })
        .map(u64::from_le_bytes)
        .collect()
}

async fn resolve_shared(chain: &GrpcSui, shared: &mut SharedObjects, id: &str, label: &str) {
    let summary = chain
        .get_object(id)
        .await
        .unwrap_or_else(|e| panic!("reading the {label} {id}: {e}"));
    let version = summary
        .shared_initial_version
        .unwrap_or_else(|| panic!("the {label} {id} must be a shared object"));
    shared.insert(addr(id), version);
}

/// An owned object at the version it is at right now. Owned versions move with every transaction
/// that touches them, so these are read rather than remembered.
async fn owned(chain: &GrpcSui, id: &str, label: &str) -> ObjectInput {
    let summary = chain
        .get_object(id)
        .await
        .unwrap_or_else(|e| panic!("reading the {label} {id}: {e}"));
    ObjectInput::owned(
        addr(&summary.reference.id),
        summary.reference.version,
        summary
            .reference
            .digest
            .parse::<Digest>()
            .expect("a digest from the chain"),
    )
}

/// Fetch the bytes, telling a pruned digest apart from a broken one.
///
/// A public fullnode keeps transaction history for a window and then drops it, so `NotFound` is an
/// expected answer about an old digest rather than a fault. Anything else is a fault and panics.
async fn landed_bytes(chain: &GrpcSui, digest: &str) -> Option<String> {
    match chain.landed_transaction_base64(digest).await {
        Ok(b64) => Some(b64),
        // Pruning only. A node that answers without the bytes the read mask asked for used to share
        // this variant, so reducing the mask made this test pass by skipping every digest: a fault
        // in the request read as a condition of the chain. That is ChainError::Malformed now and
        // falls to the arm below, which is the difference between a test that skips and one that
        // fails.
        Err(rill_chain::ChainError::NotFound(reason)) => {
            println!("  pruned from this fullnode: {reason}");
            None
        }
        Err(e) => panic!("reading the bytes of {digest}: {e}"),
    }
}

/// The transaction that landed, decoded by the code that decides what a signer will sign.
///
/// # Every digest still on the node, and at least one
///
/// A recorded digest has a shelf life. `GiL7unaYVnx7TF9QDtpUgc3nFSdWxVgkLb6sMDQfCm77`, the digest
/// commit `4ebe18a` recorded, was already unreadable from `fullnode.testnet.sui.io` eight days
/// later, and `sui client tx-block` agrees. Pruning is not a regression and must not fail this test,
/// but a test that accepted "every digest is gone" would become a green test that checks nothing.
///
/// So: each digest the node still holds must read back as the sequence the builders emit today, and
/// at least one must still be readable. When that stops being true the only remaining evidence is
/// `the_order_path_rebuilds_from_live_state_and_the_chain_accepts_it`, which needs no history, and
/// this test says so instead of passing quietly.
#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn the_recorded_orders_read_back_as_the_pinned_call_sequence() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let expected = pinned_sequence();
    let mut read = 0usize;

    for digest in RECORDED_ORDERS {
        println!("\ndigest: {digest}");

        let Some(b64) = landed_bytes(&chain, digest).await else {
            continue;
        };
        read += 1;

        let effects = chain
            .wait_for(digest)
            .await
            .unwrap_or_else(|e| panic!("{digest} has bytes, so it must have effects too: {e}"));
        assert!(
            effects.success,
            "{digest} is recorded as the path landing, and the chain reports it failed: {:?}",
            effects.error
        );

        let decoded = decode(&b64).expect("the signer's decoder must read a transaction it signed");

        for target in &decoded.targets {
            println!("  {target}");
        }
        assert_eq!(
            decoded.targets, expected,
            "the builders no longer emit the sequence that landed as {digest}"
        );
        assert!(
            decoded
                .object_inputs
                .contains(&addr(BALANCE_MANAGER).to_string()),
            "the order was placed against {BALANCE_MANAGER}; inputs were {:?}",
            decoded.object_inputs
        );

        let bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .expect("the node's own base64")
        };
        let transaction: Transaction = bcs::from_bytes(&bytes).expect("a transaction");
        let numbers = pure_u64s(&transaction);
        assert!(
            numbers.contains(&RECORDED_QUANTITY_BASE),
            "the order quantity {RECORDED_QUANTITY_BASE} is not among the amounts {numbers:?}"
        );
        assert!(
            numbers.contains(&RECORDED_PRICE_SCALED),
            "the order price {RECORDED_PRICE_SCALED} is not among the amounts {numbers:?}"
        );
    }

    assert!(
        read > 0,
        "none of the {} recorded digests is still on this fullnode, so nothing here checked \
         anything. The rebuild test is now the only evidence the order path works, and a fresh \
         digest belongs in RECORDED_ORDERS.",
        RECORDED_ORDERS.len()
    );
    println!(
        "\nPASS: {read} of {} recorded orders are still readable, and each reads back as the \
              sequence the builders emit.",
        RECORDED_ORDERS.len()
    );
}

/// The most recent run also carries the exact amount it released, which is the number a reader of
/// the commit message has to take on trust.
#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn the_recorded_order_released_the_amount_it_says_it_did() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let digest = RECORDED_ORDERS[0];

    let b64 = landed_bytes(&chain, digest).await.unwrap_or_else(|| {
        panic!(
            "{digest} is the newest recorded order and this fullnode no longer has it; record a \
             fresh one rather than leaving the amount unchecked"
        )
    });
    let bytes = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("the node's own base64")
    };
    let transaction: Transaction = bcs::from_bytes(&bytes).expect("a transaction");
    let numbers = pure_u64s(&transaction);
    println!("{digest} carries {numbers:?}");

    assert!(
        numbers.contains(&RECORDED_SPEND_MIST),
        "the spend of {RECORDED_SPEND_MIST} mist is not among the amounts {numbers:?}"
    );

    // The wallet's per-tx rule is what bounded this, and the chain applied it: a request_spend
    // over the cap aborts inside per_tx::prove, so a landed transaction carrying this amount is
    // itself the evidence that the amount was within the rule.
    let effects = chain
        .wait_for(digest)
        .await
        .unwrap_or_else(|e| panic!("reading the effects of {digest}: {e}"));
    assert!(
        effects.success,
        "{digest} must have landed for its amount to mean anything: {:?}",
        effects.error
    );
    println!("\nPASS: the amount in the commit message is the amount in the bytes.");
}

/// Rebuild it. A read-back says the shape landed once; only a rebuild says it would land again.
#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode and a funded agent address"]
async fn the_order_path_rebuilds_from_live_state_and_the_chain_accepts_it() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let pool = pool_spec(DeepBookNetwork::Testnet, POOL_KEY).expect("DEEP_SUI is listed");

    let mut shared = SharedObjects::new();
    resolve_shared(&chain, &mut shared, WALLET, "wallet").await;
    resolve_shared(&chain, &mut shared, VERSION, "version").await;
    resolve_shared(&chain, &mut shared, BALANCE_MANAGER, "balance manager").await;
    resolve_shared(&chain, &mut shared, &pool.pool_id.to_string(), "pool").await;

    // Read, as production does. A price below the node's reference is refused before anything runs.
    let gas_price = chain
        .reference_gas_price()
        .await
        .expect("the node reports its reference gas price");
    println!("reference gas price: {gas_price}");

    // The prove list comes off the wallet, not out of this file: emitting a prove for a rule that
    // is not attached aborts inside `df::borrow_mut` with no code of its own to explain it.
    let read = policy_rules_transaction(
        addr(WALLET_PACKAGE),
        addr(WALLET),
        "0x2::sui::SUI",
        &shared,
        gas_price,
    )
    .expect("build the rule read");
    let read_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&read).unwrap())
    };
    let outcome = chain
        .simulate_read(&read_b64)
        .await
        .expect("read the rules");
    assert!(outcome.ok, "reading the rules failed: {:?}", outcome.error);
    let names = parse_type_names(
        outcome
            .command_returns
            .iter()
            .flatten()
            .next()
            .expect("the wallet reports its rules"),
    )
    .expect("a vector<TypeName>");
    let modules: Vec<String> = attached_modules(&names)
        .into_iter()
        .map(str::to_owned)
        .collect();
    println!("rules attached: {modules:?}");

    // What the pool will accept today, read from the pool, through the builder the command uses.
    let params_tx = book_params_transaction(addr(TESTNET_PACKAGE_ID), &pool, &shared, gas_price)
        .expect("build the parameter read");
    let params_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&params_tx).unwrap())
    };
    let params_out = chain
        .simulate_read(&params_b64)
        .await
        .expect("read the pool's parameters");
    assert!(
        params_out.ok,
        "reading the pool's parameters failed: {:?}",
        params_out.error
    );
    let returned: Vec<&[u8]> = params_out
        .command_returns
        .iter()
        .flatten()
        .map(Vec::as_slice)
        .collect();
    let params = parse_book_params(&returned).expect("three u64s");
    println!(
        "{POOL_KEY} takes: min {} · lot {} · tick {}",
        params.min_size, params.lot_size, params.tick_size
    );
    params
        .check(RECORDED_PRICE_SCALED, RECORDED_QUANTITY_BASE)
        .unwrap_or_else(|e| {
            panic!("the recorded order is no longer legal on this pool: {e}");
        });

    let gas: Vec<ObjectInput> = chain
        .list_owned_objects(AGENT)
        .await
        .expect("list the agent's objects")
        .into_iter()
        .filter(|o| {
            o.object_type
                .as_deref()
                .is_some_and(|t| t.contains("::coin::Coin<") && t.ends_with("::sui::SUI>"))
        })
        .map(|o| {
            ObjectInput::owned(
                addr(&o.reference.id),
                o.reference.version,
                o.reference.digest.parse::<Digest>().expect("a digest"),
            )
        })
        .collect();
    assert!(
        !gas.is_empty(),
        "{AGENT} holds no SUI, so nothing can pay for this simulation"
    );

    let mut tx = TransactionBuilder::new();
    tx.set_sender(addr(AGENT));
    tx.set_gas_budget(30_000_000);
    tx.set_gas_price(gas_price);
    tx.add_gas_objects(gas);

    let binding = WalletBinding {
        package_id: addr(WALLET_PACKAGE),
        wallet_id: addr(WALLET),
        cap: owned(&chain, AGENT_CAP, "AgentCap").await,
        version_id: addr(VERSION),
        coin_type: "0x2::sui::SUI".into(),
        manifest: rill_core::manifest::CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };
    let module_refs: Vec<&str> = modules.iter().map(String::as_str).collect();
    let coin = build_gated_spend_for_modules(
        &mut tx,
        &binding,
        RECORDED_SPEND_MIST,
        &module_refs,
        &shared,
    )
    .expect("the gated spend must build");

    let order = LimitOrder {
        pool: pool.clone(),
        balance_manager_id: addr(BALANCE_MANAGER),
        trade_cap: owned(&chain, TRADE_CAP, "TradeCap").await,
        deposit_cap: owned(&chain, DEPOSIT_CAP, "DepositCap").await,
        client_order_id: 1,
        // Decimal strings, converted exactly once, inside the builder. The integers above are what
        // must come out the other end.
        price: "0.004".into(),
        quantity: "10".into(),
        is_bid: true,
        pay_with_deep: false,
    };
    place_limit_order(&mut tx, addr(TESTNET_PACKAGE_ID), &order, coin, &shared)
        .expect("the order must build");

    let built = tx.try_build().expect("the rebuilt path must compile");
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&built).unwrap())
    };

    let decoded = decode(&b64).expect("the signer must be able to read what was just built");
    for target in &decoded.targets {
        println!("  {target}");
    }
    assert_eq!(
        decoded.targets,
        pinned_sequence(),
        "the rebuilt path is not the sequence that landed"
    );
    let numbers = pure_u64s(&built);
    assert!(
        numbers.contains(&RECORDED_SPEND_MIST)
            && numbers.contains(&RECORDED_PRICE_SCALED)
            && numbers.contains(&RECORDED_QUANTITY_BASE),
        "the rebuilt path carries {numbers:?}, not the recorded amounts"
    );

    // Checks on. This is the gate the signer puts in front of a signature, not the relaxed read
    // used to pull a value back out of a Move function.
    let outcome = chain.simulate(&b64).await.expect("the node answers");
    println!(
        "\nsimulation: ok={} verification={:?} gas={}",
        outcome.ok, outcome.verification, outcome.gas_used_mist
    );
    assert!(
        outcome.ok,
        "the recorded order no longer simulates: {:?}",
        outcome.error
    );
    println!("\nPASS: the recorded order rebuilds from live state and the chain accepts it.");
    println!("Nothing was signed and nothing was submitted.");
}
