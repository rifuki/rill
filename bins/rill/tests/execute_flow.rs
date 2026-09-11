//! The flow an agent drives itself: no wallet, then a bounded wallet, then a spend, then a refusal
//! that names the rule.
//!
//! # What was missing, and what this covers
//!
//! The signer offered four tools, and two of the steps before a spend were commands a person typed.
//! So the flow an agent could drive started halfway through: it could spend from a wallet and could
//! not get one. `rill_create_wallet` and `rill_attach_rules` close that, and the refusals on every
//! one of those paths now name the rule that refused instead of handing back a sentence with a Move
//! abort quoted inside it.
//!
//! # Why most of this runs against a fake chain
//!
//! Each step is five or six round trips, a shared version that has to be read rather than assumed,
//! a gas price that has to be read because a literal is wrong on one of the two networks, and a
//! simulation gate. All of it used to be reachable only through a live testnet node, which means it
//! did not run in CI at all: an edit that broke the assembly was caught by a person running a
//! command, or not at all. [`FakeSui`] answers all of it, including what a transaction created,
//! which is how the ids can be shown to flow from one step into the next.
//!
//! What the fake cannot do is execute Move, so it does not decide whether a rule refuses: a test
//! says so, with the abort text a testnet node really returned. The live tests at the bottom are
//! where the chain has the last word.
//!
//!   cargo test -p rill --test execute_flow -- --ignored --nocapture

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_chain::{CreatedObject, ObjectRef, ObjectSummary};
use rill_cli::keystore::Keystore;
use rill_cli::rules_cmd::{attach_json_on, RulesArgs};
use rill_cli::spend_cmd::{spend_json_on, SpendArgs};
use rill_cli::stdio::{failure_response, handle, Submission, WalletContext};
use rill_cli::verdict::Failure;
use rill_cli::wallet::{create_json_on, CreateArgs};
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use serde_json::{json, Value};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Digest;

/// The shipped source, for the checks whose property is structural. See
/// `the_new_tools_keep_their_chain_client_inside_one_runtime`.
const STDIO: &str = include_str!("../src/stdio.rs");
const WALLET_CMD: &str = include_str!("../src/wallet.rs");
const RULES_CMD: &str = include_str!("../src/rules_cmd.rs");

/// Everything before the first `#[cfg(test)]`: the code that ships.
fn shipped(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

const PACKAGE: &str = "0x000000000000000000000000000000000000000000000000000000000000caf0";
const VERSION: &str = "0x0000000000000000000000000000000000000000000000000000000000000fff";
const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
const CAP: &str = "0x0000000000000000000000000000000000000000000000000000000000000cab";
const COIN: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";

/// Fully-expanded SUI, the way the chain writes it in an object type. The gas filter matches on
/// this exactly, so a fake coin typed any other way is invisible to it.
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// 0.05 SUI funded, 0.02 per transaction: the same shape as the recorded testnet run, where a 0.01
/// spend is inside both caps and a 0.03 spend is over the per-tx cap while still inside the budget.
const BUDGET_MIST: &str = "50000000";
const PER_TX_MIST: &str = "20000000";

/// The exact text a testnet node returned when a spend hit the per-transaction cap. Used rather
/// than invented, because the classifier reads the module name out of this shape.
const PER_TX_ABORT: &str = "MoveAbort(MoveLocation { module: ModuleId { address: b02f39d6, \
     name: Identifier(\"per_tx\") }, function: 2, instruction: 21, function_name: Some(\"prove\") \
     }, 1) in command 2";

/// `E_NOT_OWNER` from `agent_wallet`: what the chain answers when the agent's key tries to attach
/// rules to its own wallet.
const NOT_OWNER_ABORT: &str = "MoveAbort(MoveLocation { module: ModuleId { address: b02f39d6, \
     name: Identifier(\"agent_wallet\") }, function: 7, instruction: 4, \
     function_name: Some(\"add_rule\") }, 1) in command 0";

// ── fixtures ──────────────────────────────────────────────────────────────────────────────────

/// A key from fixed bytes. Deterministic, and it signs real signatures over real transactions,
/// which is what the paths under test do with it.
fn key(seed: u8) -> Keystore {
    let encoded = Ed25519PrivateKey::new([seed; 32])
        .to_suiprivkey()
        .expect("a key encodes");
    Keystore::from_suiprivkey(&encoded).expect("a key loads")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as u64
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

/// A `vector<TypeName>` the way the node returns one: a ULEB count, then each name as a ULEB length
/// and its bytes.
fn type_names(modules: &[&str]) -> Vec<u8> {
    let names: Vec<String> = modules
        .iter()
        .map(|m| format!("{}::{m}::Rule", &PACKAGE[2..]))
        .collect();
    let mut out = vec![names.len() as u8];
    for name in &names {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }
    out
}

fn shared(id: &str, initial: u64, suffix: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: initial + 1,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(format!("{PACKAGE}::agent_wallet::{suffix}")),
        fields: None,
        shared_initial_version: Some(initial),
    }
}

fn owned(id: &str, object_type: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: 9,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(object_type.to_owned()),
        fields: None,
        shared_initial_version: None,
    }
}

