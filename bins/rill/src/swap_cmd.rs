//! A swap the wallet paid for, under the wallet's own rules.
//!
//! A swap from the signer's own coins would be an ordinary swap with extra steps: the thing that
//! makes this one worth having is that the SUI going in was released by `agent_wallet` against the
//! rules attached to it, so the agent cannot swap more than the owner allowed and the refusal comes
//! from the chain rather than from this process.
//!
//! So the shape is the hero path with Cetus where DeepBook was: `request_spend`, every rule's
//! `prove`, `confirm_spend`, then `router::swap`, then both returned coins placed. The gated prefix
//! is the same builder `rill order` uses, not a second copy of it.
//!
//! # Both coins, always
//!
//! `router::swap` returns two, and Cetus's use-full-input flag is false so the funded side comes
//! back holding whatever the swap did not spend. `Coin` has no `drop`: leaving either one aborts the
//! whole transaction. See [`rill_ptb::cetus::SwapOutput`].

use rill_chain::grpc::GrpcSui;
use rill_chain::{SuiRead, SuiWrite};
use rill_ptb::cetus::{swap, Swap, MAX_SQRT_PRICE};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_gated_spend_for_modules, WalletBinding};
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;
use crate::verdict::Failure;

/// One gated swap. Amounts are decimal strings for the same reason every amount here is.
#[derive(Debug, Clone)]
pub struct SwapArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    /// Cetus, on the network this signer is pointed at.
    pub integrate_package_id: String,
    pub global_config_id: String,
    pub pool_id: String,
    pub coin_type_a: String,
    pub coin_type_b: String,
    /// True to spend coin A and buy B. The wallet releases SUI, so this is whichever side SUI is not.
    pub a2b: bool,
    /// The SUI to release from the wallet and swap, in decimal SUI.
    pub spend: String,
    /// The least the bought coin may hold for this swap to be allowed to land, in that coin's base
    /// units.
    ///
    /// Base units rather than a decimal string, unlike `spend`: the bought coin's decimals are a
    /// property of a token this command never reads, and a decimal floor would have to guess them.
    /// Guessing nine where the token uses six states a floor a thousand times too low and reads as
    /// protection.
    pub min_out_base_units: String,
    /// The deployed `rill_guard` package that carries `assert_min_value`.
    pub guard_package_id: String,
    /// Send the swap with no floor at all, accepting whatever the pool returns.
    ///
    /// Exists so that an unprotected swap is a thing a caller said rather than a thing that happens
    /// when a field is left out. The report names it, so the absence of protection is visible after
    /// the fact and not only before it.
    pub accept_any_output: bool,
    pub gas_budget: u64,
    pub dry_run: bool,
}

/// The call sequence a gated swap emits, for the signer's pinned run-set.
///
/// Built from the rule modules the chain reported rather than from a manifest, because the wallet's
/// live policy is what `confirm_spend` counts receipts against: a manifest would be a second source
/// for the same fact, and the one that can be stale. `expected_spend_targets` takes the other route
/// and is right to, since a caller holding a manifest is declaring what it intends rather than
/// reading what is there.
pub fn expected_targets(
    package_id: Address,
    rule_modules: &[String],
    integrate_package_id: Address,
    guard: Option<Address>,
) -> Vec<String> {
    let mut targets = vec![format!("{package_id}::agent_wallet::request_spend")];
    for module in rule_modules {
        targets.push(format!("{package_id}::{module}::prove"));
    }
    targets.push(format!("{package_id}::agent_wallet::confirm_spend"));
    targets.extend(rill_ptb::cetus::expected_swap_targets(integrate_package_id));
    // Last, because the floor reads the coin the swap produced. `transfer_objects` follows it and is
    // not a Move call, so nothing comes after this in the target list.
    if let Some(guard) = guard {
        targets.push(rill_ptb::guard::guard_target(guard));
    }
    targets
}

