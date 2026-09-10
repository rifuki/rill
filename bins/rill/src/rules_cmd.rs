//! `rill wallet rules` — the second transaction, which is what makes a capability mean anything.
//!
//! A wallet created and left alone has an empty policy, and `confirm_spend` on an empty policy
//! requires zero receipts. So this step is not configuration; it is the difference between a
//! capability that is bounded and one that is not.
//!
//! # Owner-only, and the chain is what enforces that
//!
//! Every `add` goes through `agent_wallet::add_rule`, which asserts the sender is the wallet's
//! owner. A signer holding the agent's key is refused by the contract with `E_NOT_OWNER`, named,
//! rather than by this process declining to try. That is the property worth having: the agent
//! cannot widen its own limits even if it drives every tool on this surface.
//!
//! # Two callers, one producer
//!
//! A person reads lines and an agent reads JSON on a stream a stray `println!` would corrupt, so
//! the work happens in [`attach_json`] and [`attach`] only renders it.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_core::manifest::CapabilityManifest;
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::rules::{build_reconcile_rules, RuleTarget};
use rill_ptb::shared::SharedObjects;
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;
use crate::verdict::{did_fail, no_verdict, submit_failed, would_fail, Failure};

const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

pub struct RulesArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub manifest: CapabilityManifest,
    pub gas_budget: u64,
    pub dry_run: bool,
}

/// The printing command. Everything it knows comes from [`attach_json`]; it only renders.
pub async fn attach(endpoint: &str, keystore: &Keystore, args: &RulesArgs) -> Result<(), String> {
    let report = attach_json(endpoint, keystore, args)
        .await
        .map_err(|failure| failure.to_string())?;
    let text = |key: &str| report[key].as_str().unwrap_or_default().to_owned();
    let list = |key: &str| {
        report[key]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .map(|v| v.as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };

    println!("wallet  : {}", text("wallet"));
    println!("owner   : {}", text("owner"));
    let before = list("attachedBefore");
    println!(
        "attached: {}",
        if before.is_empty() { "none" } else { &before }
    );
    if !list("reSet").is_empty() {
        println!("re-set  : {}", list("reSet"));
    }
    if !list("dropped").is_empty() {
        println!("dropping: {}", list("dropped"));
    }
    println!("result  : {}", list("rules"));
    println!("\nsimulation: ok=true gas={}", report["gasUsed"]);

    if report["submitted"] != Value::Bool(true) {
        println!("\ndry run: nothing signed. Re-run with --submit.");
        return Ok(());
    }
    println!("\ndigest  : {}", text("digest"));
    println!("success : true");
    println!("gas used: {}", report["gasUsed"]);
    println!("\n{}", text("note"));
    Ok(())
}

/// Reconcile the wallet's rules to the manifest, and return what happened as structured data.
pub async fn attach_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &RulesArgs,
) -> Result<Value, Failure> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    attach_json_on(&chain, keystore, args).await
}

