//! Compile Studio graphs into unsigned transaction kinds.
mod protected;
use protected::compile_protected;
use rill_chain::SuiRead;
use rill_core::{
    envelope::Network,
    flow::FlowGraph,
    manifest::{format_amount, CapabilityManifest},
};
use serde::Deserialize;
use sui_sdk_types::{Address, Transaction};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentWalletInput {
    pub package_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    #[serde(default = "sui_coin_type")]
    pub coin_type: String,
    pub capability_manifest: CapabilityManifest,
    pub version_id: String,
    #[serde(default)]
    pub protected_swap: Option<ProtectedSwapInput>,
}
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtectedSwapInput {
    pub adapter_package_id: String,
    pub revision: u64,
    pub owner: String,
}

fn sui_coin_type() -> String {
    "0x2::sui::SUI".into()
}

pub struct CompileOptions {
    pub sender: Option<Address>,
    pub agent_wallet: Option<AgentWalletInput>,
    pub network: Network,
    pub guard_package: Option<Address>,
}
#[derive(Debug)]
pub struct CompiledFlow {
    pub transaction: Transaction,
    pub unsigned_ptb: String,
    pub preview: String,
    pub warnings: Vec<String>,
    pub root_spend_mist: u64,
}
#[derive(Debug)]
pub struct CompileError(pub String);
impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for CompileError {}

use base64::{engine::general_purpose::STANDARD, Engine};
use rill_core::{
    amounts::{decimal_to_base_units, parse_u64_string},
    flow::{topological_sort, FlowNode},
    manifest::CapabilityRule,
};
use rill_ptb::{
    cetus, deepbook, deployments, guard, haedal,
    registry::{self, DeepBookNetwork},
    shared::SharedObjects,
    spend::{build_manifest_gated_spend, WalletBinding},
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use sui_sdk_types::{Command, Digest, TransactionKind, TypeTag};
use sui_transaction_builder::{Argument, ObjectInput, TransactionBuilder};

fn error(message: impl std::fmt::Display) -> CompileError {
    CompileError(message.to_string())
}
fn address(value: &str) -> Result<Address, CompileError> {
    value
        .parse()
        .map_err(|_| error(format!("invalid address: {value}")))
}
fn coin_type(value: &str) -> Result<String, CompileError> {
    value
        .parse::<TypeTag>()
        .map(|tag| tag.to_string())
        .map_err(|_| error(format!("invalid coin type: {value}")))
}
fn value<'a>(node: &'a FlowNode, key: &str) -> Option<&'a Value> {
    node.inputs
        .as_ref()
        .and_then(|v| v.get(key))
        .or_else(|| node.config.as_ref().and_then(|v| v.get(key)))
}
fn string<'a>(node: &'a FlowNode, key: &str) -> Result<Option<&'a str>, CompileError> {
    match value(node, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        _ => Err(error(format!(
            "Node {}: {key} must be a nonempty string",
            node.id
        ))),
    }
}
fn required<'a>(node: &'a FlowNode, key: &str) -> Result<&'a str, CompileError> {
    string(node, key)?.ok_or_else(|| error(format!("Node {}: {key} is required", node.id)))
}
fn amount(node: &FlowNode, key: &str) -> Result<u64, CompileError> {
    let amount = parse_u64_string(required(node, key)?).map_err(error)?;
    if amount == 0 {
        return Err(error(format!("Node {}: {key} must be positive", node.id)));
    }
    Ok(amount)
}
fn boolean(node: &FlowNode, key: &str, default: bool) -> Result<bool, CompileError> {
    match value(node, key) {
        None => Ok(default),
        Some(Value::Bool(v)) => Ok(*v),
        Some(Value::String(s)) if s == "true" => Ok(true),
        Some(Value::String(s)) if s == "false" => Ok(false),
        _ => Err(error(format!("Node {}: {key} must be a boolean", node.id))),
    }
}
fn guard_floor(node: &FlowNode) -> Result<u64, CompileError> {
    string(node, "minValue")?
        .map(parse_u64_string)
        .transpose()
        .map_err(error)
        .map(|v| v.unwrap_or(0))
}
fn incoming<'a>(flow: &'a FlowGraph, node: &FlowNode) -> Vec<&'a rill_core::flow::FlowEdge> {
    flow.edges
        .iter()
        .filter(|edge| edge.target == node.id)
        .collect()
}
pub(crate) fn effective_floor(flow: &FlowGraph, node: &FlowNode) -> Result<u64, CompileError> {
    let own = string(node, "min_amount_out")?
        .map(parse_u64_string)
        .transpose()
        .map_err(error)?
        .unwrap_or(0);
    let downstream = flow
        .edges
        .iter()
        .filter(|e| e.source == node.id)
        .filter_map(|e| {
            flow.nodes
                .iter()
                .find(|n| n.id == e.target && n.kind == "guardrail")
        })
        .map(guard_floor)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let floor = own.max(downstream);
    if floor == 0 {
        return Err(error(format!(
            "Node {}: a positive min_amount_out or downstream guardrail floor is required",
            node.id
        )));
    }
    Ok(floor)
}

