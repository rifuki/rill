//! A runtime wallet may tighten a published grant, never remove or widen it.
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use sui_sdk_types::{Address, TypeTag};

fn amount(s: &str) -> Result<u64, String> {
    rill_core::amounts::parse_u64_string(s).map_err(|e| e.to_string())
}
fn subset<T: std::str::FromStr + PartialEq>(actual: &[String], allowed: &[String]) -> bool {
    let Ok(actual) = actual
        .iter()
        .map(|s| s.parse::<T>())
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    let Ok(allowed) = allowed
        .iter()
        .map(|s| s.parse::<T>())
        .collect::<Result<Vec<_>, _>>()
    else {
        return false;
    };
    !actual.is_empty() && actual.iter().all(|v| allowed.contains(v))
}
pub fn ensure_narrower(
    published: &CapabilityManifest,
    actual: &CapabilityManifest,
) -> Result<(), String> {
    published.validate().map_err(|e| e.to_string())?;
    actual.validate().map_err(|e| e.to_string())?;
    if published
        .wallet_coin_type
        .parse::<TypeTag>()
        .map_err(|e| e.to_string())?
        != actual
            .wallet_coin_type
            .parse::<TypeTag>()
            .map_err(|e| e.to_string())?
    {
        return Err("wallet coin type differs from the published grant".into());
    }
    use CapabilityRule::*;
    for required in &published.rules {
        let refusal = || {
            format!(
                "wallet binding must preserve or tighten the published {} rule",
                required.kind().as_str()
            )
        };
        let rule = actual
            .rules
            .iter()
            .find(|r| r.kind() == required.kind())
            .ok_or_else(refusal)?;
        let narrower = match (required, rule) {
            (Budget { total_mist: p }, Budget { total_mist: a })
            | (PerTx { max_mist: p }, PerTx { max_mist: a }) => amount(a)? <= amount(p)?,
            (
                RateLimit {
                    window_ms: pw,
                    max_mist: p,
                },
                RateLimit {
                    window_ms: aw,
                    max_mist: a,
                },
            ) => amount(aw)? == amount(pw)? && amount(a)? <= amount(p)?,
            // Same coin, or the comparison means nothing: a floor of 10000 USDC base units is not
            // tighter than 1 SUI. A published coin-less floor binds every swap, so an applied one
            // that names a coin would loosen it.
            (
                SlippageFloor {
                    min_out_mist: p,
                    coin_type: pc,
                },
                SlippageFloor {
                    min_out_mist: a,
                    coin_type: ac,
                },
            ) => pc == ac && amount(a)? >= amount(p)?,
            (
                TimeWindow {
                    not_before_ms: pb,
                    not_after_ms: pa,
                },
                TimeWindow {
                    not_before_ms: ab,
                    not_after_ms: aa,
                },
            ) => amount(ab)? >= amount(pb)? && amount(aa)? <= amount(pa)?,
            (
                ProtocolScope {
                    allowed_packages: p,
                },
                ProtocolScope {
                    allowed_packages: a,
                },
            ) => subset::<Address>(a, p),
            (RecipientAllowlist { addresses: p }, RecipientAllowlist { addresses: a }) => {
                subset::<Address>(a, p)
            }
            (
                AssetScope {
                    allowed_coin_types: p,
                },
                AssetScope {
                    allowed_coin_types: a,
                },
            ) => subset::<TypeTag>(a, p),
            _ => false,
        };
        if !narrower {
            return Err(refusal());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn manifest(rule: Value) -> CapabilityManifest {
        serde_json::from_value(json!({"walletCoinType":"0x2::sui::SUI","rules":[rule]})).unwrap()
    }
    #[test]
    fn every_published_rule_rejects_widening_and_accepts_tightening() {
        let cases = [
            (
                json!({"kind":"budget","totalMist":"10"}),
                json!({"kind":"budget","totalMist":"9"}),
                json!({"kind":"budget","totalMist":"11"}),
            ),
            (
                json!({"kind":"per_tx","maxMist":"10"}),
                json!({"kind":"per_tx","maxMist":"9"}),
                json!({"kind":"per_tx","maxMist":"11"}),
            ),
            (
                json!({"kind":"rate_limit","windowMs":"100","maxMist":"10"}),
                json!({"kind":"rate_limit","windowMs":"100","maxMist":"9"}),
                json!({"kind":"rate_limit","windowMs":"50","maxMist":"10"}),
            ),
            (
                json!({"kind":"slippage_floor","minOutMist":"10"}),
                json!({"kind":"slippage_floor","minOutMist":"11"}),
                json!({"kind":"slippage_floor","minOutMist":"9"}),
            ),
            // Moving a floor to another coin is not tightening it, whatever the numbers say.
            (
                json!({"kind":"slippage_floor","minOutMist":"10","coinType":"0x3::usdc::USDC"}),
                json!({"kind":"slippage_floor","minOutMist":"11","coinType":"0x3::usdc::USDC"}),
                json!({"kind":"slippage_floor","minOutMist":"1000","coinType":"0x2::sui::SUI"}),
            ),
            (
                json!({"kind":"time_window","notBeforeMs":"10","notAfterMs":"20"}),
                json!({"kind":"time_window","notBeforeMs":"11","notAfterMs":"19"}),
                json!({"kind":"time_window","notBeforeMs":"9","notAfterMs":"21"}),
            ),
            (
                json!({"kind":"protocol_scope","allowedPackages":["0x1","0x2"]}),
                json!({"kind":"protocol_scope","allowedPackages":["0x1"]}),
                json!({"kind":"protocol_scope","allowedPackages":["0x3"]}),
            ),
            (
                json!({"kind":"recipient_allowlist","addresses":["0x1","0x2"]}),
                json!({"kind":"recipient_allowlist","addresses":["0x1"]}),
                json!({"kind":"recipient_allowlist","addresses":["0x3"]}),
            ),
            (
                json!({"kind":"asset_scope","allowedCoinTypes":["0x2::sui::SUI","0x3::coin::USDC"]}),
                json!({"kind":"asset_scope","allowedCoinTypes":["0x2::sui::SUI"]}),
                json!({"kind":"asset_scope","allowedCoinTypes":["0x4::coin::USDC"]}),
            ),
        ];
        for (published, narrower, wider) in cases {
            let published = manifest(published);
            assert!(
                ensure_narrower(&published, &manifest(narrower)).is_ok(),
                "{:?}",
                published.rules
            );
            assert!(
                ensure_narrower(&published, &manifest(wider)).is_err(),
                "{:?}",
                published.rules
            );
        }
    }
}
