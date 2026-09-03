//! The HTTP surface.
//!
//! # CORS
//!
//! `Authorization` and `MCP-Protocol-Version` must be **allowed** or the browser preflight fails
//! before a request is ever seen here — which looks like an unreachable server rather than a CORS
//! rejection. `WWW-Authenticate` must be **exposed**, not merely allowed: it carries the discovery
//! pointer a client reads off a 401 to find the authorization server, and an unexposed response
//! header is invisible to browser JavaScript.

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use rill_store::SkillStore;
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::envelope::{api_err, api_ok, bare};
use crate::state::AppState;

/// The frontend hard-caps its own requests, and this caps the other direction. 512 KB is far more
/// than any flow graph and far less than a memory problem.
const MAX_BODY_BYTES: usize = 512 * 1024;

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderValue::from_static("mcp-protocol-version")
                .to_str()
                .unwrap()
                .parse()
                .unwrap(),
        ])
        .expose_headers([
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("mcp-protocol-version")
                .to_str()
                .unwrap()
                .parse()
                .unwrap(),
        ])
        .max_age(std::time::Duration::from_secs(600));

    Router::new()
        // Bare JSON, no envelope — the container healthcheck and the frontend both read this.
        .route("/health", get(health))
        // Discovery must live at the origin root. See the module note in main.rs.
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(authorization_server_metadata),
        )
        // A browser landing here gets the docs; an MCP client probing for a server-push stream
        // gets 405, which its SDK understands. Both answers come before any auth check, because
        // making discovery require a token is how a connector becomes unaddable.
        .route("/mcp", get(mcp_get).post(mcp_post))
        // The four addresses the discovery document has always named. Until now none of them
        // existed, so a client that respected discovery — the point of discovery — got a 404.
        .route("/oauth/register", post(crate::oauth_routes::register))
        .route("/oauth/authorize", get(crate::oauth_routes::authorize))
        .route("/oauth/token", post(crate::oauth_routes::token))
        .route("/oauth/revoke", post(crate::oauth_routes::revoke))
        .route("/api/skills", get(list_skills))
        .route("/api/skills/{id}", get(get_skill))
        .route("/api/protocols", get(protocols))
        .route("/api/introspect", post(introspect))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors)
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Response {
    bare(json!({
        "status": "ok",
        "network": state.config.network.as_str(),
        "keyless": true,
        "docs": state.config.public_base_url,
        "mcp": {
            "endpoint": state.config.resource(),
            "auth": "oauth2.1+pkce+dcr",
            // Whether issued tokens survive a restart. An operator can see at a glance whether
            // this deployment is running on an ephemeral secret.
            "tokensDurable": state.config.oauth_secret_from_env,
        },
        "skills": state.skills.count(),
    }))
}

/// RFC 9728. Points a client that got a 401 at the authorization server it should use.
async fn protected_resource_metadata(State(state): State<AppState>) -> Response {
    bare(json!({
        "resource": state.config.resource(),
        "authorization_servers": [state.config.public_base_url],
        "bearer_methods_supported": ["header"],
        "scopes_supported": rill_auth::oauth::SUPPORTED_SCOPES,
    }))
}

/// RFC 8414. What an MCP client fetches to learn where to register, authorize, and get tokens.
async fn authorization_server_metadata(State(state): State<AppState>) -> Response {
    let base = state.config.public_base_url.trim_end_matches('/');
    bare(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "revocation_endpoint": format!("{base}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        // S256 only. OAuth 2.1 removes `plain`, which protects nothing against anyone who can
        // observe the authorization request.
        "code_challenge_methods_supported": [rill_auth::oauth::CODE_CHALLENGE_METHOD],
        // Public clients only — a secret shipped to a desktop agent is not a secret.
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": rill_auth::oauth::SUPPORTED_SCOPES,
        "resource_indicators_supported": true,
    }))
}

async fn mcp_get(State(state): State<AppState>) -> Response {
    // This server never pushes, so there is no event stream to open.
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("{}/api/docs", state.config.public_base_url),
        )],
    )
        .into_response()
}

use axum::response::IntoResponse as _;

/// The owner-scoped MCP endpoint. Every 401 carries `WWW-Authenticate` so a client can discover
/// where to authenticate rather than reporting a dead connector.
async fn mcp_post(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    crate::mcp::post(state, authorization, body).await
}

/// What you see depends on who you are. Without a token, only skills that have no owner —
/// everything published before ownership existed. A skill id is enough to build against that
/// skill's wallet binding on the public per-skill endpoint, so once ids belong to people,
/// enumerating them all is a real leak rather than a cosmetic one.
async fn list_skills(State(state): State<AppState>) -> Response {
    let visible = state.skills.list_unowned();
    let summaries: Vec<_> = visible
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "description": s.description,
                "createdAt": s.created_at,
            })
        })
        .collect();
    api_ok(json!({ "skills": summaries }))
}

async fn get_skill(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.skills.get(&id) {
        Some(skill) => api_ok(json!({
            "id": skill.id,
            "name": skill.name,
            "description": skill.description,
            "createdAt": skill.created_at,
        })),
        None => api_err(StatusCode::NOT_FOUND, "Skill not found"),
    }
}

async fn protocols(State(state): State<AppState>) -> Response {
    api_ok(json!({
        "network": state.config.network.as_str(),
        "protocols": ["deepbook_limit_order", "cetus_swap", "haedal_stake"],
    }))
}

/// Honest rather than unimplemented: the gRPC client cannot read Move bytecode or an ABI, so
/// there is nothing to introspect. Semantics come from curated manifests instead. Returning 501
/// says that; returning an empty result would imply the package had no functions.
///
/// The body is extracted and discarded on purpose. `DefaultBodyLimit` only refuses a payload when
/// something actually reads it, so a handler that ignores its body silently opts out of the cap —
/// which was not obvious until a test asked for a 413 and got a 501.
async fn introspect(_body: axum::body::Bytes) -> Response {
    api_err(
        StatusCode::NOT_IMPLEMENTED,
        "Introspection is unavailable: the gRPC client cannot read Move bytecode or ABIs. Use the \
         curated semantic manifests instead.",
    )
}
