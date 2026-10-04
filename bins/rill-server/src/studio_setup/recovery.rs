//! Owner-only revocation, order cancellation and withdrawal in one unsigned transaction.
use super::*;
use rill_ptb::registry;
use std::collections::BTreeSet;

pub(super) fn recovery_owner(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
) -> Result<Address, String> {
    let owner = address(owner)?;
    if address(required(body, "sender")?)? != owner
        || skill.owner.as_deref().map(address).transpose()? != Some(owner)
    {
        return Err("recovery requires the published action's owner".into());
    }
    Ok(owner)
}

pub async fn recovery_plan(
    body: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
) -> Result<Value, String> {
    let owner = recovery_owner(body, skill, owner)?;
    let flow = studio_api::parse_flow(&skill.flow).map_err(|_| "invalid published flow")?;
    let network = deepbook_network(context.network);
    let mut pools = Vec::new();
    let mut seen = BTreeSet::new();
    for node in flow
        .nodes
        .iter()
        .filter(|n| n.kind == "deepbook_limit_order")
    {
        let key = node
            .inputs
            .as_ref()
            .and_then(|v| v.get("poolKey"))
            .or_else(|| node.config.as_ref().and_then(|v| v.get("poolKey")))
            .and_then(Value::as_str)
            .ok_or("DeepBook poolKey is missing")?;
        let pool = registry::pool_spec(network, key).ok_or("unknown DeepBook pool")?;
        if seen.insert(pool.pool_id) {
            pools.push(pool);
        }
    }
    if pools.is_empty() {
        return Err("this action has no DeepBook manager to recover".into());
    }
    let (package, _) = deployments::wallet_deployment(
        context.network,
        context.wallet_package_id.as_deref(),
        context.wallet_version_id.as_deref(),
    )?;
    let mut objects = SharedObjects::new();
    let wallet_id = address(required(body, "walletId")?)?;
    let wallet = shared(chain, &mut objects, wallet_id).await?;
    let sui = "0x2::sui::SUI";
    let expected = canonical_type(&format!(
        "{}::agent_wallet::AgentWallet<{sui}>",
        context
            .wallet_type_package
            .as_deref()
            .unwrap_or(&package.to_string())
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
    let fields = wallet.fields.as_ref().ok_or("wallet fields unavailable")?;
    if address(required(fields, "owner")?)? != owner {
        return Err("wallet owner does not match sender".into());
    }
    let revoked = fields["revoked"]
        .as_bool()
        .ok_or("wallet revoked status unavailable")?;
    let manager_id = address(required(body, "balanceManagerId")?)?;
    let manager = shared(chain, &mut objects, manager_id).await?;
    let origins = deepbook_types(context)?;
    let expected = canonical_type(&format!("{}::balance_manager::BalanceManager", origins[0]))?;
    if manager
        .object_type
        .as_deref()
        .map(canonical_type)
        .transpose()?
        != Some(expected)
    {
        return Err("balance manager type does not match the configured deployment".into());
    }
    if address(required(
        manager
            .fields
            .as_ref()
            .ok_or("manager fields unavailable")?,
        "owner",
    )?)? != owner
    {
        return Err("balance manager owner does not match sender".into());
    }
    let deepbook = address(network.package_id())?;
    let mut tx = builder(owner, chain).await?;
    let m = tx.object(objects.input(manager_id, true).map_err(err)?);
    let clock = tx.object(objects.input(address("0x6")?, false).map_err(err)?);
    let mut coins = BTreeSet::new();
    for pool in pools {
        let id = pool.pool_id;
        shared(chain, &mut objects, id).await?;
        let p = tx.object(objects.input(id, true).map_err(err)?);
        let proof = tx.move_call(
            Function::new(
                deepbook,
                Identifier::new("balance_manager").map_err(err)?,
                Identifier::new("generate_proof_as_owner").map_err(err)?,
            ),
            vec![m],
        );
        let base: TypeTag = pool.base_coin_type.parse().map_err(err)?;
        let quote: TypeTag = pool.quote_coin_type.parse().map_err(err)?;
        tx.move_call(
            Function::new(
                deepbook,
                Identifier::new("pool").map_err(err)?,
                Identifier::new("cancel_all_orders").map_err(err)?,
            )
            .with_type_args(vec![base, quote]),
            vec![p, m, proof, clock],
        );
        coins.insert(pool.base_coin_type.clone());
        coins.insert(pool.quote_coin_type.clone());
    }
    let mut reclaimed = Vec::new();
    for coin in coins {
        reclaimed.push(
            tx.move_call(
                Function::new(
                    deepbook,
                    Identifier::new("balance_manager").map_err(err)?,
                    Identifier::new("withdraw_all").map_err(err)?,
                )
                .with_type_args(vec![coin.parse::<TypeTag>().map_err(err)?]),
                vec![m],
            ),
        );
    }
    if !revoked {
        let wallet = tx.object(objects.input(wallet_id, true).map_err(err)?);
        reclaimed.push(
            tx.move_call(
                Function::new(
                    package,
                    Identifier::new("agent_wallet").map_err(err)?,
                    Identifier::new("revoke").map_err(err)?,
                )
                .with_type_args(vec![sui.parse::<TypeTag>().map_err(err)?]),
                vec![wallet],
            ),
        );
    }
    let recipient = tx.pure(&owner);
    tx.transfer_objects(reclaimed, recipient);
    Ok(
        json!({"recoveryPtb":encode_kind(&finish(tx)?)?,"owner":owner.to_string(),"balanceManagerId":manager_id.to_string(),"submitted":false}),
    )
}
