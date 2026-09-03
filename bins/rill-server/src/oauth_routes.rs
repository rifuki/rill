//! The four OAuth 2.1 endpoints the discovery document promises.
//!
//! # Advertising an endpoint that does not exist is worse than not advertising it
//!
//! `/.well-known/oauth-authorization-server` told every client where to register, authorize and get
//! tokens, and all four addresses returned 404. An MCP client that respects discovery — which is
//! the point of discovery — could not connect at all, and the failure looked like a broken server
//! rather than a missing one. Nine hundred lines of working auth logic sat behind routes nobody had
//! written.
//!
//! # What is deliberately not here
//!
//! No client secret. Dynamic registration issues a public client, and the security comes from PKCE
//! plus an exact redirect-URI match rather than from a shared secret an MCP client would have to
//! store somewhere a model can read.
//!
//! No consent screen. `/oauth/authorize` here binds an authorization code to a subject the operator
//! configures; a hosted deployment puts a human in front of it. That difference is stated in the
//! response rather than hidden, so nobody mistakes this for an approval flow it is not.

use axum::extract::{Query, State};
use axum::response::Response;
use axum::Json;
use rill_auth::oauth::{
    check_redirect_uri_registered, is_allowed_redirect_uri, is_valid_pkce_value, normalize_scope,
    resolve_resource, verify_pkce,
};
use rill_auth::tokens::{random_id, sign_token, verify_token, Expectation, TokenClaims, TokenKind};
use rill_store::{AuthorizationCode, OAuthClient, OAuthStore, RefreshHandle};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::envelope::{oauth_err, oauth_ok};
use crate::state::AppState;

/// How long an access token lives. Short, because a refresh is cheap and a leaked access token is
/// not revocable.
const ACCESS_TTL_SECS: u64 = 60 * 60;
/// How long a refresh handle lives. Rotated on every use.
const REFRESH_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// How long an authorization code lives. Long enough to redirect, short enough to be useless if
/// it leaks into a log or a referrer header.
const CODE_TTL_MS: u64 = 60 * 1000;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn bad_request(code: &str, description: &str) -> Response {
    oauth_err(axum::http::StatusCode::BAD_REQUEST, code, description)
}

// ── /oauth/register — RFC 7591 ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RegisterRequest {
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub scope: Option<String>,
}

/// Register a public client.
///
/// Every redirect URI is checked before anything is stored. An open redirect here is the classic
/// way an authorization code reaches somebody who should not have it, and refusing at registration
/// is cheaper than refusing at every authorize.
pub async fn register(
    State(state): State<AppState>,
    body: Option<Json<RegisterRequest>>,
) -> Response {
    let Some(Json(request)) = body else {
        return bad_request("invalid_client_metadata", "redirect_uris is required");
    };
    if request.redirect_uris.is_empty() {
        return bad_request("invalid_client_metadata", "redirect_uris must not be empty");
    }
    for uri in &request.redirect_uris {
        if !is_allowed_redirect_uri(uri) {
            return bad_request(
                "invalid_redirect_uri",
                &format!("{uri} is not a redirect URI this server will send a code to"),
            );
        }
    }

    let scope = match normalize_scope(request.scope.as_deref().unwrap_or("mcp")) {
        Ok(scope) => scope,
        Err(e) => return bad_request("invalid_client_metadata", &e.description),
    };

    let client = OAuthClient {
        client_id: random_id(),
        client_name: request.client_name,
        redirect_uris: request.redirect_uris,
        scope,
        created_at: chrono_now(),
    };

    if let Err(e) = state.oauth.save_client(client.clone()) {
        return oauth_err(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            e.to_string(),
        );
    }

    oauth_ok(json!({
        "client_id": client.client_id,
        "client_name": client.client_name,
        "redirect_uris": client.redirect_uris,
        "scope": client.scope,
        // Stated rather than omitted: a client that expects one should learn there is none here,
        // not discover it by sending one and being ignored.
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"]
    }))
}

