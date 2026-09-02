//! Turning a `rill_build_action` call into a [`BuildRequest`].
//!
//! Everything here arrives from an agent, so everything here is parsed rather than trusted. Two
//! rules govern the whole module:
//!
//! **Only public identifiers.** Object ids, addresses, a capability manifest. `rill-mcp`'s keyless
//! guard has already refused anything key-shaped by name; this layer would have no use for a key
//! even if one arrived, because nothing downstream of it can sign.
//!
//! **Amounts are strings.** A JSON number here would put an IEEE-754 double on the money path
//! before any of this project's careful integer arithmetic gets a chance to run — which is exactly
//! how the reference ended up computing an order price with a float.

use rill_core::envelope::Network;
use rill_core::manifest::CapabilityManifest;
use rill_ptb::deepbook::PoolSpec;
use serde_json::Value;
use sui_sdk_types::{Address, Digest};

use crate::build::{gas_object, BuildRequest};

/// Why a call could not be turned into a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub field: String,
    pub reason: String,
}

impl RequestError {
    fn at(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

fn string_at<'a>(value: &'a Value, path: &str) -> Result<&'a str, RequestError> {
    let mut current = value;
    for segment in path.split('.') {
        current = current
            .get(segment)
            .ok_or_else(|| RequestError::at(path, "is required"))?;
    }
    current
        .as_str()
        .ok_or_else(|| RequestError::at(path, "must be a string"))
}

fn address_at(value: &Value, path: &str) -> Result<Address, RequestError> {
    string_at(value, path)?
        .parse()
        .map_err(|_| RequestError::at(path, "is not a Sui address"))
}

fn u64_at(value: &Value, path: &str) -> Result<u64, RequestError> {
    let mut current = value;
    for segment in path.split('.') {
        current = current
            .get(segment)
            .ok_or_else(|| RequestError::at(path, "is required"))?;
    }
    // A JSON number is accepted only where the value is a count or a version — never for an
    // amount. See the module note.
    current
        .as_u64()
        .ok_or_else(|| RequestError::at(path, "must be a non-negative integer"))
}

/// An amount, taken as a decimal string and kept as one.
///
/// A JSON number is refused outright rather than converted. Accepting one would mean the value had
/// already been through a double before this code ever saw it, and no amount of careful arithmetic
/// afterwards can recover a digit that was lost at parse time.
fn amount_at(value: &Value, path: &str) -> Result<String, RequestError> {
    let mut current = value;
    for segment in path.split('.') {
        current = current
            .get(segment)
            .ok_or_else(|| RequestError::at(path, "is required"))?;
    }
    match current {
        Value::String(s) => {
            rill_core::amounts::Decimal::parse(s)
                .map_err(|e| RequestError::at(path, e.to_string()))?;
            Ok(s.clone())
        }
        Value::Number(_) => Err(RequestError::at(
            path,
            "must be a decimal string, not a number — a JSON number is a float by the time it \
             reaches here, and an amount must never have been one",
        )),
        _ => Err(RequestError::at(path, "must be a decimal string")),
    }
}

