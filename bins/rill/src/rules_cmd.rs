//! `rill wallet rules` — the second transaction, which is what makes a capability mean anything.
//!
//! A wallet created and left alone has an empty policy, and `confirm_spend` on an empty policy
//! requires zero receipts. So this step is not configuration; it is the difference between a
//! capability that is bounded and one that is not.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_core::manifest::CapabilityManifest;
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::rules::{build_reconcile_rules, RuleTarget};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;

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

pub async fn attach(endpoint: &str, keystore: &Keystore, args: &RulesArgs) -> Result<(), String> {
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
        return Err(format!("{sender} holds no SUI to pay for this"));
    }

    // What the wallet actually carries. Attaching is not idempotent — add_rule aborts
    // E_RULE_ALREADY_SET — so this must be a reconciliation against the live set, not an attach.
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
    let attached: Vec<String> = attached_modules(&names)
        .into_iter()
        .map(str::to_owned)
        .collect();

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

    let target = RuleTarget {
        package_id: args
            .package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        wallet_id,
        version_id,
        coin_type: "0x2::sui::SUI".into(),
        manifest: args.manifest.clone(),
    };

    let module_refs: Vec<&str> = attached.iter().map(String::as_str).collect();
    let result = build_reconcile_rules(&mut tx, &target, &module_refs, &shared)
        .map_err(|e| e.to_string())?;

    println!("wallet  : {wallet_id}");
    println!("owner   : {sender}");
    println!(
        "attached: {}",
        if attached.is_empty() {
            "none".to_string()
        } else {
            attached.join(", ")
        }
    );
    if !result.removed.is_empty() {
        println!("re-set  : {}", result.removed.join(", "));
    }
    if !result.orphaned.is_empty() {
        println!("dropping: {}", result.orphaned.join(", "));
    }
    println!("result  : {}", result.added.join(", "));

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
        println!("\ndry run — nothing signed. Re-run with --submit.");
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
    println!(
        "\nThe wallet is now bounded by {} rule(s). Every spend must satisfy all of them.",
        result.added.len()
    );
    Ok(())
}
