//! `rill wallet revoke` — the owner's kill switch.
//!
//! Marks the wallet revoked and returns its whole remaining balance as a coin. Every later
//! `request_spend` aborts `E_REVOKED`, whatever capability the agent still holds.
//!
//! The returned coin must be consumed or the transaction aborts with `UnusedValueWithoutDrop` —
//! and `try_build` does not catch that, so a revoke that forgot it would compile, simulate, and
//! recover nothing. It is transferred here at the call site rather than trusted to a later step.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_ptb::lifecycle::build_revoke;
use rill_ptb::shared::SharedObjects;
use rill_ptb::transfer::transfer_coin;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;

const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

pub struct RevokeArgs {
    pub package_id: String,
    pub wallet_id: String,
    /// Where the recovered funds go. The signer when absent.
    pub recipient: Option<String>,
    pub gas_budget: u64,
    pub dry_run: bool,
}

pub async fn revoke(endpoint: &str, keystore: &Keystore, args: &RevokeArgs) -> Result<(), String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    let sender = keystore.address();
    let wallet_id: Address = args
        .wallet_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.wallet_id))?;

    let summary = chain
        .get_object(&args.wallet_id)
        .await
        .map_err(|e| format!("reading the wallet: {e}"))?;
    let mut shared = SharedObjects::new();
    shared.insert(
        wallet_id,
        summary
            .shared_initial_version
            .ok_or("that object is not a shared AgentWallet")?,
    );

    let gas: Vec<_> = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(|e| format!("listing the sender's objects: {e}"))?
        .into_iter()
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

    let coin = build_revoke(
        &mut tx,
        args.package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        wallet_id,
        "0x2::sui::SUI",
        &shared,
    )
    .map_err(|e| e.to_string())?;

    let recipient: Address = match &args.recipient {
        Some(r) => r.parse().map_err(|_| format!("{r} is not an address"))?,
        None => sender,
    };
    transfer_coin(&mut tx, coin, recipient);

    println!("wallet   : {wallet_id}");
    println!("owner    : {sender}");
    println!("recovered to: {recipient}");

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
        "\nsimulation: ok={} gas={}",
        outcome.ok, outcome.gas_used_mist
    );
    if !outcome.ok {
        let error = outcome.error.unwrap_or_else(|| "no reason given".into());
        return Err(match rill_chain::aborts::classify_rule_abort(&error) {
            Some(refusal) => refusal.to_string(),
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
    if let Some(error) = &outcome.error {
        return Err(format!("the transaction failed on chain: {error}"));
    }
    println!("\ndigest: {}", outcome.digest);
    for d in &outcome.balance_changes {
        println!("balance: {} {}", d.amount, d.coin_type);
    }
    println!(
        "\nThe wallet is revoked. Every later request_spend aborts E_REVOKED, whatever capability \
         the agent still holds — the cap was not taken back, it simply stopped meaning anything."
    );
    Ok(())
}