async fn shared(
    chain: &impl SuiRead,
    objects: &mut SharedObjects,
    id: Address,
) -> Result<(), CompileError> {
    if objects.get(id).is_ok() {
        return Ok(());
    }
    let object = chain.get_object(&id.to_string()).await.map_err(error)?;
    let version = object
        .shared_initial_version
        .ok_or_else(|| error(format!("{id} is not shared")))?;
    objects.insert(id, version);
    Ok(())
}
async fn owned(chain: &impl SuiRead, id: &str) -> Result<ObjectInput, CompileError> {
    let id = address(id)?;
    let object = chain.get_object(&id.to_string()).await.map_err(error)?;
    if object.shared_initial_version.is_some() {
        return Err(error(format!("{id} must be owned")));
    }
    Ok(ObjectInput::owned(
        id,
        object.reference.version,
        object.reference.digest.parse().map_err(error)?,
    ))
}
fn split(
    tx: &mut TransactionBuilder,
    source: Argument,
    amount: u64,
) -> Result<Argument, CompileError> {
    let amount = tx.pure(&amount);
    tx.split_coins(source, vec![amount])
        .into_iter()
        .next()
        .ok_or_else(|| error("split produced no coin"))
}
#[derive(Clone)]
struct Coin {
    argument: Argument,
    coin_type: String,
}

