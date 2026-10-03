//! Two-transaction Studio onboarding: create empty, then attach rules and fund atomically.
use crate::{
    envelope::{api_err_typed, api_ok},
    state::AppState,
    studio_api,
    studio_compile::{self, AgentWalletInput, CompileOptions},
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rill_chain::{ObjectSummary, SuiRead};
use rill_core::{
    amounts::parse_u64_string,
    envelope::Network,
    flow::{topological_sort, FlowGraph},
    manifest::{CapabilityManifest, CapabilityRule},
};
use rill_ptb::{
    balance_manager::build_provision_manager_with_type_package,
    create::{build_create_wallet, NewWallet},
    deployments,
    lifecycle::build_top_up,
    registry::DeepBookNetwork,
    rules::{build_attach_rules, RuleTarget},
    shared::SharedObjects,
};
use rill_store::{PublishedSkill, SkillStore};
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest, Identifier, Transaction, TypeTag};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

pub struct SetupContext {
    pub network: Network,
    pub guard_package: Option<Address>,
    pub now_ms: u64,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn address(s: &str) -> Result<Address, String> {
    s.parse().map_err(|_| format!("invalid Sui address: {s}"))
}
fn required<'a>(body: &'a Value, key: &str) -> Result<&'a str, String> {
    body[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} must be a nonempty string"))
}
fn amount(body: &Value, key: &str) -> Result<u64, String> {
    parse_u64_string(required(body, key)?).map_err(|e| format!("{key}: {e}"))
}
fn optional_amount(body: &Value, key: &str, default: u64) -> Result<u64, String> {
    if body.get(key).is_some() {
        amount(body, key)
    } else {
        Ok(default)
    }
}
fn canonical_type(s: &str) -> Result<String, String> {
    s.parse::<TypeTag>().map(|t| t.to_string()).map_err(err)
}

