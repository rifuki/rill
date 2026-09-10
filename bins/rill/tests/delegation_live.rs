//! The delegation, in both directions, against testnet.
//!
//! `be5d9ea` proved by hand that an agent key spends from a wallet the owner created and bounded,
//! and that the owner is refused when it tries the same spend: four digests in a commit message,
//! and nothing to stop it regressing. This is the automation.
//!
//! # Two refusals, from two different places
//!
//! The owner-signed spend is not refused by the contract. The `AgentCap` is an owned object held
//! by the agent, so a transaction the owner signs cannot present it, and Sui rejects it on
//! ownership before any Move code runs. `E_NOT_AGENT` (7), the contract's own assertion, is the
//! second line of defence and is reachable only by a sender who owns a cap without being the
//! agent; that lives in `move/agent_wallet/tests/agent_wallet_tests.move`. So the assertion here
//! matches the ownership refusal and checks that it is *not* a Move abort: a test that accepted
//! either would pass over the object model quietly going missing.
//!
//! # What costs SUI and what does not
//!
//! A refusal is proved by simulation, which is keyless and free: the sender is an address, and the
//! node answers what would happen if that address signed. A digest is the evidence only where
//! something has to land: the wallet, its rules, the agent's spend, and the revoke that returns
//! the budget to the owner afterwards. The wallet `be5d9ea` proved on is revoked now, and neither
//! refusal depends on the wallet being live, so two tests run against it with no key and no gas.
//! The flow test mints a fresh bounded wallet each run and revokes it at the end.
//!
//!   cargo test -p rill --test delegation_live -- --ignored --nocapture

use rill_chain::aborts::classify_rule_abort;
use rill_chain::grpc::GrpcSui;
use rill_chain::{ChainError, ExecutionOutcome, SuiRead, SuiWrite};
use rill_cli::keystore::Keystore;
use rill_cli::spend_cmd::{spend_json, SpendArgs};
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::create::{build_create_wallet, NewWallet};
use rill_ptb::lifecycle::build_revoke;
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::rules::{build_attach_rules, RuleTarget};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_gated_spend_for_modules, WalletBinding};
use rill_ptb::transfer::transfer_coin;
use sui_sdk_types::{Address, Digest, Transaction};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const PACKAGE: &str = "0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740";
const VERSION_ID: &str = "0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65";
const SUI: &str = "0x2::sui::SUI";
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// The two identities from `be5d9ea`. The owner creates and bounds; the agent spends.
const OWNER: &str = "0xb649a075e07c7cf0baebeaa82150416218c63943e2e767fe93a24aa5c7ce64a9";
const AGENT: &str = "0xb93cbb8f841a3442e5112c50880f20db9735cb1bb5f1459e745c5f602a2fe29a";

/// The wallet `be5d9ea` proved on, read back from the cap the agent still holds. Revoked since
/// (`revoked: true`, budget 0), which suits the refusals: an ownership rejection happens before
/// the contract could notice the revocation, and `add_rule` checks the owner before anything else.
const RECORDED_WALLET: &str = "0xd362cf61d069ae8cd564a5cae5d88bfd1997d4719476d3c81d02fbe673c23ab4";
const RECORDED_CAP: &str = "0xea3f352a207d1e714028928a6dc933c555b9b53d399cc05aa8abd52da24dc867";

/// The fresh wallet's funding and per-transaction cap, in mist. 0.05 SUI funded and capped at the
/// same total, 0.02 per transaction: a 0.01 spend is inside both, and a 0.03 spend is over the
/// per-tx cap while still inside the budget, so the refusal names `per_tx` and nothing else.
const FUNDING_MIST: u64 = 50_000_000;
const PER_TX_MIST: u64 = 20_000_000;
const INSIDE_SUI: &str = "0.01";
const INSIDE_MIST: u64 = 10_000_000;
const OVER_PER_TX_SUI: &str = "0.03";
const OVER_PER_TX_MIST: u64 = 30_000_000;

/// The owner's transactions carry the funding, so they get the default budget. The agent holds a
/// few hundredths of a SUI on testnet and a budget it cannot cover is rejected outright.
const OWNER_GAS_BUDGET: u64 = 100_000_000;
const AGENT_GAS_BUDGET: u64 = 20_000_000;

fn manifest() -> CapabilityManifest {
    CapabilityManifest {
        wallet_coin_type: SUI.into(),
        rules: vec![
            CapabilityRule::Budget {
                total_mist: FUNDING_MIST.to_string(),
            },
            CapabilityRule::PerTx {
                max_mist: PER_TX_MIST.to_string(),
            },
        ],
    }
}