fn created(
    id: &str,
    object_type: &str,
    owner: Option<&str>,
    shared_at: Option<u64>,
) -> CreatedObject {
    CreatedObject {
        object_id: id.to_owned(),
        object_type: Some(object_type.to_owned()),
        shared_initial_version: shared_at,
        owner: owner.map(str::to_owned),
    }
}

fn manifest() -> CapabilityManifest {
    CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![
            CapabilityRule::Budget {
                total_mist: BUDGET_MIST.into(),
            },
            CapabilityRule::PerTx {
                max_mist: PER_TX_MIST.into(),
            },
        ],
    }
}

fn create_args(agent: &str) -> CreateArgs {
    CreateArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        agent: Some(agent.to_owned()),
        amount: "0.05".into(),
        expires_in_days: 30,
        manifest: manifest(),
        gas_budget: 100_000_000,
        dry_run: false,
    }
}

fn rules_args(wallet_id: &str) -> RulesArgs {
    RulesArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        wallet_id: wallet_id.to_owned(),
        manifest: manifest(),
        gas_budget: 100_000_000,
        dry_run: false,
    }
}

fn spend_args(wallet_id: &str, amount: &str) -> SpendArgs {
    SpendArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        wallet_id: wallet_id.to_owned(),
        cap_id: CAP.into(),
        amount: amount.to_owned(),
        recipient: None,
        gas_budget: 20_000_000,
        dry_run: false,
    }
}

/// A chain that can answer the owner's create: the Version object, a coin to pay with, a price, and
/// the two objects the transaction brings into existence.
///
/// The wallet is staged as an object too, because a node has it once the transaction lands and the
/// create waits for exactly that before it promises the id to the next step.
fn chain_for_create(owner: &Keystore, agent: &Keystore) -> FakeSui {
    FakeSui::new()
        .with_object(None, shared(VERSION, 3, "Version"))
        .with_object(None, shared(WALLET, 4, "AgentWallet"))
        .with_object(
            Some(&owner.address().to_string()),
            owned(COIN, SUI_COIN_TYPE),
        )
        .with_reference_gas_price(1_000)
        .with_created(vec![
            created(
                WALLET,
                &format!("{PACKAGE}::agent_wallet::AgentWallet<0x2::sui::SUI>"),
                None,
                Some(4),
            ),
            created(
                CAP,
                &format!("{PACKAGE}::agent_wallet::AgentCap"),
                Some(&agent.address().to_string()),
                None,
            ),
        ])
}

/// A chain that can answer the owner's attach, reporting `attached` as the wallet's live rules.
fn chain_for_attach(owner: &Keystore, attached: &[&str]) -> FakeSui {
    FakeSui::new()
        .with_object(None, shared(WALLET, 4, "AgentWallet"))
        .with_object(None, shared(VERSION, 3, "Version"))
        .with_object(
            Some(&owner.address().to_string()),
            owned(COIN, SUI_COIN_TYPE),
        )
        // Two answers to the same question: what the wallet carries before the attach, and what it
        // carries after. The attach waits for the second before it promises anything to the spend.
        .with_read_sequence(vec![
            type_names(attached),
            type_names(&["budget", "per_tx"]),
        ])
        .with_reference_gas_price(1_000)
}

/// A chain that can answer the agent's spend from a wallet carrying budget and per_tx.
fn chain_for_spend(agent: &Keystore) -> FakeSui {
    FakeSui::new()
        .with_object(None, shared(WALLET, 4, "AgentWallet"))
        .with_object(None, shared(VERSION, 3, "Version"))
        .with_object(
            Some(&agent.address().to_string()),
            owned(CAP, &format!("{PACKAGE}::agent_wallet::AgentCap")),
        )
        .with_object(
            Some(&agent.address().to_string()),
            owned(COIN, SUI_COIN_TYPE),
        )
        .with_read_return(type_names(&["budget", "per_tx"]))
        .with_reference_gas_price(1_000)
}

/// One `tools/call`, through the transport's own entry point.
fn call(context: &mut WalletContext, tool: &str, arguments: Value) -> Value {
    handle(
        context,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }),
    )
    .expect("a request gets a reply")
}