struct Grant {
    owner: Address,
    agent: Address,
    budget: u64,
    per_tx: u64,
    reserve: u64,
    expiry: u64,
    manifest: CapabilityManifest,
    flow: FlowGraph,
}
fn grant(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
) -> Result<Grant, String> {
    rill_mcp::assert_keyless_arguments(body)?;
    let sender = address(required(body, "sender")?)?;
    if sender != address(owner)? {
        return Err("sender must match the authenticated owner".into());
    }
    if skill.owner.as_deref().map(address).transpose()? != Some(sender) {
        return Err("skill must belong to the authenticated owner".into());
    }
    if required(body, "skillId")? != skill.id {
        return Err("skillId does not match the published skill".into());
    }
    if context.network != Network::Testnet {
        return Err("Studio setup requires a configured wallet deployment; no mainnet deployment is configured".into());
    }
    let agent = body
        .get("agent")
        .map(|_| required(body, "agent").and_then(address))
        .transpose()?
        .unwrap_or(sender);
    let budget = amount(body, "budgetMist")?;
    let per_tx = amount(body, "perTxMist")?;
    if budget == 0 || per_tx == 0 || per_tx > budget {
        return Err(
            "budgetMist and perTxMist must be positive, with perTxMist <= budgetMist".into(),
        );
    }
    let reserve = optional_amount(body, "minimumRemainingMist", 0)?;
    if reserve >= budget {
        return Err("minimumRemainingMist must be below budgetMist".into());
    }
    let default_expiry = context
        .now_ms
        .checked_add(30 * 86_400_000)
        .ok_or("expiry overflow")?;
    let expiry = optional_amount(body, "expiresAtMs", default_expiry)?;
    if expiry <= context.now_ms {
        return Err("expiresAtMs must be in the future".into());
    }
    let mut manifest = match skill.flow.get("capabilityManifest") {
        Some(value) => {
            let manifest: CapabilityManifest =
                serde_json::from_value(value.clone()).map_err(err)?;
            manifest.validate().map_err(err)?;
            manifest
        }
        None => CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };
    if canonical_type(&manifest.wallet_coin_type)? != canonical_type("0x2::sui::SUI")? {
        return Err("Studio setup requires an AgentWallet<SUI>".into());
    }
    let mut has_budget = false;
    let mut has_per_tx = false;
    for rule in &mut manifest.rules {
        match rule {
            CapabilityRule::Budget { total_mist } => {
                if budget > parse_u64_string(total_mist).map_err(err)? {
                    return Err("budgetMist exceeds the published budget limit".into());
                }
                *total_mist = budget.to_string();
                has_budget = true;
            }
            CapabilityRule::PerTx { max_mist } => {
                if per_tx > parse_u64_string(max_mist).map_err(err)? {
                    return Err("perTxMist exceeds the published per_tx limit".into());
                }
                *max_mist = per_tx.to_string();
                has_per_tx = true;
            }
            _ => {}
        }
    }
    if !has_budget {
        manifest.rules.push(CapabilityRule::Budget {
            total_mist: budget.to_string(),
        });
    }
    if !has_per_tx {
        manifest.rules.push(CapabilityRule::PerTx {
            max_mist: per_tx.to_string(),
        });
    }
    manifest.validate().map_err(err)?;
    let mut flow: FlowGraph = serde_json::from_value(skill.flow.clone()).map_err(err)?;
    if flow.nodes.is_empty() {
        return Err("published flow is empty".into());
    }
    topological_sort(&flow).map_err(err)?;
    for node in &mut flow.nodes {
        if !matches!(
            node.kind.as_str(),
            "cetus_swap" | "haedal_stake" | "deepbook_limit_order" | "guardrail" | "ptb"
        ) {
            return Err(format!("unsupported node type: {}", node.kind));
        }
        if node.kind == "deepbook_limit_order" {
            if let Some(price) = body.get("price") {
                let price = price
                    .as_str()
                    .ok_or("price must be an exact decimal string")?;
                let mut parts = price.split('.');
                let integer = parts.next().unwrap_or_default();
                let fraction = parts.next();
                if integer.is_empty()
                    || !integer.bytes().all(|c| c.is_ascii_digit())
                    || fraction
                        .is_some_and(|s| s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()))
                    || parts.next().is_some()
                    || !price.bytes().any(|c| matches!(c, b'1'..=b'9'))
                {
                    return Err("price must be a positive plain decimal string".into());
                }
                set_node_value(node, "price", json!(price))?;
            }
        }
    }
    Ok(Grant {
        owner: sender,
        agent,
        budget,
        per_tx,
        reserve,
        expiry,
        manifest,
        flow,
    })
}
fn set_node_value(
    node: &mut rill_core::flow::FlowNode,
    key: &str,
    value: Value,
) -> Result<(), String> {
    // Compiler inputs take precedence over config, so update both when an input exists.
    if let Some(inputs) = node.inputs.as_mut() {
        inputs
            .as_object_mut()
            .ok_or("node inputs must be an object")?
            .insert(key.into(), value.clone());
    }
    node.config
        .get_or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("node config must be an object")?
        .insert(key.into(), value);
    Ok(())
}
async fn shared(
    chain: &impl SuiRead,
    objects: &mut SharedObjects,
    id: Address,
) -> Result<ObjectSummary, String> {
    let object = chain.get_object(&id.to_string()).await.map_err(err)?;
    objects.insert(
        id,
        object
            .shared_initial_version
            .ok_or_else(|| format!("{id} must be shared"))?,
    );
    Ok(object)
}
async fn builder(owner: Address, chain: &impl SuiRead) -> Result<TransactionBuilder, String> {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(owner);
    tx.set_gas_budget(100_000_000);
    tx.set_gas_price(chain.reference_gas_price().await.map_err(err)?);
    Ok(tx)
}
fn finish(mut tx: TransactionBuilder) -> Result<Transaction, String> {
    // The placeholder satisfies builder validation only; a wallet selects actual gas for the kind.
    tx.add_gas_objects([ObjectInput::owned(address("0x1")?, 1, Digest::ZERO)]);
    let mut tx = tx.try_build().map_err(err)?;
    tx.gas_payment.objects.clear();
    Ok(tx)
}
fn encode_kind(tx: &Transaction) -> Result<String, String> {
    Ok(STANDARD.encode(bcs::to_bytes(&tx.kind).map_err(err)?))
}

