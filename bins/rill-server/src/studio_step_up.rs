//! The owner's step-up approval, relayed between an agent's signer and Studio.
//!
//! Above the threshold a grant names, the signer pins a transaction and asks for the owner's own
//! approval of it. This server only carries the request and the answer: it checks each before
//! holding it, so an owner is never shown a request the signer would refuse and never told their
//! approval was taken when it was not, but the signer checks everything again and trusts none of
//! it. No Studio session is needed on either side: the agent's signature opens a request, and the
//! owner's signature is the approval.
//!
//! What it binds: the MCP path and a compromised builder. Whoever can read the agent's key can
//! sign without asking the signer, bounded only by the wallet's on-chain rules. A chain-level
//! variant, with the owner as co-signer, needs a package upgrade and is not built.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::Response,
};
use rill_chain::{SignatureCheck, SuiRead};
use rill_core::grant::SignedGrant;
use rill_core::step_up::StepUpApproval;
use rill_store::step_up::{StepUpEntry, StepUpStore};
use rill_store::StoreError;
use serde_json::{json, Value};
use sui_sdk_types::Address;

use crate::envelope::{api_err, api_ok};
use crate::state::AppState;
use crate::studio_api;

fn same_address(a: &str, b: &str) -> bool {
    matches!(
        (a.parse::<Address>(), b.parse::<Address>()),
        (Ok(a), Ok(b)) if a == b
    )
}

fn text(fields: &Value, name: &str) -> Option<String> {
    let value = fields.get(name)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// A wallet field, read from the chain now.
async fn wallet_field(chain: &impl SuiRead, wallet_id: &str, name: &str) -> Result<String, String> {
    let wallet = chain
        .get_object(wallet_id)
        .await
        .map_err(|e| format!("reading the wallet: {e}"))?;
    wallet
        .fields
        .as_ref()
        .and_then(|fields| text(fields, name))
        .ok_or_else(|| format!("the wallet names no {name}"))
}

/// Check a signer's request before it is shown to anyone, and return the approval and its bytes.
///
/// In order: no key-shaped argument; the bytes are the ones the approval pins and request the
/// amount it names; it lives no longer than an envelope may; the wallet names this agent now; the
/// grant it names is the one held here; and the agent signed this exact request.
pub async fn check_request(
    body: &Value,
    chain: &impl SuiRead,
    network: &str,
    stored: &[SignedGrant],
    now_ms: u64,
) -> Result<(StepUpApproval, String), String> {
    rill_mcp::assert_keyless_arguments(body)?;
    let approval: StepUpApproval = serde_json::from_value(body["approval"].clone())
        .map_err(|e| format!("not a step-up approval: {e}"))?;
    let unsigned_ptb = body["unsignedPtb"]
        .as_str()
        .ok_or("unsignedPtb must be the transaction as base64")?;
    let agent_signature = body["agentSignature"]
        .as_str()
        .ok_or("agentSignature must be the agent's signature over the request")?;

    if approval.network != network {
        return Err(format!(
            "the request is for {}, and this deployment serves {network}",
            approval.network
        ));
    }
    if rill_core::envelope::digest_unsigned_ptb(unsigned_ptb) != approval.pinned_digest {
        return Err("the transaction is not the one the request's pinned digest names".into());
    }
    let decoded = rill_policy::decode::decode(unsigned_ptb).map_err(|e| e.to_string())?;
    if rill_policy::decode::transaction_digest(unsigned_ptb).map_err(|e| e.to_string())?
        != approval.transaction_digest
    {
        return Err("the transaction digest does not match the transaction's bytes".into());
    }
    if decoded.requested_spend_base_units.map(|n| n.to_string())
        != Some(approval.spend_base_units.clone())
    {
        return Err(format!(
            "the transaction does not request {} base units, so the owner would be shown an \
             amount its bytes do not carry",
            approval.spend_base_units
        ));
    }
    let expires = approval
        .expires_at_ms
        .parse::<u64>()
        .map_err(|_| "expiresAtMs is not a time")?;
    if expires <= now_ms || expires - now_ms > rill_policy::MAX_TTL_MS {
        return Err(format!(
            "a request must expire within {} minutes, as its envelope does",
            rill_policy::MAX_TTL_MS / 60_000
        ));
    }

    let wallet_agent = wallet_field(chain, &approval.wallet_id, "agent").await?;
    if !same_address(&wallet_agent, &approval.agent) {
        return Err("the wallet does not name this agent".into());
    }
    let granted = stored.iter().any(|signed| {
        let grant = &signed.grant;
        grant.action_id == approval.action_id
            && same_address(&grant.agent, &approval.agent)
            && same_address(&grant.wallet_id, &approval.wallet_id)
            && grant.revision == approval.grant_revision
            && grant.digest() == approval.grant_digest
    });
    if !granted {
        return Err("no grant held here matches the revision and digest this request names".into());
    }
    match chain
        .verify_personal_message(
            approval.request_message().as_bytes(),
            agent_signature,
            &approval.agent,
        )
        .await
        .map_err(|e| format!("the node could not verify the agent's signature: {e}"))?
    {
        SignatureCheck::Valid => Ok((approval, unsigned_ptb.to_owned())),
        SignatureCheck::Invalid(why) => Err(format!(
            "the request is not signed by the agent it names: {why}"
        )),
    }
}

/// Store the owner's signature on request `id`, only when it is the wallet owner's over this
/// approval's text. A refusal leaves the request as it was, so the owner sees the reason while
/// still on the page that can try again.
pub async fn approve(
    store: &StepUpStore,
    id: &str,
    signature: &str,
    chain: &impl SuiRead,
    now_ms: u64,
) -> Result<StepUpEntry, (StatusCode, String)> {
    let entry = store
        .get(id, now_ms)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "this approval request is not held here, or it expired".to_owned(),
        ))?;
    if entry.owner_signature.is_some() {
        return Err((
            StatusCode::CONFLICT,
            "this request is already approved".into(),
        ));
    }
    let owner = wallet_field(chain, &entry.approval.wallet_id, "owner")
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    match chain
        .verify_personal_message(entry.approval.message().as_bytes(), signature, &owner)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("the node could not verify the signature: {e}"),
            )
        })? {
        SignatureCheck::Valid => {}
        SignatureCheck::Invalid(why) => {
            return Err((
                StatusCode::FORBIDDEN,
                format!("the signature is not the wallet owner's over this approval: {why}"),
            ))
        }
    }
    store.approve(id, signature, now_ms).map_err(|e| match e {
        StoreError::Conflict(why) => (StatusCode::CONFLICT, why),
        StoreError::NotFound(why) => (StatusCode::NOT_FOUND, why),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    })
}