fn structured(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

// ── the flow, offline ─────────────────────────────────────────────────────────────────────────

/// An agent creates a wallet, attaches its rules, and spends inside them.
///
/// The property is that each step's output is the next step's input: the wallet id and the cap id
/// exist only in the first transaction's effects, and a person was reading them off a terminal and
/// typing them into the second command. Nothing here is staged between the steps except what the
/// chain itself would have answered.
///
/// Each step gets its own fake because the fake does not execute Move: the rule list a wallet
/// reports is staged, so "no rules yet" and "budget and per_tx attached" cannot be the same fake
/// answering twice. The live test at the bottom is where one chain answers all three.
#[test]
fn an_agent_creates_a_wallet_attaches_its_rules_and_spends_inside_them() {
    let owner = key(7);
    let agent = key(11);

    let created = run(create_json_on(
        &chain_for_create(&owner, &agent),
        &owner,
        &create_args(&agent.address().to_string()),
        now_ms(),
    ))
    .expect("the create assembles and submits");

    assert_eq!(created["submitted"], json!(true));
    assert_eq!(created["sender"], json!(owner.address().to_string()));
    assert_eq!(
        created["agent"],
        json!(agent.address().to_string()),
        "the cap is minted to the agent, not to the key that signed"
    );
    assert_eq!(created["funding"], json!("0.05 SUI (50000000 mist)"));
    assert_eq!(
        created["rules"],
        json!(["budget 50000000", "per-tx 20000000"])
    );
    assert!(
        created["note"].as_str().unwrap().contains("NO rules"),
        "a wallet with no rules has no limits, and the answer has to say so: {created}"
    );

    // The two ids the next step needs, out of the effects rather than out of a state file.
    let wallet_id = created["wallet"]
        .as_str()
        .expect("the wallet id comes out of the effects")
        .to_owned();
    let cap_id = created["cap"].as_str().expect("the cap id too").to_owned();
    assert_eq!((wallet_id.as_str(), cap_id.as_str()), (WALLET, CAP));
    assert_eq!(
        created["visibleOnNode"],
        json!(true),
        "the id is only worth handing on once the node can answer for it: {created}"
    );

    let attached = run(attach_json_on(
        &chain_for_attach(&owner, &[]),
        &owner,
        &rules_args(&wallet_id),
    ))
    .expect("the attach assembles and submits");

    assert_eq!(attached["submitted"], json!(true));
    assert_eq!(attached["wallet"], json!(wallet_id));
    assert_eq!(attached["owner"], json!(owner.address().to_string()));
    assert_eq!(attached["attachedBefore"], json!([]));
    assert_eq!(
        attached["rules"],
        json!(["budget", "per_tx"]),
        "both rules must be attached, or the capability is unbounded: {attached}"
    );
    assert_eq!(
        attached["visibleOnNode"],
        json!(true),
        "the spend reads this list to decide which proofs to emit: {attached}"
    );

    let spent = run(spend_json_on(
        &chain_for_spend(&agent),
        &agent,
        &spend_args(&wallet_id, "0.01"),
    ))
    .expect("a spend inside both caps must pass the gate");

    assert_eq!(spent["submitted"], json!(true));
    assert_eq!(
        spent["rules"],
        json!(["budget", "per_tx"]),
        "the prove list is read from the wallet, not assumed: {spent}"
    );
    let sequence = spent["callSequence"].to_string();
    for target in [
        "agent_wallet::request_spend",
        "budget::prove",
        "per_tx::prove",
        "agent_wallet::confirm_spend",
    ] {
        assert!(
            sequence.contains(target),
            "{target} missing from {sequence}"
        );
    }
}

/// A spend the per-transaction cap refuses comes back naming `per_tx`, and nothing is submitted.
#[test]
fn a_spend_a_rule_refuses_names_the_rule_and_submits_nothing() {
    let agent = key(11);
    let chain = chain_for_spend(&agent).with_simulation(SimulationBehavior::Fails {
        error: PER_TX_ABORT.into(),
    });

    let failure = run(spend_json_on(&chain, &agent, &spend_args(WALLET, "0.03")))
        .expect_err("a spend over the cap must not pass the gate");

    let Failure::Refused(refusal) = &failure else {
        panic!("the per-tx cap refusing is a refusal, not a failure: {failure}");
    };
    assert_eq!(refusal.module, "per_tx");
    assert_eq!(refusal.code, 1, "E_OVER_PER_TX is 1");
    assert!(
        chain.submitted().is_empty(),
        "a refused spend must never reach the chain"
    );
}

/// The refusal as an agent receives it: the rule in a field, not in prose.
///
/// This is the MCP half of the same scenario. `rule_refused` with `rule: "per_tx"` is what lets a
/// caller decide between spending less and stopping; `code: "refused"` with the name somewhere in a
/// sentence leaves that decision to a substring match, which is what R3 is about.
#[test]
fn the_mcp_response_for_a_refusal_carries_the_rule_name_in_a_field() {
    let mut context = WalletContext::new(None, "testnet".into(), false);
    let refusal = rill_chain::aborts::classify_rule_abort(PER_TX_ABORT).expect("a rule abort");

    let response = failure_response(
        &mut context,
        json!(1),
        "spend_failed",
        &Failure::Refused(refusal),
    );

    assert_eq!(response["result"]["isError"], json!(true));
    let out = structured(&response);
    assert_eq!(out["code"], json!("rule_refused"));
    assert_eq!(out["rule"], json!("per_tx"));
    assert_eq!(out["abortCode"], json!(1));
    assert!(
        out["advice"].as_str().unwrap().contains("Spend less"),
        "the advice must follow the rule: {out}"
    );
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .contains("larger than the per-transaction cap"),
        "{out}"
    );

    // And the refusal survives into the next question an operator asks.
    let status = call(&mut context, "rill_status", json!({}));
    assert!(structured(&status)["lastRejection"]
        .as_str()
        .unwrap()
        .starts_with("per_tx refused it"));
}

