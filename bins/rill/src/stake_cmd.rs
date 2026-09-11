//! A liquid stake the wallet paid for, under the wallet's own rules.
//!
//! `request_spend`, every rule's `prove`, `confirm_spend`, then Haedal's `interface::request_stake`,
//! which consumes the released SUI and sends haSUI to the sender. One transaction: the SUI that
//! enters Haedal is released by `agent_wallet` against its rules, so an agent cannot stake more than
//! the owner allowed and the refusal comes from the chain.
//!
//! # What the adapter used to call
//!
//! `staking::request_stake`, which the deployed testnet package does not have. Nothing had ever
//! submitted through the adapter, so its one test checked only that the sequence had one entry and
//! passed. Reading the package first is what found it; `interface::request_stake` is the function
//! with the documented behaviour. See `rill-chain/tests/haedal_signature.rs`.
//!
//! # No floor, and why that is not the swap's problem again
//!
//! A swap's output depends on a pool that can move, which is why `rill_swap` requires `minOut`. A
//! Haedal stake mints haSUI at Haedal's own exchange rate, which a sandwich cannot move within a
//! transaction, and the function returns nothing to assert against anyway: the haSUI is transferred
//! inside the call. What is reported instead is what the node says arrived.

use rill_chain::grpc::GrpcSui;
use rill_chain::{SuiRead, SuiWrite};
use rill_ptb::haedal::{expected_stake_targets, request_stake, Stake, MIN_STAKE_MIST};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_gated_spend_for_modules, WalletBinding};
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;
use crate::swap_cmd::{encode, owned_input};
use crate::verdict::Failure;

/// One gated stake.
#[derive(Debug, Clone)]
pub struct StakeArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    pub haedal_package_id: String,
    pub staking_object_id: String,
    /// The validator to delegate to, or `0x0` for none, which lets Haedal choose.
    pub validator: String,
    /// The SUI to release from the wallet and stake, in decimal SUI. At least 1.
    pub spend: String,
    pub gas_budget: u64,
    pub dry_run: bool,
}

/// The call sequence a gated stake emits, for the signer's pinned run-set.
pub fn expected_targets(
    package_id: Address,
    rule_modules: &[String],
    haedal_package_id: Address,
) -> Vec<String> {
    let mut targets = vec![format!("{package_id}::agent_wallet::request_spend")];
    for module in rule_modules {
        targets.push(format!("{package_id}::{module}::prove"));
    }
    targets.push(format!("{package_id}::agent_wallet::confirm_spend"));
    targets.extend(expected_stake_targets(haedal_package_id));
    targets
}

pub async fn stake_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &StakeArgs,
) -> Result<Value, Failure> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    stake_json_on(&chain, keystore, args).await
}

