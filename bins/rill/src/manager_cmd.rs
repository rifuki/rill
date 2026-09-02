//! `rill deepbook provision` — create a BalanceManager and delegate it.
//!
//! One transaction: create the manager, mint both capabilities, hand them to the agent, share the
//! manager. The manager's owner is whoever signs this; the agent gets only the capabilities.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_ptb::balance_manager::build_provision_manager;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;

const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

pub struct ProvisionArgs {
    pub deepbook_package: String,
    /// Who receives the capabilities. The signer when absent.
    pub agent: Option<String>,
    pub gas_budget: u64,
    pub dry_run: bool,
}

pub async fn provision(
    endpoint: &str,
    keystore: &Keystore,
    args: &ProvisionArgs,
) -> Result<(), String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    let sender = keystore.address();
    let agent: Address = match &args.agent {
        Some(a) => a.parse().map_err(|_| format!("{a} is not an address"))?,
        None => sender,
    };

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

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(args.gas_budget);
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

    build_provision_manager(
        &mut tx,
        args.deepbook_package
            .parse()
            .map_err(|_| format!("{} is not an address", args.deepbook_package))?,
        agent,
    )
    .map_err(|e| e.to_string())?;

    println!("owner : {sender}");
    println!("agent : {agent}");
    println!("caps  : TradeCap + DepositCap, minted to the agent");

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
    if let Some(error) = &outcome.error {
        return Err(format!("the transaction failed on chain: {error}"));
    }
    println!("\ndigest: {}", outcome.digest);

    println!("\ncreated:");
    for object in &outcome.created {
        let kind = match object.shared_initial_version {
            Some(v) => format!("shared at version {v}"),
            None => "owned".to_string(),
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

    for (label, needle) in [
        ("manager", "::balance_manager::BalanceManager"),
        ("tradeCap", "::balance_manager::TradeCap"),
        ("depositCap", "::balance_manager::DepositCap"),
    ] {
        let found = outcome.created.iter().find(|o| {
            o.object_type
                .as_deref()
                .is_some_and(|t| t.ends_with(needle))
        });
        match found {
            Some(o) => println!("\n{label:11}: {}", o.object_id),
            None => println!("\n{label:11}: not found in the effects"),
        }
    }
    Ok(())
}
