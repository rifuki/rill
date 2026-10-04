//! Owner-created, agent-proved, owner-confirmed signer pairing. No spend authority is issued here.
use crate::{
    envelope::{api_err, api_ok},
    state::AppState,
    studio_auth::now_ms,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use rill_auth::tokens::{bearer_from_header, random_id, verify_bearer, TokenKind};
use rill_chain::{SignatureCheck, SuiRead};
use rill_store::pairing::PairingRequest;
use serde::Deserialize;
use serde_json::json;

fn owner(state: &AppState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    let bearer = bearer_from_header(headers.get("authorization").and_then(|h| h.to_str().ok()))
        .ok_or_else(|| Box::new(api_err(StatusCode::UNAUTHORIZED, "owner session required")))?;
    let claims = verify_bearer(
        bearer,
        &state.config.oauth_secret,
        &state.config.resource(),
        now_ms() / 1000,
    )
    .map_err(|e| Box::new(api_err(StatusCode::UNAUTHORIZED, e.to_string())))?;
    if claims.t != TokenKind::Access
        || claims.cid != "rill_studio"
        || !claims.scope.split_whitespace().any(|s| s == "mcp")
    {
        return Err(Box::new(api_err(
            StatusCode::FORBIDDEN,
            "pairing requires the owner's Studio wallet session",
        )));
    }
    let address = claims
        .sub
        .parse::<sui_sdk_types::Address>()
        .map_err(|_| Box::new(api_err(StatusCode::UNAUTHORIZED, "invalid owner address")))?;
    Ok(address.to_string())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepare {
    agent: String,
    network: String,
}
pub async fn prepare(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Prepare>,
) -> Response {
    let owner = match owner(&state, &headers) {
        Ok(o) => o,
        Err(e) => return *e,
    };
    let agent = match body.agent.parse::<sui_sdk_types::Address>() {
        Ok(a) => a.to_string(),
        Err(_) => return api_err(StatusCode::BAD_REQUEST, "invalid agent address"),
    };
    if body.network != state.config.network.as_str() {
        return api_err(
            StatusCode::BAD_REQUEST,
            "pairing network differs from this server",
        );
    }
    if owner == agent {
        return api_err(
            StatusCode::BAD_REQUEST,
            "agent signer must differ from owner wallet",
        );
    }
    let request = PairingRequest {
        request_id: random_id(),
        owner,
        agent,
        network: body.network,
        domain: state.config.base().trim_end_matches('/').into(),
        nonce: random_id(),
        expires_at: now_ms() + 600_000,
        proved: false,
    };
    match state.pairing.prepare(request.clone(), now_ms()) {
        Ok(()) => api_ok(
            json!({"requestId":request.request_id,"message":request.message(),"expiresAt":request.expires_at}),
        ),
        Err(rill_store::StoreError::AtCapacity { .. }) => api_err(StatusCode::TOO_MANY_REQUESTS,
            "Too many pending pairing requests. Confirm an existing request or wait for it to expire."),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
pub async fn challenge(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.pairing.get(&id, now_ms()) {
        Ok(Some(r)) => api_ok(
            json!({"requestId":r.request_id,"owner":r.owner,"agent":r.agent,"network":r.network,"domain":r.domain,"nonce":r.nonce,"expiresAt":r.expires_at,"message":r.message(),"status":if r.proved{"proved"}else{"pending"}}),
        ),
        Ok(None) => api_err(StatusCode::NOT_FOUND, "pairing request missing or expired"),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proof {
    request_id: String,
    agent: String,
    signature: String,
}
pub async fn prove(State(state): State<AppState>, Json(body): Json<Proof>) -> Response {
    let agent = match body.agent.parse::<sui_sdk_types::Address>() {
        Ok(a) => a.to_string(),
        Err(_) => return api_err(StatusCode::BAD_REQUEST, "invalid agent address"),
    };
    let r = match state.pairing.get(&body.request_id, now_ms()) {
        Ok(Some(r)) => r,
        Ok(None) => return api_err(StatusCode::NOT_FOUND, "pairing request missing or expired"),
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if r.agent != agent || r.proved {
        return api_err(
            StatusCode::CONFLICT,
            "signer mismatch or proof already used",
        );
    }
    if let Err(error) = verify_proof(&r, &agent, &body.signature, state.chain.as_ref()).await {
        return api_err(StatusCode::FORBIDDEN, error);
    }
    match state.pairing.prove(&body.request_id, &agent, now_ms()) {
        Ok(()) => api_ok(json!({"requestId":body.request_id,"status":"proved"})),
        Err(e) => api_err(StatusCode::CONFLICT, e.to_string()),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Confirm {
    request_id: String,
}
pub async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Confirm>,
) -> Response {
    let owner = match owner(&state, &headers) {
        Ok(o) => o,
        Err(e) => return *e,
    };
    match state.pairing.confirm(&body.request_id, &owner, now_ms()) {
        Ok(agent) => api_ok(agent),
        Err(e) => api_err(StatusCode::CONFLICT, e.to_string()),
    }
}
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let owner = match owner(&state, &headers) {
        Ok(o) => o,
        Err(e) => return *e,
    };
    match state.pairing.list(&owner) {
        Ok(agents) => api_ok(agents),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Verify the exact domain-bound challenge against the claimed signer using the node's verifier.
pub async fn verify_proof(
    request: &PairingRequest,
    agent: &str,
    signature: &str,
    chain: &impl SuiRead,
) -> Result<(), String> {
    if request.agent != agent || request.proved {
        return Err("signer mismatch or proof already used".into());
    }
    match chain
        .verify_personal_message(request.message().as_bytes(), signature, agent)
        .await
        .map_err(|e| e.to_string())?
    {
        SignatureCheck::Valid => Ok(()),
        SignatureCheck::Invalid(_) => Err("invalid agent signature".into()),
    }
}
