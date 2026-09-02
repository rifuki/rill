//! `rill spend` — the gated spend, which is the whole point of the wallet.
//!
//! `request_spend` mints a hot potato that cannot be dropped. Every attached rule's `prove` must
//! stamp a receipt on it, and `confirm_spend` counts them against the wallet's policy before
//! releasing a single coin. Miss one and the transaction aborts; the funds do not move.
//!
//! # The released coin must be consumed
//!
//! `confirm_spend` returns a `Coin<T>`, and a coin left unconsumed aborts execution with
//! `UnusedValueWithoutDrop`. So this transfers it — to a recipient if one is named, back to the
//! sender otherwise. There is no path here that builds a spend and forgets the coin.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_gated_spend_for_modules, WalletBinding};
use rill_ptb::transfer::transfer_coin;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;

const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

pub struct SpendArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    /// Decimal SUI to release from the wallet.
    pub amount: String,
    /// Where the released coin goes. The sender when absent.
    pub recipient: Option<String>,
    pub gas_budget: u64,
    pub dry_run: bool,
}

/// The printing command. Everything it knows comes from [`spend_json`]; it only renders.
pub async fn spend(endpoint: &str, keystore: &Keystore, args: &SpendArgs) -> Result<(), String> {
    let result = spend_json(endpoint, keystore, args).await?;
    for (label, key) in [
        ("wallet   ", "wallet"),
        ("amount   ", "amount"),
        ("recipient", "recipient"),
        ("rules    ", "rules"),
    ] {
        if let Some(value) = result.get(key) {
            println!("{label}: {}", render(value));
        }
    }
    if let Some(sequence) = result.get("callSequence").and_then(|s| s.as_array()) {
        println!("call sequence:");
        for target in sequence {
            println!("  {}", render(target));
        }
    }
    println!(
        "\nsimulation: ok=true gas={}",
        render(result.get("gasUsed").unwrap_or(&serde_json::Value::Null))
    );
    if args.dry_run {
        println!("\ndry run — nothing signed. Re-run with --submit.");
        return Ok(());
    }
    println!(
        "\ndigest  : {}",
        render(result.get("digest").unwrap_or(&serde_json::Value::Null))
    );
    println!("success : true");
    Ok(())
}

fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(render).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// Build, simulate, sign, submit — and return what happened as structured data.
///
/// Returns `Err` for a refusal as well as a failure, with the rule named when the contract is what
/// refused. A caller that renders this for an agent must not present a refusal as an error to
/// retry: the amount has to change, or nothing will.
pub async fn spend_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &SpendArgs,
) -> Result<serde_json::Value, String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    let sender = keystore.address();

    let wallet_id: Address = args
        .wallet_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.wallet_id))?;
    let version_id: Address = args
        .version_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.version_id))?;

    let mut shared = SharedObjects::new();
    for (label, id, raw) in [
        ("wallet", wallet_id, args.wallet_id.as_str()),
        ("version", version_id, args.version_id.as_str()),
    ] {
        let summary = chain
            .get_object(raw)
            .await
            .map_err(|e| format!("reading the {label} object: {e}"))?;
        shared.insert(
            id,
            summary
                .shared_initial_version
                .ok_or_else(|| format!("the {label} object {raw} is not shared"))?,
        );
    }

    // The capability is owned, so it enters by reference — id, version, digest, all current.
    let cap = chain
        .get_object(&args.cap_id)
        .await
        .map_err(|e| format!("reading the AgentCap: {e}"))?;

    let owned = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(|e| format!("listing the sender's objects: {e}"))?;
    let gas: Vec<_> = owned
        .iter()
        .filter(|o| o.object_type.as_deref() == Some(SUI_COIN_TYPE))
        .collect();
    if gas.is_empty() {
        return Err(format!("{sender} holds no SUI to pay for this"));
    }

    // The prove list must name exactly the rules this wallet carries — not what a flag says. Too
    // many aborts inside df::borrow_mut with no code of its own; too few aborts at the last
    // command, after everything else has passed.
    let read_tx = policy_rules_transaction(
        args.package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        wallet_id,
        "0x2::sui::SUI",
        &shared,
    )
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
    let modules: Vec<String> = attached_modules(&names)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if modules.len() != names.len() {
        return Err(format!(
            "this wallet carries {} rule(s), and only {} of them have an emitter here: {names:?}\n\
             Every attached rule must be proved, so this spend cannot be built.",
            names.len(),
            modules.len()
        ));
    }

    let amount_mist = rill_core::amounts::decimal_to_base_units(&args.amount, 9)
        .map_err(|e| format!("the amount: {e}"))?;

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

    let binding = WalletBinding {
        package_id: args
            .package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        wallet_id,
        cap: ObjectInput::owned(
            cap.reference.id.parse().expect("an id from the chain"),
            cap.reference.version,
            cap.reference.digest.parse::<Digest>().expect("a digest"),
        ),
        version_id,
        coin_type: "0x2::sui::SUI".into(),
        // Only used for the targets projection, which this path no longer takes — the rules come
        // from the chain above.
        manifest: rill_core::manifest::CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };

    let module_refs: Vec<&str> = modules.iter().map(String::as_str).collect();
    let coin = build_gated_spend_for_modules(&mut tx, &binding, amount_mist, &module_refs, &shared)
        .map_err(|e| e.to_string())?;

    let recipient = match &args.recipient {
        Some(r) => r.parse().map_err(|_| format!("{r} is not an address"))?,
        None => sender,
    };
    // The released coin, consumed. See the module note.
    transfer_coin(&mut tx, coin, recipient);

    let call_sequence: Vec<String> = std::iter::once(format!(
        "{}::agent_wallet::request_spend",
        binding.package_id
    ))
    .chain(
        modules
            .iter()
            .map(|m| format!("{}::{m}::prove", binding.package_id)),
    )
    .chain(std::iter::once(format!(
        "{}::agent_wallet::confirm_spend",
        binding.package_id
    )))
    .collect();

    let built = tx.try_build().map_err(|e| format!("compiling: {e}"))?;
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&built).map_err(|e| e.to_string())?)
    };

    let outcome = chain
        .simulate(&b64)
        .await
        .map_err(|e| format!("the node did not answer, so there is no verdict: {e}"))?;

    if !outcome.ok {
        let error = outcome.error.unwrap_or_else(|| "no reason given".into());
        // A rule refusing is the wallet working. Saying so is the difference between a caller who
        // trusts the limits and one who thinks the tool is broken — and between an agent that
        // changes the amount and one that retries the same call forever.
        return Err(match rill_chain::aborts::classify_rule_abort(&error) {
            Some(refusal) => format!(
                "{refusal}.\n\nThe limit is on chain, not in this client — raising it here \
                 changes nothing, and neither will retrying with the same amount. Spend less, or \
                 have the wallet's owner attach different rules."
            ),
            None => format!("the chain refused it: {error}"),
        });
    }

    let mut result = serde_json::json!({
        "wallet": wallet_id.to_string(),
        "amount": format!("{} SUI ({amount_mist} mist)", args.amount),
        "recipient": recipient.to_string(),
        "rules": modules,
        "rulesSource": "read from chain",
        "callSequence": call_sequence,
        "gasUsed": outcome.gas_used_mist,
        "submitted": false,
    });

    if args.dry_run {
        result["note"] = serde_json::Value::String(
            "Simulated only. Nothing was signed and nothing was submitted.".into(),
        );
        return Ok(result);
    }

    let signature = keystore.sign(&built).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(|e| format!("submitting: {e}"))?;

    if let Some(error) = &outcome.error {
        return Err(format!("the transaction failed on chain: {error}"));
    }

    result["submitted"] = serde_json::Value::Bool(true);
    result["digest"] = serde_json::Value::String(outcome.digest.clone());
    result["gasUsed"] = serde_json::json!(outcome.gas_used_mist);
    result["balanceChanges"] = serde_json::json!(outcome
        .balance_changes
        .iter()
        .map(|d| serde_json::json!({ "amount": d.amount, "coinType": d.coin_type }))
        .collect::<Vec<_>>());
    result["note"] = serde_json::Value::String(
        "Submitted and confirmed. This cannot be undone, and calling again sends a second payment."
            .into(),
    );
    Ok(result)
}
