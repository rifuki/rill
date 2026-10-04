//! Public-address portfolio reads. No signature or private key leaves this process.
use rill_chain::{grpc::GrpcSui, SuiRead};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Extract the underlying coin type from a chain object type.
pub fn coin_type(object_type: &str) -> Option<&str> {
    let (module, inner) = object_type.split_once("::coin::Coin<")?;
    let package: sui_sdk_types::Address = module.parse().ok()?;
    if package != "0x2".parse().ok()? {
        return None;
    }
    inner.strip_suffix('>')
}

pub async fn portfolio_json(endpoint: &str, owner: &str) -> Result<Value, String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    portfolio_json_on(&chain, owner).await
}

pub async fn portfolio_json_on(chain: &impl SuiRead, owner: &str) -> Result<Value, String> {
    let address: sui_sdk_types::Address =
        owner.parse().map_err(|_| "owner is not a Sui address")?;
    let owner = address.to_string();
    let objects = chain
        .list_owned_objects(&owner)
        .await
        .map_err(|e| e.to_string())?;
    let mut types = BTreeMap::<String, usize>::new();
    let mut other = Vec::new();
    for object in &objects {
        if let Some(asset) = object.object_type.as_deref().and_then(coin_type) {
            *types.entry(asset.to_owned()).or_default() += 1;
        } else {
            other.push(json!({"objectId": object.reference.id, "type": object.object_type}));
        }
    }
    // Include native gas even when the index currently lists no native coin objects.
    let sui_type: sui_sdk_types::TypeTag = "0x2::sui::SUI"
        .parse()
        .map_err(|_| "invalid native coin type")?;
    if !types
        .keys()
        .any(|asset| asset.parse::<sui_sdk_types::TypeTag>().ok().as_ref() == Some(&sui_type))
    {
        types.entry("0x2::sui::SUI".into()).or_default();
    }
    let mut balances = Vec::new();
    for (asset, count) in types {
        // get_balance reads the aggregate; object count is index-derived and may lag a transaction.
        let amount = chain
            .get_balance(&owner, &asset)
            .await
            .map_err(|e| e.to_string())?;
        balances.push(json!({"coinType":asset,"balanceBaseUnits":amount.to_string(),"indexedCoinObjects":count}));
    }
    Ok(
        json!({"owner":owner,"submitted":false,"balances":balances,"otherObjects":other,
        "scope":"Directly owned coins and objects. Shared Rill vault balances and external lending positions are not included.",
        "note":"Amounts are exact base units, not USD prices. Index-derived object lists can lag recent transactions."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregate_balances_keep_values_above_float_precision() {
        use rill_chain::{fake::FakeSui, ObjectRef, ObjectSummary};
        let owner = "0x9".parse::<sui_sdk_types::Address>().unwrap().to_string();
        let chain = FakeSui::new()
            .with_object(
                Some(&owner),
                ObjectSummary {
                    reference: ObjectRef {
                        id: "0xa".into(),
                        version: 1,
                        digest: sui_sdk_types::Digest::ZERO.to_string(),
                    },
                    object_type: Some("0x2::coin::Coin<0x7::usdc::USDC>".into()),
                    fields: None,
                    shared_initial_version: None,
                },
            )
            .with_balance(&owner, "0x7::usdc::USDC", 9_007_199_254_740_993);
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(portfolio_json_on(&chain, &owner))
            .unwrap();
        let balance = result["balances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["coinType"] == "0x7::usdc::USDC")
            .unwrap();
        assert_eq!(balance["balanceBaseUnits"], "9007199254740993");
        assert!(chain.submitted().is_empty());
    }

    #[test]
    fn extracts_only_sui_framework_coins() {
        assert_eq!(coin_type("0x2::coin::Coin<0x3::x::T>"), Some("0x3::x::T"));
        assert_eq!(coin_type("0x7::coin::Coin<0x3::x::T>"), None);
        assert_eq!(coin_type("0x2::coin::Coin<0x3::x::T"), None);
    }
}
