//! `rill wallet create` — the whole first transaction, end to end.
//!
//! Build, strictly simulate, sign, submit, and read back the ids the next step needs. Every stage
//! is a separate refusal: a simulation that fails never reaches the signing code, and a signature
//! is never produced for bytes the chain has not already agreed would execute.
//!
//! # Why the ids are returned rather than remembered
//!
//! `create_wallet` shares a wallet and mints a capability, and neither id exists until the
//! transaction lands. Everything after this step needs both. They come out of the effects and are
//! handed back. The alternative, writing them into a state file the user did not ask for, is a
//! second source of truth about what exists on chain.
//!
//! # Two callers, one producer
//!
//! A person runs `rill wallet create` and reads lines. An agent calls `rill_create_wallet` over MCP
//! and reads JSON, on a stream where a stray `println!` corrupts the protocol. So the work happens
//! in [`create_json`], which returns the data, and [`create`] only renders it. Neither can drift
//! from the other, because there is nothing to drift: the printing path knows only what the JSON
//! carries.

use rill_chain::{grpc::GrpcSui, ChainError, SuiRead, SuiWrite};
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::create::{build_create_wallet, NewWallet};
use rill_ptb::shared::SharedObjects;
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;
use crate::verdict::{did_fail, no_verdict, submit_failed, would_fail, Failure};

/// Fully-expanded SUI, the way the chain writes it in an object type.
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// What a freshly created wallet permits, which is nothing, said where both callers read it.
const NO_RULES_YET: &str =
    "This wallet has NO rules attached yet, and confirm_spend on an empty policy requires zero \
     receipts, so the capability is unbounded until rules are attached. Attach them before the \
     cap is handed to anything: `rill wallet rules --wallet <id> --submit`, or the rill_attach_rules \
     tool with this wallet id.";

pub struct CreateArgs {
    pub package_id: String,
    pub version_id: String,
    /// Who receives the `AgentCap`. Defaults to the signer.
    pub agent: Option<String>,
    /// Decimal SUI to fund the wallet with.
    pub amount: String,
    pub expires_in_days: u64,
    pub manifest: CapabilityManifest,
    pub gas_budget: u64,
    /// Stop after the simulation rather than signing. The default, because submitting is not
    /// something to do by accident.
    pub dry_run: bool,
}

/// The printing command. Everything it knows comes from [`create_json`]; it only renders.
pub async fn create(
    endpoint: &str,
    keystore: &Keystore,
    args: &CreateArgs,
    now_ms: u64,
) -> Result<(), String> {
    let report = create_json(endpoint, keystore, args, now_ms)
        .await
        .map_err(|failure| failure.to_string())?;
    let text = |key: &str| report[key].as_str().unwrap_or_default().to_owned();

    println!("sender  : {}", text("sender"));
    println!("agent   : {}", text("agent"));
    println!("funding : {}", text("funding"));
    println!(
        "rules   : {}",
        report["rules"]
            .as_array()
            .map(|rules| rules
                .iter()
                .map(|r| r.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", "))
            .unwrap_or_else(|| "none".into())
    );
    println!("\nsimulation: ok=true gas={}", report["gasUsed"]);

    if report["submitted"] != Value::Bool(true) {
        println!("\ndry run: nothing signed, nothing submitted.");
        println!("re-run with --submit to sign and send it.");
        return Ok(());
    }

    println!("\ndigest  : {}", text("digest"));
    println!("success : true");
    println!("gas used: {}", report["gasUsed"]);

    println!("\ncreated:");
    let nothing = Vec::new();
    for object in report["created"].as_array().unwrap_or(&nothing) {
        println!(
            "  {}  {}\n      {}",
            object["objectId"].as_str().unwrap_or_default(),
            object["objectType"]
                .as_str()
                .unwrap_or("(type not reported)"),
            object["ownership"].as_str().unwrap_or("owned")
        );
    }

    // The two ids every later step needs, named rather than left to be picked out of the list.
    println!("\nnext step needs:");
    for (label, key) in [("wallet", "wallet"), ("cap", "cap")] {
        match report[key].as_str() {
            Some(id) => println!("  {label:7}: {id}"),
            None => println!("  {label:7}: not found in the effects, check the type filter"),
        }
    }
    // The note, not the constant: it carries what a fresh wallet permits and, when the node has
    // not indexed it yet, that too.
    println!("\n{}", text("note"));
    Ok(())
}

/// Mint the wallet and the capability, and return what happened as structured data.
pub async fn create_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &CreateArgs,
    now_ms: u64,
) -> Result<Value, Failure> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    create_json_on(&chain, keystore, args, now_ms).await
}

