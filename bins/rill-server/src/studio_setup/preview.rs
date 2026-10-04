//! Read-only single-swap preview before any owner funding transaction.
use super::*;
use rill_ptb::cetus_quote::{self, CetusQuote};

const MAINNET_CLMM: &str = "0x260693ec785a6e6c9d81d58c7d2ff72f1288ae0fa6a9725abe05a6478b11f084";

pub(super) fn quote_result(
    input: u64,
    floor: u64,
    output_type: &str,
    quote: CetusQuote,
) -> Result<Value, String> {
    if quote.amount_in.checked_add(quote.fee_amount) != Some(input) {
        return Err("Pool quote did not account for the full input including its fee".into());
    }
    Ok(
        json!({"inputBaseUnits":input.to_string(),"outputCoinType":output_type,
        "quotedOutputBaseUnits":quote.amount_out.to_string(),"minimumOutputBaseUnits":floor.to_string(),
        "feeBaseUnits":quote.fee_amount.to_string(),"outputFloorMet":quote.amount_out>=floor,
        "note":"Preview only. Prices can change; the contract checks minimum output at execution."}),
    )
}

pub(super) async fn plan(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
) -> Result<Option<Value>, String> {
    let grant = grant(body, skill, owner, context)?;
    let actions: Vec<_> = grant
        .flow
        .nodes
        .iter()
        .filter(|n| !matches!(n.kind.as_str(), "ptb" | "guardrail"))
        .collect();
    if actions.len() != 1 || actions[0].kind != "cetus_swap" {
        return Ok(None);
    }
    let package = std::env::var("RILL_CETUS_QUOTE_PACKAGE_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| (context.network == Network::Mainnet).then(|| MAINNET_CLMM.to_owned()));
    let Some(package) = package else {
        return Ok(None);
    };
    let node = actions[0];
    let value = |key| {
        node.inputs
            .as_ref()
            .and_then(|v| v.get(key))
            .or_else(|| node.config.as_ref().and_then(|v| v.get(key)))
    };
    let pool_id = address(
        value("pool")
            .and_then(Value::as_str)
            .ok_or("Swap pool is missing")?,
    )?;
    let input = canonical_type(&grant.manifest.wallet_coin_type)?;
    if let Some(configured) = value("inputCoinType") {
        if canonical_type(
            configured
                .as_str()
                .ok_or("Swap input asset must be a type string")?,
        )? != input
        {
            return Err("Swap input asset must match the vault funding asset".into());
        }
    }
    if value("by_amount_in").is_some_and(|v| v != &json!(true)) {
        return Err("Funding preview requires an exact-input swap".into());
    }
    let pool = chain.get_object(&pool_id.to_string()).await.map_err(err)?;
    let (a, b) =
        rill_ptb::cetus::pool_coin_types(pool.object_type.as_deref().ok_or("Pool type missing")?)
            .ok_or("Invalid Cetus pool type")?;
    let (a, b) = (canonical_type(&a)?, canonical_type(&b)?);
    let a2b = input == a;
    if !a2b && input != b {
        return Err("Vault input asset is not in the swap pool".into());
    }
    let amount = parse_u64_string(
        value("amount_in")
            .and_then(Value::as_str)
            .ok_or("Swap input missing")?,
    )
    .map_err(err)?;
    let floor = studio_compile::effective_floor(&grant.flow, node).map_err(err)?;
    let quote = cetus_quote::quote(chain, address(&package)?, pool_id, a2b, true, amount)
        .await
        .map_err(err)?;
    let mut result = quote_result(amount, floor, if a2b { &b } else { &a }, quote)?;
    result["inputCoinType"] = json!(input);
    Ok(Some(result))
}

pub(super) fn ensure_floor(preview: &Option<Value>) -> Result<(), String> {
    if let Some(p) = preview {
        if p["outputFloorMet"] != json!(true) {
            return Err(format!("Current quote returns {} output base units, below the published minimum {}. Increase the owner-approved input cap or publish and approve a different floor before funding.",p["quotedOutputBaseUnits"].as_str().unwrap_or("unknown"),p["minimumOutputBaseUnits"].as_str().unwrap_or("unknown")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn floor_checks_preserve_fee_units_and_never_relax_the_owner_minimum() {
        let p = quote_result(
            5_000_000,
            10_000,
            "usdc",
            CetusQuote {
                amount_in: 4_987_500,
                fee_amount: 12_500,
                amount_out: 5_873,
            },
        )
        .unwrap();
        assert_eq!(p["inputBaseUnits"], "5000000");
        assert_eq!(p["minimumOutputBaseUnits"], "10000");
        assert!(ensure_floor(&Some(p)).is_err());
        let p = quote_result(
            10_000_000,
            10_000,
            "usdc",
            CetusQuote {
                amount_in: 9_975_000,
                fee_amount: 25_000,
                amount_out: 11_746,
            },
        )
        .unwrap();
        assert!(ensure_floor(&Some(p)).is_ok());
        assert!(quote_result(
            5_000_000,
            1,
            "usdc",
            CetusQuote {
                amount_in: 5_000_000,
                fee_amount: 12_500,
                amount_out: 5_873
            }
        )
        .is_err());
    }
}
