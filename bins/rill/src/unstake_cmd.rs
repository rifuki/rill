//! Immediate Haedal redemption of the configured signer's own haSUI coins.
use crate::{
    keystore::Keystore,
    swap_cmd::{encode, owned_input},
    verdict::Failure,
};
use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_ptb::{
    haedal::{request_unstake_instant, Unstake},
    shared::SharedObjects,
};
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

pub const MAINNET_HAEDAL: &str =
    "0x126e4cfb051cad744706df590ec399e8c02b6feae195c35b8b496280d5442a62";
pub const MAINNET_STAKING: &str =
    "0x47b224762220393057ebf4f70501b6e657c3e56684737568439a04f80849b2ca";
pub const MAINNET_HASUI: &str =
    "0xbde4ba4c2e274a60ce15c1cfff9e5c42e41654ac8b6d906a57efa4bd3c29f47d::hasui::HASUI";

#[derive(Debug, Clone)]
pub struct UnstakeArgs {
    pub package_id: String,
    pub staking_object_id: String,
    pub coin_type: String,
    pub guard_package_id: String,
    /// Decimal haSUI, nine decimals.
    pub amount: String,
    /// Decimal SUI minimum, after the live protocol fee.
    pub min_out: String,
    pub receiver: String,
    pub gas_budget: u64,
    pub dry_run: bool,
}

pub fn validated_amounts(args: &UnstakeArgs) -> Result<(u64, u64), Failure> {
    let actual: sui_sdk_types::TypeTag =
        args.coin_type.parse().map_err(|_| "invalid haSUI type")?;
    let expected: sui_sdk_types::TypeTag = MAINNET_HASUI
        .parse()
        .map_err(|_| "invalid pinned haSUI type")?;
    if actual != expected {
        return Err("only the pinned mainnet Haedal haSUI asset is supported".into());
    }
    let amount =
        rill_core::amounts::decimal_to_base_units(&args.amount, 9).map_err(|e| e.to_string())?;
    let min =
        rill_core::amounts::decimal_to_base_units(&args.min_out, 9).map_err(|e| e.to_string())?;
    if amount == 0 || min == 0 {
        return Err("amount and minOut must both be positive decimal strings".into());
    }
    Ok((amount, min))
}

pub async fn unstake_json(
    endpoint: &str,
    key: &Keystore,
    args: &UnstakeArgs,
) -> Result<Value, Failure> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    unstake_json_on(&chain, key, args).await
}