/// Builds a transaction kind for wallet SDKs and a gas-less transaction for keyless previews.
/// Anonymous previews must be compiled again with the real sender before signing.
pub async fn compile(
    flow: &FlowGraph,
    options: &CompileOptions,
    chain: &impl SuiRead,
) -> Result<CompiledFlow, CompileError> {
    let ordered = topological_sort(flow).map_err(error)?;
    if ordered.is_empty() || ordered.iter().all(|n| n.kind == "ptb") {
        return Err(error("flow must contain at least one action"));
    }
    let mut outgoing = BTreeSet::new();
    for edge in &flow.edges {
        if !outgoing.insert((&edge.source, &edge.source_handle)) {
            return Err(error(format!(
                "Node {} has multiple consumers for the same coin",
                edge.source
            )));
        }
    }
    let sui = coin_type("0x2::sui::SUI")?;
    let mut root_total = 0u64;
    let mut swap_floors = Vec::new();
    for node in &ordered {
        if node.id.trim().is_empty() {
            return Err(error("node id cannot be empty"));
        }
        for config in [&node.config, &node.inputs].into_iter().flatten() {
            if !config.is_object() {
                return Err(error(format!(
                    "Node {}: config and inputs must be objects",
                    node.id
                )));
            }
        }
        let edges = incoming(flow, node);
        if node.kind != "guardrail" && edges.len() > 1 {
            return Err(error(format!(
                "Node {} accepts only one input coin",
                node.id
            )));
        }
        let root_amount = match node.kind.as_str() {
            "ptb" => 0,
            "cetus_swap" => {
                // Validated here, before any read, and recorded with its output coin below.
                effective_floor(flow, node)?;
                let amount = amount(node, "amount_in")?;
                if !boolean(node, "by_amount_in", true)? {
                    return Err(error(
                        "Studio swaps require by_amount_in=true to bound input spending",
                    ));
                }
                if edges.is_empty()
                    && coin_type(string(node, "inputCoinType")?.unwrap_or("0x2::sui::SUI"))? == sui
                {
                    amount
                } else {
                    0
                }
            }
            "haedal_stake" => {
                let amount = amount(node, "amount")?;
                let min_stake = string(node, "minStakeMist")?
                    .map(parse_u64_string)
                    .transpose()
                    .map_err(error)?
                    .unwrap_or(haedal::MIN_STAKE_MIST)
                    .max(haedal::MIN_STAKE_MIST);
                if amount < min_stake {
                    return Err(error(format!("Haedal requires at least {min_stake} mist")));
                }
                if edges.is_empty() {
                    amount
                } else {
                    0
                }
            }
            "deepbook_limit_order" => {
                required(node, "price")?;
                required(node, "quantity")?;
                let deposit =
                    decimal_to_base_units(required(node, "depositSui")?, 9).map_err(error)?;
                if deposit == 0 {
                    return Err(error("depositSui must be positive"));
                }
                deposit
            }
            "guardrail" => {
                guard_floor(node)?;
                0
            }
            other => return Err(error(format!("unsupported node type: {other}"))),
        };
        root_total = root_total
            .checked_add(root_amount)
            .ok_or_else(|| error("root funding exceeds u64"))?;
    }
    if let Some(wallet) = &options.agent_wallet {
        if let Some(protection) = &wallet.protected_swap {
            return compile_protected(flow, options, chain, wallet, protection, root_total).await;
        }
    }
    let mut tx = TransactionBuilder::new();
    tx.set_sender(options.sender.unwrap_or(Address::ZERO));
    tx.set_gas_budget(100_000_000);
    tx.set_gas_price(chain.reference_gas_price().await.map_err(error)?);
    let mut objects = SharedObjects::new();
    let mut extra_coins = Vec::new();
    let mut outputs: BTreeMap<String, Coin> = BTreeMap::new();
    let mut moved_types = BTreeSet::from([sui.clone()]);
    let mut warnings = Vec::new();
    if options.sender.is_none() {
        warnings.push(
            "Anonymous preview only: compile again with the connected sender before signing."
                .into(),
        );
    }
    let budget = if let Some(wallet) = &options.agent_wallet {
        wallet.capability_manifest.validate().map_err(error)?;
        if coin_type(&wallet.coin_type)? != sui
            || coin_type(&wallet.capability_manifest.wallet_coin_type)? != sui
        {
            return Err(error(
                "Studio root funding requires an AgentWallet<SUI> with a matching manifest",
            ));
        }
        let wallet_id = address(&wallet.wallet_id)?;
        let version_id = address(&wallet.version_id)?;
        shared(chain, &mut objects, wallet_id).await?;
        shared(chain, &mut objects, version_id).await?;
        let binding = WalletBinding {
            package_id: address(&wallet.package_id)?,
            wallet_id,
            version_id,
            cap: owned(chain, &wallet.cap_id).await?,
            coin_type: sui.clone(),
            manifest: wallet.capability_manifest.clone(),
        };
        if root_total > 0 {
            let budget = build_manifest_gated_spend(&mut tx, &binding, root_total, &objects)
                .map_err(error)?;
            extra_coins.push(Coin {
                argument: budget,
                coin_type: sui.clone(),
            });
            Some(budget)
        } else {
            warnings.push("Agent wallet bound, but this graph needs no root SUI funding.".into());
            None
        }
    } else {
        None
    };
    // Root guards borrow the whole budget before any action takes its share.
    for node in &ordered {
        if node.kind != "guardrail" || !incoming(flow, node).is_empty() {
            continue;
        }
        if flow.edges.iter().any(|e| e.source == node.id) {
            return Err(error(format!(
                "Guardrail {} has no input coin to forward",
                node.id
            )));
        }
        let floor = guard_floor(node)?;
        let Some(budget) = budget else {
            return Err(error(format!(
                "Guardrail {} has no incoming coin or agent wallet budget",
                node.id
            )));
        };
        let configured_type = coin_type(string(node, "coinType")?.unwrap_or("0x2::sui::SUI"))?;
        if configured_type != sui {
            return Err(error("root guardrail coinType must match the SUI budget"));
        }
        guard::assert_min_value(&mut tx, options.guard_package, budget, &sui, floor)
            .map_err(error)?;
        if floor == 0 {
            warnings.push(format!(
                "Guardrail {} has no minimum; no protection is enforced.",
                node.id
            ));
        }
    }
    for node in ordered {
        let edges = incoming(flow, node);
        match node.kind.as_str() {
            "ptb" => {}
            "cetus_swap" => {
                let amount = amount(node, "amount_in")?;
                let input_type =
                    coin_type(string(node, "inputCoinType")?.unwrap_or("0x2::sui::SUI"))?;
                let pool_id = address(required(node, "pool")?)?;
                let pool = chain
                    .get_object(&pool_id.to_string())
                    .await
                    .map_err(error)?;
                let pool_version = pool
                    .shared_initial_version
                    .ok_or_else(|| error("Cetus pool is not shared"))?;
                objects.insert(pool_id, pool_version);
                let (a, b) = cetus::pool_coin_types(
                    pool.object_type
                        .as_deref()
                        .ok_or_else(|| error("pool type is missing"))?,
                )
                .ok_or_else(|| error("pool does not have two coin type arguments"))?;
                let (a, b) = (coin_type(&a)?, coin_type(&b)?);
                let a2b = if input_type == a {
                    true
                } else if input_type == b {
                    false
                } else {
                    return Err(error("inputCoinType is not in this pool"));
                };
                let output_type = if a2b { b.clone() } else { a.clone() };
                swap_floors.push((
                    node.id.clone(),
                    effective_floor(flow, node)?,
                    output_type.clone(),
                ));
                moved_types.extend([a.clone(), b.clone()]);
                let funded = if let Some(edge) = edges.first() {
                    let coin = outputs.remove(&edge.source).ok_or_else(|| {
                        error(format!("Node {}: upstream coin is missing", node.id))
                    })?;
                    if coin.coin_type != input_type {
                        return Err(error(format!(
                            "Node {}: upstream coin type does not match inputCoinType",
                            node.id
                        )));
                    }
                    coin.argument
                } else if input_type == sui {
                    let source = budget.unwrap_or_else(|| tx.gas());
                    split(&mut tx, source, amount)?
                } else {
                    source_owned_coin(&mut tx, chain, options.sender, &input_type, amount).await?
                };
                let (default_package, default_config) = cetus_defaults(options.network);
                let config_id = address(string(node, "globalConfigId")?.unwrap_or(default_config))?;
                shared(chain, &mut objects, config_id).await?;
                let configured_limit = string(node, "sqrt_price_limit")?.or(string(
                    node,
                    if a2b { "minSqrtPrice" } else { "maxSqrtPrice" },
                )?);
                let price_limit = match configured_limit {
                    Some(v) if v.bytes().all(|c| c.is_ascii_digit()) => {
                        v.parse::<u128>().map_err(error)?
                    }
                    Some(_) => {
                        return Err(error("sqrt_price_limit must be an unsigned integer string"))
                    }
                    None => {
                        if a2b {
                            cetus::MIN_SQRT_PRICE
                        } else {
                            cetus::MAX_SQRT_PRICE
                        }
                    }
                };
                let result = cetus::swap(
                    &mut tx,
                    &cetus::Swap {
                        integrate_package_id: address(
                            string(node, "integratePackageId")?.unwrap_or(default_package),
                        )?,
                        global_config_id: config_id,
                        pool_id,
                        coin_type_a: a,
                        coin_type_b: b,
                        a2b,
                        by_amount_in: true,
                        amount,
                        sqrt_price_limit: price_limit,
                    },
                    funded,
                    &objects,
                )
                .map_err(error)?;
                if let Some(min) = string(node, "min_amount_out")? {
                    guard::assert_min_value(
                        &mut tx,
                        options.guard_package,
                        result.output(),
                        &output_type,
                        parse_u64_string(min).map_err(error)?,
                    )
                    .map_err(error)?;
                }
                outputs.insert(
                    node.id.clone(),
                    Coin {
                        argument: result.output(),
                        coin_type: output_type,
                    },
                );
                extra_coins.push(Coin {
                    argument: result.residual(),
                    coin_type: input_type,
                });
            }
            "haedal_stake" => {
                let stake_amount = amount(node, "amount")?;
                let funded = if let Some(edge) = edges.first() {
                    let coin = outputs
                        .remove(&edge.source)
                        .ok_or_else(|| error("upstream stake coin is missing"))?;
                    if coin.coin_type != sui {
                        return Err(error("Haedal input must be SUI"));
                    }
                    // Stake exactly the approved amount; settle any upstream surplus once.
                    let funded = split(&mut tx, coin.argument, stake_amount)?;
                    extra_coins.push(coin);
                    funded
                } else {
                    let source = budget.unwrap_or_else(|| tx.gas());
                    split(&mut tx, source, stake_amount)?
                };
                let (default_package, default_staking) = haedal_defaults(options.network);
                let package = match string(node, "stakeTarget")? {
                    Some(target) => {
                        let (package, suffix) = target
                            .split_once("::")
                            .ok_or_else(|| error("invalid stakeTarget"))?;
                        if suffix != "interface::request_stake" {
                            return Err(error("unsupported stakeTarget"));
                        }
                        package
                    }
                    None => default_package,
                };
                if let Some(system) = string(node, "suiSystemStateId")? {
                    if address(system)? != address("0x5")? {
                        return Err(error("suiSystemStateId must be 0x5"));
                    }
                }
                let staking_id =
                    address(string(node, "stakingObjectId")?.unwrap_or(default_staking))?;
                shared(chain, &mut objects, staking_id).await?;
                haedal::request_stake(
                    &mut tx,
                    &haedal::Stake {
                        package_id: address(package)?,
                        staking_object_id: staking_id,
                        validator: address(string(node, "validator")?.unwrap_or("0x0"))?,
                        amount_mist: stake_amount,
                    },
                    funded,
                    &objects,
                )
                .map_err(error)?;
                let staking = chain
                    .get_object(&staking_id.to_string())
                    .await
                    .map_err(error)?;
                let defining_package = staking
                    .object_type
                    .as_deref()
                    .and_then(|t| t.split_once("::"))
                    .map(|(p, _)| p)
                    .ok_or_else(|| error("Haedal staking object type is missing"))?;
                moved_types.insert(coin_type(&format!("{defining_package}::hasui::HASUI"))?);
            }
            "guardrail" if !edges.is_empty() => {
                let floor = guard_floor(node)?;
                let mut coins = Vec::new();
                for edge in edges {
                    let coin = outputs.remove(&edge.source).ok_or_else(|| {
                        error(format!("Guardrail {}: input coin missing", node.id))
                    })?;
                    guard::assert_min_value(
                        &mut tx,
                        options.guard_package,
                        coin.argument,
                        &coin.coin_type,
                        floor,
                    )
                    .map_err(error)?;
                    coins.push(coin);
                }
                let mut coins = coins.into_iter();
                let first = coins.next().ok_or_else(|| error("guardrail has no coin"))?;
                let rest = coins.collect::<Vec<_>>();
                if rest.iter().any(|c| c.coin_type != first.coin_type) {
                    return Err(error("guardrail inputs must have matching coin types"));
                }
                if !rest.is_empty() {
                    tx.merge_coins(
                        first.argument,
                        rest.into_iter().map(|c| c.argument).collect(),
                    );
                }
                if floor == 0 {
                    warnings.push(format!(
                        "Guardrail {} has no minimum; no protection is enforced.",
                        node.id
                    ));
                }
                outputs.insert(node.id.clone(), first);
            }
            "deepbook_limit_order" => {
                if options.agent_wallet.is_none() {
                    return Err(error("DeepBook requires an AgentWallet binding"));
                }
                let network = match options.network {
                    Network::Testnet => DeepBookNetwork::Testnet,
                    Network::Mainnet => DeepBookNetwork::Mainnet,
                };
                let pool = registry::pool_spec(network, required(node, "poolKey")?)
                    .ok_or_else(|| error("unknown DeepBook poolKey"))?;
                let is_bid = boolean(node, "isBid", false)?;
                let funding_type = if is_bid {
                    &pool.quote_coin_type
                } else {
                    &pool.base_coin_type
                };
                if coin_type(funding_type)? != sui {
                    return Err(error("depositSui requires an order side funded in SUI"));
                }
                moved_types.extend([
                    coin_type(&pool.base_coin_type)?,
                    coin_type(&pool.quote_coin_type)?,
                ]);
                if boolean(node, "payWithDeep", false)? {
                    let deep = registry::coin(network, "DEEP")
                        .ok_or_else(|| error("DEEP fee coin is missing from registry"))?;
                    moved_types.insert(coin_type(deep.coin_type)?);
                }
                let manager_id = address(required(node, "balanceManagerId")?)?;
                shared(chain, &mut objects, manager_id).await?;
                shared(chain, &mut objects, pool.pool_id).await?;
                let order = deepbook::LimitOrder {
                    pool,
                    balance_manager_id: manager_id,
                    trade_cap: owned(chain, required(node, "tradeCapId")?).await?,
                    deposit_cap: owned(chain, required(node, "depositCapId")?).await?,
                    client_order_id: parse_u64_string(
                        string(node, "clientOrderId")?.unwrap_or("1"),
                    )
                    .map_err(error)?,
                    price: required(node, "price")?.into(),
                    quantity: required(node, "quantity")?.into(),
                    is_bid,
                    pay_with_deep: boolean(node, "payWithDeep", false)?,
                };
                let price_units = rill_core::amounts::deepbook_price_to_base_units(
                    &order.price,
                    deepbook::FLOAT_SCALAR,
                    order.pool.quote_scalar,
                    order.pool.base_scalar,
                )
                .map_err(error)?;
                let quantity_units = rill_core::amounts::deepbook_quantity_to_base_units(
                    &order.quantity,
                    order.pool.base_scalar,
                )
                .map_err(error)?;
                if price_units == 0 || quantity_units == 0 {
                    return Err(error("DeepBook price and quantity must be positive"));
                }
                let funding_amount =
                    decimal_to_base_units(required(node, "depositSui")?, 9).map_err(error)?;
                let source = budget.ok_or_else(|| error("DeepBook wallet budget is missing"))?;
                let funding_coin = split(&mut tx, source, funding_amount)?;
                deepbook::place_limit_order(
                    &mut tx,
                    address(network.package_id())?,
                    &order,
                    funding_coin,
                    &objects,
                )
                .map_err(error)?;
            }
            _ => {}
        }
    }
    let pending = outputs
        .into_values()
        .chain(extra_coins)
        .map(|coin| coin.argument)
        .collect::<Vec<_>>();
    if !pending.is_empty() {
        let recipient = tx.pure(&options.sender.unwrap_or(Address::ZERO));
        tx.transfer_objects(pending, recipient);
    }
    // The builder requires a gas reference even for a kind. Remove its construction-only
    // placeholder before returning anything: the preview node performs its own gas selection.
    tx.add_gas_objects([ObjectInput::owned(address("0x1")?, 1, Digest::ZERO)]);
    let mut transaction = tx.try_build().map_err(error)?;
    transaction.gas_payment.objects.clear();
    enforce_manifest(
        options,
        &transaction,
        &moved_types,
        &swap_floors,
        root_total,
    )?;
    let unsigned_ptb = STANDARD.encode(bcs::to_bytes(&transaction.kind).map_err(error)?);
    let preview = format!(
        "{} action(s), {} root SUI mist; {} commands",
        flow.nodes.iter().filter(|n| n.kind != "ptb").count(),
        root_total,
        match &transaction.kind {
            TransactionKind::ProgrammableTransaction(ptb) => ptb.commands.len(),
            _ => 0,
        }
    );
    Ok(CompiledFlow {
        transaction,
        unsigned_ptb,
        preview,
        warnings,
        root_spend_mist: root_total,
    })
}

