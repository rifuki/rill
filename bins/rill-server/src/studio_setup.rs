//! Two-transaction Studio onboarding: create empty, then attach rules and fund atomically.
mod defaults;
mod http;
mod preview;
mod recovery;
use crate::{
    envelope::{api_err_typed, api_ok},
    state::AppState,
    studio_api,
    studio_compile::{self, AgentWalletInput, CompileOptions, ProtectedSwapInput},
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::STANDARD, Engine};
pub use defaults::setup_defaults;
pub use http::{attach, prepare, preview_setup, recover, setup_options};
pub use recovery::recovery_plan;
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
    pub wallet_package_id: Option<String>,
    pub wallet_version_id: Option<String>,
    pub wallet_type_package: Option<String>,
    pub deepbook_type_packages: Option<[String; 3]>,
}
fn deepbook_network(network: Network) -> DeepBookNetwork {
    match network {
        Network::Mainnet => DeepBookNetwork::Mainnet,
        Network::Testnet => DeepBookNetwork::Testnet,
    }
}
fn deepbook_types(context: &SetupContext) -> Result<[String; 3], String> {
    match &context.deepbook_type_packages {
        Some(types) => Ok(types.clone()),
        None if context.network == Network::Testnet => Ok([
            deployments::TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE.into(),
            deployments::TESTNET_DEEPBOOK_TRADE_CAP_TYPE_PACKAGE.into(),
            deployments::TESTNET_DEEPBOOK_DEPOSIT_CAP_TYPE_PACKAGE.into(),
        ]),
        None => Err("mainnet DeepBook type origins must be resolved from chain".into()),
    }
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
    deployments::wallet_deployment(
        context.network,
        context.wallet_package_id.as_deref(),
        context.wallet_version_id.as_deref(),
    )?;
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
    let actions: Vec<_> = flow
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| !matches!(node.kind.as_str(), "ptb" | "guardrail"))
        .map(|(index, _)| index)
        .collect();
    if actions.len() == 1 && flow.nodes[actions[0]].kind == "cetus_swap" {
        let node = &mut flow.nodes[actions[0]];
        let requested = node
            .inputs
            .as_ref()
            .and_then(|v| v.get("amount_in"))
            .or_else(|| node.config.as_ref().and_then(|v| v.get("amount_in")))
            .and_then(Value::as_str)
            .ok_or("swap amount_in must be an exact base-unit string")?;
        let default = parse_u64_string(requested)
            .map_err(err)?
            .min(per_tx)
            .min(budget - reserve);
        // Runtime amount is owner-bounded; the immutable publication and output floor stay intact.
        set_node_value(node, "amount_in", json!(default.to_string()))?;
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
/// [`shared`], for a wallet the owner created moments ago.
///
/// The owner's wallet submits through its own fullnode, and ours may not have seen the result yet:
/// on testnet the very next request answered "not found on chain" for a wallet whose creation had
/// already succeeded. Only absence is waited out, and only briefly; every other error, and an id
/// that never appears, is still a refusal.
async fn shared_once_visible(
    chain: &impl SuiRead,
    objects: &mut SharedObjects,
    id: Address,
) -> Result<ObjectSummary, String> {
    for _ in 0..FRESH_OBJECT_ATTEMPTS {
        match chain.get_object(&id.to_string()).await {
            Err(rill_chain::ChainError::NotFound(_)) => {
                tokio::time::sleep(FRESH_OBJECT_PAUSE).await;
            }
            _ => return shared(chain, objects, id).await,
        }
    }
    shared(chain, objects, id).await
}
const FRESH_OBJECT_ATTEMPTS: usize = 20;
const FRESH_OBJECT_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);
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
    let swap_preview = preview::plan(body, skill, owner, context, chain).await?;
    preview::ensure_floor(&swap_preview)?;
    let grant = grant(body, skill, owner, context)?;
    let (package, version) = deployments::wallet_deployment(
        context.network,
        context.wallet_package_id.as_deref(),
        context.wallet_version_id.as_deref(),
    )?;
    let deepbook = address(deepbook_network(context.network).package_id())?;
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
            address(&deepbook_types(context)?[0])?,
            grant.agent,
        )
        .map_err(err)?;
    }
    let protected = std::env::var("RILL_CETUS_ADAPTER_PACKAGE_ID").ok()
        .filter(|id| !id.trim().is_empty())
        .filter(|_| {
            let actions:Vec<_>=grant.flow.nodes.iter().filter(|n| !matches!(n.kind.as_str(),"ptb"|"guardrail")).collect();
            actions.len()==1 && actions[0].kind=="cetus_swap"
        }).map(|adapter|json!({"adapterPackageId":adapter,"revision":1,"owner":grant.owner.to_string()}));
    Ok(
        json!({"setupPtb":encode_kind(&finish(tx)?)?,"runSetTemplate":{},"requiresTradeCap":needs_manager,"walletPackageId":package.to_string(),"deepbookPackageId":deepbook.to_string(),"versionId":version.to_string(),"capabilityManifest":grant.manifest,"budgetMist":grant.budget.to_string(),"owner":grant.owner.to_string(),"agent":grant.agent.to_string(),"ownerIsAgent":grant.owner==grant.agent,"protection":protected,"swapPreview":swap_preview}),
    )
}
fn chain_amount(fields: &Value, key: &str) -> Result<u64, String> {
    match &fields[key] {
        Value::String(s) => parse_u64_string(s).map_err(err),
        Value::Number(n) => n.as_u64().ok_or_else(|| format!("invalid wallet {key}")),
        _ => Err(format!("wallet {key} is missing")),
    }
}
/// `fresh`: the capability was minted moments ago, by a transaction this node's ownership index
/// may not have caught up with, so its absence from the agent's list is waited out before it is a
/// refusal. The end-to-end run on testnet met exactly this one request after the wallet itself.
async fn check_owned_cap(
    chain: &impl SuiRead,
    agent: Address,
    id: Address,
    expected_type: &str,
    fresh: bool,
) -> Result<ObjectSummary, String> {
    let attempts = if fresh { FRESH_OBJECT_ATTEMPTS } else { 1 };
    let mut owned_by_agent = false;
    for attempt in 0..attempts {
        let owned = chain
            .list_owned_objects(&agent.to_string())
            .await
            .map_err(err)?;
        if owned
            .iter()
            .any(|o| address(&o.reference.id).ok() == Some(id))
        {
            owned_by_agent = true;
            break;
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(FRESH_OBJECT_PAUSE).await;
        }
    }
    if !owned_by_agent {
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

/// What onboarding and an action grant both derive from a wallet and a published action: the
/// checked wallet, the run set the signer will pin, and the arguments it builds with.
///
/// One derivation for both, so a run set an owner approves in a grant is exactly the run set
/// onboarding would have exported for the same wallet and action.
struct Binding {
    grant: Grant,
    package: Address,
    version: Address,
    wallet_id: Address,
    objects: SharedObjects,
    run_set: Value,
    build_arguments: Value,
    protection: Option<ProtectedSwapInput>,
}

/// `fresh` is onboarding: the wallet must still be empty with no rules, because the next step funds
/// it. Otherwise the wallet must already carry rules, because a grant runs inside them.
async fn bind(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
    fresh: bool,
) -> Result<Binding, String> {
    let mut grant = grant(body, skill, owner, context)?;
    let (package, version) = deployments::wallet_deployment(
        context.network,
        context.wallet_package_id.as_deref(),
        context.wallet_version_id.as_deref(),
    )?;
    let wallet_id = address(required(body, "walletId")?)?;
    let cap_id = address(required(body, "agentCapId")?)?;
    let mut objects = SharedObjects::new();
    let wallet = if fresh {
        shared_once_visible(chain, &mut objects, wallet_id).await?
    } else {
        shared(chain, &mut objects, wallet_id).await?
    };
    let expected = canonical_type(&format!(
        "{}::agent_wallet::AgentWallet<{}>",
        context
            .wallet_type_package
            .as_deref()
            .unwrap_or(&package.to_string()),
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
    let rules = &fields["policy"]["rules"];
    let rules = rules
        .as_array()
        .or_else(|| rules["contents"].as_array())
        .ok_or("wallet policy rules are unavailable")?;
    if fresh {
        if chain_amount(fields, "budget")? != 0 || chain_amount(fields, "spent")? != 0 {
            return Err(
                "wallet is already funded or has spent funds; refusing duplicate funding".into(),
            );
        }
        if !rules.is_empty() {
            return Err("wallet already has attached rules".into());
        }
    } else if rules.is_empty() {
        return Err(
            "wallet has no rules attached yet; finish onboarding before granting an action".into(),
        );
    }
    let cap = check_owned_cap(
        chain,
        grant.agent,
        cap_id,
        &format!(
            "{}::agent_wallet::AgentCap",
            context
                .wallet_type_package
                .as_deref()
                .unwrap_or(&package.to_string())
        ),
        fresh,
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
        let origins = deepbook_types(context)?;
        let deepbook = &origins[0];
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
            ("tradeCapId", "TradeCap", origins[1].as_str()),
            ("depositCapId", "DepositCap", origins[2].as_str()),
        ] {
            let id = address(required(body, key)?)?;
            let cap = check_owned_cap(
                chain,
                grant.agent,
                id,
                &format!("{defining_package}::balance_manager::{kind}"),
                fresh,
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
    let protection = if let Some(adapter) = std::env::var("RILL_CETUS_ADAPTER_PACKAGE_ID")
        .ok()
        .filter(|a| !a.trim().is_empty())
    {
        let actions: Vec<_> = grant
            .flow
            .nodes
            .iter()
            .filter(|n| !matches!(n.kind.as_str(), "ptb" | "guardrail"))
            .collect();
        if actions.len() == 1 && actions[0].kind == "cetus_swap" {
            let revision = if fresh {
                1
            } else {
                let mut read = rill_ptb::policy_read::policy_rules_transaction(
                    package,
                    wallet_id,
                    &grant.manifest.wallet_coin_type,
                    &objects,
                    chain.reference_gas_price().await.map_err(err)?,
                )
                .map_err(err)?;
                if let sui_sdk_types::TransactionKind::ProgrammableTransaction(ptb) = &mut read.kind
                {
                    if let sui_sdk_types::Command::MoveCall(call) = &mut ptb.commands[0] {
                        call.function = Identifier::new("protected_revision").map_err(err)?;
                    }
                }
                let bytes = chain
                    .simulate_read(&STANDARD.encode(bcs::to_bytes(&read).map_err(err)?))
                    .await
                    .map_err(err)?;
                bcs::from_bytes::<u64>(
                    bytes
                        .command_returns
                        .iter()
                        .flatten()
                        .next()
                        .ok_or("protected revision read returned no value")?,
                )
                .map_err(err)?
            };
            if revision > 0 {
                Some(ProtectedSwapInput {
                    adapter_package_id: address(&adapter)?.to_string(),
                    revision,
                    owner: grant.owner.to_string(),
                })
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    let compiled = studio_compile::compile(
        &grant.flow,
        &CompileOptions {
            sender: Some(grant.agent),
            agent_wallet: Some(AgentWalletInput {
                protected_swap: protection.clone(),
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
    let runset = json!({"label":skill.name,"network":context.network,"sender":grant.agent.to_string(),"actionId":skill.id,"walletPackageId":package.to_string(),"walletId":wallet_id.to_string(),"agentCapId":cap_id.to_string(),"versionId":version.to_string(),"capabilityManifest":grant.manifest,"allowedTargets":decoded.targets,"allowedObjectIds":decoded.object_inputs,"maxAmountBaseUnits":grant.per_tx.to_string(),"declaredSpendBaseUnits":compiled.root_spend_mist.to_string(),"minimumRemainingBaseUnits":grant.reserve.to_string(),"gasCeilingBaseUnits":compiled.transaction.gas_payment.budget.to_string()});
    let params: serde_json::Map<String, Value> = grant
        .flow
        .nodes
        .iter()
        .filter(|node| matches!(node.kind.as_str(), "deepbook_limit_order" | "cetus_swap"))
        .map(|node| {
            let mut values = serde_json::Map::new();
            for key in [
                "balanceManagerId",
                "tradeCapId",
                "depositCapId",
                "price",
                "amount_in",
            ] {
                if let Some(value) = node
                    .inputs
                    .as_ref()
                    .and_then(|inputs| inputs.get(key))
                    .or_else(|| node.config.as_ref().and_then(|config| config.get(key)))
                {
                    values.insert(key.into(), value.clone());
                }
            }
            (node.id.clone(), Value::Object(values))
        })
        .collect();
    let mut build_arguments = json!({
        "actionId": skill.id,
        "sender": grant.agent.to_string(),
        "agentWallet": {
            "packageId": package.to_string(), "walletId": wallet_id.to_string(),
            "capId": cap_id.to_string(), "versionId": version.to_string(),
            "coinType": grant.manifest.wallet_coin_type, "capabilityManifest": grant.manifest
        },
        "params": params
    });
    if let Some(p) = &protection {
        build_arguments["agentWallet"]["protectedSwap"] = json!(p);
    }
    Ok(Binding {
        grant,
        package,
        version,
        wallet_id,
        objects,
        run_set: runset,
        build_arguments,
        protection,
    })
}

pub async fn attach_plan(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
) -> Result<Value, String> {
    let swap_preview = preview::plan(body, skill, owner, context, chain).await?;
    preview::ensure_floor(&swap_preview)?;
    let Binding {
        grant,
        package,
        version,
        wallet_id,
        objects,
        run_set,
        build_arguments,
        protection,
    } = bind(body, skill, owner, context, chain, true).await?;
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
    if let Some(protection) = &protection {
        let node = grant
            .flow
            .nodes
            .iter()
            .find(|n| n.kind == "cetus_swap")
            .ok_or("protected swap node missing")?;
        let config = node
            .inputs
            .as_ref()
            .or(node.config.as_ref())
            .ok_or("swap config missing")?;
        let pool_id = address(required(config, "pool")?)?;
        let pool = chain.get_object(&pool_id.to_string()).await.map_err(err)?;
        let (a, b) = rill_ptb::cetus::pool_coin_types(
            pool.object_type.as_deref().ok_or("pool type missing")?,
        )
        .ok_or("invalid pool type")?;
        let a2b = canonical_type(&a)? == canonical_type(&grant.manifest.wallet_coin_type)?;
        let swap = rill_ptb::protected::ProtectedSwap {
            adapter_package: address(&protection.adapter_package_id)?,
            pool_id,
            config_id: address(deployments::MAINNET_CETUS_GLOBAL_CONFIG)?,
            coin_type_a: a,
            coin_type_b: b,
            a2b,
            revision: protection.revision,
            min_output: studio_compile::effective_floor(&grant.flow, node).map_err(err)?,
            sqrt_price_limit: 0,
        };
        rill_ptb::protected::configure(&mut tx, &swap, wallet_id, version, &objects)?;
    }
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
    Ok(
        json!({"attachPtb":encode_kind(&finish(tx)?)?,"runSet":run_set,"buildArguments":build_arguments,"protection":protection}),
    )
}

/// The unsigned grant for running `skill` from an already bounded wallet: what the owner is asked
/// to sign. Its revision is left at zero for the caller, which knows what was granted before.
pub async fn grant_plan(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
) -> Result<rill_core::grant::Grant, String> {
    let binding = bind(body, skill, owner, context, chain, false).await?;
    let network = serde_json::to_value(context.network)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or("network is not representable")?;
    Ok(rill_core::grant::Grant {
        network,
        action_id: skill.id.clone(),
        action_name: skill.name.clone(),
        agent: binding.grant.agent.to_string(),
        wallet_id: binding.wallet_id.to_string(),
        wallet_package_id: binding.package.to_string(),
        expires_at_ms: binding.grant.expiry.to_string(),
        revision: 0,
        run_set: binding.run_set,
        build_arguments: binding.build_arguments,
    })
}

/// The deployment a setup or grant for `skill` runs against: network, guard, wallet ids, and the
/// type origins an upgraded package reports, read from the chain rather than assumed.
pub(crate) async fn setup_context(
    state: &AppState,
    skill: &PublishedSkill,
) -> Result<SetupContext, Box<Response>> {
    let options = match studio_api::options(state, &json!({})) {
        Ok(v) => v,
        Err(e) => return Err(e),
    };
    let mut context = SetupContext {
        network: options.network,
        guard_package: options.guard_package,
        now_ms: studio_api::now_ms(),
        wallet_package_id: state.config.wallet_package_id.clone(),
        wallet_version_id: state.config.wallet_version_id.clone(),
        wallet_type_package: None,
        deepbook_type_packages: None,
    };
    let (package, _) = match deployments::wallet_deployment(
        context.network,
        context.wallet_package_id.as_deref(),
        context.wallet_version_id.as_deref(),
    ) {
        Ok(ids) => ids,
        Err(e) => return Err(Box::new(studio_api::invalid(e))),
    };
    if context.network == Network::Mainnet || context.wallet_package_id.is_some() {
        context.wallet_type_package = match rill_chain::describe::datatype_origin(
            &state.config.sui_rpc_url,
            &package.to_string(),
            "agent_wallet",
            "AgentWallet",
        )
        .await
        {
            Ok(origin) => Some(origin),
            Err(e) => {
                return Err(Box::new(api_err_typed(
                    StatusCode::BAD_GATEWAY,
                    e.to_string(),
                    "ChainError",
                )))
            }
        };
    }
    if skill.flow["nodes"]
        .as_array()
        .is_some_and(|nodes| nodes.iter().any(|n| n["type"] == "deepbook_limit_order"))
    {
        let package = deepbook_network(context.network).package_id();
        let mut origins = Vec::new();
        for kind in ["BalanceManager", "TradeCap", "DepositCap"] {
            match rill_chain::describe::datatype_origin(
                &state.config.sui_rpc_url,
                package,
                "balance_manager",
                kind,
            )
            .await
            {
                Ok(origin) => origins.push(origin),
                Err(e) => {
                    return Err(Box::new(api_err_typed(
                        StatusCode::BAD_GATEWAY,
                        e.to_string(),
                        "ChainError",
                    )))
                }
            }
        }
        context.deepbook_type_packages =
            Some([origins[0].clone(), origins[1].clone(), origins[2].clone()]);
    }
    Ok(context)
}