pub async fn stake_json_on(
    chain: &(impl SuiRead + SuiWrite),
    keystore: &Keystore,
    args: &StakeArgs,
) -> Result<Value, Failure> {
    let sender = keystore.address();
    let parse = |s: &str, what: &str| -> Result<Address, Failure> {
        s.parse()
            .map_err(|_| Failure::Failed(format!("the {what} is not an address")))
    };
    let wallet_id = parse(&args.wallet_id, "wallet id")?;
    let version_id = parse(&args.version_id, "version object id")?;
    let package_id = parse(&args.package_id, "package id")?;
    let haedal = parse(&args.haedal_package_id, "Haedal package id")?;
    let staking = parse(&args.staking_object_id, "Staking object id")?;
    let validator = parse(&args.validator, "validator")?;

    let spend_mist = rill_core::amounts::decimal_to_base_units(&args.spend, 9)
        .map_err(|e| format!("the stake amount: {e}"))?;
    // Refused before a single object is read. Haedal aborts below one SUI with a code that names
    // neither the amount nor the minimum, after the gated spend has already been built and paid for.
    if spend_mist < MIN_STAKE_MIST {
        return Err(Failure::Failed(format!(
            "Haedal's minimum stake is 1 SUI and this is {} SUI. It would abort inside Haedal after \
             the wallet's rules had already been checked, so it is refused here first.",
            args.spend
        )));
    }

    let mut shared = SharedObjects::new();
    for id in [
        wallet_id.to_string(),
        version_id.to_string(),
        staking.to_string(),
        rill_ptb::haedal::SUI_SYSTEM_STATE_ID.to_string(),
    ] {
        let summary = chain
            .get_object(&id)
            .await
            .map_err(|e| format!("reading {id}: {e}"))?;
        let initial = summary
            .shared_initial_version
            .ok_or_else(|| format!("{id} is not a shared object"))?;
        shared.insert(
            summary
                .reference
                .id
                .parse()
                .map_err(|_| format!("{id} is not an address"))?,
            initial,
        );
    }
    // The clock is also shared, and the gated spend reads it.
    let clock = chain
        .get_object(rill_ptb::spend::CLOCK_ID)
        .await
        .map_err(|e| format!("reading the clock: {e}"))?;
    shared.insert(
        "0x6".parse().expect("0x6 is an address"),
        clock
            .shared_initial_version
            .ok_or("the clock is not a shared object")?,
    );

    let gas_price = chain
        .reference_gas_price()
        .await
        .map_err(|e| format!("reading the reference gas price: {e}"))?;

    let modules = rill_ptb::policy_read::attached_modules(
        &rill_ptb::policy_read::parse_type_names(
            chain
                .simulate_read(&encode(
                    &rill_ptb::policy_read::policy_rules_transaction(
                        package_id,
                        wallet_id,
                        "0x2::sui::SUI",
                        &shared,
                        gas_price,
                    )
                    .map_err(|e| e.to_string())?,
                ))
                .await
                .map_err(|e| format!("reading the wallet's rules: {e}"))?
                .command_returns
                .iter()
                .flatten()
                .next()
                .ok_or("the wallet did not report its rules")?,
        )
        .map_err(|_| "the rule list did not decode".to_string())?,
    )
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<String>>();

    let owned = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(|e| format!("listing the sender's objects: {e}"))?;
    let sui_coin = "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";
    let gas: Vec<&rill_chain::ObjectSummary> = owned
        .iter()
        .filter(|o| o.object_type.as_deref() == Some(sui_coin))
        .collect();
    if gas.is_empty() {
        return Err(Failure::Failed(format!(
            "{sender} holds no SUI, so it cannot pay for anything"
        )));
    }

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

    let binding = WalletBinding {
        package_id,
        wallet_id,
        cap: owned_input(chain, &args.cap_id, "AgentCap").await?,
        version_id,
        coin_type: "0x2::sui::SUI".into(),
        manifest: rill_core::manifest::CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };
    let module_refs: Vec<&str> = modules.iter().map(String::as_str).collect();
    let funded =
        build_gated_spend_for_modules(&mut tx, &binding, spend_mist, &module_refs, &shared)
            .map_err(|e| e.to_string())?;

    request_stake(
        &mut tx,
        &Stake {
            package_id: haedal,
            staking_object_id: staking,
            validator,
            amount_mist: spend_mist,
        },
        funded,
        &shared,
    )
    .map_err(|e| e.to_string())?;

    let built = tx.try_build().map_err(|e| e.to_string())?;
    let b64 = encode(&built);

    let simulated = chain
        .simulate(&b64)
        .await
        .map_err(|e| Failure::Failed(crate::verdict::no_verdict(e)))?;
    if !simulated.ok {
        return Err(crate::verdict::would_fail(simulated.error.clone()));
    }

    let report = json!({
        "sender": sender.to_string(),
        "wallet": args.wallet_id,
        "stakeBaseUnits": spend_mist.to_string(),
        "validator": args.validator,
        "rulesProved": modules,
        "callSequence": expected_targets(package_id, &modules, haedal),
        "simulation": {
            "ok": true,
            "gasEstimate": simulated.gas_used_mist,
            "balanceChanges": simulated
                .balance_changes
                .iter()
                .map(|c| json!({ "coinType": c.coin_type, "amount": c.amount }))
                .collect::<Vec<Value>>(),
        },
    });

    if args.dry_run {
        let mut report = report;
        report["submitted"] = json!(false);
        return Ok(report);
    }

    let signature = keystore
        .sign(&built)
        .map_err(|e| Failure::Failed(e.to_string()))?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(|e| Failure::Failed(crate::verdict::submit_failed(e)))?;
    if let Some(error) = &outcome.error {
        return Err(crate::verdict::did_fail(error));
    }

    let mut report = report;
    report["submitted"] = json!(true);
    report["digest"] = json!(outcome.digest);
    report["gasUsed"] = json!(outcome.gas_used_mist);
    report["balanceChanges"] = json!(outcome
        .balance_changes
        .iter()
        .map(|c| json!({ "coinType": c.coin_type, "amount": c.amount }))
        .collect::<Vec<Value>>());
    report["note"] = json!(
        "Submitted and confirmed. The haSUI is the sender's; the wallet's SUI left under its own \
         rules. This cannot be undone, and calling again stakes again."
    );
    Ok(report)
}