/// Parse a `rill_build_action` argument object.
#[allow(clippy::too_many_arguments)]
pub fn parse_build_request(
    arguments: &Value,
    action_id: &str,
    network: Network,
    deepbook_package_id: Address,
    gas_budget: u64,
    gas_price: u64,
) -> Result<BuildRequest, RequestError> {
    let sender = address_at(arguments, "sender")?;

    // Everything below navigates from the request root and names the full dotted path in any
    // refusal. An error reading `price: must be a decimal string` leaves an operator guessing
    // which `price`; `params.price` does not.
    let _ = arguments
        .get("agentWallet")
        .ok_or_else(|| RequestError::at("agentWallet", "is required"))?;
    let manifest_value = arguments
        .get("agentWallet")
        .and_then(|w| w.get("capabilityManifest"))
        .ok_or_else(|| RequestError::at("agentWallet.capabilityManifest", "is required"))?;
    let manifest: CapabilityManifest = serde_json::from_value(manifest_value.clone())
        .map_err(|e| RequestError::at("agentWallet.capabilityManifest", e.to_string()))?;
    // A manifest that does not validate would produce a spend sequence the chain refuses, so it is
    // rejected here where the error can name the field rather than on-chain where it cannot.
    manifest
        .validate()
        .map_err(|e| RequestError::at("agentWallet.capabilityManifest", e.to_string()))?;

    let cap_id = address_at(arguments, "agentWallet.capId")?;
    let cap_digest: Digest = string_at(arguments, "agentWallet.capDigest")?
        .parse()
        .map_err(|_| RequestError::at("agentWallet.capDigest", "is not an object digest"))?;

    let params = arguments
        .get("params")
        .ok_or_else(|| RequestError::at("params", "is required"))?;
    let _ = params;

    let trade_cap_id = address_at(arguments, "params.tradeCapId")?;
    let trade_cap_digest: Digest = string_at(arguments, "params.tradeCapDigest")?
        .parse()
        .map_err(|_| RequestError::at("params.tradeCapDigest", "is not an object digest"))?;

    // Funding the manager and trading on it are two delegations, so they are two capabilities.
    // Required rather than optional: a request that omits it would build a deposit the agent
    // cannot authorise, and the failure would arrive as an owner assertion on chain.
    let deposit_cap_id = address_at(arguments, "params.depositCapId")?;
    let deposit_cap_digest: Digest = string_at(arguments, "params.depositCapDigest")?
        .parse()
        .map_err(|_| RequestError::at("params.depositCapDigest", "is not an object digest"))?;

    let gas_id = address_at(arguments, "params.gasObjectId")?;
    let gas_digest: Digest = string_at(arguments, "params.gasObjectDigest")?
        .parse()
        .map_err(|_| RequestError::at("params.gasObjectDigest", "is not an object digest"))?;

    Ok(BuildRequest {
        action_id: action_id.to_owned(),
        sender,
        network,
        wallet_package_id: address_at(arguments, "agentWallet.packageId")?,
        wallet_id: address_at(arguments, "agentWallet.walletId")?,
        agent_cap: gas_object(
            cap_id,
            u64_at(arguments, "agentWallet.capVersion")?,
            cap_digest,
        ),
        agent_cap_id: cap_id.to_string(),
        version_id: address_at(arguments, "agentWallet.versionId")?,
        manifest,
        deepbook_package_id,
        // Supplied by the caller rather than looked up. These are public identifiers, the signer
        // pins them against its own run-set before signing, and a table kept here would be one
        // more thing to go stale against the chain.
        pool: PoolSpec {
            pool_id: address_at(arguments, "params.poolId")?,
            base_coin_type: string_at(arguments, "params.baseCoinType")?.to_owned(),
            quote_coin_type: string_at(arguments, "params.quoteCoinType")?.to_owned(),
            base_scalar: u64_at(arguments, "params.baseScalar")? as u128,
            quote_scalar: u64_at(arguments, "params.quoteScalar")? as u128,
        },
        balance_manager_id: address_at(arguments, "params.balanceManagerId")?,
        trade_cap: gas_object(
            trade_cap_id,
            u64_at(arguments, "params.tradeCapVersion")?,
            trade_cap_digest,
        ),
        trade_cap_id: trade_cap_id.to_string(),
        deposit_cap: gas_object(
            deposit_cap_id,
            u64_at(arguments, "params.depositCapVersion")?,
            deposit_cap_digest,
        ),
        deposit_cap_id: deposit_cap_id.to_string(),
        client_order_id: u64_at(arguments, "params.clientOrderId")?,
        price: amount_at(arguments, "params.price")?,
        quantity: amount_at(arguments, "params.quantity")?,
        is_bid: arguments
            .get("params")
            .and_then(|p| p.get("isBid"))
            .and_then(Value::as_bool)
            .ok_or_else(|| RequestError::at("params.isBid", "must be a boolean"))?,
        pay_with_deep: arguments
            .get("params")
            .and_then(|p| p.get("payWithDeep"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        spend_base_units: {
            let raw = string_at(arguments, "params.spendAmountMist")?;
            rill_core::amounts::parse_u64_string(raw)
                .map_err(|e| RequestError::at("params.spendAmountMist", e.to_string()))?
        },
        gas_budget,
        gas_price,
        gas_objects: vec![gas_object(
            gas_id,
            u64_at(arguments, "params.gasObjectVersion")?,
            gas_digest,
        )],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn addr(n: u8) -> String {
        format!("0x{:064x}", n)
    }

    fn arguments() -> Value {
        json!({
            "actionId": "skill_hero",
            "sender": addr(9),
            "agentWallet": {
                "packageId": addr(0xca),
                "walletId": addr(1),
                "capId": addr(2),
                "capVersion": 1,
                "capDigest": "11111111111111111111111111111111",
                "versionId": addr(3),
                "capabilityManifest": {
                    "walletCoinType": "0x2::sui::SUI",
                    "rules": [{ "kind": "budget", "totalMist": "5000000000" }]
                }
            },
            "params": {
                "balanceManagerId": addr(0x21),
                "tradeCapId": addr(0x22),
                "tradeCapVersion": 1,
                "tradeCapDigest": "11111111111111111111111111111111",
                "depositCapId": addr(0x23),
                "depositCapVersion": 1,
                "depositCapDigest": "11111111111111111111111111111111",
                "gasObjectId": addr(0x0a),
                "gasObjectVersion": 1,
                "gasObjectDigest": "11111111111111111111111111111111",
                "poolId": addr(0x20),
                "baseCoinType": "0xde::deep::DEEP",
                "quoteCoinType": "0x2::sui::SUI",
                "baseScalar": 1000000,
                "quoteScalar": 1000000000,
                "clientOrderId": 1,
                "price": "2.5",
                "quantity": "1",
                "isBid": true,
                "payWithDeep": false,
                "spendAmountMist": "1000000000"
            }
        })
    }

    /// `unwrap_err` needs the Ok side to be Debug, and a BuildRequest holds an ObjectInput that
    /// is not. Discarding it explicitly is clearer than adding a Debug impl only tests would use.
    fn error(args: &Value) -> RequestError {
        match parse(args) {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => e,
        }
    }

    fn parse(args: &Value) -> Result<BuildRequest, RequestError> {
        parse_build_request(
            args,
            "skill_hero",
            Network::Testnet,
            addr(0xde).parse().unwrap(),
            50_000_000,
            1_000,
        )
    }

    #[test]
    fn a_complete_call_parses() {
        let request = parse(&arguments()).expect("should parse");
        assert_eq!(request.price, "2.5");
        assert_eq!(request.spend_base_units, 1_000_000_000);
    }

    /// The rule this module exists for.
    #[test]
    fn a_numeric_price_is_refused_rather_than_converted() {
        let mut args = arguments();
        args["params"]["price"] = json!(2.5);
        let err = error(&args);
        assert_eq!(err.field, "params.price");
        assert!(
            err.reason.contains("not a number"),
            "the refusal should say why: {}",
            err.reason
        );
    }

    #[test]
    fn a_numeric_quantity_is_refused_too() {
        let mut args = arguments();
        args["params"]["quantity"] = json!(1);
        assert_eq!(error(&args).field, "params.quantity");
    }

    #[test]
    fn a_price_in_scientific_notation_is_refused() {
        let mut args = arguments();
        args["params"]["price"] = json!("1e-9");
        assert_eq!(error(&args).field, "params.price");
    }

    #[test]
    fn a_manifest_with_no_rules_is_refused_where_the_error_can_name_the_field() {
        let mut args = arguments();
        args["agentWallet"]["capabilityManifest"]["rules"] = json!([]);
        let err = error(&args);
        assert_eq!(err.field, "agentWallet.capabilityManifest");
    }

    #[test]
    fn a_missing_field_names_itself() {
        for path in ["sender", "agentWallet", "params"] {
            let mut args = arguments();
            args.as_object_mut().unwrap().remove(path);
            assert_eq!(error(&args).field, path);
        }
    }

    #[test]
    fn a_malformed_address_is_refused() {
        let mut args = arguments();
        args["sender"] = json!("not-an-address");
        assert_eq!(error(&args).field, "sender");
    }

    #[test]
    fn a_spend_amount_that_is_not_a_u64_string_is_refused() {
        let mut args = arguments();
        args["params"]["spendAmountMist"] = json!("1.5");
        assert_eq!(error(&args).field, "params.spendAmountMist");
    }
}
