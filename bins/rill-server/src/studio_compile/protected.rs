//! Protected terminal swap compilation and owner settlement checks.
use super::{
    address, coin_type, effective_floor, enforce_manifest, error, guard_floor, incoming, owned,
    required, shared, string, AgentWalletInput, CompileError, CompileOptions, CompiledFlow,
    ProtectedSwapInput,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rill_chain::SuiRead;
use rill_core::{envelope::Network, flow::FlowGraph, manifest::CapabilityRule};
use rill_ptb::{cetus, deployments, shared::SharedObjects, spend::WalletBinding};
use std::collections::BTreeSet;
use sui_sdk_types::Digest;
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

pub(super) async fn compile_protected(
    flow: &FlowGraph,
    options: &CompileOptions,
    chain: &impl SuiRead,
    wallet: &AgentWalletInput,
    protection: &ProtectedSwapInput,
    spend: u64,
) -> Result<CompiledFlow, CompileError> {
    let actions: Vec<_> = flow
        .nodes
        .iter()
        .filter(|n| !matches!(n.kind.as_str(), "ptb" | "guardrail"))
        .collect();
    if actions.len() != 1 || actions[0].kind != "cetus_swap" {
        return Err(error("protected vaults require one terminal Cetus swap"));
    }
    let node = actions[0];
    let sender = options
        .sender
        .ok_or_else(|| error("protected execution requires an agent sender"))?;
    let wallet_id = address(&wallet.wallet_id)?;
    let object = chain
        .get_object(&wallet_id.to_string())
        .await
        .map_err(error)?;
    let fields = object
        .fields
        .as_ref()
        .ok_or_else(|| error("wallet fields unavailable"))?;
    if address(
        fields["owner"]
            .as_str()
            .ok_or_else(|| error("wallet owner missing"))?,
    )? != address(&protection.owner)?
        || address(
            fields["agent"]
                .as_str()
                .ok_or_else(|| error("wallet agent missing"))?,
        )? != sender
    {
        return Err(error(
            "protected binding must match the vault owner and agent",
        ));
    }
    let pool_id = address(required(node, "pool")?)?;
    let pool = chain
        .get_object(&pool_id.to_string())
        .await
        .map_err(error)?;
    let (a, b) = cetus::pool_coin_types(
        pool.object_type
            .as_deref()
            .ok_or_else(|| error("pool type missing"))?,
    )
    .ok_or_else(|| error("invalid Cetus pool type"))?;
    let (a, b) = (coin_type(&a)?, coin_type(&b)?);
    let input = coin_type(string(node, "inputCoinType")?.unwrap_or("0x2::sui::SUI"))?;
    let a2b = input == a;
    if (!a2b && input != b) || coin_type(&wallet.coin_type)? != input {
        return Err(error("protected input asset must match the vault and pool"));
    }
    let output_type = if a2b { &b } else { &a };
    for guard in flow.nodes.iter().filter(|node| node.kind == "guardrail") {
        let edges = incoming(flow, guard);
        if edges.len() != 1
            || edges[0].source != node.id
            || edges[0].source_handle != "coin_out"
            || flow.edges.iter().any(|edge| edge.source == guard.id)
        {
            return Err(error(
                "protected swaps accept only terminal guards on their output coin",
            ));
        }
        if coin_type(string(guard, "coinType")?.unwrap_or(output_type))? != *output_type {
            return Err(error("protected guard asset must match the swap output"));
        }
        if guard_floor(guard)? == 0 {
            return Err(error("protected output guard requires a positive floor"));
        }
    }
    let config_id = address(string(node, "globalConfigId")?.unwrap_or(
        if options.network == Network::Mainnet {
            deployments::MAINNET_CETUS_GLOBAL_CONFIG
        } else {
            deployments::TESTNET_CETUS_GLOBAL_CONFIG
        },
    ))?;
    let swap = rill_ptb::protected::ProtectedSwap {
        adapter_package: address(&protection.adapter_package_id)?,
        pool_id,
        config_id,
        coin_type_a: a.clone(),
        coin_type_b: b.clone(),
        a2b,
        revision: protection.revision,
        min_output: effective_floor(flow, node)?,
        sqrt_price_limit: string(node, "sqrt_price_limit")?
            .map(str::parse)
            .transpose()
            .map_err(error)?
            .unwrap_or(if a2b {
                cetus::MIN_SQRT_PRICE + 1
            } else {
                cetus::MAX_SQRT_PRICE - 1
            }),
    };
    let mut objects = SharedObjects::new();
    objects.insert(
        wallet_id,
        object
            .shared_initial_version
            .ok_or_else(|| error("vault must be shared"))?,
    );
    objects.insert(
        pool_id,
        pool.shared_initial_version
            .ok_or_else(|| error("Cetus pool must be shared"))?,
    );
    for id in [address(&wallet.version_id)?, address("0x6")?, config_id] {
        shared(chain, &mut objects, id).await?;
    }
    let binding = WalletBinding {
        package_id: address(&wallet.package_id)?,
        wallet_id,
        version_id: address(&wallet.version_id)?,
        cap: owned(chain, &wallet.cap_id).await?,
        coin_type: input,
        manifest: wallet.capability_manifest.clone(),
    };
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_price(chain.reference_gas_price().await.map_err(error)?);
    tx.set_gas_budget(100_000_000);
    rill_ptb::protected::execute(&mut tx, &swap, &binding, spend, &objects).map_err(error)?;
    tx.add_gas_objects([ObjectInput::owned(address("0x1")?, 1, Digest::ZERO)]);
    let mut transaction = tx.try_build().map_err(error)?;
    transaction.gas_payment.objects.clear();
    let mut checked = wallet.clone();
    // The contract pins the adapter and forces settlement to the independently checked owner.
    // Apply remaining manifest checks to the actual adapter call and actual recipient.
    for rule in &mut checked.capability_manifest.rules {
        if let CapabilityRule::ProtocolScope { allowed_packages } = rule {
            let protocol = address(string(node, "integratePackageId")?.unwrap_or(
                if options.network == Network::Mainnet {
                    deployments::MAINNET_CETUS_INTEGRATE
                } else {
                    deployments::TESTNET_CETUS_INTEGRATE
                },
            ))?;
            if !allowed_packages
                .iter()
                .any(|p| address(p).ok() == Some(protocol))
            {
                return Err(error("protocol_scope refuses the protected Cetus action"));
            }
            allowed_packages.push(protection.adapter_package_id.clone());
        }
    }
    let checks = CompileOptions {
        sender: Some(address(&protection.owner)?),
        agent_wallet: Some(checked),
        network: options.network,
        guard_package: options.guard_package,
    };
    enforce_manifest(
        &checks,
        &transaction,
        &BTreeSet::from([a, b.clone()]),
        &[(
            node.id.clone(),
            swap.min_output,
            if a2b { b } else { swap.coin_type_a.clone() },
        )],
        spend,
    )?;
    let unsigned_ptb = STANDARD.encode(bcs::to_bytes(&transaction.kind).map_err(error)?);
    Ok(CompiledFlow {
        transaction,
        unsigned_ptb,
        preview: format!(
            "Protected Cetus swap: {spend} input base units, owner settlement, policy revision {}",
            protection.revision
        ),
        warnings: Vec::new(),
        root_spend_mist: spend,
    })
}