/// A failure that is not a rule keeps its own code, so "the node is down" never reads as "your
/// limits stopped you".
#[test]
fn a_failure_that_is_not_a_refusal_is_not_reported_as_a_rule() {
    let mut context = WalletContext::new(None, "testnet".into(), false);
    let response = failure_response(
        &mut context,
        json!(1),
        "spend_failed",
        &Failure::Failed("the node did not answer, so there is no verdict".into()),
    );
    let out = structured(&response);
    assert_eq!(out["code"], json!("spend_failed"));
    assert!(out.get("rule").is_none(), "nothing refused this: {out}");
}

/// The agent's own key cannot bound the agent's own wallet, and the chain says which rule said so.
///
/// `add_rule` asserts the wallet's owner, so this is refused with `E_NOT_OWNER` whoever offers the
/// tool. That is the reason an owner-side tool can sit on the agent's surface at all.
#[test]
fn an_attach_signed_by_the_agent_is_refused_naming_agent_wallet() {
    let agent = key(11);
    let chain = chain_for_attach(&agent, &[]).with_simulation(SimulationBehavior::Fails {
        error: NOT_OWNER_ABORT.into(),
    });

    let failure = run(attach_json_on(&chain, &agent, &rules_args(WALLET)))
        .expect_err("attaching is owner-only");

    let Failure::Refused(refusal) = &failure else {
        panic!("E_NOT_OWNER is a named refusal: {failure}");
    };
    assert_eq!((refusal.module.as_str(), refusal.code), ("agent_wallet", 1));
    assert!(
        failure.to_string().contains("--as <owner>"),
        "the advice must name the fix: {failure}"
    );
    assert!(
        chain.submitted().is_empty(),
        "a refused attach must never reach the chain"
    );
}

/// A create the chain would refuse is never signed.
#[test]
fn a_create_the_chain_would_refuse_is_never_signed() {
    let owner = key(7);
    let agent = key(11);
    let chain = chain_for_create(&owner, &agent).with_simulation(SimulationBehavior::Fails {
        error: "InsufficientCoinBalance".into(),
    });

    let failure = run(create_json_on(
        &chain,
        &owner,
        &create_args(&agent.address().to_string()),
        now_ms(),
    ))
    .expect_err("the simulation gate must hold");

    assert!(
        failure.to_string().contains("InsufficientCoinBalance"),
        "the node's own words, kept: {failure}"
    );
    assert!(chain.submitted().is_empty(), "nothing may be submitted");
}

// ── the surface ───────────────────────────────────────────────────────────────────────────────

/// Every step from no wallet to a bounded spend is on the wire, and only the submitting tools are
/// destructive.
///
/// Asserted through `tools/list` rather than on the producer, because the annotation an MCP client
/// reads is the one that crossed the transport. A client decides from `destructiveHint` whether to
/// stop and ask a human.
#[test]
fn the_transport_advertises_the_whole_flow_and_marks_only_the_submitting_tools_destructive() {
    let mut context = WalletContext::new(None, "testnet".into(), false);
    let response = handle(
        &mut context,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .expect("a request gets a reply");
    let tools = response["result"]["tools"].as_array().expect("a tool list");

    const SUBMITS: &[&str] = &[
        "rill_create_wallet",
        "rill_attach_rules",
        "rill_spend",
        "rill_swap",
        "rill_stake",
        "rill_execute",
    ];
    for step in SUBMITS {
        assert!(
            tools.iter().any(|t| t["name"] == *step),
            "{step} is missing, so the flow still needs a human at a terminal"
        );
    }
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or_default();
        let destructive = tool["annotations"]["destructiveHint"] == json!(true);
        assert_eq!(
            SUBMITS.contains(&name),
            destructive,
            "{name}: submits={} but destructive={destructive}",
            SUBMITS.contains(&name)
        );
    }
}

#[test]
fn creating_a_wallet_with_no_key_says_whose_key_is_missing() {
    let mut context = WalletContext::new(None, "testnet".into(), false);
    let response = call(
        &mut context,
        "rill_create_wallet",
        json!({ "agent": WALLET, "amount": "0.05", "budget": BUDGET_MIST, "perTx": PER_TX_MIST }),
    );
    let out = structured(&response);
    assert_eq!(out["code"], json!("no_key"));
    assert!(
        out["message"].as_str().unwrap().contains("owner"),
        "the key this signer holds is what becomes the owner: {out}"
    );
}

#[test]
fn attaching_rules_without_a_wallet_id_says_which_argument_is_missing() {
    let mut context = WalletContext::new(Some(key(7)), "testnet".into(), false);
    let response = call(
        &mut context,
        "rill_attach_rules",
        json!({ "budget": BUDGET_MIST, "perTx": PER_TX_MIST }),
    );
    let out = structured(&response);
    assert_eq!(out["code"], json!("invalid_arguments"));
    assert!(out["message"].as_str().unwrap().contains("wallet"), "{out}");
}

