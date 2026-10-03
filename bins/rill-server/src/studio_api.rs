//! HTTP contracts consumed by Studio, backed by the same Rust compiler as published actions.
use crate::{
    envelope::{api_err_typed, api_ok},
    state::{AppState, Network},
    studio_compile::{self, CompileOptions},
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rill_core::{
    flow::{topological_sort, FlowGraph},
    manifest::{to_declaration, to_on_chain_rule_params, to_signer_policy, CapabilityManifest},
};
use rill_ptb::{deployments, registry::DeepBookNetwork};
use rill_store::{PublishedSkill, SkillStore};
use serde_json::{json, Value};
use sui_sdk_types::Address;

pub fn invalid(message: impl ToString) -> Response {
    api_err_typed(
        StatusCode::UNPROCESSABLE_ENTITY,
        message.to_string(),
        "ValidationError",
    )
}
pub use crate::studio_auth::now_ms;
pub fn parse_body(body: &Bytes) -> Result<Value, Box<Response>> {
    let value: Value = serde_json::from_slice(body).map_err(invalid)?;
    if !value.is_object() {
        return Err(Box::new(invalid("Body must be a JSON object")));
    }
    rill_mcp::assert_keyless_arguments(&value).map_err(invalid)?;
    Ok(value)
}
pub fn parse_flow(value: &Value) -> Result<FlowGraph, Box<Response>> {
    let flow: FlowGraph = serde_json::from_value(value.clone()).map_err(invalid)?;
    if flow.nodes.is_empty() {
        return Err(Box::new(invalid("Add at least one action to the flow")));
    }
    topological_sort(&flow).map_err(invalid)?;
    for node in &flow.nodes {
        if !matches!(
            node.kind.as_str(),
            "cetus_swap" | "haedal_stake" | "deepbook_limit_order" | "guardrail" | "ptb"
        ) {
            return Err(Box::new(invalid(format!(
                "Unsupported node type: {}",
                node.kind
            ))));
        }
    }
    Ok(flow)
}
pub fn manifest(value: &Value) -> Result<CapabilityManifest, Box<Response>> {
    let parsed: CapabilityManifest = serde_json::from_value(value.clone()).map_err(invalid)?;
    parsed.validate().map_err(invalid)?;
    Ok(parsed)
}
pub fn owner(state: &AppState, headers: &HeaderMap) -> Result<Option<String>, Box<Response>> {
    match headers.get(axum::http::header::AUTHORIZATION) {
        None => Ok(None),
        Some(h) => crate::mcp::authenticate(state, h.to_str().ok()).map(Some),
    }
}
pub fn skill_urls(state: &AppState, skill: &PublishedSkill) -> Value {
    json!({"id":skill.id,"name":skill.name,"description":skill.description,"createdAt":skill.created_at,
        "mcpUrl":format!("{}/api/mcp/{}",state.config.base(),skill.id),
        "skillUrl":format!("{}/api/skills/{}/skill.md",state.config.base(),skill.id)})
}
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let visible = match owner(&state, &headers) {
        Ok(Some(o)) => state.skills.list_by_owner(&o),
        Ok(None) => state.skills.list_unowned(),
        Err(e) => return *e,
    };
    api_ok(
        visible
            .iter()
            .map(|s| skill_urls(&state, s))
            .collect::<Vec<_>>(),
    )
}
pub async fn capability_preview(body: Bytes) -> Response {
    let result = (|| {
        let body = parse_body(&body)?;
        let m = manifest(&body["manifest"])?;
        let rules = to_on_chain_rule_params(&m)
            .map_err(invalid)?
            .into_iter()
            .map(|r| {
                let config: serde_json::Map<String, Value> = r
                    .config
                    .into_iter()
                    .map(|(k, v)| (k.into(), json!(v.to_string())))
                    .collect();
                json!({"module":r.module,"config":config})
            })
            .collect::<Vec<_>>();
        Ok::<_, Box<Response>>(
            json!({"onChainRules":rules,"signerPolicy":to_signer_policy(&m).map_err(invalid)?,"declaration":to_declaration(&m).map_err(invalid)?}),
        )
    })();
    match result {
        Ok(v) => api_ok(v),
        Err(e) => *e,
    }
}
pub async fn protocols(State(state): State<AppState>) -> Response {
    let testnet = state.config.network == Network::Testnet;
    let net = if testnet {
        DeepBookNetwork::Testnet
    } else {
        DeepBookNetwork::Mainnet
    };
    let (integrate, config, pool, usdc, haedal, staking) = if testnet {
        (
            deployments::TESTNET_CETUS_INTEGRATE,
            deployments::TESTNET_CETUS_GLOBAL_CONFIG,
            "0x2603c08065a848b719f5f465e40dbef485ec4fd9c967ebe83a7565269a74a2b2",
            "0x14a71d857b34677a7d57e0feb303df1adb515a37780645ab763d42ce8d1a5e48::usdc::USDC",
            deployments::TESTNET_HAEDAL_PACKAGE,
            deployments::TESTNET_HAEDAL_STAKING,
        )
    } else {
        (
            "0x996c4d9480708fb8b92aa7acf819fb0497b5ec8e65ba06601cae2fb6db3312c3",
            "0xdaa46292632c3c4d8f31f23ea0f9b36a28ff3677e9684980e4438403a67a3d8f",
            "0xb8d7d9e66a60c239e7a60110efcf8de6c705580ed924d0dde141f4a0e2c90105",
            "0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC",
            "0x126e4cfb051cad744706df590ec399e8c02b6feae195c35b8b496280d5442a62",
            "0x47b224762220393057ebf4f70501b6e657c3e56684737568439a04f80849b2ca",
        )
    };
    api_ok(json!({"network":state.config.network.as_str(),
        "cetus_swap":{"integratePackageId":integrate,"globalConfigId":config,"defaultPoolId":pool,"defaultInputCoinType":"0x2::sui::SUI","tokens":[{"symbol":"SUI","coinType":"0x2::sui::SUI"},{"symbol":"USDC","coinType":usdc}],"minSqrtPrice":rill_ptb::cetus::MIN_SQRT_PRICE.to_string(),"maxSqrtPrice":rill_ptb::cetus::MAX_SQRT_PRICE.to_string()},
        "haedal_stake":{"packageId":haedal,"stakeTarget":format!("{haedal}::interface::request_stake"),"suiSystemStateId":"0x5","stakingObjectId":staking,"minStakeMist":rill_ptb::haedal::MIN_STAKE_MIST.to_string(),"coinType":"0x2::sui::SUI"},
        "deepbook_limit_order":{"pools":net.pools().iter().map(|p|p.key).collect::<Vec<_>>(),"coins":net.coins().iter().map(|c|c.symbol).collect::<Vec<_>>(),"requiresBalanceManager":true,"requiresDepositCap":true}}))
}
pub fn options(state: &AppState, body: &Value) -> Result<CompileOptions, Box<Response>> {
    let sender = body
        .get("sender")
        .map(|v| -> Result<Address, Box<Response>> {
            Ok(v.as_str()
                .ok_or_else(|| invalid("sender must be an address string"))?
                .parse::<Address>()
                .map_err(invalid)?)
        })
        .transpose()?;
    let agent_wallet = body
        .get("agentWallet")
        .filter(|v| !v.is_null())
        .map(|v| {
            let mut binding = v.clone();
            if let Some(fields) = binding.as_object_mut() {
                fields.remove("capVersion");
                fields.remove("capDigest");
            }
            serde_json::from_value(binding).map_err(|e| Box::new(invalid(e)))
        })
        .transpose()?;
    let guard =
        state
            .config
            .guard_package_id
            .as_deref()
            .or(if state.config.network == Network::Testnet {
                Some(deployments::TESTNET_RILL_GUARD)
            } else {
                None
            });
    Ok(CompileOptions {
        sender,
        agent_wallet,
        network: state.config.network.into(),
        guard_package: guard.map(str::parse).transpose().map_err(invalid)?,
    })
}
pub async fn compile(State(state): State<AppState>, body: Bytes) -> Response {
    compile_or_simulate(&state, body, false).await
}
pub async fn simulate(State(state): State<AppState>, body: Bytes) -> Response {
    compile_or_simulate(&state, body, true).await
}
async fn compile_or_simulate(state: &AppState, body: Bytes, simulate: bool) -> Response {
    let body = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let flow = match parse_flow(&body["flow"]) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let options = match options(state, &body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let built = match studio_compile::compile(&flow, &options, state.chain.as_ref()).await {
        Ok(v) => v,
        Err(e) => return invalid(e),
    };
    let mut data = json!({"unsignedPtb":built.unsigned_ptb,"encoding":"bcs-transaction-kind","preview":built.preview,"warnings":built.warnings,"agentWalletBound":options.agent_wallet.is_some(),"budgetSpendMist":built.root_spend_mist.to_string()});
    if simulate {
        let bytes = match bcs::to_bytes(&built.transaction) {
            Ok(v) => STANDARD.encode(v),
            Err(e) => return invalid(e),
        };
        match state.chain.simulate_preview(&bytes).await {
            Ok(sim) => {
                data["simulation"] = json!({"ok":sim.ok,"verification":"unverified","error":sim.error,"gasEstimate":sim.gas_used_mist.to_string(),"balanceChanges":sim.balance_changes.iter().map(|d|json!({"owner":d.address,"coinType":d.coin_type,"amount":d.amount})).collect::<Vec<_>>(),"objectChanges":[]});
                if let Some(w) = data["warnings"].as_array_mut() {
                    w.push(json!("Preview only: ownership and gas checks are disabled. Local execution requires a fresh, verified simulation."));
                }
            }
            Err(e) => {
                data["simulation"] = json!({"ok":false,"verification":"unverified","error":e.to_string(),"gasEstimate":"0","balanceChanges":[],"objectChanges":[]})
            }
        }
    }
    api_ok(data)
}
pub fn tool_definition(skill: &PublishedSkill) -> Value {
    let mut nodes = serde_json::Map::new();
    if let Ok(flow) = serde_json::from_value::<FlowGraph>(skill.flow.clone()) {
        for node in flow.nodes {
            let mut properties = serde_json::Map::new();
            for key in runtime_keys(&node.kind) {
                let mut field = json!({"type":if matches!(*key,"isBid"|"payWithDeep"){"boolean"}else{"string"}});
                if let Some(default) = node
                    .inputs
                    .as_ref()
                    .and_then(|v| v.get(*key))
                    .or_else(|| node.config.as_ref().and_then(|v| v.get(*key)))
                {
                    if !default.is_null() {
                        field["default"] = default.clone();
                    }
                }
                if *key == "min_amount_out" {
                    field["description"] =
                        json!("May tighten the published floor, never lower it.");
                }
                properties.insert((*key).into(), field);
            }
            if !properties.is_empty() {
                nodes.insert(
                    node.id,
                    json!({"type":"object","properties":properties,"additionalProperties":false}),
                );
            }
        }
    }
    json!({"name":"build_action","description":skill.description,"inputSchema":{"type":"object","properties":{"sender":{"type":"string"},"agentWallet":{"type":"object","description":"Public wallet binding: packageId, walletId, capId, versionId, coinType, capabilityManifest"},"params":{"type":"object","description":"Runtime overrides keyed by these node IDs; amounts are exact strings. Omitted values retain the published configuration.","properties":nodes,"additionalProperties":false}},"required":["sender","agentWallet"],"additionalProperties":false}})
}
pub async fn publish(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if ["owner", "id", "createdAt"]
        .iter()
        .any(|k| body.get(k).is_some())
    {
        return invalid("owner, id and createdAt are set by the server");
    }
    let flow = match parse_flow(&body["flow"]) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let owner = match owner(&state, &headers) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let mut stored = body["flow"].clone();
    stored["studio"] = json!(true);
    if let Some(value) = body.get("manifest") {
        let m = match manifest(value) {
            Ok(v) => v,
            Err(e) => return *e,
        };
        stored["capabilityManifest"] = json!(m);
    }
    let actions = flow
        .nodes
        .iter()
        .filter(|n| !matches!(n.kind.as_str(), "ptb" | "guardrail"))
        .map(|n| n.kind.replace('_', " "))
        .collect::<Vec<_>>();
    if actions.is_empty() {
        return invalid("Add a transaction action before publishing");
    }
    let name = actions.join(" + ");
    let skill = PublishedSkill {
        id: format!("skill_{}", rill_auth::tokens::random_id()),
        name: name.clone(),
        description: format!("Build {name} as one unsigned Sui transaction."),
        flow: stored,
        tool_defs: None,
        policy_id: body["policyId"].as_str().map(str::to_owned),
        owner,
        created_at: crate::build::format_rfc3339_ms(now_ms()),
    };
    if let Err(e) = state.skills.save(skill.clone()) {
        return api_err_typed(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
            "StoreError",
        );
    }
    let mut result = skill_urls(&state, &skill);
    result["skillId"] = json!(skill.id);
    result["toolDefs"] = tool_definition(&skill);
    result["warnings"]=json!(["Published metadata. Building requires a sender, wallet binding, and successful strict simulation."]);
    if let Some(owner) = &skill.owner {
        result["owner"] = json!(owner);
        result["ownerMcpUrl"] = json!(state.config.resource());
    }
    api_ok(result)
}
pub async fn skill_doc(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(skill) = state.skills.get(&id) else {
        return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound");
    };
    ([(axum::http::header::CONTENT_TYPE,"text/markdown; charset=utf-8")],format!("# {}\n\n{}\n\nMCP: {}/api/mcp/{}\n\nBuild with `build_action`, providing sender, agentWallet and params. The server returns an unsigned envelope only after strict simulation. Use local rill-wallet to validate, re-simulate and sign.\n",skill.name,skill.description,state.config.base(),skill.id)).into_response()
}
pub async fn introspect(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let package = match body["packageId"]
        .as_str()
        .and_then(|s| s.parse::<Address>().ok())
    {
        Some(v) => v,
        None => return invalid("packageId must be a Sui address"),
    };
    match rill_chain::describe::describe_package(&state.config.sui_rpc_url, &package.to_string())
        .await
    {
        Ok(functions) => api_ok(
            functions
                .iter()
                .map(|f| function_json(&package.to_string(), f))
                .collect::<Vec<_>>(),
        ),
        Err(e) => api_err_typed(StatusCode::BAD_GATEWAY, e.to_string(), "ChainError"),
    }
}
fn function_json(package: &str, f: &rill_chain::describe::FunctionSignature) -> Value {
    json!({"packageId":package,"module":f.module,"name":f.name,"isEntry":f.is_entry,"parameters":f.call_arguments().iter().enumerate().map(|(i,p)|json!({"index":i,"name":null,"moveType":p.to_string(),"class":if p.type_name.contains("::"){"object"}else{"pure"}})).collect::<Vec<_>>(),"returns":f.returns.iter().map(ToString::to_string).collect::<Vec<_>>()})
}
pub async fn resolve(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let Some(package) = body["packageId"].as_str() else {
        return invalid("packageId is required");
    };
    if package.parse::<Address>().is_err() {
        return invalid("packageId is not a Sui address");
    }
    let Some(module) = body["moduleName"].as_str() else {
        return invalid("moduleName is required");
    };
    let Some(function) = body["functionName"].as_str() else {
        return invalid("functionName is required");
    };
    match rill_chain::describe::describe_function(
        &state.config.sui_rpc_url,
        package,
        module,
        function,
    )
    .await
    {
        Ok(f) => {
            let mut value = function_json(package, &f);
            value["description"]=json!(format!("{module}::{function}. Parameter names and business semantics are not supplied by the ABI."));
            value["confidence"] = json!(0);
            api_ok(value)
        }
        Err(e) => api_err_typed(StatusCode::BAD_GATEWAY, e.to_string(), "ChainError"),
    }
}

fn runtime_keys(kind: &str) -> &[&str] {
    match kind {
        "cetus_swap" => &["amount_in", "min_amount_out"],
        "haedal_stake" => &["amount"],
        "deepbook_limit_order" => &[
            "poolKey",
            "balanceManagerId",
            "tradeCapId",
            "depositCapId",
            "price",
            "quantity",
            "isBid",
            "payWithDeep",
            "clientOrderId",
            "depositSui",
        ],
        _ => &[],
    }
}

fn resolve_runtime_flow(stored: &Value, params: &Value) -> Result<FlowGraph, String> {
    let mut flow: FlowGraph = serde_json::from_value(stored.clone()).map_err(|e| e.to_string())?;
    if let Some(params) = Some(params).and_then(Value::as_object) {
        let action_count = flow
            .nodes
            .iter()
            .filter(|n| !matches!(n.kind.as_str(), "guardrail" | "ptb"))
            .count();
        for node in &mut flow.nodes {
            let overrides = params.get(&node.id).and_then(Value::as_object).or({
                if action_count == 1 && !matches!(node.kind.as_str(), "guardrail" | "ptb") {
                    Some(params)
                } else {
                    None
                }
            });
            if let Some(overrides) = overrides {
                for (key, value) in overrides {
                    if value.is_object() || !runtime_keys(&node.kind).contains(&key.as_str()) {
                        return Err(format!(
                            "Node {}: {key} is not a runtime parameter",
                            node.id
                        ));
                    }
                    if key == "min_amount_out" {
                        let published = node
                            .inputs
                            .as_ref()
                            .and_then(|v| v.get(key))
                            .or_else(|| node.config.as_ref().and_then(|v| v.get(key)));
                        if let Some(published) = published.and_then(Value::as_str) {
                            let minimum = rill_core::amounts::parse_u64_string(published)
                                .map_err(|e| e.to_string())?;
                            let actual = value.as_str().ok_or("min_amount_out must be a string")?;
                            if rill_core::amounts::parse_u64_string(actual)
                                .map_err(|e| e.to_string())?
                                < minimum
                            {
                                return Err(
                                    "min_amount_out cannot lower the published swap floor".into()
                                );
                            }
                        }
                    }
                }
                let config = node.config.get_or_insert_with(|| json!({}));
                let map = config
                    .as_object_mut()
                    .ok_or("node config must be an object")?;
                for (key, value) in overrides {
                    if !value.is_object() {
                        map.insert(key.clone(), value.clone());
                        if let Some(inputs) = node.inputs.as_mut().and_then(Value::as_object_mut) {
                            inputs.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(flow)
}

/// Compile the stored graph, rather than silently substituting a different protocol action.
pub async fn build_published(
    state: &AppState,
    skill: &PublishedSkill,
    arguments: &Value,
) -> Result<Value, String> {
    rill_mcp::assert_keyless_arguments(arguments)?;
    let flow = resolve_runtime_flow(&skill.flow, &arguments["params"])?;
    let opts =
        options(state, arguments).map_err(|_| "sender or agentWallet is invalid".to_owned())?;
    if let Some(stored) = skill.flow.get("capabilityManifest") {
        let published = serde_json::from_value(stored.clone()).map_err(|e| e.to_string())?;
        let wallet = opts
            .agent_wallet
            .as_ref()
            .ok_or("published action requires an agent wallet")?;
        crate::studio_manifest::ensure_narrower(&published, &wallet.capability_manifest)?;
    }
    let envelope =
        studio_compile::build_action(&flow, &opts, &skill.id, state.chain.as_ref(), now_ms())
            .await
            .map_err(|e| e.to_string())?;
    serde_json::to_value(envelope).map_err(|e| e.to_string())
}
pub async fn execute(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let Some(id) = body["skillId"].as_str() else {
        return invalid("skillId is required");
    };
    let Some(skill) = state.skills.get(id) else {
        return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound");
    };
    if let Some(expected) = &skill.owner {
        match owner(&state, &headers) {
            Ok(Some(actual)) if &actual == expected => {}
            Err(e) => return *e,
            _ => return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound"),
        }
    }
    match build_published(&state, &skill, &body).await {
        Ok(v) => api_ok(v),
        Err(e) => invalid(e),
    }
}
pub async fn public_mcp_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.skills.get(&id).is_none() {
        return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound");
    }
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}
pub async fn public_mcp(
    State(state): State<AppState>,
    Path(skill_id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(skill) = state.skills.get(&skill_id) else {
        return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound");
    };
    let msg: Value =
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Json(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Invalid JSON"}}),
            )
            .into_response(),
        };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    if id.is_null() {
        return StatusCode::ACCEPTED.into_response();
    }
    let result=match msg["method"].as_str(){
        Some("initialize")=>json!({"protocolVersion":rill_mcp::negotiate_protocol_version(msg["params"]["protocolVersion"].as_str()),"capabilities":{"tools":{}},"serverInfo":{"name":"rill-actions","version":env!("CARGO_PKG_VERSION")}}),
        Some("ping")=>json!({}),
        Some("tools/list")=>json!({"tools":[tool_definition(&skill)]}),
        Some("tools/call") if msg["params"]["name"]=="build_action"=>{
            let (value,failed)=match build_published(&state,&skill,&msg["params"]["arguments"]).await{Ok(v)=>(v,false),Err(e)=>(json!({"refused":true,"reason":e}),true)};
            json!({"content":[{"type":"text","text":value.to_string()}],"isError":failed})
        },
        _=>return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unknown method or tool"}})).into_response()
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    #[test]
    fn runtime_bindings_override_both_input_and_config_values() {
        let stored = json!({"nodes":[{"id":"order","type":"deepbook_limit_order","config":{"tradeCapId":"old"},"inputs":{"tradeCapId":"old"}}],"edges":[]});
        let flow = resolve_runtime_flow(&stored, &json!({"order":{"tradeCapId":"new"}})).unwrap();
        assert_eq!(flow.nodes[0].config.as_ref().unwrap()["tradeCapId"], "new");
        assert_eq!(flow.nodes[0].inputs.as_ref().unwrap()["tradeCapId"], "new");
        assert_eq!(stored["nodes"][0]["inputs"]["tradeCapId"], "old");
    }
    #[test]
    fn runtime_cannot_lower_published_swap_floor_or_replace_protocol() {
        let stored = json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{"min_amount_out":"100","integratePackageId":"0x1"}}],"edges":[]});
        assert!(resolve_runtime_flow(&stored, &json!({"swap":{"min_amount_out":"1"}})).is_err());
        assert!(
            resolve_runtime_flow(&stored, &json!({"swap":{"integratePackageId":"0x2"}})).is_err()
        );
        assert!(resolve_runtime_flow(&stored, &json!({"swap":{"min_amount_out":"101"}})).is_ok());
    }
}
