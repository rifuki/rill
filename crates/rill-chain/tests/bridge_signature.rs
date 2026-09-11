//! What the deployed Sui Bridge actually declares on testnet.
//!
//! A bridge adapter written against a guessed signature fails with an arity or type error that names
//! neither, so the shape comes from the chain before any builder exists. `0xb` is the bridge system
//! package and `0x9` is its `Bridge` shared object, both present on testnet.
//!
//!   cargo test -p rill-chain --test bridge_signature -- --ignored --nocapture

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::GetFunctionRequest;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const BRIDGE_PACKAGE: &str = "0x000000000000000000000000000000000000000000000000000000000000000b";

fn render(p: &sui_rpc::proto::sui::rpc::v2::OpenSignature) -> String {
    format!("{:?}", p.body)
        .replace("Some(", "")
        .replace("OpenSignatureBody", "")
        .chars()
        .filter(|c| !"\"".contains(*c))
        .collect()
}

async fn describe(client: &Client, module: &str, function: &str) -> Option<usize> {
    let mut request = GetFunctionRequest::default();
    request.package_id = Some(BRIDGE_PACKAGE.to_owned());
    request.module_name = Some(module.to_owned());
    request.name = Some(function.to_owned());

    let descriptor = client
        .clone()
        .package_client()
        .get_function(request)
        .await
        .ok()?
        .into_inner()
        .function?;

    println!(
        "  {module}::{function}: {} type param(s), {} param(s), {} return(s), visibility {:?}",
        descriptor.type_parameters.len(),
        descriptor.parameters.len(),
        descriptor.returns.len(),
        descriptor.visibility(),
    );
    for (i, p) in descriptor.parameters.iter().enumerate() {
        println!("    {i:2}  {}", render(p));
    }
    Some(descriptor.parameters.len())
}

/// What the bridge module offers, and what shape a send takes.
///
/// Printed rather than asserted: this test exists to establish the shape, and an assertion written
/// before the shape is known would be asserting a guess.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_deployed_bridge_declares_its_entry_points() {
    let client = Client::new(TESTNET).expect("connect");
    println!("bridge package {BRIDGE_PACKAGE} on testnet\n");

    for function in [
        "send_token",
        "claim_token",
        "claim_and_transfer_token",
        "register_foreign_token",
        "committee_registration",
        "get_token_transfer_action_status",
    ] {
        if describe(&client, "bridge", function).await.is_none() {
            println!("  bridge::{function}: absent");
        }
    }
}

/// Which chain ids and tokens the deployed bridge knows, read from its own object.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_bridge_object_says_what_it_will_carry() {
    use rill_chain::grpc::GrpcSui;
    use rill_chain::SuiRead;

    let chain = GrpcSui::new(TESTNET).expect("connect");
    let bridge = chain
        .get_object("0x9")
        .await
        .expect("the Bridge shared object reads");

    println!("type: {}", bridge.object_type.as_deref().unwrap_or("?"));
    println!("shared at: {:?}", bridge.shared_initial_version);
    match bridge.fields.as_ref() {
        Some(fields) => println!("fields:\n{}", serde_json::to_string_pretty(fields).unwrap()),
        None => println!("the node returned no fields"),
    }
}

/// The bridge's inner state: which chains it is open to, and whether it is frozen.
///
/// `0x9`'s own fields are a versioned wrapper naming an inner object id. The route configuration,
/// the chain id this deployment believes it is, and the frozen flag are in there, and a builder that
/// guessed a target chain id would be refused by the bridge with a code rather than a name.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_bridges_inner_state_names_its_routes() {
    use rill_chain::grpc::GrpcSui;
    use rill_chain::SuiRead;

    let chain = GrpcSui::new(TESTNET).expect("connect");
    let bridge = chain.get_object("0x9").await.expect("the Bridge reads");
    let inner_id = bridge.fields.as_ref().and_then(|f| {
        f.get("inner")
            .and_then(|i| i.get("id"))
            .and_then(|i| i.as_str())
            .map(str::to_owned)
    });
    let Some(inner_id) = inner_id else {
        println!("the Bridge object names no inner id");
        return;
    };
    println!("inner: {inner_id}");

    // The inner object is a dynamic field of the Bridge, so it is read through the parent.
    let fields = chain
        .list_dynamic_fields("0x9")
        .await
        .expect("the Bridge's dynamic fields read");
    println!("{} dynamic field(s) on the Bridge", fields.len());
    for f in &fields {
        println!("  type: {}", f.value_type.as_deref().unwrap_or("?"));
        if let Some(v) = f.fields.as_ref() {
            let rendered = serde_json::to_string_pretty(v).unwrap_or_default();
            // Long, and the parts that matter are near the top: chain_id, frozen, and the
            // limiter's per-route configuration.
            for line in rendered.lines().take(40) {
                println!("    {line}");
            }
        }
    }
}