/// The creation itself, against any chain.
///
/// Split out so the whole path can be driven offline against [`rill_chain::fake::FakeSui`]: the
/// Version object's shared version read rather than assumed, every SUI coin collected rather than
/// the first, a gas price read because a literal is wrong on one of the two networks, the
/// simulation gate, and the ids picked out of the effects afterwards. All of it was reachable only
/// through a live node, so none of it ran in CI.
///
/// The client is created by the caller and used here, which keeps it inside the caller's runtime. A
/// tonic channel built in one runtime and used in another is already closed.
pub async fn create_json_on(
    chain: &(impl SuiRead + SuiWrite),
    keystore: &Keystore,
    args: &CreateArgs,
    now_ms: u64,
) -> Result<Value, Failure> {
    let sender = keystore.address();
    let version_id: Address = args
        .version_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.version_id))?;

    // The Version object's initial shared version, read rather than assumed.
    let summary = chain
        .get_object(&args.version_id)
        .await
        .map_err(|e: ChainError| format!("reading the Version object: {e}"))?;
    let initial = summary
        .shared_initial_version
        .ok_or("the Version object is not shared, which means this is not the right address")?;
    let mut shared = SharedObjects::new();
    shared.insert(version_id, initial);

    // Every SUI coin, not the first. A split the first coin alone cannot cover fails with
    // `InsufficientCoinBalance`, which reads like an empty account when it is not.
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
            "{sender} holds no SUI, so it cannot pay for anything"
        )));
    }

    let amount_mist = rill_core::amounts::decimal_to_base_units(&args.amount, 9)
        .map_err(|e| format!("the funding amount: {e}"))?;

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(args.gas_budget);
    // Read, not assumed. Testnet answers 1000 and mainnet answers 100, so a literal that is right
    // on one network is ten times the price on the other, and a price below the reference is
    // rejected outright rather than merely running slow.
    tx.set_gas_price(
        chain
            .reference_gas_price()
            .await
            .map_err(|e| format!("reading the reference gas price: {e}"))?,
    );
    tx.add_gas_objects(gas.iter().map(|c| {
        ObjectInput::owned(
            c.reference.id.parse().expect("an id from the chain"),
            c.reference.version,
            c.reference.digest.parse::<Digest>().expect("a digest"),
        )
    }));

    let value = tx.pure(&amount_mist);
    let gas_arg = tx.gas();
    let funds = tx
        .split_coins(gas_arg, vec![value])
        .into_iter()
        .next()
        .expect("one split result per amount");

    let wallet = NewWallet {
        package_id: args
            .package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        version_id,
        agent: match &args.agent {
            Some(a) => a.parse().map_err(|_| format!("{a} is not an address"))?,
            None => sender,
        },
        expires_at_ms: now_ms + args.expires_in_days * 86_400_000,
        coin_type: "0x2::sui::SUI".into(),
        manifest: args.manifest.clone(),
    };

    build_create_wallet(&mut tx, &wallet, funds, &shared, now_ms).map_err(|e| e.to_string())?;

    let built = tx.try_build().map_err(|e| format!("compiling: {e}"))?;
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&built).map_err(|e| e.to_string())?)
    };

    let mut report = json!({
        "sender": sender.to_string(),
        "agent": wallet.agent.to_string(),
        "funding": format!("{} SUI ({amount_mist} mist)", args.amount),
        "fundingBaseUnits": amount_mist.to_string(),
        "rules": describe(&args.manifest),
        "expiresAtMs": wallet.expires_at_ms.to_string(),
        "submitted": false,
    });

    // The gate. Nothing below runs unless the chain has already agreed this would execute.
    let outcome = chain.simulate(&b64).await.map_err(no_verdict)?;
    if !outcome.ok {
        return Err(would_fail(outcome.error));
    }
    report["gasUsed"] = json!(outcome.gas_used_mist);

    if args.dry_run {
        report["note"] = json!(format!(
            "Simulated only. Nothing was signed and nothing was submitted. {NO_RULES_YET}"
        ));
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
    report["created"] = json!(outcome
        .created
        .iter()
        .map(|object| json!({
            "objectId": object.object_id,
            "objectType": object.object_type,
            "ownership": match object.shared_initial_version {
                Some(v) => format!("shared at version {v}"),
                None => match &object.owner {
                    Some(owner) => format!("owned by {owner}"),
                    None => "owned".to_string(),
                },
            },
        }))
        .collect::<Vec<_>>());

    // The two ids every later step needs, named rather than left to be picked out of the list.
    let found = |predicate: fn(&str) -> bool| {
        outcome
            .created
            .iter()
            .find(|o| o.object_type.as_deref().is_some_and(predicate))
            .map(|o| o.object_id.clone())
    };
    let wallet_id = found(|t| t.contains("AgentWallet"));
    report["wallet"] = json!(wallet_id);
    report["cap"] = json!(found(|t| t.ends_with("::AgentCap")));

    // The next step reads this wallet, and certified is not the same as visible: see
    // [`rill_chain::settle`], where the live failure that put this here is recorded. Waited for
    // inside the same runtime and on the same client as everything above.
    let readable = match &wallet_id {
        Some(id) => rill_chain::settle::wait_until_readable(chain, id).await,
        None => false,
    };
    report["visibleOnNode"] = json!(readable);

    report["note"] = json!(format!(
        "Submitted and confirmed. This cannot be undone, and calling again mints a second wallet \
         and funds it again. {NO_RULES_YET}{}",
        if readable {
            ""
        } else {
            " The node that answered has not indexed the new wallet yet, so the next call may \
             answer that it does not exist; the transaction landed regardless, and its digest is \
             above. Read the wallet again before deciding anything."
        }
    ));
    Ok(report)
}

/// The manifest in words, one entry per rule, for whoever has to check what was asked for.
fn describe(manifest: &CapabilityManifest) -> Vec<String> {
    manifest
        .rules
        .iter()
        .map(|r| match r {
            CapabilityRule::Budget { total_mist } => format!("budget {total_mist}"),
            CapabilityRule::PerTx { max_mist } => format!("per-tx {max_mist}"),
            CapabilityRule::RateLimit {
                window_ms,
                max_mist,
            } => format!("rate-limit {max_mist}/{window_ms}ms"),
            CapabilityRule::TimeWindow {
                not_before_ms,
                not_after_ms,
            } => format!("window {not_before_ms}..{not_after_ms}"),
            other => format!("{:?}", other.kind()),
        })
        .collect()
}