/// `POST /api/step-up`: open a request the agent signed. Answers the link the owner opens.
pub async fn open(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match studio_api::parse_body(&body) {
        Ok(body) => body,
        Err(e) => return *e,
    };
    let agent = body["approval"]["agent"].as_str().unwrap_or_default();
    let stored = state.grants.list_for_agent(agent);
    let now = studio_api::now_ms();
    let (approval, unsigned_ptb) = match check_request(
        &body,
        state.chain.as_ref(),
        state.config.network.as_str(),
        &stored,
        now,
    )
    .await
    {
        Ok(checked) => checked,
        Err(e) => return api_err(StatusCode::BAD_REQUEST, e),
    };
    let id = rill_auth::tokens::random_id();
    let message = approval.message();
    match state
        .step_ups
        .prepare(id.clone(), approval, unsigned_ptb, now)
    {
        Ok(()) => api_ok(json!({
            "id": id,
            "approvalUrl": format!("{}/approve?request={id}", state.config.studio_origin()),
            "message": message,
        })),
        Err(StoreError::AtCapacity { limit })
            if limit == rill_store::step_up::MAX_PENDING_STEP_UPS_PER_AGENT =>
        {
            api_err(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "This agent already has {limit} approval requests pending. Wait for one to be \
                     approved or to expire."
                ),
            )
        }
        Err(StoreError::AtCapacity { .. }) => api_err(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many approval requests are pending. Wait for one to be approved or to expire.",
        ),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `GET /api/step-up/{id}`: the request, its exact text and its bytes. Public: everything in it is
/// public, and the signer and Studio both read it without a session.
pub async fn show(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.step_ups.get(&id, studio_api::now_ms()) {
        Ok(Some(entry)) => {
            let mut shown = json!({
                "id": id,
                "approval": entry.approval,
                "message": entry.approval.message(),
                "unsignedPtb": entry.unsigned_ptb,
                "status": if entry.owner_signature.is_some() { "approved" } else { "pending" },
                "approvalUrl": format!("{}/approve?request={id}", state.config.studio_origin()),
            });
            if let Some(signature) = entry.owner_signature {
                shown["ownerSignature"] = json!(signature);
            }
            api_ok(shown)
        }
        Ok(None) => api_err(
            StatusCode::NOT_FOUND,
            "this approval request is not held here, or it expired",
        ),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/step-up/{id}/approve`: the owner's signature over the approval's text.
pub async fn approve_route(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let body = match studio_api::parse_body(&body) {
        Ok(body) => body,
        Err(e) => return *e,
    };
    let Some(signature) = body["signature"].as_str() else {
        return api_err(
            StatusCode::BAD_REQUEST,
            "signature must be the owner's signature over the approval's message",
        );
    };
    match approve(
        &state.step_ups,
        &id,
        signature,
        state.chain.as_ref(),
        studio_api::now_ms(),
    )
    .await
    {
        Ok(entry) => api_ok(json!({
            "id": id,
            "status": "approved",
            "expiresAtMs": entry.approval.expires_at_ms,
        })),
        Err((status, message)) => api_err(status, message),
    }
}