/// An amount that is not whole mist is refused before anything is read from the chain, and the
/// answer names the argument the caller typed rather than a field from the manifest projection.
#[test]
fn a_cap_that_is_not_whole_mist_is_refused_by_the_name_the_caller_used() {
    let mut context = WalletContext::new(Some(key(7)), "testnet".into(), false);
    let response = call(
        &mut context,
        "rill_attach_rules",
        json!({ "wallet": WALLET, "budget": "0.05", "perTx": PER_TX_MIST }),
    );
    let out = structured(&response);
    assert_eq!(out["code"], json!("invalid_arguments"));
    let message = out["message"].as_str().unwrap();
    assert!(message.contains("budget"), "{message}");
    assert!(
        message.contains("1000000000 mist"),
        "say what the unit is: {message}"
    );
}

// ── execute: the run-set, and the second call ─────────────────────────────────────────────────

/// A run without a run-set is refused before signing, and names what is missing and where to put it.
#[test]
fn an_execute_without_a_run_set_is_refused_before_signing_and_names_what_is_missing() {
    let mut context = WalletContext::new(Some(key(7)), "testnet".into(), false);
    let response = call(&mut context, "rill_execute", json!({ "envelope": {} }));
    let out = structured(&response);
    assert_eq!(out["code"], json!("no_run_set"));
    let message = out["message"].as_str().unwrap();
    assert!(
        message.contains("RILL_RUN_SET_PATH"),
        "a refusal that cannot be acted on is half a refusal: {message}"
    );
    assert!(
        context.submitted.is_empty(),
        "nothing may be remembered as submitted when nothing was signed"
    );
}

/// Re-issuing an execute call does not start a second operation.
///
/// The signer remembers every envelope it handed to the chain, by the digest pinned from the bytes
/// it signed, and refuses the second call with the first one's digest. The envelope is identical on
/// a retry, so the agent cannot tell its retry from its first attempt; the signer can.
#[test]
fn re_issuing_an_execute_call_for_a_submitted_envelope_is_refused_with_the_first_digest() {
    let mut context = context_with_run_set();
    let envelope = envelope();
    let pinned = rill_core::envelope::digest_unsigned_ptb(&real_ptb());
    context.submitted.insert(
        pinned.clone(),
        Submission {
            digest: Some("dxzyeAfW5eRdGUobBNUGeu2mnmaN4xyzY7J8dZxL5fZ".into()),
        },
    );

    let response = call(
        &mut context,
        "rill_execute",
        json!({ "envelope": envelope }),
    );
    let out = structured(&response);
    assert_eq!(out["code"], json!("already_submitted"));
    assert_eq!(out["pinnedDigest"], json!(pinned));
    assert_eq!(
        out["priorDigest"],
        json!("dxzyeAfW5eRdGUobBNUGeu2mnmaN4xyzY7J8dZxL5fZ"),
        "the answer has to carry the digest of the transaction that did land: {out}"
    );
    assert!(out["message"].as_str().unwrap().contains("second"), "{out}");
}

/// A submission whose answer was lost is not retried blind: that is how one intended spend becomes
/// two. The refusal says to look on chain first.
#[test]
fn an_envelope_whose_submission_was_lost_is_refused_until_someone_looks_on_chain() {
    let mut context = context_with_run_set();
    let pinned = rill_core::envelope::digest_unsigned_ptb(&real_ptb());
    context
        .submitted
        .insert(pinned, Submission { digest: None });

    let response = call(
        &mut context,
        "rill_execute",
        json!({ "envelope": envelope() }),
    );
    let out = structured(&response);
    assert_eq!(out["code"], json!("already_submitted"));
    assert_eq!(out["priorDigest"], Value::Null);
    let message = out["message"].as_str().unwrap();
    assert!(message.contains("unknown"), "{message}");
    assert!(message.contains("Look for it on chain"), "{message}");
}

/// The code and the tool's own words agree about what a second call does.
///
/// They did not. The answer said "calling again with the same envelope submits a second
/// transaction", which described a footgun instead of removing it, and the plan's scenario says the
/// opposite. Whichever way that was settled, a note and a description that disagree with the code
/// are worse than either: an agent reads them and not the code.
#[test]
fn the_note_and_the_description_say_what_the_code_now_does() {
    let shipped = shipped(STDIO);
    // The note itself, not the file: the comment above it explains the behaviour that was removed
    // and has to be free to quote it. Everything after the last `"note":` inside `execute` is the
    // text the successful answer carries.
    let execute = shipped
        .split("fn execute(")
        .nth(1)
        .and_then(|body| body.split("\nfn decode_for_signing(").next())
        .expect("stdio.rs defines execute, followed by decode_for_signing");
    let note = execute
        .rsplit("\"note\":")
        .next()
        .expect("the successful answer carries a note");
    for promise in ["submits a second", "sends a second", "starts a second"] {
        assert!(
            !note.contains(promise),
            "the success note still promises the second submission the code now refuses: {note}"
        );
    }
    assert!(
        note.contains("refused"),
        "the note must say a second call is refused: {note}"
    );
    assert!(
        shipped.contains("already_submitted"),
        "the refusal this note describes must exist in the code"
    );

    let description = rill_mcp::tools(rill_mcp::Surface::Wallet)
        .into_iter()
        .find(|t| t.name == "rill_execute")
        .and_then(|t| t.description.map(|d| d.to_string()))
        .expect("rill_execute has a description");
    assert!(
        description.contains("refused on a second call"),
        "the description must say what happens on a retry: {description}"
    );
    assert!(
        !description.contains("a second call submits a second transaction"),
        "the description still describes the behaviour that was removed"
    );
}