async fn source_owned_coin(
    tx: &mut TransactionBuilder,
    chain: &impl SuiRead,
    sender: Option<Address>,
    coin: &str,
    amount: u64,
) -> Result<Argument, CompileError> {
    let sender =
        sender.ok_or_else(|| error("non-SUI root input requires a sender with owned coins"))?;
    let expected = coin_type(&format!("0x2::coin::Coin<{coin}>"))?;
    let owned = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(error)?;
    let mut inputs = Vec::new();
    let mut balance = 0u128;
    for object in owned {
        if object
            .object_type
            .as_deref()
            .and_then(|t| coin_type(t).ok())
            .as_deref()
            != Some(expected.as_str())
        {
            continue;
        }
        let object = if object.fields.is_none() {
            chain
                .get_object(&object.reference.id)
                .await
                .map_err(error)?
        } else {
            object
        };
        let fields = object
            .fields
            .as_ref()
            .ok_or_else(|| error("coin balance fields are missing"))?;
        let raw = fields
            .get("balance")
            .ok_or_else(|| error("coin balance is missing"))?;
        let units = match raw {
            Value::String(s) => parse_u64_string(s).map_err(error)?,
            Value::Number(n) => n.as_u64().ok_or_else(|| error("invalid coin balance"))?,
            _ => return Err(error("invalid coin balance")),
        };
        balance += u128::from(units);
        inputs.push(tx.object(ObjectInput::owned(
            address(&object.reference.id)?,
            object.reference.version,
            object.reference.digest.parse().map_err(error)?,
        )));
        if balance >= u128::from(amount) {
            break;
        }
    }
    if balance < u128::from(amount) {
        return Err(error(format!(
            "insufficient {coin}: have {balance}, need {amount}"
        )));
    }
    let mut inputs = inputs.into_iter();
    let primary = inputs.next().ok_or_else(|| error("no input coin"))?;
    let rest = inputs.collect::<Vec<_>>();
    if !rest.is_empty() {
        tx.merge_coins(primary, rest);
    }
    split(tx, primary, amount)
}

