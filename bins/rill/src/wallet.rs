//! `rill wallet create` — the whole first transaction, end to end.
//!
//! Build, strictly simulate, sign, submit, and read back the ids the next step needs. Every stage
//! is a separate refusal: a simulation that fails never reaches the signing code, and a signature
//! is never produced for bytes the chain has not already agreed would execute.
//!
//! # Why the ids are printed rather than remembered
//!
//! `create_wallet` shares a wallet and mints a capability, and neither id exists until the
//! transaction lands. Everything after this step needs both. They come out of the effects and are
//! printed, because the alternative — writing them into a state file the user did not ask for — is
//! a second source of truth about what exists on chain.

use rill_chain::{grpc::GrpcSui, ChainError, SuiRead, SuiWrite};
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::create::{build_create_wallet, NewWallet};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;

/// Fully-expanded SUI, the way the chain writes it in an object type.
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

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

pub async fn create(
    endpoint: &str,
    keystore: &Keystore,
    args: &CreateArgs,
    now_ms: u64,
) -> Result<(), String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
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
        return Err(format!(
            "{sender} holds no SUI, so it cannot pay for anything"
        ));
    }

    let amount_mist = rill_core::amounts::decimal_to_base_units(&args.amount, 9)
        .map_err(|e| format!("the funding amount: {e}"))?;

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(args.gas_budget);
    // Read, not assumed. Testnet answers 1000 and mainnet answers 100, so a literal that is right
    // on one network is ten times the price on the other — and a price below the reference is
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

    println!("sender  : {sender}");
    println!("agent   : {}", wallet.agent);
    println!("funding : {} SUI ({amount_mist} mist)", args.amount);
    println!("rules   : {}", describe(&args.manifest));

    // The gate. Nothing below runs unless the chain has already agreed this would execute.
    let outcome = chain
        .simulate(&b64)
        .await
        .map_err(|e| format!("the node did not answer, so there is no verdict: {e}"))?;
    println!(
        "\nsimulation: ok={} verification={:?} gas={}",
        outcome.ok, outcome.verification, outcome.gas_used_mist
    );
    if !outcome.ok {
        return Err(format!(
            "the chain says this would fail: {}",
            outcome.error.unwrap_or_else(|| "no reason given".into())
        ));
    }

    if args.dry_run {
        println!("\ndry run — nothing signed, nothing submitted.");
        println!("re-run with --submit to sign and send it.");
        return Ok(());
    }

    let signature = keystore.sign(&built).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(|e| format!("submitting: {e}"))?;

    println!("\ndigest  : {}", outcome.digest);
    println!("success : {}", outcome.success);
    if let Some(error) = &outcome.error {
        return Err(format!("the transaction failed on chain: {error}"));
    }
    println!("gas used: {}", outcome.gas_used_mist);

    println!("\ncreated:");
    for object in &outcome.created {
        let kind = match object.shared_initial_version {
            Some(v) => format!("shared at version {v}"),
            None => match &object.owner {
                Some(owner) => format!("owned by {owner}"),
                None => "owned".into(),
            },
        };
        println!(
            "  {}  {}\n      {kind}",
            object.object_id,
            object
                .object_type
                .as_deref()
                .unwrap_or("(type not reported)")
        );
    }

    // The two ids every later step needs, named rather than left to be picked out of the list.
    let wallet_id = outcome.created.iter().find(|o| {
        o.object_type
            .as_deref()
            .is_some_and(|t| t.contains("AgentWallet"))
    });
    let cap_id = outcome.created.iter().find(|o| {
        o.object_type
            .as_deref()
            .is_some_and(|t| t.ends_with("::AgentCap"))
    });

    println!("\nnext step needs:");
    match wallet_id {
        Some(w) => println!("  wallet : {}", w.object_id),
        None => println!("  wallet : not found in the effects — check the type filter"),
    }
    match cap_id {
        Some(c) => println!("  cap    : {}", c.object_id),
        None => println!("  cap    : not found in the effects"),
    }
    println!(
        "\nThe wallet has NO rules attached yet, and confirm_spend on an empty policy requires\n\
         zero receipts. Run `rill wallet rules` before this capability is worth anything."
    );

    Ok(())
}

fn describe(manifest: &CapabilityManifest) -> String {
    if manifest.rules.is_empty() {
        return "none".into();
    }
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
        .collect::<Vec<_>>()
        .join(", ")
}
