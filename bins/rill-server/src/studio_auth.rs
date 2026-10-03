//! Wallet sessions and consent use single-use challenges with the same identity proof.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    Json,
};
use rill_auth::{
    siws::{build_sign_in_message, verify_sign_in_signature, SignInMessage},
    tokens::{random_id, sign_token, TokenClaims, TokenKind},
};
use rill_store::{AuthorizationCode, AuthorizationRequest, OAuthStore, RequestKind};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::{
    build::format_rfc3339_ms,
    envelope::{api_ok, oauth_err},
    state::AppState,
};

pub(crate) const REQUEST_TTL_MS: u64 = 10 * 60 * 1000;
const ACCESS_TTL_SECS: u64 = 60 * 60;
const STUDIO_CLIENT_ID: &str = "rill_studio";

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn message(
    state: &AppState,
    client_name: &str,
    resource: &str,
    scope: &str,
    expires_at: u64,
) -> Result<String, String> {
    let base =
        Url::parse(state.config.base()).map_err(|_| "PUBLIC_BASE_URL must be a valid URL")?;
    let domain = base[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
    Ok(build_sign_in_message(&SignInMessage {
        domain: &domain,
        client_name,
        resource,
        scope,
        nonce: &random_id(),
        issued_at: &format_rfc3339_ms(now_ms()),
        expires_at: &format_rfc3339_ms(expires_at),
    }))
}

pub async fn wallet_challenge(State(state): State<AppState>) -> Response {
    let resource = state.config.resource();
    let expires_at = now_ms() + REQUEST_TTL_MS;
    let message = match message(&state, "Rill Studio", &resource, "mcp", expires_at) {
        Ok(message) => message,
        Err(error) => return oauth_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", error),
    };
    let request = AuthorizationRequest {
        request_id: random_id(),
        kind: RequestKind::Studio,
        client_id: STUDIO_CLIENT_ID.into(),
        client_name: Some("Rill Studio".into()),
        redirect_uri: String::new(),
        state: None,
        scope: "mcp".into(),
        code_challenge: String::new(),
        resource,
        message,
        expires_at,
    };
    if let Err(error) = state.oauth.save_request(request.clone()) {
        return oauth_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            error.to_string(),
        );
    }
    api_ok(
        json!({ "challengeId": request.request_id, "message": request.message,
        "expiresAt": format_rfc3339_ms(request.expires_at) }),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletTokenRequest {
    challenge_id: String,
    signature: String,
}

pub async fn wallet_token(
    State(state): State<AppState>,
    body: Option<Json<WalletTokenRequest>>,
) -> Response {
    let Some(Json(input)) = body else {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "challengeId and signature are required",
        );
    };
    let (request, address) = match complete(
        &state,
        &input.challenge_id,
        &input.signature,
        RequestKind::Studio,
    ) {
        Ok(result) => result,
        Err(response) => return *response,
    };
    let claims = TokenClaims {
        t: TokenKind::Access,
        sub: address.clone(),
        cid: request.client_id,
        scope: request.scope,
        aud: request.resource,
        exp: now_ms() / 1000 + ACCESS_TTL_SECS,
        jti: random_id(),
    };
    match sign_token(&claims, &state.config.oauth_secret) {
        Ok(token) => api_ok(json!({ "access_token": token, "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS, "address": address })),
        Err(error) => oauth_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            error.to_string(),
        ),
    }
}

pub async fn consent_prompt(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
) -> Response {
    let Some(request) = state
        .oauth
        .get_request(&request_id, now_ms())
        .filter(|request| request.kind == RequestKind::Agent)
    else {
        return missing_request();
    };
    api_ok(
        json!({ "requestId": request.request_id, "clientName": request.client_name.as_deref().unwrap_or("an AI agent"),
        "scope": request.scope, "resource": request.resource, "network": state.config.network.as_str(),
        "message": request.message, "expiresAt": format_rfc3339_ms(request.expires_at) }),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentRequest {
    request_id: String,
    signature: String,
}

pub async fn complete_consent(
    State(state): State<AppState>,
    body: Option<Json<ConsentRequest>>,
) -> Response {
    let Some(Json(input)) = body else {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "requestId and signature are required",
        );
    };
    let (request, address) = match complete(
        &state,
        &input.request_id,
        &input.signature,
        RequestKind::Agent,
    ) {
        Ok(result) => result,
        Err(response) => return *response,
    };
    let mut redirect = match Url::parse(&request.redirect_uri) {
        Ok(url) => url,
        Err(_) => {
            return oauth_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Stored redirect URI is invalid",
            )
        }
    };
    let code = AuthorizationCode {
        code: random_id(),
        client_id: request.client_id,
        redirect_uri: request.redirect_uri,
        code_challenge: request.code_challenge,
        sub: address.clone(),
        scope: request.scope,
        resource: request.resource,
        expires_at: now_ms() + 60_000,
    };
    if let Err(error) = state.oauth.save_code(code.clone()) {
        return oauth_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            error.to_string(),
        );
    }
    // Replace reserved response parameters if a registered callback already has them.
    let existing: Vec<(String, String)> = redirect
        .query_pairs()
        .filter(|(key, _)| key != "code" && key != "state")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    redirect.set_query(None);
    redirect
        .query_pairs_mut()
        .extend_pairs(existing)
        .append_pair("code", &code.code);
    if let Some(state) = request.state {
        redirect.query_pairs_mut().append_pair("state", &state);
    }
    api_ok(json!({ "redirectTo": redirect.as_str(), "address": address }))
}

fn missing_request() -> Response {
    oauth_err(
        StatusCode::NOT_FOUND,
        "invalid_request",
        "This sign-in request has expired or was already used. Start again.",
    )
}

// Verify before consuming so a cancelled or incorrect wallet signature can be retried. The
// atomic take after verification prevents concurrent submissions from issuing two credentials.
fn complete(
    state: &AppState,
    id: &str,
    signature: &str,
    kind: RequestKind,
) -> Result<(AuthorizationRequest, String), Box<Response>> {
    let pending = state
        .oauth
        .get_request(id.trim(), now_ms())
        .filter(|request| request.kind == kind)
        .ok_or_else(missing_request)?;
    let address = verify_sign_in_signature(&pending.message, signature).map_err(|error| {
        oauth_err(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            error.to_string(),
        )
    })?;
    let request = state
        .oauth
        .take_request(id.trim(), now_ms())
        .ok_or_else(|| {
            oauth_err(
                StatusCode::CONFLICT,
                "invalid_request",
                "This sign-in request was already completed or expired.",
            )
        })?;
    Ok((request, address))
}