/// The attach itself, against any chain.
///
/// Split out so the whole path can be driven offline against [`rill_chain::fake::FakeSui`]: two
/// shared versions read rather than assumed, the live rule list read back before anything is
/// built, the reconciliation, and the simulation gate. All of it needed a live node, so none of it
/// ran in CI.
///
/// The client is created by the caller and used here, which keeps it inside the caller's runtime. A
/// tonic channel built in one runtime and used in another is already closed.
pub async fn attach_json_on(
    chain: &(impl SuiRead + SuiWrite),
    keystore: &Keystore,
    args: &RulesArgs,
) -> Result<Value, Failure> {
    let sender = keystore.address();

    let wallet_id: Address = args
        .wallet_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.wallet_id))?;
    let version_id: Address = args
        .version_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.version_id))?;

    // Both are shared, and both must be referenced by the version they were shared at.
    let mut shared = SharedObjects::new();
    for (label, id, raw) in [
        ("wallet", wallet_id, args.wallet_id.as_str()),
        ("version", version_id, args.version_id.as_str()),
    ] {
        let summary = chain
            .get_object(raw)
            .await
            .map_err(|e| format!("reading the {label} object: {e}"))?;
        let initial = summary
            .shared_initial_version
            .ok_or_else(|| format!("the {label} object {raw} is not shared"))?;
        shared.insert(id, initial);
    }

    let owned = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(|e| format!("listing the sender's objects: {e}"))?;
    let gas: Vec<_> = owned
        .iter()
        .filter(|o| o.object_type.as_deref() == Some(SUI_COIN_TYPE))
        .collect();
    if gas.is_empty() {
        return Err(Failure::Failed(format!(
            "{sender} holds no SUI to pay for this"
        )));
    }

    // Read, not assumed. Testnet answers 1000 and mainnet answers 100, so a literal that is right
    // on one network is ten times the price on the other, and a price below the reference is
    // rejected outright rather than merely running slow. Read once and used twice: the node
    // refuses a read priced below the reference exactly as it refuses a submission.
    let gas_price = chain
        .reference_gas_price()
        .await
        .map_err(|e| format!("reading the reference gas price: {e}"))?;

    // What the wallet actually carries. Attaching is not idempotent: add_rule aborts
    // E_RULE_ALREADY_SET, so this must be a reconciliation against the live set, not an attach.
    let package_id: Address = args
        .package_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.package_id))?;
    let attached = live_rules(chain, package_id, wallet_id, &shared, gas_price).await?;

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(args.gas_budget);
    tx.set_gas_price(gas_price);
    tx.add_gas_objects(gas.iter().map(|c| {
        ObjectInput::owned(
            c.reference.id.parse().expect("an id from the chain"),
            c.reference.version,
            c.reference.digest.parse::<Digest>().expect("a digest"),
        )
    }));

    let target = RuleTarget {
        package_id,
        wallet_id,
        version_id,
        coin_type: "0x2::sui::SUI".into(),
        manifest: args.manifest.clone(),
    };

    let module_refs: Vec<&str> = attached.iter().map(String::as_str).collect();
    let result = build_reconcile_rules(&mut tx, &target, &module_refs, &shared)
        .map_err(|e| e.to_string())?;

    let built = tx.try_build().map_err(|e| format!("compiling: {e}"))?;
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&built).map_err(|e| e.to_string())?)
    };

    let mut report = json!({
        "wallet": wallet_id.to_string(),
        "owner": sender.to_string(),
        "attachedBefore": attached,
        "reSet": result.removed,
        "dropped": result.orphaned,
        "rules": result.added,
        "rulesSource": "read from chain",
        "submitted": false,
    });

    let outcome = chain.simulate(&b64).await.map_err(no_verdict)?;
    if !outcome.ok {
        return Err(would_fail(outcome.error));
    }
    report["gasUsed"] = json!(outcome.gas_used_mist);

    if args.dry_run {
        report["note"] = json!("Simulated only. Nothing was signed and nothing was submitted.");
        return Ok(report);
    }

    let signature = keystore.sign(&built).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(submit_failed)?;
    if let Some(error) = &outcome.error {
        return Err(did_fail(error));
    }

    report["submitted"] = json!(true);
    report["digest"] = json!(outcome.digest);
    report["gasUsed"] = json!(outcome.gas_used_mist);

    // The rules this wrote have to be readable before the next step builds anything, and certified
    // is not the same as visible: see [`rill_chain::settle`]. The spend reads a wallet's live policy
    // to decide which `prove` calls to emit, and one call after this landed it read the list from
    // before it, emitted no proofs, and the chain aborted the spend with E_POLICY_UNSATISFIED (10).
    // That refusal is correct and unactionable: the agent had done nothing wrong.
    let visible = rules_settled(
        chain,
        package_id,
        wallet_id,
        &shared,
        gas_price,
        &result.added,
    )
    .await;
    report["visibleOnNode"] = json!(visible);

    report["note"] = json!(format!(
        "Submitted and confirmed. This cannot be undone. The wallet is now bounded by {} rule(s), \
         and every spend must satisfy all of them. Calling again reads the live rules first, so a \
         repeat with the same manifest changes nothing; a rule whose value differs is removed and \
         re-added, because add_rule is not idempotent.{}",
        result.added.len(),
        if visible {
            ""
        } else {
            " The node that answered still reports a different rule list than this transaction \
             wrote, so a spend built right now may emit the wrong proofs and be aborted. The \
             transaction landed regardless, and its digest is above. Read the wallet again before \
             spending."
        }
    ));
    Ok(report)
}

/// The rule modules a wallet carries, read from the chain that holds them.
///
/// Read rather than assumed, and read again after a write: the list decides what `add` calls a
/// reconciliation emits and what `prove` calls a spend emits, and a guess is wrong in both
/// directions.
async fn live_rules(
    chain: &impl SuiRead,
    package_id: Address,
    wallet_id: Address,
    shared: &SharedObjects,
    gas_price: u64,
) -> Result<Vec<String>, Failure> {
    let read_tx =
        policy_rules_transaction(package_id, wallet_id, "0x2::sui::SUI", shared, gas_price)
            .map_err(|e| e.to_string())?;
    let read_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&read_tx).map_err(|e| e.to_string())?)
    };
    let read = chain
        .simulate_read(&read_b64)
        .await
        .map_err(|e| format!("reading the wallet's rules: {e}"))?;
    let names = read
        .command_returns
        .iter()
        .flatten()
        .next()
        .ok_or("the wallet did not report its rules")
        .and_then(|b| parse_type_names(b).map_err(|_| "the rule list did not decode"))?;
    Ok(attached_modules(&names)
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// Wait until a read of the wallet reports the rules that were just attached.
///
/// `false` means the node never agreed within the budget. Not an error: the transaction is on chain
/// and its digest is the proof. What the caller can no longer promise is that a spend built now will
/// emit the right proofs, which is something to say rather than to hope about.
///
/// Compared as sets, because the order a wallet reports its rules in is the chain's business.
async fn rules_settled(
    chain: &impl SuiRead,
    package_id: Address,
    wallet_id: Address,
    shared: &SharedObjects,
    gas_price: u64,
    expected: &[String],
) -> bool {
    let mut wanted: Vec<&str> = expected.iter().map(String::as_str).collect();
    wanted.sort_unstable();
    for attempt in 0..rill_chain::settle::TRIES {
        if let Ok(live) = live_rules(chain, package_id, wallet_id, shared, gas_price).await {
            let mut live: Vec<&str> = live.iter().map(String::as_str).collect();
            live.sort_unstable();
            if live == wanted {
                return true;
            }
        }
        if attempt + 1 < rill_chain::settle::TRIES {
            std::thread::sleep(rill_chain::settle::PAUSE);
        }
    }
    false
}