pub async fn unstake_json_on(
    chain: &(impl SuiRead + SuiWrite),
    key: &Keystore,
    args: &UnstakeArgs,
) -> Result<Value, Failure> {
    let (amount, min) = validated_amounts(args)?;
    let parse = |s: &str| {
        s.parse::<Address>()
            .map_err(|_| Failure::Failed(format!("invalid address: {s}")))
    };
    let package = parse(&args.package_id)?;
    let staking = parse(&args.staking_object_id)?;
    let receiver = parse(&args.receiver)?;
    let guard = parse(&args.guard_package_id)?;
    let sender = key.address();
    let mut shared = SharedObjects::new();
    for id in ["0x5".to_owned(), staking.to_string()] {
        let object = chain.get_object(&id).await.map_err(|e| e.to_string())?;
        shared.insert(
            parse(&id)?,
            object
                .shared_initial_version
                .ok_or("staking/system object is not shared")?,
        );
    }
    let listed = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(|e| e.to_string())?;
    let expected: sui_sdk_types::TypeTag =
        args.coin_type.parse().map_err(|_| "invalid coin type")?;
    let mut coins = Vec::new();
    let mut held = 0u64;
    for object in listed {
        let Some(asset) = object
            .object_type
            .as_deref()
            .and_then(crate::portfolio_cmd::coin_type)
        else {
            continue;
        };
        if asset.parse::<sui_sdk_types::TypeTag>().ok().as_ref() != Some(&expected) {
            continue;
        }
        let current = chain
            .get_object(&object.reference.id)
            .await
            .map_err(|e| e.to_string())?;
        let balance =
            rill_chain::gas::coin_balance(&current).ok_or("haSUI balance was not returned")?;
        held = held.checked_add(balance).ok_or("haSUI balance overflow")?;
        coins.push(current);
    }
    if held < amount {
        return Err(format!("signer owns {held} haSUI base units, needs {amount}").into());
    }
    let gas = rill_chain::gas::sui_gas_coins(chain, &sender.to_string())
        .await
        .map_err(|e| e.to_string())?;
    if gas.is_empty() {
        return Err("signer has no SUI for gas".into());
    }
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(rill_chain::gas::affordable_budget(args.gas_budget, &gas));
    tx.set_gas_price(
        chain
            .reference_gas_price()
            .await
            .map_err(|e| e.to_string())?,
    );
    tx.add_gas_objects(
        gas.iter()
            .map(|c| {
                Ok(ObjectInput::owned(
                    parse(&c.reference.id)?,
                    c.reference.version,
                    c.reference
                        .digest
                        .parse::<Digest>()
                        .map_err(|_| Failure::Failed("invalid gas digest".into()))?,
                ))
            })
            .collect::<Result<Vec<_>, Failure>>()?,
    );
    let mut inputs = Vec::new();
    for coin in &coins {
        inputs.push(tx.object(owned_input(chain, &coin.reference.id, "haSUI coin").await?));
    }
    let first = inputs.remove(0);
    if !inputs.is_empty() {
        tx.merge_coins(first, inputs);
    }
    let value = tx.pure(&amount);
    let input = tx
        .split_coins(first, vec![value])
        .into_iter()
        .next()
        .ok_or("split produced no coin")?;
    let output = request_unstake_instant(
        &mut tx,
        &Unstake {
            package_id: package,
            staking_object_id: staking,
        },
        input,
        &shared,
    )
    .map_err(|e| e.to_string())?;
    rill_ptb::guard::assert_min_value(&mut tx, Some(guard), output, "0x2::sui::SUI", min)
        .map_err(|e| e.to_string())?;
    let to = tx.pure(&receiver);
    tx.transfer_objects(vec![output], to);
    let back = tx.pure(&sender);
    tx.transfer_objects(vec![first], back);
    let built = tx.try_build().map_err(|e| e.to_string())?;
    let b64 = encode(&built);
    let simulated = chain
        .simulate(&b64)
        .await
        .map_err(|e| Failure::Failed(crate::verdict::no_verdict(e)))?;
    if !simulated.ok {
        return Err(crate::verdict::would_fail(simulated.error.clone()));
    }
    let mut report = json!({"sender":sender.to_string(),"receiver":receiver.to_string(),"mode":"instant","amountBaseUnits":amount.to_string(),"minOutBaseUnits":min.to_string(),"submitted":false,
        "callSequence":[format!("{package}::staking::request_unstake_instant_coin"),rill_ptb::guard::guard_target(guard)],
        "simulation":{"ok":true,"gasEstimate":simulated.gas_used_mist,"balanceChanges":simulated.balance_changes.iter().map(|c|json!({"address":c.address,"coinType":c.coin_type,"amount":c.amount})).collect::<Vec<_>>()},
        "note":"Immediate redemption charges the protocol's live fee and can fail for insufficient liquidity. Delayed redemption requires an epoch-locked ticket and later claim; this tool does not create one."});
    if args.dry_run {
        return Ok(report);
    }
    let signature = key.sign(&built).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(|e| Failure::Failed(crate::verdict::submit_failed(e)))?;
    if let Some(error) = &outcome.error {
        return Err(crate::verdict::did_fail(error));
    }
    report["submitted"] = json!(true);
    report["digest"] = json!(outcome.digest);
    report["gasUsed"] = json!(outcome.gas_used_mist);
    report["balanceChanges"] = json!(outcome
        .balance_changes
        .iter()
        .map(|c| json!({"address":c.address,"coinType":c.coin_type,"amount":c.amount}))
        .collect::<Vec<_>>());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args() -> UnstakeArgs {
        UnstakeArgs {
            package_id: MAINNET_HAEDAL.into(),
            staking_object_id: MAINNET_STAKING.into(),
            coin_type: MAINNET_HASUI.into(),
            guard_package_id: "0x8".into(),
            amount: "0.01".into(),
            min_out: "0.009".into(),
            receiver: "0x9".into(),
            gas_budget: 50_000_000,
            dry_run: true,
        }
    }
    #[test]
    fn decimal_floor_is_exact() {
        assert_eq!(validated_amounts(&args()).unwrap(), (10_000_000, 9_000_000));
    }
    #[test]
    fn zero_floor_is_refused() {
        let mut a = args();
        a.min_out = "0".into();
        assert!(validated_amounts(&a).is_err());
    }
    #[test]
    fn wrong_asset_is_refused() {
        let mut a = args();
        a.coin_type = "0x2::sui::SUI".into();
        assert!(validated_amounts(&a).is_err());
    }
}