pub async fn prepare_plan(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
) -> Result<Value, String> {
    let grant = grant(body, skill, owner, context)?;
    let package = address(deployments::TESTNET_AGENT_WALLET)?;
    let version = address(deployments::TESTNET_AGENT_WALLET_VERSION)?;
    let deepbook = address(DeepBookNetwork::Testnet.package_id())?;
    let mut objects = SharedObjects::new();
    shared(chain, &mut objects, version).await?;
    let mut tx = builder(grant.owner, chain).await?;
    let zero = tx.move_call(
        Function::new(
            address("0x2")?,
            Identifier::new("coin").map_err(err)?,
            Identifier::new("zero").map_err(err)?,
        )
        .with_type_args(vec![grant
            .manifest
            .wallet_coin_type
            .parse()
            .map_err(err)?]),
        vec![],
    );
    build_create_wallet(
        &mut tx,
        &NewWallet {
            package_id: package,
            version_id: version,
            agent: grant.agent,
            expires_at_ms: grant.expiry,
            coin_type: grant.manifest.wallet_coin_type.clone(),
            manifest: grant.manifest.clone(),
        },
        zero,
        &objects,
        context.now_ms,
    )
    .map_err(err)?;
    let needs_manager = grant
        .flow
        .nodes
        .iter()
        .any(|n| n.kind == "deepbook_limit_order");
    if needs_manager {
        build_provision_manager_with_type_package(
            &mut tx,
            deepbook,
            address(deployments::TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE)?,
            grant.agent,
        )
        .map_err(err)?;
    }
    Ok(
        json!({"setupPtb":encode_kind(&finish(tx)?)?,"runSetTemplate":{},"requiresTradeCap":needs_manager,"walletPackageId":package.to_string(),"deepbookPackageId":deepbook.to_string(),"versionId":version.to_string(),"capabilityManifest":grant.manifest,"budgetMist":grant.budget.to_string(),"owner":grant.owner.to_string(),"agent":grant.agent.to_string(),"ownerIsAgent":grant.owner==grant.agent}),
    )
}
fn chain_amount(fields: &Value, key: &str) -> Result<u64, String> {
    match &fields[key] {
        Value::String(s) => parse_u64_string(s).map_err(err),
        Value::Number(n) => n.as_u64().ok_or_else(|| format!("invalid wallet {key}")),
        _ => Err(format!("wallet {key} is missing")),
    }
}
async fn check_owned_cap(
    chain: &impl SuiRead,
    agent: Address,
    id: Address,
    expected_type: &str,
) -> Result<ObjectSummary, String> {
    let owned = chain
        .list_owned_objects(&agent.to_string())
        .await
        .map_err(err)?;
    if !owned
        .iter()
        .any(|o| address(&o.reference.id).ok() == Some(id))
    {
        return Err(format!("agent does not own capability {id}"));
    }
    let cap = chain.get_object(&id.to_string()).await.map_err(err)?;
    if cap.shared_initial_version.is_some()
        || cap.object_type.as_deref().map(canonical_type).transpose()?
            != Some(canonical_type(expected_type)?)
    {
        return Err(format!("invalid capability type for {id}"));
    }
    Ok(cap)
}

