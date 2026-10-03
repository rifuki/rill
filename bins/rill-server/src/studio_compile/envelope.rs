//! Strict, funded envelopes for published Studio graphs.
use super::*;
use rill_core::envelope::{
    digest_unsigned_ptb, BalanceChange, EnvelopeStep, ExecutionEnvelope, StrictSimulationResult,
    Verification, EXECUTION_ENVELOPE_VERSION,
};
use sui_sdk_types::ObjectReference;

/// Compile the stored graph, resolve real gas, and return an envelope only after strict simulation.
pub async fn build_action(
    flow: &FlowGraph,
    options: &CompileOptions,
    action_id: &str,
    chain: &impl SuiRead,
    now_ms: u64,
) -> Result<ExecutionEnvelope, CompileError> {
    let sender = options
        .sender
        .ok_or_else(|| error("published action requires a sender"))?;
    let wallet = options
        .agent_wallet
        .as_ref()
        .ok_or_else(|| error("published action requires an agent wallet"))?;
    let compiled = compile(flow, options, chain).await?;
    if compiled.root_spend_mist == 0 {
        return Err(error(
            "published action requires a positive manifest-gated wallet spend",
        ));
    }
    let mut transaction = compiled.transaction;
    let preview_bytes = STANDARD.encode(bcs::to_bytes(&transaction).map_err(error)?);
    let input_ids = rill_policy::decode::decode(&preview_bytes)
        .map_err(error)?
        .object_inputs;
    let expected = coin_type("0x2::coin::Coin<0x2::sui::SUI>")?;
    let mut gas_total = 0u128;
    for candidate in chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(error)?
    {
        if candidate
            .object_type
            .as_deref()
            .and_then(|t| coin_type(t).ok())
            .as_deref()
            != Some(expected.as_str())
        {
            continue;
        }
        if input_ids.contains(&address(&candidate.reference.id)?.to_string()) {
            continue;
        }
        let coin = chain
            .get_object(&candidate.reference.id)
            .await
            .map_err(error)?;
        let units = match coin.fields.as_ref().and_then(|f| f.get("balance")) {
            Some(Value::String(s)) => parse_u64_string(s).map_err(error)?,
            Some(Value::Number(n)) => n
                .as_u64()
                .ok_or_else(|| error("invalid gas coin balance"))?,
            _ => return Err(error("gas coin balance is missing")),
        };
        if coin.shared_initial_version.is_some() {
            return Err(error("gas coin cannot be shared"));
        }
        transaction.gas_payment.objects.push(ObjectReference::new(
            address(&coin.reference.id)?,
            coin.reference.version,
            coin.reference.digest.parse().map_err(error)?,
        ));
        gas_total += u128::from(units);
        if gas_total >= u128::from(transaction.gas_payment.budget) {
            break;
        }
        if transaction.gas_payment.objects.len() >= 256 {
            return Err(error(
                "gas requires more than 256 coins; consolidate SUI first",
            ));
        }
    }
    if gas_total < u128::from(transaction.gas_payment.budget) {
        return Err(error(format!(
            "insufficient sender gas: have {gas_total}, need {}",
            transaction.gas_payment.budget
        )));
    }
    let unsigned_ptb = STANDARD.encode(bcs::to_bytes(&transaction).map_err(error)?);
    let simulation = chain
        .simulate(&unsigned_ptb)
        .await
        .map_err(|e| error(format!("strict simulation unavailable: {e}")))?;
    if simulation.verification != rill_chain::Verification::Verified {
        return Err(error(format!(
            "strict simulation is unverified: {}",
            simulation.error.as_deref().unwrap_or("inconclusive")
        )));
    }
    if !simulation.ok {
        return Err(error(format!(
            "strict simulation failed: {}",
            simulation.error.as_deref().unwrap_or("transaction refused")
        )));
    }
    let decoded = rill_policy::decode::decode(&unsigned_ptb).map_err(error)?;
    let required_guards = decoded
        .targets
        .iter()
        .filter(|t| t.ends_with("::guard::assert_min_value"))
        .cloned()
        .collect();
    let expires_at = now_ms
        .checked_add(crate::build::ENVELOPE_TTL_MS)
        .ok_or_else(|| error("envelope expiry overflow"))?;
    let envelope = ExecutionEnvelope {
        version: EXECUTION_ENVELOPE_VERSION.into(),
        action_id: action_id.into(),
        action_digest: digest_unsigned_ptb(&unsigned_ptb),
        network: options.network,
        sender: sender.to_string(),
        wallet_package_id: address(&wallet.package_id)?.to_string(),
        wallet_id: address(&wallet.wallet_id)?.to_string(),
        agent_cap_id: address(&wallet.cap_id)?.to_string(),
        balance_manager_id: None,
        trade_cap_id: None,
        resolved_params: None,
        // This is one atomic graph. The released wallet budget is counted once even when several
        // nodes consume it or the same coin passes through several guards and protocol calls.
        steps: vec![EnvelopeStep {
            node_id: "flow".into(),
            kind: "flow".into(),
            targets: decoded.targets.clone(),
            spend_amount_mist: Some(compiled.root_spend_mist.to_string()),
            object_ids: decoded.object_inputs.clone(),
        }],
        allowed_targets: decoded.targets,
        required_object_ids: decoded.object_inputs,
        required_guards,
        unsigned_ptb,
        preview: compiled.preview,
        simulation: StrictSimulationResult {
            ok: true,
            verification: Verification::Verified,
            error: None,
            gas_estimate: simulation.gas_used_mist.to_string(),
            balance_changes: simulation
                .balance_changes
                .into_iter()
                .map(|b| BalanceChange {
                    owner: b.address,
                    coin_type: b.coin_type,
                    amount: b.amount,
                })
                .collect(),
            object_changes: Vec::new(),
        },
        expires_at: crate::build::format_rfc3339_ms(expires_at),
    };
    envelope.validate_shape().map_err(error)?;
    Ok(envelope)
}