fn enforce_manifest(
    options: &CompileOptions,
    transaction: &Transaction,
    moved: &BTreeSet<String>,
    floors: &[(String, u64, String)],
    spend: u64,
) -> Result<(), CompileError> {
    let Some(wallet) = &options.agent_wallet else {
        return Ok(());
    };
    let TransactionKind::ProgrammableTransaction(ptb) = &transaction.kind else {
        return Err(error("expected programmable transaction"));
    };
    for rule in &wallet.capability_manifest.rules {
        match rule {
            // Say what spends how much against which limit: the bare "budget rule exceeded" left an
            // owner who set a 0.05 SUI budget for an action that spends 0.1 SUI with nothing to fix.
            CapabilityRule::Budget { total_mist }
                if spend > parse_u64_string(total_mist).map_err(error)? =>
            {
                return Err(error(format!(
                    "budget rule exceeded: this action spends {} per run, above the {} budget",
                    format_amount(&spend.to_string(), &wallet.coin_type),
                    format_amount(total_mist, &wallet.coin_type),
                )))
            }
            CapabilityRule::PerTx { max_mist }
                if spend > parse_u64_string(max_mist).map_err(error)? =>
            {
                return Err(error(format!(
                    "per_tx rule exceeded: this action spends {} per run, above the {} per-transaction limit",
                    format_amount(&spend.to_string(), &wallet.coin_type),
                    format_amount(max_mist, &wallet.coin_type),
                )))
            }
            CapabilityRule::ProtocolScope { allowed_packages } => {
                let allowed = allowed_packages
                    .iter()
                    .map(|p| address(p))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                let mut infrastructure = BTreeSet::from([
                    address("0x1")?,
                    address("0x2")?,
                    address("0x3")?,
                    address(&wallet.package_id)?,
                ]);
                if let Some(guard) = options.guard_package {
                    infrastructure.insert(guard);
                }
                for command in &ptb.commands {
                    if let Command::MoveCall(call) = command {
                        if !infrastructure.contains(&call.package)
                            && !allowed.contains(&call.package)
                        {
                            return Err(error(format!("protocol_scope refuses {}", call.package)));
                        }
                    }
                }
            }
            CapabilityRule::AssetScope { allowed_coin_types } => {
                let allowed = allowed_coin_types
                    .iter()
                    .map(|t| coin_type(t))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                if let Some(denied) = moved.iter().find(|t| !allowed.contains(*t)) {
                    return Err(error(format!("asset_scope refuses {denied}")));
                }
            }
            CapabilityRule::RecipientAllowlist { addresses } => {
                let sender = options
                    .sender
                    .ok_or_else(|| error("recipient_allowlist requires a sender"))?;
                let allowed = addresses
                    .iter()
                    .map(|a| address(a))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                if !allowed.contains(&sender) {
                    return Err(error("recipient_allowlist refuses the sender"));
                }
            }
            // Compared in the swap's output coin, the only units both numbers share. A floor that
            // names a coin binds only the swaps that output it.
            CapabilityRule::SlippageFloor {
                min_out_mist,
                coin_type: floor_coin,
            } => {
                let min = parse_u64_string(min_out_mist).map_err(error)?;
                let floor_coin = floor_coin.as_deref().map(coin_type).transpose()?;
                for (node, floor, output) in floors {
                    if floor_coin.as_ref().is_some_and(|c| c != output) {
                        continue;
                    }
                    if *floor < min {
                        return Err(error(format!(
                            "slippage_floor exceeds the swap's effective guard floor: swap {node} \
                             accepts as little as {}, below the wallet's {}",
                            format_amount(&floor.to_string(), output),
                            format_amount(min_out_mist, output),
                        )));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn cetus_defaults(network: Network) -> (&'static str, &'static str) {
    match network {
        Network::Testnet => (
            deployments::TESTNET_CETUS_INTEGRATE,
            deployments::TESTNET_CETUS_GLOBAL_CONFIG,
        ),
        Network::Mainnet => (
            deployments::MAINNET_CETUS_INTEGRATE,
            deployments::MAINNET_CETUS_GLOBAL_CONFIG,
        ),
    }
}
fn haedal_defaults(network: Network) -> (&'static str, &'static str) {
    match network {
        Network::Testnet => (
            deployments::TESTNET_HAEDAL_PACKAGE,
            deployments::TESTNET_HAEDAL_STAKING,
        ),
        Network::Mainnet => (
            "0x126e4cfb051cad744706df590ec399e8c02b6feae195c35b8b496280d5442a62",
            "0x47b224762220393057ebf4f70501b6e657c3e56684737568439a04f80849b2ca",
        ),
    }
}

mod envelope;
pub use envelope::build_action;