pub async fn attach_plan(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
) -> Result<Value, String> {
    let mut grant = grant(body, skill, owner, context)?;
    let package = address(deployments::TESTNET_AGENT_WALLET)?;
    let version = address(deployments::TESTNET_AGENT_WALLET_VERSION)?;
    let wallet_id = address(required(body, "walletId")?)?;
    let cap_id = address(required(body, "agentCapId")?)?;
    let mut objects = SharedObjects::new();
    let wallet = shared(chain, &mut objects, wallet_id).await?;
    let expected = canonical_type(&format!(
        "{package}::agent_wallet::AgentWallet<{}>",
        grant.manifest.wallet_coin_type
    ))?;
    if wallet
        .object_type
        .as_deref()
        .map(canonical_type)
        .transpose()?
        != Some(expected)
    {
        return Err("wallet type does not match the configured deployment".into());
    }
    let fields = wallet
        .fields
        .as_ref()
        .ok_or("wallet fields are unavailable")?;
    if address(required(fields, "owner")?)? != grant.owner {
        return Err("wallet owner does not match sender".into());
    }
    if address(required(fields, "agent")?)? != grant.agent {
        return Err("wallet agent does not match requested agent".into());
    }
    if address(required(fields, "cap_id")?)? != cap_id {
        return Err("wallet active cap does not match agentCapId".into());
    }
    if fields["revoked"].as_bool() != Some(false) {
        return Err("wallet is revoked or its revoked status is unavailable".into());
    }
    if chain_amount(fields, "expires_at_ms")? <= context.now_ms {
        return Err("wallet has expired".into());
    }
    if chain_amount(fields, "budget")? != 0 || chain_amount(fields, "spent")? != 0 {
        return Err(
            "wallet is already funded or has spent funds; refusing duplicate funding".into(),
        );
    }
    let rules = &fields["policy"]["rules"];
    let rules = rules
        .as_array()
        .or_else(|| rules["contents"].as_array())
        .ok_or("wallet policy rules are unavailable")?;
    if !rules.is_empty() {
        return Err("wallet already has attached rules".into());
    }
    let cap = check_owned_cap(
        chain,
        grant.agent,
        cap_id,
        &format!("{package}::agent_wallet::AgentCap"),
    )
    .await?;
    if cap
        .fields
        .as_ref()
        .and_then(|v| v["wallet"].as_str())
        .map(address)
        .transpose()?
        != Some(wallet_id)
    {
        return Err("agent capability belongs to another wallet".into());
    }
    shared(chain, &mut objects, version).await?;
    let needs_manager = grant
        .flow
        .nodes
        .iter()
        .any(|n| n.kind == "deepbook_limit_order");
    if needs_manager {
        let manager = address(required(body, "balanceManagerId")?)?;
        let deepbook = deployments::TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE;
        let manager_object = shared(chain, &mut objects, manager).await?;
        if manager_object
            .object_type
            .as_deref()
            .map(canonical_type)
            .transpose()?
            != Some(canonical_type(&format!(
                "{deepbook}::balance_manager::BalanceManager"
            ))?)
        {
            return Err("balance manager type does not match the configured deployment".into());
        }
        if manager_object
            .fields
            .as_ref()
            .and_then(|v| v["owner"].as_str())
            .map(address)
            .transpose()?
            != Some(grant.owner)
        {
            return Err("balance manager owner does not match sender".into());
        }
        for (key, kind, defining_package) in [
            (
                "tradeCapId",
                "TradeCap",
                deployments::TESTNET_DEEPBOOK_TRADE_CAP_TYPE_PACKAGE,
            ),
            (
                "depositCapId",
                "DepositCap",
                deployments::TESTNET_DEEPBOOK_DEPOSIT_CAP_TYPE_PACKAGE,
            ),
        ] {
            let id = address(required(body, key)?)?;
            let cap = check_owned_cap(
                chain,
                grant.agent,
                id,
                &format!("{defining_package}::balance_manager::{kind}"),
            )
            .await?;
            if cap
                .fields
                .as_ref()
                .and_then(|v| v["balance_manager_id"].as_str())
                .map(address)
                .transpose()?
                != Some(manager)
            {
                return Err(format!("{key} belongs to another balance manager"));
            }
        }
        for node in grant
            .flow
            .nodes
            .iter_mut()
            .filter(|n| n.kind == "deepbook_limit_order")
        {
            for key in ["balanceManagerId", "tradeCapId", "depositCapId"] {
                set_node_value(node, key, json!(address(required(body, key)?)?.to_string()))?;
            }
        }
    }
    let compiled = studio_compile::compile(
        &grant.flow,
        &CompileOptions {
            sender: Some(grant.agent),
            agent_wallet: Some(AgentWalletInput {
                package_id: package.to_string(),
                wallet_id: wallet_id.to_string(),
                cap_id: cap_id.to_string(),
                coin_type: grant.manifest.wallet_coin_type.clone(),
                capability_manifest: grant.manifest.clone(),
                version_id: version.to_string(),
            }),
            network: context.network,
            guard_package: context.guard_package,
        },
        chain,
    )
    .await
    .map_err(err)?;
    if compiled.root_spend_mist == 0 {
        return Err("published flow must spend from the agent wallet".into());
    }
    if compiled.root_spend_mist > grant.budget.saturating_sub(grant.reserve) {
        return Err("flow spending would violate minimumRemainingMist".into());
    }
    let decoded = rill_policy::decode::decode(
        &STANDARD.encode(bcs::to_bytes(&compiled.transaction).map_err(err)?),
    )
    .map_err(err)?;
    let mut tx = builder(grant.owner, chain).await?;
    build_attach_rules(
        &mut tx,
        &RuleTarget {
            package_id: package,
            wallet_id,
            version_id: version,
            coin_type: grant.manifest.wallet_coin_type.clone(),
            manifest: grant.manifest.clone(),
        },
        &objects,
    )
    .map_err(err)?;
    let amount = tx.pure(&grant.budget);
    let gas = tx.gas();
    let funds = tx
        .split_coins(gas, vec![amount])
        .into_iter()
        .next()
        .ok_or("funding coin was not produced")?;
    build_top_up(
        &mut tx,
        package,
        wallet_id,
        &grant.manifest.wallet_coin_type,
        funds,
        &objects,
    )
    .map_err(err)?;
    let runset = json!({"label":skill.name,"network":context.network,"sender":grant.agent.to_string(),"actionId":skill.id,"walletPackageId":package.to_string(),"walletId":wallet_id.to_string(),"agentCapId":cap_id.to_string(),"versionId":version.to_string(),"capabilityManifest":grant.manifest,"allowedTargets":decoded.targets,"allowedObjectIds":decoded.object_inputs,"maxAmountBaseUnits":grant.per_tx.to_string(),"declaredSpendBaseUnits":compiled.root_spend_mist.to_string(),"minimumRemainingBaseUnits":grant.reserve.to_string(),"gasCeilingBaseUnits":compiled.transaction.gas_payment.budget.to_string()});
    let params: serde_json::Map<String, Value> = grant
        .flow
        .nodes
        .iter()
        .filter(|node| node.kind == "deepbook_limit_order")
        .map(|node| {
            let mut values = serde_json::Map::new();
            for key in ["balanceManagerId", "tradeCapId", "depositCapId", "price"] {
                if let Some(value) = node.config.as_ref().and_then(|config| config.get(key)) {
                    values.insert(key.into(), value.clone());
                }
            }
            (node.id.clone(), Value::Object(values))
        })
        .collect();
    let build_arguments = json!({
        "actionId": skill.id,
        "sender": grant.agent.to_string(),
        "agentWallet": {
            "packageId": package.to_string(), "walletId": wallet_id.to_string(),
            "capId": cap_id.to_string(), "versionId": version.to_string(),
            "coinType": grant.manifest.wallet_coin_type, "capabilityManifest": grant.manifest
        },
        "params": params
    });
    Ok(
        json!({"attachPtb":encode_kind(&finish(tx)?)?,"runSet":runset,"buildArguments":build_arguments}),
    )
}