fn chrono_now() -> String {
    // Seconds since the epoch, as text. Not a formatted date: this is compared and sorted, never
    // parsed by a human, and a format is one more thing to disagree about.
    now_secs().to_string()
}

// ── /oauth/authorize ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: Option<String>,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

/// Issue an authorization code bound to a PKCE challenge.
///
/// Returns the code as JSON rather than redirecting. A redirect is what a browser flow needs and
/// this deployment has no consent screen to redirect back from — pretending otherwise would give a
/// client a code it did not have a human approve.
pub async fn authorize(
    State(state): State<AppState>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let Some(client) = state.oauth.get_client(&query.client_id) else {
        return bad_request("invalid_client", "no client with that id is registered");
    };

    if let Err(e) = check_redirect_uri_registered(&query.redirect_uri, &client.redirect_uris) {
        return bad_request(e.code, &e.description);
    }

    // S256 only. `plain` is in the spec and defeats the purpose — an interceptor who has the
    // challenge has the verifier.
    if query.code_challenge_method.as_deref().unwrap_or("S256") != "S256" {
        return bad_request(
            "invalid_request",
            "only S256 is accepted; plain PKCE offers no protection against an interceptor",
        );
    }
    if !is_valid_pkce_value(&query.code_challenge) {
        return bad_request(
            "invalid_request",
            "code_challenge is not a valid PKCE value",
        );
    }

    let scope = match normalize_scope(query.scope.as_deref().unwrap_or(&client.scope)) {
        Ok(scope) => scope,
        Err(e) => return bad_request(e.code, &e.description),
    };
    // The canonical resource is this server's own MCP endpoint: a token minted for it must not be
    // replayable at another server that happens to share the secret.
    let canonical = format!("{}/mcp", state.config.public_base_url);
    let resource = match resolve_resource(
        query.resource.as_deref(),
        &canonical,
        &state.config.public_base_url,
    ) {
        Ok(resource) => resource,
        Err(e) => return bad_request(e.code, &e.description),
    };

    let code = AuthorizationCode {
        code: random_id(),
        client_id: client.client_id.clone(),
        redirect_uri: query.redirect_uri.clone(),
        code_challenge: query.code_challenge,
        // The subject this deployment issues for. A hosted one puts a human here.
        sub: format!("client:{}", client.client_id),
        scope,
        resource,
        expires_at: now_ms() + CODE_TTL_MS,
    };

    if let Err(e) = state.oauth.save_code(code.clone()) {
        return oauth_err(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            e.to_string(),
        );
    }

    oauth_ok(json!({
        "code": code.code,
        "state": query.state,
        "redirect_uri": code.redirect_uri,
        "expires_in": CODE_TTL_MS / 1000,
        "note": "This deployment has no consent screen: the code is returned directly rather than \
                 redirected. A hosted deployment puts a human in front of this endpoint."
    }))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── /oauth/token ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: Option<String>,
    pub code_verifier: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_id: Option<String>,
    pub refresh_token: Option<String>,
    pub resource: Option<String>,
}

/// Exchange a code, or rotate a refresh token.
pub async fn token(State(state): State<AppState>, body: Option<Json<TokenRequest>>) -> Response {
    let Some(Json(request)) = body else {
        return bad_request("invalid_request", "a token request body is required");
    };
    match request.grant_type.as_str() {
        "authorization_code" => authorization_code_grant(state, request).await,
        "refresh_token" => refresh_token_grant(state, request).await,
        other => bad_request(
            "unsupported_grant_type",
            &format!("{other} is not a grant type this server issues"),
        ),
    }
}