fn address(s: &str) -> Address {
    s.parse().expect("a constant address")
}

/// What `rill spend --wallet … --cap … --amount …` hands the library, for one wallet.
fn spend_args(
    wallet_id: Address,
    cap_id: &str,
    amount: &str,
    gas_budget: u64,
    dry_run: bool,
) -> SpendArgs {
    SpendArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION_ID.into(),
        wallet_id: wallet_id.to_string(),
        cap_id: cap_id.to_owned(),
        amount: amount.into(),
        recipient: None,
        gas_budget,
        dry_run,
    }
}

fn encode(tx: &Transaction) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(tx).expect("BCS"))
}

/// Both keys, by address. They must be different: that is the entire point of the file.
fn keys() -> (Keystore, Keystore) {
    let owner = Keystore::load_for(address(OWNER))
        .expect("the owner key must be in ~/.sui/sui_config/sui.keystore");
    let agent = Keystore::load_for(address(AGENT))
        .expect("the agent key must be in ~/.sui/sui_config/sui.keystore");
    assert_ne!(owner.address(), agent.address());
    (owner, agent)
}

/// Wait until the node's owned-object index agrees with its ledger about `sender`'s SUI coins.
///
/// A submission returns once the transaction is final, and the index that answers
/// `list_owned_objects` catches up a moment later. Gas smashing deletes every coin but the first,
/// so a transaction built from the stale listing references an object that no longer exists and
/// is rejected for a reason that has nothing to do with what is being tested.
async fn settled_gas(chain: &GrpcSui, sender: Address) -> Vec<ObjectInput> {
    for _ in 0..60 {
        let owned = chain
            .list_owned_objects(&sender.to_string())
            .await
            .expect("list the sender's objects");
        let coins: Vec<_> = owned
            .iter()
            .filter(|o| o.object_type.as_deref() == Some(SUI_COIN_TYPE))
            .collect();
        let mut current = Vec::with_capacity(coins.len());
        for coin in &coins {
            match chain.get_object(&coin.reference.id).await {
                Ok(live) if live.reference.version == coin.reference.version => {
                    current.push(ObjectInput::owned(
                        live.reference.id.parse().expect("an id from the chain"),
                        live.reference.version,
                        live.reference.digest.parse::<Digest>().expect("a digest"),
                    ));
                }
                Ok(_) | Err(ChainError::NotFound(_)) => break,
                Err(e) => panic!("reading {}: {e}", coin.reference.id),
            }
        }
        if !coins.is_empty() && current.len() == coins.len() {
            return current;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    panic!("{sender}'s SUI coins did not settle on the node in 30s, or there are none");
}

/// A transaction with sender, gas and price filled in, ready for the builder calls.
async fn transaction_for(chain: &GrpcSui, sender: Address, gas_budget: u64) -> TransactionBuilder {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(gas_budget);
    tx.set_gas_price(
        chain
            .reference_gas_price()
            .await
            .expect("the reference gas price"),
    );
    tx.add_gas_objects(settled_gas(chain, sender).await);
    tx
}

async fn shared_of(chain: &GrpcSui, ids: &[Address]) -> SharedObjects {
    let mut shared = SharedObjects::new();
    for id in ids {
        let summary = chain
            .get_object(&id.to_string())
            .await
            .unwrap_or_else(|e| panic!("reading {id}: {e}"));
        shared.insert(
            *id,
            summary
                .shared_initial_version
                .unwrap_or_else(|| panic!("{id} is not a shared object")),
        );
    }
    shared
}

/// The rule modules a wallet carries, read from the chain the way `rill spend` reads them.
///
/// The price is read rather than named here for the same reason the command reads it: the node
/// refuses a read priced below the reference exactly as it refuses a submission, and the reference
/// differs by an order of magnitude between the two networks.
async fn rules_of(chain: &GrpcSui, wallet_id: Address, shared: &SharedObjects) -> Vec<String> {
    let gas_price = chain
        .reference_gas_price()
        .await
        .expect("the node answers with its reference gas price");
    let read = policy_rules_transaction(address(PACKAGE), wallet_id, SUI, shared, gas_price)
        .expect("the policy read builds");
    let outcome = chain
        .simulate_read(&encode(&read))
        .await
        .expect("the node answers the read");
    let bytes = outcome
        .command_returns
        .iter()
        .flatten()
        .next()
        .expect("policy_rules returns a value");
    attached_modules(&parse_type_names(bytes).expect("a vector<TypeName>"))
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The gated spend `rill spend` builds, for any sender.
///
/// The sender is an address, not a key. A simulation asks the chain what would happen if that
/// address signed, which is exactly the question the ownership refusal answers.
async fn gated_spend(
    chain: &GrpcSui,
    sender: Address,
    gas_budget: u64,
    wallet_id: Address,
    cap_id: &str,
    amount_mist: u64,
) -> Transaction {
    let shared = shared_of(chain, &[wallet_id, address(VERSION_ID)]).await;
    let cap = chain.get_object(cap_id).await.expect("the AgentCap");
    let modules = rules_of(chain, wallet_id, &shared).await;
    let module_refs: Vec<&str> = modules.iter().map(String::as_str).collect();

    let mut tx = transaction_for(chain, sender, gas_budget).await;
    let binding = WalletBinding {
        package_id: address(PACKAGE),
        wallet_id,
        cap: ObjectInput::owned(
            cap.reference.id.parse().expect("an id from the chain"),
            cap.reference.version,
            cap.reference.digest.parse::<Digest>().expect("a digest"),
        ),
        version_id: address(VERSION_ID),
        coin_type: SUI.into(),
        manifest: CapabilityManifest {
            wallet_coin_type: SUI.into(),
            rules: Vec::new(),
        },
    };
    let coin = build_gated_spend_for_modules(&mut tx, &binding, amount_mist, &module_refs, &shared)
        .expect("the spend builds");
    transfer_coin(&mut tx, coin, sender);
    tx.try_build().expect("a valid transaction")
}

/// An attach of this file's manifest, for any sender.
async fn attach_rules(
    chain: &GrpcSui,
    sender: Address,
    gas_budget: u64,
    wallet_id: Address,
) -> Transaction {
    let shared = shared_of(chain, &[wallet_id, address(VERSION_ID)]).await;
    let mut tx = transaction_for(chain, sender, gas_budget).await;
    build_attach_rules(
        &mut tx,
        &RuleTarget {
            package_id: address(PACKAGE),
            wallet_id,
            version_id: address(VERSION_ID),
            coin_type: SUI.into(),
            manifest: manifest(),
        },
        &shared,
    )
    .expect("the attach builds");
    tx.try_build().expect("a valid transaction")
}

/// What the contract says about a transaction that executes and aborts.
///
/// Execution happened, so there are effects, and the abort text rides in them. A refusal that
/// never reached execution has no effects and comes back as an error instead; that shape is
/// asserted separately by [`refused_before_execution`], and treating it as a plain refusal here
/// would let the two lines of defence blur into one.
async fn contract_refusal(chain: &GrpcSui, tx: &Transaction) -> String {
    let outcome = chain
        .simulate(&encode(tx))
        .await
        .expect("the node executes it and reports the abort in the effects");
    assert!(
        !outcome.ok,
        "this transaction must not pass the strict gate, and it did"
    );
    outcome.error.unwrap_or_default()
}

/// The refusal Sui makes before any Move code runs, and nothing else.
///
/// Three shapes are possible and only one is right. A failed outcome means the transaction
/// executed and aborted, so the object model let it through. A transport error means the node was
/// counted as unreachable when it had just answered, which is what `rill spend` used to print for
/// exactly this case. `Rejected` is the node having read the inputs and said no.
async fn refused_before_execution(chain: &GrpcSui, tx: &Transaction) -> String {
    match chain.simulate(&encode(tx)).await {
        Err(ChainError::Rejected(why)) => why,
        Err(ChainError::Transport(why)) => {
            panic!("the node answered, and the answer was reported as an outage: {why}")
        }
        Err(other) => panic!("unexpected: {other}"),
        Ok(outcome) => panic!(
            "the transaction reached execution (ok={}, error={:?}); the object model must \
             refuse it before that",
            outcome.ok, outcome.error
        ),
    }
}

/// Sign, submit, and wait until the node has the transaction. Every digest recorded from this
/// file comes through here, and nothing continues past a failure.
async fn land(chain: &GrpcSui, key: &Keystore, tx: &Transaction, what: &str) -> ExecutionOutcome {
    let b64 = encode(tx);
    let gate = chain.simulate(&b64).await.expect("the node answers");
    assert!(
        gate.ok,
        "{what}: the strict gate refused it: {:?}",
        gate.error
    );

    let signature = key.sign(tx).expect("sign");
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .unwrap_or_else(|e| panic!("{what}: submitting: {e}"));
    assert!(
        outcome.success && outcome.error.is_none(),
        "{what} failed on chain: {:?}",
        outcome.error
    );
    assert!(!outcome.digest.is_empty(), "{what}: no digest came back");

    // Final is not the same as visible: the node that certified it may not have indexed it yet.
    for _ in 0..60 {
        if chain.wait_for(&outcome.digest).await.is_ok() {
            println!("  {what}: {}", outcome.digest);
            return outcome;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    panic!("{what}: {} did not become readable in 30s", outcome.digest);
}

fn same_address(a: &str, b: &str) -> bool {
    a.trim_start_matches("0x")
        .trim_start_matches('0')
        .eq_ignore_ascii_case(b.trim_start_matches("0x").trim_start_matches('0'))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as u64
}

/// The first line: an owner-signed `request_spend` never executes.
///
/// Keyless. The recorded wallet is revoked, and that is the point of running it here: if the
/// refusal came from the contract, it would be `E_REVOKED` (2) or `E_NOT_AGENT` (7), and it is
/// neither, because no Move code ran.
#[tokio::test]
#[ignore = "requires testnet; keyless, nothing is signed"]
async fn an_owner_signed_spend_is_refused_before_execution() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let tx = gated_spend(
        &chain,
        address(OWNER),
        OWNER_GAS_BUDGET,
        address(RECORDED_WALLET),
        RECORDED_CAP,
        INSIDE_MIST,
    )
    .await;

    let refusal = refused_before_execution(&chain, &tx).await;
    println!("owner-signed request_spend, presenting the agent's cap:\n  {refusal}");

    assert!(
        refusal.contains("is owned by account address"),
        "the refusal must be Sui's ownership check, not something later: {refusal}"
    );
    assert!(
        refusal.contains(RECORDED_CAP) && refusal.contains(AGENT),
        "the refusal must name the cap and who does own it: {refusal}"
    );
    assert!(
        !refusal.contains("MoveAbort"),
        "a Move abort here would mean the object model let the owner present the cap: {refusal}"
    );
    assert!(
        classify_rule_abort(&refusal).is_none(),
        "this is not the contract refusing, and must not be reported as if it were"
    );
    println!("\nPASS: the owner cannot even present the agent's capability.");
}

/// The agent may spend within the rules. It may not change them.
///
/// Keyless, against the recorded wallet: `add_rule` asserts the owner before it looks at
/// anything else, so the refusal is the same on a revoked wallet as on a live one.
#[tokio::test]
#[ignore = "requires testnet; keyless, nothing is signed"]
async fn an_agent_signed_add_rule_aborts_e_not_owner() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let tx = attach_rules(
        &chain,
        address(AGENT),
        AGENT_GAS_BUDGET,
        address(RECORDED_WALLET),
    )
    .await;

    let refusal = contract_refusal(&chain, &tx).await;
    println!("agent-signed add_rule:\n  {refusal}");

    let named = classify_rule_abort(&refusal).expect("a Move abort in agent_wallet");
    assert_eq!(
        (named.module.as_str(), named.code),
        ("agent_wallet", 1),
        "E_NOT_OWNER is 1: {refusal}"
    );
    assert!(
        named.advice().contains("owner's key"),
        "the advice must name the fix: {}",
        named.advice()
    );
    println!("\nPASS: {named}. {}", named.advice());
}

/// The whole delegation, with both keys, on a wallet minted for this run.
///
/// In order: the owner creates a wallet naming the agent and attaches the rules; the agent spends
/// inside the cap; the owner is refused the same spend before execution; the agent is refused
/// over the cap, by name; the agent is refused `add_rule`; the owner revokes and the budget comes
/// back. Four digests, the same four `be5d9ea` recorded by hand, plus the revoke.
#[tokio::test]
#[ignore = "requires testnet and both keys in ~/.sui/sui_config/sui.keystore"]
async fn the_delegation_holds_in_both_directions() {
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let (owner, agent) = keys();
    println!("owner : {}\nagent : {}\n", owner.address(), agent.address());

    // 1. The owner creates the wallet, naming the agent. The cap must land with the agent.
    println!("1. owner creates a {FUNDING_MIST} mist wallet for the agent");
    let created = {
        let now = now_ms();
        let shared = shared_of(&chain, &[address(VERSION_ID)]).await;
        let mut tx = transaction_for(&chain, owner.address(), OWNER_GAS_BUDGET).await;
        let value = tx.pure(&FUNDING_MIST);
        let gas_arg = tx.gas();
        let funds = tx
            .split_coins(gas_arg, vec![value])
            .into_iter()
            .next()
            .expect("one split result");
        build_create_wallet(
            &mut tx,
            &NewWallet {
                package_id: address(PACKAGE),
                version_id: address(VERSION_ID),
                agent: agent.address(),
                expires_at_ms: now + 30 * 86_400_000,
                coin_type: SUI.into(),
                manifest: manifest(),
            },
            funds,
            &shared,
            now,
        )
        .expect("the create builds");
        land(
            &chain,
            &owner,
            &tx.try_build().expect("a valid transaction"),
            "create_wallet (owner)",
        )
        .await
    };
    let wallet = created
        .created
        .iter()
        .find(|o| {
            o.object_type
                .as_deref()
                .is_some_and(|t| t.contains("AgentWallet"))
        })
        .expect("the effects name the wallet");
    let cap = created
        .created
        .iter()
        .find(|o| {
            o.object_type
                .as_deref()
                .is_some_and(|t| t.ends_with("::AgentCap"))
        })
        .expect("the effects name the cap");
    let wallet_id: Address = wallet.object_id.parse().expect("an id from the chain");
    println!("  wallet: {wallet_id}\n  cap   : {}", cap.object_id);
    assert!(
        wallet.shared_initial_version.is_some(),
        "the wallet is shared"
    );
    assert!(
        cap.owner.as_deref().is_some_and(|o| same_address(o, AGENT)),
        "the cap must go to the agent, not to the key that signed: {:?}",
        cap.owner
    );

    // 2. The owner attaches the rules. Owner-signed add_rule succeeds; this is the digest for it.
    println!("\n2. owner attaches budget {FUNDING_MIST} and per_tx {PER_TX_MIST}");
    let tx = attach_rules(&chain, owner.address(), OWNER_GAS_BUDGET, wallet_id).await;
    let attached = land(&chain, &owner, &tx, "add_rule x2 (owner)").await;
    let shared = shared_of(&chain, &[wallet_id]).await;
    let modules = rules_of(&chain, wallet_id, &shared).await;
    assert_eq!(
        modules,
        vec!["budget".to_string(), "per_tx".to_string()],
        "the chain must report both rules"
    );
    println!("  rules : {modules:?}");

    // 3. The agent spends inside the cap, through the same library path `rill spend` and
    //    `rill_spend` take.
    println!("\n3. agent spends {INSIDE_SUI} SUI, inside per_tx");
    settled_gas(&chain, agent.address()).await;
    let spent = spend_json(
        TESTNET,
        &agent,
        &spend_args(
            wallet_id,
            &cap.object_id,
            INSIDE_SUI,
            AGENT_GAS_BUDGET,
            false,
        ),
    )
    .await
    .expect("the agent's spend inside the cap must land");
    assert_eq!(spent["submitted"], serde_json::Value::Bool(true));
    let spend_digest = spent["digest"].as_str().expect("a digest").to_owned();
    assert!(!spend_digest.is_empty());
    println!("  request_spend -> prove x2 -> confirm_spend (agent): {spend_digest}");
    let sequence = spent["callSequence"].to_string();
    for call in [
        "agent_wallet::request_spend",
        "budget::prove",
        "per_tx::prove",
        "agent_wallet::confirm_spend",
    ] {
        assert!(
            sequence.contains(call),
            "the call sequence must carry {call}: {sequence}"
        );
    }

    // 4. The owner attempts the same spend. Refused before execution, by the object model.
    println!("\n4. owner attempts the same spend");
    let tx = gated_spend(
        &chain,
        owner.address(),
        OWNER_GAS_BUDGET,
        wallet_id,
        &cap.object_id,
        INSIDE_MIST,
    )
    .await;
    let refusal = refused_before_execution(&chain, &tx).await;
    println!("  {refusal}");
    assert!(
        refusal.contains("is owned by account address"),
        "the refusal must be Sui's ownership check, not something later: {refusal}"
    );
    assert!(
        refusal.contains(&cap.object_id) && refusal.contains(&agent.address().to_string()),
        "the refusal must name the cap and who does own it: {refusal}"
    );
    assert!(
        !refusal.contains("MoveAbort"),
        "a Move abort here would mean the object model let the owner present the cap: {refusal}"
    );
    // And the same refusal reaches whoever runs `rill spend --as <owner>`, as the verdict it is.
    let reported = spend_json(
        TESTNET,
        &owner,
        &spend_args(
            wallet_id,
            &cap.object_id,
            INSIDE_SUI,
            OWNER_GAS_BUDGET,
            true,
        ),
    )
    .await
    .expect_err("the owner's spend must not pass the gate")
    .to_string();
    println!("  rill spend: {reported}");
    // The property, not the sentence. An earlier form of this pinned the opening words, and the
    // first rewording of the message failed the test while the behaviour was correct. What has to
    // hold is that the refusal is reported as one, that it carries the node's own explanation so
    // the reader can see which object was whose, and that it is never dressed up as an outage.
    assert!(
        reported.contains("refused it"),
        "rill spend must report the ownership refusal as a refusal: {reported}"
    );
    assert!(
        reported.contains("is owned by account address"),
        "the refusal must keep the node's words, which name the object and its owner: {reported}"
    );
    assert!(
        !reported.contains("did not answer"),
        "a refusal reported as an outage sends the reader to check a network that is fine: \
         {reported}"
    );

    // 5. The agent goes over the per-transaction cap. Refused by the contract, and named.
    println!("\n5. agent attempts {OVER_PER_TX_SUI} SUI, over per_tx");
    let tx = gated_spend(
        &chain,
        agent.address(),
        AGENT_GAS_BUDGET,
        wallet_id,
        &cap.object_id,
        OVER_PER_TX_MIST,
    )
    .await;
    let refusal = contract_refusal(&chain, &tx).await;
    println!("  {refusal}");
    let named = classify_rule_abort(&refusal).expect("a rule abort");
    assert_eq!(
        named.module, "per_tx",
        "the refusing rule must be named: {refusal}"
    );
    assert_eq!(named.code, 1, "E_OVER_PER_TX is 1");
    let reported = spend_json(
        TESTNET,
        &agent,
        &spend_args(
            wallet_id,
            &cap.object_id,
            OVER_PER_TX_SUI,
            AGENT_GAS_BUDGET,
            true,
        ),
    )
    .await
    .expect_err("the over-cap spend must not pass the gate")
    .to_string();
    assert!(
        reported.starts_with("per_tx refused it"),
        "rill spend must name the rule first: {reported}"
    );
    println!("  {}", reported.lines().next().unwrap_or_default());

    // 6. The agent tries to change its own rules. E_NOT_OWNER, named.
    println!("\n6. agent attempts add_rule");
    let tx = attach_rules(&chain, agent.address(), AGENT_GAS_BUDGET, wallet_id).await;
    let refusal = contract_refusal(&chain, &tx).await;
    println!("  {refusal}");
    let named = classify_rule_abort(&refusal).expect("a Move abort in agent_wallet");
    assert_eq!(
        (named.module.as_str(), named.code),
        ("agent_wallet", 1),
        "E_NOT_OWNER is 1: {refusal}"
    );

    // 7. The owner revokes. The budget less what the agent spent comes back, and the cap the
    //    agent still holds stops meaning anything.
    println!("\n7. owner revokes");
    let revoked = {
        let shared = shared_of(&chain, &[wallet_id]).await;
        let mut tx = transaction_for(&chain, owner.address(), OWNER_GAS_BUDGET).await;
        let coin = build_revoke(&mut tx, address(PACKAGE), wallet_id, SUI, &shared)
            .expect("the revoke builds");
        transfer_coin(&mut tx, coin, owner.address());
        land(
            &chain,
            &owner,
            &tx.try_build().expect("a valid transaction"),
            "revoke (owner)",
        )
        .await
    };
    for delta in &revoked.balance_changes {
        println!("  balance: {} {}", delta.amount, delta.coin_type);
    }

    println!(
        "\nPASS: the delegation holds in both directions.\n\
         \x20 owner  {}\n\
         \x20 agent  {}\n\
         \x20 wallet {wallet_id}\n\
         \x20 cap    {}\n\
         \x20 create_wallet (owner)   {}\n\
         \x20 add_rule x2 (owner)     {}\n\
         \x20 gated spend (AGENT)     {spend_digest}\n\
         \x20 same spend (OWNER)      refused before execution\n\
         \x20 revoke (owner)          {}",
        owner.address(),
        agent.address(),
        cap.object_id,
        created.digest,
        attached.digest,
        revoked.digest
    );
}