/// Each step waits for the node to catch up before it hands an id to the next one.
///
/// Structural, because the cheap half of this property is covered where it lives
/// (`rill_chain::settle`'s own tests, which exercise giving up in milliseconds) and the expensive
/// half is a node that is slow: an offline fake either has the object or never gets it, so a test
/// that drove the wait for thirty seconds would be paying half a minute to learn what the unit test
/// already says. What is left is the wiring, and the wiring is what went missing: both of these
/// waits were added after a live run, one failure each, with the transaction already on chain.
///
/// create: `reading the wallet object: not found on chain: object 0x78f283fe…`, one call after the
/// effects named that id.
/// attach: the spend read the wallet's policy, got the list from before the attach, emitted no
/// proofs, and the chain aborted it with `E_POLICY_UNSATISFIED` (10).
#[test]
fn each_writing_step_waits_for_the_node_before_promising_anything_to_the_next() {
    let create = shipped(WALLET_CMD);
    assert!(
        create.contains("settle::wait_until_readable("),
        "create must wait until the wallet it minted can be read, or the attach fails on it"
    );
    assert!(
        create.contains(r#"report["visibleOnNode"]"#),
        "and must say whether the wait succeeded"
    );

    let attach = shipped(RULES_CMD);
    // The call, not the name: `async fn rules_settled(` carries the name too, so a check for the
    // name alone passes on a version that defines the wait and never calls it. It was written that
    // way first, and deleting the call left the test green.
    assert!(
        attach.contains("let visible = rules_settled("),
        "attach must wait until a read reports the rules it wrote, or the spend emits the wrong \
         proofs and the chain aborts it"
    );
    assert!(
        attach.contains(r#"report["visibleOnNode"]"#),
        "and must say whether the wait succeeded"
    );
}

/// Both new tools do all of their chain work inside one runtime.
///
/// `block_on` builds a current-thread runtime per call and drops it on return, taking the tonic
/// channel's connection task with it, so a client built in one `block_on` and used in another is
/// already closed. That cost `rill_execute` its submission once, and the guard for it lives in
/// `enforcement_claims.rs`. This is the same guard for the two tools added here, which talk to the
/// chain the same way.
#[test]
fn the_new_tools_keep_their_chain_client_inside_one_runtime() {
    let shipped = shipped(STDIO);
    for (tool, next_item) in [
        ("fn create_wallet(", "\nfn attach_rules("),
        ("fn attach_rules(", "\nfn bounded_manifest("),
    ] {
        let start = shipped
            .find(tool)
            .unwrap_or_else(|| panic!("stdio.rs must define {tool}"));
        let body = &shipped[start..];
        let end =
            body.len()
                .min(body.find(next_item).unwrap_or_else(|| {
                    panic!("{tool} is followed by {next_item}; re-anchor this")
                }));
        let runtimes = body[..end]
            .lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| line.contains("block_on("))
            .count();
        assert_eq!(
            runtimes, 1,
            "{tool} uses {runtimes} runtimes; a tonic channel built in one and used in another is \
             already closed, so the submission can never land"
        );
    }
}

// ── the envelope fixture ──────────────────────────────────────────────────────────────────────

fn context_with_run_set() -> WalletContext {
    let run_set = serde_json::from_value(json!({
        "label": "u4-flow",
        "network": "testnet",
        "sender": WALLET,
        "actionId": "skill_hero",
        "walletPackageId": PACKAGE,
        "walletId": WALLET,
        "agentCapId": CAP,
        "versionId": VERSION,
        "capabilityManifest": {
            "walletCoinType": "0x2::sui::SUI",
            "rules": [{ "kind": "budget", "totalMist": BUDGET_MIST }]
        },
        "allowedTargets": [format!("{PACKAGE}::agent_wallet::request_spend")],
        "allowedObjectIds": [WALLET],
        "maxAmountBaseUnits": "2000000000",
        "declaredSpendBaseUnits": "2000000000",
        "minimumRemainingBaseUnits": "0",
        "gasCeilingBaseUnits": "50000000"
    }))
    .expect("the run-set parses");
    WalletContext::new(Some(key(13)), "testnet".into(), false).with_run_set(Some(run_set))
}

/// A real transaction, built the way the server builds one. Three zero bytes would pass every check
/// that does not read the bytes, and `pin_bytes` reads them.
fn real_ptb() -> String {
    use sui_sdk_types::{Address, Identifier};
    use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

    let mut tx = TransactionBuilder::new();
    tx.set_sender(WALLET.parse::<Address>().unwrap());
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(COIN.parse().unwrap(), 1, Digest::ZERO)]);
    let wallet = tx.object(ObjectInput::shared(WALLET.parse().unwrap(), 400_001, true));
    tx.move_call(
        Function::new(
            PACKAGE.parse().unwrap(),
            Identifier::new("agent_wallet").unwrap(),
            Identifier::new("request_spend").unwrap(),
        ),
        vec![wallet],
    );
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .encode(bcs::to_bytes(&tx.try_build().unwrap()).unwrap())
}

