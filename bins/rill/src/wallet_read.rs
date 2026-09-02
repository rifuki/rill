//! Reading a wallet's limits from the chain that enforces them.
//!
//! Not from a run-set, and not from anything this process was told at startup. A limit reported
//! from a local copy is a limit an agent could be shown after it had already changed — and the
//! whole claim rill makes is that the limits are on chain, so the answer has to come from there.

use rill_chain::{grpc::GrpcSui, SuiRead};
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::shared::SharedObjects;
use serde_json::{json, Value};
use sui_sdk_types::Address;

/// Read the rules attached to a wallet, and how it is identified.
pub async fn read_limits(
    endpoint: &str,
    package_id: &str,
    wallet_id: &str,
) -> Result<Value, String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    let wallet: Address = wallet_id
        .parse()
        .map_err(|_| format!("{wallet_id} is not an address"))?;

    let summary = chain
        .get_object(wallet_id)
        .await
        .map_err(|e| format!("reading the wallet: {e}"))?;
    let initial = summary.shared_initial_version.ok_or_else(|| {
        format!("{wallet_id} is not a shared object, so it is not an AgentWallet")
    })?;

    let mut shared = SharedObjects::new();
    shared.insert(wallet, initial);

    let tx = policy_rules_transaction(
        package_id
            .parse()
            .map_err(|_| format!("{package_id} is not an address"))?,
        wallet,
        "0x2::sui::SUI",
        &shared,
    )
    .map_err(|e| e.to_string())?;

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&tx).map_err(|e| e.to_string())?)
    };

    let outcome = chain
        .simulate_read(&b64)
        .await
        .map_err(|e| format!("reading the wallet's rules: {e}"))?;
    let names = outcome
        .command_returns
        .iter()
        .flatten()
        .next()
        .ok_or("the wallet did not report its rules")
        .and_then(|b| parse_type_names(b).map_err(|_| "the rule list did not decode"))?;

    let modules = attached_modules(&names);
    let unrecognised: Vec<&String> = names
        .iter()
        .filter(|n| rill_ptb::policy_read::rule_module(n).is_none())
        .collect();

    Ok(json!({
        "wallet": wallet_id,
        "objectType": summary.object_type,
        "sharedInitialVersion": initial,
        "rules": modules,
        "unrecognisedRules": unrecognised,
        "enforcement": "on-chain",
        "note": "These rules are enforced by a Move contract. Nothing in this process, and nothing \
                 you can pass to it, can widen them — a spend that exceeds one is refused by the \
                 chain. Read them again after any change; this is a live read, not a cached copy."
    }))
}
