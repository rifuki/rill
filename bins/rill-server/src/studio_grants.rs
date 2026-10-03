//! Owner-signed action grants: prepared for the owner to sign, checked, stored, and served to the
//! signer they name. See `docs/plans/2026-10-04-owner-signed-action-grants.md`.
//!
//! This server never decides what a signer may do. It derives the grant the same way onboarding
//! derives a run set, refuses to store one the wallet's owner did not sign, and hands it to a signer
//! that checks all of it again against the chain before using any of it.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use rill_chain::{SignatureCheck, SuiRead};
use rill_core::grant::SignedGrant;
use rill_store::{PublishedSkill, SkillStore, StoreError};
use serde_json::{json, Value};
use sui_sdk_types::Address;

use crate::envelope::{api_err_typed, api_ok};
use crate::state::AppState;
use crate::studio_api;
use crate::studio_setup::{grant_plan, setup_context, SetupContext};

fn address(s: &str) -> Result<Address, String> {
    s.parse().map_err(|_| format!("invalid Sui address: {s}"))
}

fn text(fields: &Value, name: &str) -> Option<String> {
    let value = fields.get(name)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// The grant the owner of `request.walletId` is asked to sign for running `skill`, and its message.
///
/// The request names the action, the wallet and the limits; the agent and its capability are read
/// from the wallet, so a grant can only be prepared for the agent the wallet actually names.
pub async fn prepare_grant(
    request: &Value,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
    current_revision: impl Fn(&str, &str, &str) -> u64,
) -> Result<Value, String> {
    rill_mcp::assert_keyless_arguments(request)?;
    let wallet_id = address(
        request["walletId"]
            .as_str()
            .ok_or("walletId must be a nonempty string")?,
    )?;
    let wallet = chain
        .get_object(&wallet_id.to_string())
        .await
        .map_err(|e| format!("reading the wallet: {e}"))?;
    let fields = wallet.fields.ok_or("wallet fields are unavailable")?;
    let agent = text(&fields, "agent").ok_or("the wallet names no agent")?;
    let cap = text(&fields, "cap_id").ok_or("the wallet names no agent capability")?;

    let mut bind = json!({
        "skillId": skill.id,
        "sender": owner,
        "agent": agent,
        "agentCapId": cap,
        "walletId": wallet_id.to_string(),
    });
    for key in [
        "budgetMist",
        "perTxMist",
        "minimumRemainingMist",
        "expiresAtMs",
        "balanceManagerId",
        "tradeCapId",
        "depositCapId",
        "price",
    ] {
        if let Some(value) = request.get(key) {
            bind[key] = value.clone();
        }
    }
    let mut grant = grant_plan(&bind, skill, owner, context, chain).await?;
    grant.revision = current_revision(&grant.agent, &grant.wallet_id, &grant.action_id) + 1;
    let message = grant.message();
    Ok(json!({ "grant": grant, "message": message }))
}

/// Accept a signed grant for storage, or say why not.
///
/// Every check a signer will repeat is made here too, so a grant that would be refused at use is
/// refused at submission, while the owner is still at the screen that can fix it.
pub async fn accept_grant(
    signed: &SignedGrant,
    skill: &PublishedSkill,
    owner: &str,
    context: &SetupContext,
    chain: &impl SuiRead,
    current_revision: u64,
) -> Result<(), String> {
    let grant = &signed.grant;
    let owner_address = address(owner)?;
    if skill.id != grant.action_id {
        return Err("the grant names a different action".into());
    }
    if skill.owner.as_deref().map(address).transpose()? != Some(owner_address) {
        return Err("the action must belong to the signed-in owner".into());
    }
    let network = serde_json::to_value(context.network)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    if grant.network != network {
        return Err(format!(
            "the grant is for {}, and this deployment serves {network}",
            grant.network
        ));
    }
    let (package, _) = rill_ptb::deployments::wallet_deployment(
        context.network,
        context.wallet_package_id.as_deref(),
        context.wallet_version_id.as_deref(),
    )?;
    if address(&grant.wallet_package_id)? != package {
        return Err("the grant names another agent_wallet deployment".into());
    }
    if grant.is_expired(context.now_ms) {
        return Err("the grant has already expired".into());
    }
    if grant.revision <= current_revision {
        return Err(format!(
            "revision {} is not newer than revision {current_revision} already granted",
            grant.revision
        ));
    }

    let wallet = chain
        .get_object(&grant.wallet_id)
        .await
        .map_err(|e| format!("reading the wallet: {e}"))?;
    let fields = wallet.fields.ok_or("wallet fields are unavailable")?;
    let wallet_owner = text(&fields, "owner").ok_or("the wallet names no owner")?;
    if address(&wallet_owner)? != owner_address {
        return Err("the wallet belongs to another owner".into());
    }
    let wallet_agent = text(&fields, "agent").ok_or("the wallet names no agent")?;
    if address(&wallet_agent)? != address(&grant.agent)? {
        return Err("the wallet's agent is not the agent this grant names".into());
    }
    if fields.get("revoked") == Some(&Value::Bool(true)) {
        return Err("the wallet has been revoked".into());
    }

    match chain
        .verify_personal_message(
            grant.message().as_bytes(),
            &signed.signature,
            &owner_address.to_string(),
        )
        .await
        .map_err(|e| format!("the node could not verify the signature: {e}"))?
    {
        SignatureCheck::Valid => Ok(()),
        SignatureCheck::Invalid(why) => Err(format!(
            "the signature is not the owner's over this grant: {why}"
        )),
    }
}

fn signed_in(state: &AppState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    match studio_api::owner(state, headers)? {
        Some(owner) => Ok(owner),
        None => Err(Box::new(api_err_typed(
            StatusCode::UNAUTHORIZED,
            "Sign in with the wallet that owns this action",
            "Unauthorized",
        ))),
    }
}

/// `POST /api/grants/prepare`: the grant and the exact message for the owner to sign.
pub async fn prepare(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request = match studio_api::parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let owner = match signed_in(&state, &headers) {
        Ok(owner) => owner,
        Err(e) => return *e,
    };
    let Some(skill) = request["actionId"]
        .as_str()
        .and_then(|id| state.skills.get(id))
    else {
        return api_err_typed(StatusCode::NOT_FOUND, "Action not found", "NotFound");
    };
    let context = match setup_context(&state, &skill).await {
        Ok(context) => context,
        Err(e) => return *e,
    };
    let grants = state.grants.clone();
    match prepare_grant(
        &request,
        &skill,
        &owner,
        &context,
        state.chain.as_ref(),
        |agent, wallet, action| grants.current_revision(agent, wallet, action),
    )
    .await
    {
        Ok(value) => api_ok(value),
        Err(e) => studio_api::invalid(e),
    }
}

/// `POST /api/grants`: store a grant its wallet's owner signed.
pub async fn store(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request = match studio_api::parse_body(&body) {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let owner = match signed_in(&state, &headers) {
        Ok(owner) => owner,
        Err(e) => return *e,
    };
    let signed: SignedGrant = match serde_json::from_value(request) {
        Ok(signed) => signed,
        Err(e) => return studio_api::invalid(format!("not a signed grant: {e}")),
    };
    let Some(skill) = state.skills.get(&signed.grant.action_id) else {
        return api_err_typed(StatusCode::NOT_FOUND, "Action not found", "NotFound");
    };
    let context = match setup_context(&state, &skill).await {
        Ok(context) => context,
        Err(e) => return *e,
    };
    let current = state.grants.current_revision(
        &signed.grant.agent,
        &signed.grant.wallet_id,
        &signed.grant.action_id,
    );
    if let Err(e) = accept_grant(
        &signed,
        &skill,
        &owner,
        &context,
        state.chain.as_ref(),
        current,
    )
    .await
    {
        return studio_api::invalid(e);
    }
    match state.grants.save(signed.clone()) {
        Ok(()) => api_ok(json!({
            "stored": true,
            "agent": signed.grant.agent,
            "actionId": signed.grant.action_id,
            "revision": signed.grant.revision,
        })),
        Err(e @ StoreError::StaleRevision { .. }) => {
            api_err_typed(StatusCode::CONFLICT, e.to_string(), "StaleRevision")
        }
        Err(e) => api_err_typed(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
            "StoreError",
        ),
    }
}

/// `GET /api/grants/{agent}`: every grant held for one agent. Public: grants carry public ids and
/// a signature, nothing secret, and a signer reads its own without a session.
pub async fn list(State(state): State<AppState>, Path(agent): Path<String>) -> Response {
    let agent = match address(&agent) {
        Ok(agent) => agent.to_string(),
        Err(e) => return studio_api::invalid(e),
    };
    api_ok(json!({ "agent": agent, "grants": state.grants.list_for_agent(&agent) }))
}