/// Build, gate, sign and submit, against any chain.
///
/// Takes the chain so the whole path runs offline against the fake, and the client is created by the
/// caller and used here: a tonic channel built in one runtime and used in another is already closed.
pub async fn swap_json_on(
    chain: &(impl SuiRead + SuiWrite),
    keystore: &Keystore,
    args: &SwapArgs,
) -> Result<Value, Failure> {
    let sender = keystore.address();
    let wallet_id: Address = args
        .wallet_id
        .parse()
        .map_err(|_| "the wallet id is not an address".to_string())?;
    let version_id: Address = args
        .version_id
        .parse()
        .map_err(|_| "the version object id is not an address".to_string())?;
    let integrate: Address = args
        .integrate_package_id
        .parse()
        .map_err(|_| "the integrate package id is not an address".to_string())?;
    let global_config: Address = args
        .global_config_id
        .parse()
        .map_err(|_| "the global config id is not an address".to_string())?;
    let pool: Address = args
        .pool_id
        .parse()
        .map_err(|_| "the pool id is not an address".to_string())?;

    // Which side the wallet's SUI funds, and therefore which bound leaves the price open. Derived
    // rather than taken from the caller: a bound on the wrong side aborts inside Cetus with a code
    // that names neither the value nor the field.
    let sqrt_price_limit = if args.a2b { 0 } else { MAX_SQRT_PRICE };

    let spend_mist = rill_core::amounts::decimal_to_base_units(&args.spend, 9)
        .map_err(|e| format!("the spend amount: {e}"))?;

    // The floor, resolved before any object is read, so a swap that was never going to be allowed
    // costs no round trips. Zero is not a floor: the guard would emit nothing and the transaction
    // would carry no bound, which is the one outcome a caller must not reach by omission.
    let min_out = rill_core::amounts::parse_u64_string(&args.min_out_base_units)
        .map_err(|e| format!("the minimum output: {e}"))?;
    if min_out == 0 && !args.accept_any_output {
        // `Failed`, not `Refused`: `Refused` means a rule on the wallet stopped this, and an agent
        // that cannot tell a missing argument from a policy decision retries the policy decision.
        return Err(Failure::Failed(
            "minOut is zero, so this swap would accept any output including none. The wallet's \
             rules bound what goes into the swap and nothing bounds what comes back, so a thin pool \
             or a sandwich returns dust and the transaction still succeeds. Set minOut to the least \
             the bought coin may hold, in that coin's base units, or pass acceptAnyOutput to send it \
             unprotected on purpose."
                .to_string(),
        ));
    }
    let guard_package: Option<Address> = if min_out == 0 {
        None
    } else {
        Some(
            args.guard_package_id
                .parse()
                .map_err(|_| "the guard package id is not an address".to_string())?,
        )
    };

    // Every shared object this touches, at the initial version the node reports. Read, never assumed.
    let mut shared = SharedObjects::new();
    for id in [
        wallet_id.to_string(),
        version_id.to_string(),
        global_config.to_string(),
        pool.to_string(),
        rill_ptb::spend::CLOCK_ID.to_string(),
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

    let modules = rill_ptb::policy_read::attached_modules(
        &rill_ptb::policy_read::parse_type_names(
            chain
                .simulate_read(&encode(
                    &rill_ptb::policy_read::policy_rules_transaction(
                        args.package_id
                            .parse()
                            .map_err(|_| "the package id is not an address".to_string())?,
                        wallet_id,
                        "0x2::sui::SUI",
                        &shared,
                        chain
                            .reference_gas_price()
                            .await
                            .map_err(|e| format!("reading the reference gas price: {e}"))?,
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

    let gas_price = chain
        .reference_gas_price()
        .await
        .map_err(|e| format!("reading the reference gas price: {e}"))?;
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
        package_id: args
            .package_id
            .parse()
            .map_err(|_| "the package id is not an address".to_string())?,
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

    let out = swap(
        &mut tx,
        &Swap {
            integrate_package_id: integrate,
            global_config_id: global_config,
            pool_id: pool,
            coin_type_a: args.coin_type_a.clone(),
            coin_type_b: args.coin_type_b.clone(),
            a2b: args.a2b,
            by_amount_in: true,
            amount: spend_mist,
            sqrt_price_limit,
        },
        funded,
        &shared,
    )
    .map_err(|e| e.to_string())?;

    // The floor, on the coin the swap bought, before it goes anywhere. The wallet's rules bound what
    // entered the swap; nothing in them bounds what came back out, and `sqrt_price_limit` cannot:
    // the only value that does not abort inside Cetus on the funded side is the extreme one. So this
    // is the bound, and it is a Move call the node enforces rather than a check in this process.
    let bought_type = if args.a2b {
        &args.coin_type_b
    } else {
        &args.coin_type_a
    };
    let floor = rill_ptb::guard::assert_min_value(
        &mut tx,
        guard_package,
        out.output(),
        bought_type,
        min_out,
    )
    .map_err(|e| e.to_string())?;

    // Both, to the agent. The output is what it bought; the residual is what the swap did not spend,
    // and leaving either aborts the transaction.
    let to = tx.pure(&sender);
    tx.transfer_objects(out.both().to_vec(), to);

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
        "spendBaseUnits": spend_mist.to_string(),
        "pool": args.pool_id,
        "a2b": args.a2b,
        "rulesProved": modules,
        "minOutBaseUnits": min_out.to_string(),
        // From the builder's own return rather than from `min_out`, so the report cannot say
        // "enforced" about a call that was not emitted.
        "slippageFloor": match floor {
            rill_ptb::guard::GuardOutcome::Enforced => "enforced",
            rill_ptb::guard::GuardOutcome::NotRequested => "none",
        },
        "callSequence": expected_targets(
            args.package_id
                .parse()
                .map_err(|_| "the package id is not an address".to_string())?,
            &modules,
            integrate,
            match floor {
                rill_ptb::guard::GuardOutcome::Enforced => guard_package,
                rill_ptb::guard::GuardOutcome::NotRequested => None,
            },
        ),
        "simulation": { "ok": true, "gasEstimate": simulated.gas_used_mist },
    });

    if args.dry_run {
        let mut report = report;
        report["submitted"] = json!(false);
        report["note"] = json!(
            "Nothing was signed. The gate passed, which is what a dry run can tell you; add \
             --submit to send it."
        );
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
    Ok(report)
}

/// The same swap against the live network.
pub async fn swap_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &SwapArgs,
) -> Result<Value, Failure> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    swap_json_on(&chain, keystore, args).await
}

fn encode(tx: &sui_sdk_types::Transaction) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .encode(bcs::to_bytes(tx).expect("a built transaction encodes"))
}

/// One owned object, read at the version the node holds.
///
/// Takes the chain generically rather than `GrpcSui`, so this path can be driven against the fake.
async fn owned_input(chain: &impl SuiRead, id: &str, label: &str) -> Result<ObjectInput, String> {
    let summary = chain
        .get_object(id)
        .await
        .map_err(|e| format!("reading the {label} {id}: {e}"))?;
    Ok(ObjectInput::owned(
        summary
            .reference
            .id
            .parse()
            .map_err(|_| format!("{id} is not an object id"))?,
        summary.reference.version,
        summary
            .reference
            .digest
            .parse::<Digest>()
            .map_err(|_| format!("the {label} digest did not decode"))?,
    ))
}
