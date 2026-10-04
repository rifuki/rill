//! Owner-only display defaults, never a grant or transaction.
use super::*;

pub fn setup_defaults(skill: &PublishedSkill, owner: &str) -> Result<Value, String> {
    if skill.owner.as_deref().map(address).transpose()? != Some(address(owner)?) {
        return Err("skill must belong to the authenticated owner".into());
    }
    let manifest: CapabilityManifest = match skill.flow.get("capabilityManifest") {
        Some(value) => serde_json::from_value(value.clone()).map_err(err)?,
        None => CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };
    manifest.validate().map_err(err)?;
    if canonical_type(&manifest.wallet_coin_type)? != canonical_type("0x2::sui::SUI")? {
        return Err(
            "Studio currently funds SUI vaults only. This action uses another funding asset."
                .into(),
        );
    }
    let mut budget_limit = None;
    let mut per_tx_limit = None;
    let mut rate_limit = None;
    for rule in &manifest.rules {
        match rule {
            CapabilityRule::Budget { total_mist } => {
                budget_limit = Some(parse_u64_string(total_mist).map_err(err)?)
            }
            CapabilityRule::PerTx { max_mist } => {
                per_tx_limit = Some(parse_u64_string(max_mist).map_err(err)?)
            }
            CapabilityRule::RateLimit { max_mist, .. } => {
                rate_limit = Some(parse_u64_string(max_mist).map_err(err)?)
            }
            _ => {}
        }
    }
    let budget = budget_limit.unwrap_or(1_000_000_000).min(1_000_000_000);
    let per_tx = per_tx_limit
        .unwrap_or(budget)
        .min(budget)
        .min(rate_limit.unwrap_or(budget));
    if budget == 0 || per_tx == 0 {
        return Err("Published spending limits must be positive".into());
    }
    let declaration = rill_core::manifest::to_declaration(&manifest).map_err(err)?;
    let requires_order_price = skill.flow["nodes"].as_array().is_some_and(|nodes| {
        nodes
            .iter()
            .any(|node| node["type"] == "deepbook_limit_order")
    });
    Ok(json!({
        "budgetMist":budget.to_string(), "perTxMist":per_tx.to_string(),
        "budgetLimitMist":budget_limit.map(|n|n.to_string()), "perTxLimitMist":per_tx_limit.map(|n|n.to_string()),
        "requiresOrderPrice":requires_order_price, "restrictions":declaration.caps,
        "note":"Suggestions only. Owner approval, fresh setup validation, and on-chain rules remain authoritative."
    }))
}