/// An envelope that passes every local check, so the refusals under test are the ones being tested
/// rather than a malformed fixture.
fn envelope() -> Value {
    json!({
        "version": "1",
        "actionId": "skill_hero",
        "actionDigest": rill_core::envelope::digest_unsigned_ptb(&real_ptb()),
        "network": "testnet",
        "sender": WALLET,
        "walletPackageId": PACKAGE,
        "walletId": WALLET,
        "agentCapId": CAP,
        "balanceManagerId": "0xbm",
        "tradeCapId": "0xtc",
        "resolvedParams": {
            "poolKey": "DEEP_SUI", "poolId": "0xpool", "clientOrderId": "1",
            "spendAmountMist": "1000000000", "price": "2.5", "quantity": "1",
            "depositSui": "1", "isBid": true, "payWithDeep": false
        },
        "allowedTargets": [format!("{PACKAGE}::agent_wallet::request_spend")],
        "requiredObjectIds": [WALLET],
        "requiredGuards": [],
        "unsignedPtb": real_ptb(),
        "preview": "place a limit order",
        "simulation": {
            "ok": true, "verification": "verified", "gasEstimate": "2000000",
            "balanceChanges": [], "objectChanges": []
        },
        "expiresAt": expiry_at(now_ms() + 60_000)
    })
}