/// The route configuration hangs off the inner object, not off `0x9`.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_inner_object_carries_the_route_configuration() {
    use rill_chain::grpc::GrpcSui;
    use rill_chain::SuiRead;

    const INNER: &str = "0x7e1cbb5e18bf371232f9efe1e954a0f80bd72533a9da06a347087c434e6224b9";
    let chain = GrpcSui::new(TESTNET).expect("connect");

    let fields = chain
        .list_dynamic_fields(INNER)
        .await
        .expect("the inner object's dynamic fields read");
    println!("{} field(s) on the inner object", fields.len());
    for f in &fields {
        println!("\n  type: {}", f.value_type.as_deref().unwrap_or("?"));
        if let Some(v) = f.fields.as_ref() {
            // Only the parts a builder depends on. The committee is long and irrelevant here.
            let inner = v.get("value").unwrap_or(v);
            for key in ["bridge_version", "chain_id", "frozen", "sequence_nums"] {
                if let Some(found) = inner.get(key) {
                    println!(
                        "    {key}: {}",
                        serde_json::to_string(found).unwrap_or_default()
                    );
                }
            }
            // The two questions a builder must answer before it can emit anything: which tokens
            // this bridge carries, and whether the route out of this chain is open.
            if let Some(map) = inner
                .get("treasury")
                .and_then(|t| t.get("id_token_type_map"))
                .and_then(|m| m.get("contents"))
                .and_then(|c| c.as_array())
            {
                println!("    tokens this bridge carries:");
                for entry in map {
                    println!(
                        "      id {} -> {}",
                        entry["key"],
                        entry["value"].as_str().unwrap_or("?")
                    );
                }
            }
            if let Some(limits) = inner
                .get("limiter")
                .and_then(|l| l.get("transfer_limits"))
                .and_then(|t| t.get("contents"))
                .and_then(|c| c.as_array())
            {
                println!("    routes with a configured limit:");
                for entry in limits {
                    println!(
                        "      source {} -> destination {}: {}",
                        entry["key"]["source"],
                        entry["key"]["destination"],
                        entry["value"].as_str().unwrap_or("?")
                    );
                }
            }
            for key in ["token_transfer_records"] {
                if let Some(found) = inner.get(key) {
                    let rendered = serde_json::to_string_pretty(found).unwrap_or_default();
                    println!("    {key}:");
                    let _ = &rendered;
                }
            }
        }
    }
}

/// The bridge does not carry SUI, which decides whether a SUI-funded agent wallet can use it.
///
/// Every `AgentWallet` in this project is `AgentWallet<SUI>`. The bridge's treasury registers a token
/// id per type it will carry, and SUI has none: the five are all Ethereum-origin assets wrapped on
/// Sui. So `send_token<SUI>` is not a call that can succeed, and the limitation is the bridge's
/// registry rather than anything here.
///
/// Asserted rather than printed, because a builder's refusal is written against this fact and a
/// reader will otherwise assume the obvious thing. The day SUI is registered, this fails and the
/// refusal it justifies should be revisited.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_bridge_carries_no_sui_so_a_sui_wallet_cannot_bridge_its_own_funds() {
    use rill_chain::grpc::GrpcSui;
    use rill_chain::SuiRead;

    const INNER: &str = "0x7e1cbb5e18bf371232f9efe1e954a0f80bd72533a9da06a347087c434e6224b9";
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let fields = chain
        .list_dynamic_fields(INNER)
        .await
        .expect("the inner object reads");
    let inner = fields
        .first()
        .and_then(|f| f.fields.clone())
        .expect("the BridgeInner field");
    let inner = inner.get("value").cloned().unwrap_or(inner);
    let types: Vec<String> = inner["treasury"]["id_token_type_map"]["contents"]
        .as_array()
        .expect("the token map")
        .iter()
        .filter_map(|e| e["value"].as_str().map(str::to_owned))
        .collect();

    assert!(!types.is_empty(), "the bridge registers at least one token");
    assert!(
        !types.iter().any(|t| t.ends_with("::sui::SUI")),
        "SUI is now a bridgeable token, so the refusal written against this is stale: {types:?}"
    );
    // And the five that are there, so a refusal can name them rather than saying "not supported".
    for expected in ["::btc::BTC", "::eth::ETH", "::usdc::USDC", "::usdt::USDT"] {
        assert!(
            types.iter().any(|t| t.ends_with(expected)),
            "{expected} is no longer carried, and a refusal that lists it would be wrong: {types:?}"
        );
    }
}