async fn authorization_code_grant(state: AppState, request: TokenRequest) -> Response {
    let (Some(code), Some(verifier)) = (request.code, request.code_verifier) else {
        return bad_request(
            "invalid_request",
            "code and code_verifier are both required",
        );
    };

    // Taken, not read: a code is single use, and taking it here means a replay finds nothing even
    // if the rest of this function fails.
    let Some(stored) = state.oauth.take_code(&code, now_ms()) else {
        return bad_request("invalid_grant", "that code is unknown, used, or expired");
    };

    if let Err(e) = verify_pkce(&verifier, &stored.code_challenge) {
        return bad_request(e.code, &e.description);
    }
    if let Some(uri) = &request.redirect_uri {
        if uri != &stored.redirect_uri {
            return bad_request(
                "invalid_grant",
                "redirect_uri does not match the one the code was issued for",
            );
        }
    }
    if let Some(client_id) = &request.client_id {
        if client_id != &stored.client_id {
            return bad_request("invalid_grant", "that code belongs to a different client");
        }
    }

    issue(
        state,
        &stored.sub,
        &stored.client_id,
        &stored.scope,
        &stored.resource,
    )
}

async fn refresh_token_grant(state: AppState, request: TokenRequest) -> Response {
    let Some(refresh) = request.refresh_token else {
        return bad_request("invalid_request", "refresh_token is required");
    };

    let claims = match verify_token(
        &refresh,
        &state.config.oauth_secret,
        Expectation {
            kind: TokenKind::Refresh,
            audience: &format!("{}/mcp", state.config.public_base_url),
            now_secs: now_secs(),
        },
    ) {
        Ok(claims) => claims,
        Err(e) => return bad_request("invalid_grant", &e.to_string()),
    };

    // Rotation: the handle is taken, so replaying an old refresh token finds nothing.
    if state.oauth.take_refresh(&claims.jti, now_ms()).is_none() {
        return bad_request(
            "invalid_grant",
            "that refresh token has already been used or was revoked",
        );
    }

    issue(state, &claims.sub, &claims.cid, &claims.scope, &claims.aud)
}

/// Mint an access token, and a refresh token when the scope asks for one.
fn issue(state: AppState, sub: &str, client_id: &str, scope: &str, resource: &str) -> Response {
    let now = now_secs();
    let access = TokenClaims {
        t: TokenKind::Access,
        sub: sub.to_owned(),
        cid: client_id.to_owned(),
        scope: scope.to_owned(),
        aud: resource.to_owned(),
        exp: now + ACCESS_TTL_SECS,
        jti: random_id(),
    };
    let access_token = match sign_token(&access, &state.config.oauth_secret) {
        Ok(token) => token,
        Err(e) => {
            return oauth_err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                e.to_string(),
            )
        }
    };

    let mut body = json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
        "scope": scope,
    });

    if scope.split_whitespace().any(|s| s == "offline_access") {
        let refresh = TokenClaims {
            t: TokenKind::Refresh,
            sub: sub.to_owned(),
            cid: client_id.to_owned(),
            scope: scope.to_owned(),
            aud: resource.to_owned(),
            exp: now + REFRESH_TTL_SECS,
            jti: random_id(),
        };
        let handle = RefreshHandle {
            jti: refresh.jti.clone(),
            sub: sub.to_owned(),
            client_id: client_id.to_owned(),
            scope: scope.to_owned(),
            resource: resource.to_owned(),
            expires_at: (now + REFRESH_TTL_SECS) * 1000,
        };
        if state.oauth.save_refresh(handle).is_ok() {
            if let Ok(token) = sign_token(&refresh, &state.config.oauth_secret) {
                body["refresh_token"] = Value::String(token);
            }
        }
    }

    oauth_ok(body)
}

// ── /oauth/revoke — RFC 7009 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub token: String,
}

/// Revoke a refresh token.
///
/// RFC 7009 requires 200 whether or not the token existed, so that an attacker cannot use this
/// endpoint to learn which tokens are real.
pub async fn revoke(State(state): State<AppState>, body: Option<Json<RevokeRequest>>) -> Response {
    let Some(Json(request)) = body else {
        return oauth_ok(json!({ "revoked": true }));
    };
    if let Ok(claims) = verify_token(
        &request.token,
        &state.config.oauth_secret,
        Expectation {
            kind: TokenKind::Refresh,
            audience: &format!("{}/mcp", state.config.public_base_url),
            now_secs: now_secs(),
        },
    ) {
        let _ = state.oauth.take_refresh(&claims.jti, now_ms());
    }
    oauth_ok(json!({ "revoked": true }))
}