/// RFC 3339, because the freshness check parses one and rejects a TTL beyond five minutes, so a
/// fixed far-future date will not do. Nothing in this crate's dependency tree formats a timestamp,
/// and pulling in a date library for one test fixture is the worse trade.
fn expiry_at(ms: u64) -> String {
    let secs = ms / 1000;
    let rem = secs % 86_400;
    let (y, m, d) = civil((secs / 86_400) as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60,
        ms % 1000
    )
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── live, against testnet ─────────────────────────────────────────────────────────────────────
//
// Everything above runs offline. These three drive the same tools through the same entry point
// against a real node, which is the only place the chain gets the last word about a refusal.
//
//   cargo test -p rill --test execute_flow -- --ignored --nocapture
//
// Two of them cost nothing: a refusal is reached in simulation, so nothing is signed and no gas is
// spent. The flow test submits four transactions and revokes the wallet at the end, which returns
// the unspent funding to the owner.

/// The two identities the recorded runs use. The owner creates and bounds; the agent spends.
const LIVE_OWNER: &str = "0xb649a075e07c7cf0baebeaa82150416218c63943e2e767fe93a24aa5c7ce64a9";
const LIVE_AGENT: &str = "0xb93cbb8f841a3442e5112c50880f20db9735cb1bb5f1459e745c5f602a2fe29a";

/// The bounded wallet already on testnet: budget 0.2 SUI, per-transaction cap 0.05 SUI. Used by the
/// two free refusals, which need a bounded wallet and do not need a fresh one.
const LIVE_WALLET: &str = "0x74d0e7b3d0956b08d40834ef19ae0fc9c48f35b09a928c57a517d6a20d8859cf";
const LIVE_CAP: &str = "0x564865bc159794e5873d8ef2548cfa110ff50fefe650f1bbd70b08a8d621ef76";

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// A signer holding one of the two keys, as `rill-wallet mcp` would have loaded it.
fn live_context(address: &str) -> WalletContext {
    let keystore = Keystore::load_for(address.parse().expect("a constant address")).expect(
        "both keys live in the local sui keystore; this test reads them and never prints one",
    );
    assert_eq!(
        keystore.address().to_string(),
        address,
        "the keystore handed back a different key than the one asked for"
    );
    WalletContext::new(Some(keystore), "testnet".into(), false)
}

/// A spend over the wallet's per-transaction cap, refused by the chain and named, over the
/// transport. Costs nothing: the refusal is reached in simulation, so nothing is signed.
#[test]
#[ignore = "reads live testnet state"]
fn an_over_cap_spend_over_the_transport_is_refused_naming_per_tx() {
    let mut context = live_context(LIVE_AGENT);
    let response = call(
        &mut context,
        "rill_spend",
        json!({ "wallet": LIVE_WALLET, "cap": LIVE_CAP, "amount": "0.06" }),
    );
    eprintln!("{}", serde_json::to_string_pretty(&response).unwrap());

    let out = structured(&response);
    assert_eq!(response["result"]["isError"], json!(true));
    assert_eq!(out["code"], json!("rule_refused"));
    assert_eq!(
        out["rule"],
        json!("per_tx"),
        "0.06 is inside the 0.2 budget and over the 0.05 cap, so per_tx is what refused"
    );
    assert_eq!(out["abortCode"], json!(1));
    assert!(out["advice"].as_str().unwrap().contains("Spend less"));
}

/// The agent cannot widen its own limits, and the contract is what says so. Costs nothing, for the
/// same reason: `add_rule` asserts the owner before anything moves.
#[test]
#[ignore = "reads live testnet state"]
fn an_agent_signed_attach_over_the_transport_is_refused_naming_agent_wallet() {
    let mut context = live_context(LIVE_AGENT);
    let response = call(
        &mut context,
        "rill_attach_rules",
        json!({ "wallet": LIVE_WALLET, "budget": "200000000", "perTx": "50000000" }),
    );
    eprintln!("{}", serde_json::to_string_pretty(&response).unwrap());

    let out = structured(&response);
    assert_eq!(response["result"]["isError"], json!(true));
    assert_eq!(out["code"], json!("rule_refused"));
    assert_eq!(
        (out["rule"].as_str(), out["abortCode"].as_u64()),
        (Some("agent_wallet"), Some(1)),
        "E_NOT_OWNER is 1, and the refusal must name the module that asserted it"
    );
    assert!(out["advice"].as_str().unwrap().contains("owner"));
}

/// The whole of R3, over the transport, against the chain: no wallet, then a bounded wallet, then a
/// spend inside the bound, then a refusal that names the rule.
///
/// Four submissions and a revoke. The funding is small and the revoke returns what is left, so the
/// standing cost of running this is gas. Every id used after the first step comes out of the step
/// before it, which is the part that used to be a person reading a terminal.
#[test]
#[ignore = "submits four testnet transactions"]
fn the_whole_flow_runs_over_the_mcp_transport_against_testnet() {
    let mut owner = live_context(LIVE_OWNER);

    println!("1. the owner mints a bounded wallet, 0.02 SUI funded, 0.01 per transaction");
    let created = call(
        &mut owner,
        "rill_create_wallet",
        json!({
            "agent": LIVE_AGENT, "amount": "0.02",
            "budget": "20000000", "perTx": "10000000"
        }),
    );
    println!("{}", serde_json::to_string_pretty(&created).unwrap());
    assert_eq!(created["result"]["isError"], json!(false), "{created}");
    let created = structured(&created).clone();
    assert_eq!(created["agent"], json!(LIVE_AGENT));
    let wallet_id = created["wallet"]
        .as_str()
        .expect("the wallet id comes out of the effects")
        .to_owned();
    let cap_id = created["cap"].as_str().expect("and the cap id").to_owned();
    println!("   create : {}", created["digest"]);

    println!("\n2. the owner attaches the rules that bound it");
    let attached = call(
        &mut owner,
        "rill_attach_rules",
        json!({ "wallet": wallet_id, "budget": "20000000", "perTx": "10000000" }),
    );
    println!("{}", serde_json::to_string_pretty(&attached).unwrap());
    assert_eq!(attached["result"]["isError"], json!(false), "{attached}");
    let attached = structured(&attached).clone();
    assert_eq!(attached["rules"], json!(["budget", "per_tx"]));
    println!("   attach : {}", attached["digest"]);

    println!("\n3. the agent spends 0.005 SUI, inside both caps");
    let mut agent = live_context(LIVE_AGENT);
    let spent = call(
        &mut agent,
        "rill_spend",
        json!({ "wallet": wallet_id, "cap": cap_id, "amount": "0.005" }),
    );
    println!("{}", serde_json::to_string_pretty(&spent).unwrap());
    assert_eq!(spent["result"]["isError"], json!(false), "{spent}");
    let spent = structured(&spent).clone();
    assert_eq!(spent["submitted"], json!(true));
    assert_eq!(spent["rules"], json!(["budget", "per_tx"]));
    println!("   spend  : {}", spent["digest"]);

    println!("\n4. the agent tries 0.015 SUI, over the per-transaction cap");
    let refused = call(
        &mut agent,
        "rill_spend",
        json!({ "wallet": wallet_id, "cap": cap_id, "amount": "0.015" }),
    );
    println!("{}", serde_json::to_string_pretty(&refused).unwrap());
    let refused = structured(&refused).clone();
    assert_eq!(refused["code"], json!("rule_refused"));
    assert_eq!(refused["rule"], json!("per_tx"), "{refused}");

    println!("\n5. the owner revokes, and the unspent funding comes back");
    let keystore = Keystore::load_for(LIVE_OWNER.parse().expect("a constant address"))
        .expect("the owner's key");
    run(rill_cli::revoke_cmd::revoke(
        TESTNET,
        &keystore,
        &rill_cli::revoke_cmd::RevokeArgs {
            package_id: rill_ptb::deployments::TESTNET_AGENT_WALLET.to_string(),
            wallet_id: wallet_id.clone(),
            recipient: None,
            gas_budget: 30_000_000,
            dry_run: false,
        },
    ))
    .expect("the owner can always revoke");

    println!(
        "\nPASS: no wallet to a bounded spend and a named refusal, entirely over MCP.\n\
         \x20 wallet {wallet_id}\n\
         \x20 cap    {cap_id}\n\
         \x20 create {}\n\
         \x20 attach {}\n\
         \x20 spend  {}\n\
         \x20 refusal per_tx, nothing submitted",
        created["digest"], attached["digest"], spent["digest"]
    );
}
