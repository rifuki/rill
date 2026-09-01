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
use rill_core::manifest::CapabilityManifest;
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_manifest_gated_spend, expected_spend_targets, WalletBinding};
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
    pub manifest: CapabilityManifest,
    pub gas_budget: u64,
    pub dry_run: bool,
}

pub async fn spend(endpoint: &str, keystore: &Keystore, args: &SpendArgs) -> Result<(), String> {
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

    let amount_mist = rill_core::amounts::decimal_to_base_units(&args.amount, 9)
        .map_err(|e| format!("the amount: {e}"))?;

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(args.gas_budget);
    tx.set_gas_price(1_000);
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
        manifest: args.manifest.clone(),
    };

    let coin = build_manifest_gated_spend(&mut tx, &binding, amount_mist, &shared)
        .map_err(|e| e.to_string())?;

    let recipient = match &args.recipient {
        Some(r) => r.parse().map_err(|_| format!("{r} is not an address"))?,
        None => sender,
    };
    // The released coin, consumed. See the module note.
    transfer_coin(&mut tx, coin, recipient);

    println!("wallet   : {wallet_id}");
    println!("amount   : {} SUI ({amount_mist} mist)", args.amount);
    println!("recipient: {recipient}");
    println!("call sequence:");
    for target in expected_spend_targets(&binding).map_err(|e| e.to_string())? {
        println!("  {target}");
    }

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
        let error = outcome.error.unwrap_or_else(|| "no reason given".into());
        // A rule refusing is the wallet working. Saying so is the difference between a user who
        // trusts the limits and one who thinks the tool is broken.
        return Err(match rill_chain::aborts::classify_rule_abort(&error) {
            Some(refusal) => format!(
                "{refusal}.\n\nThe limit is on chain, not in this client — raising it here \
                 changes nothing. Attach different rules, or spend less.",
            ),
            None => format!("the chain refused it: {error}"),
        });
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
    for delta in &outcome.balance_changes {
        println!("balance : {} {}", delta.amount, delta.coin_type);
    }
    Ok(())
}