pub async fn prepare(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle(state, headers, body, false).await
}
pub async fn attach(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle(state, headers, body, true).await
}
async fn handle(state: AppState, headers: HeaderMap, body: Bytes, attach: bool) -> Response {
    let body = match studio_api::parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let owner = match studio_api::owner(&state, &headers) {
        Ok(Some(o)) => o,
        Ok(None) => {
            return api_err_typed(
                StatusCode::UNAUTHORIZED,
                "Sign in with the wallet that owns this skill",
                "Unauthorized",
            )
        }
        Err(e) => return *e,
    };
    let Some(skill) = body["skillId"].as_str().and_then(|id| state.skills.get(id)) else {
        return api_err_typed(StatusCode::NOT_FOUND, "Skill not found", "NotFound");
    };
    let options = match studio_api::options(&state, &json!({})) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let context = SetupContext {
        network: options.network,
        guard_package: options.guard_package,
        now_ms: studio_api::now_ms(),
    };
    let result = if attach {
        attach_plan(&body, &skill, &owner, &context, state.chain.as_ref()).await
    } else {
        prepare_plan(&body, &skill, &owner, &context, state.chain.as_ref()).await
    };
    match result {
        Ok(value) => api_ok(value),
        Err(e) => studio_api::invalid(e),
    }
}
