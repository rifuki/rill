//! The HTTP contract the deployed frontend already parses.
//!
//! R4 says that client runs against this server unchanged, so these tests are about compatibility
//! rather than about what a nicer API would look like. The two error shapes in particular are not
//! a design choice being defended — they are what the client reads today.
//!
//! Handlers are exercised through `tower::ServiceExt::oneshot`, so no socket is bound and no port
//! is needed.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

#[path = "../src/envelope.rs"]
mod envelope;
#[path = "../src/routes.rs"]
mod routes;
#[path = "../src/state.rs"]
mod state;

use state::{AppState, Config, Network};

fn app() -> axum::Router {
    let dir = std::env::temp_dir().join(format!("rill-server-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: "https://api.rill.test".into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: "test-secret".into(),
        oauth_secret_from_env: true,
        guard_package_id: Some("0xguard".into()),
        skills_store_path: dir.join("skills.json").to_string_lossy().into(),
        oauth_store_path: dir.join("oauth.json").to_string_lossy().into(),
    };
    routes::router(AppState::new(config))
}

async fn get(path: &str) -> (StatusCode, Value, axum::http::HeaderMap) {
    let response = app()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json, headers)
}

#[tokio::test]
async fn health_is_a_bare_object_with_no_envelope() {
    let (status, body, _) = get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("success").is_none(),
        "the client reads /health directly; an envelope here would break it"
    );
    assert_eq!(body["mcp"]["endpoint"], "https://api.rill.test/mcp");
    assert_eq!(body["keyless"], true);
}

/// An operator should be able to see whether this deployment's tokens survive a restart.
#[tokio::test]
async fn health_reports_whether_tokens_are_durable() {
    let (_, body, _) = get("/health").await;
    assert_eq!(body["mcp"]["tokensDurable"], true);
}

/// The client rejects any response where `success` is true but `data` is missing.
#[tokio::test]
async fn every_api_success_carries_data() {
    for path in ["/api/skills", "/api/protocols"] {
        let (status, body, _) = get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body["success"], true, "{path}");
        assert!(
            body.get("data").is_some_and(|d| !d.is_null()),
            "{path}: a success with no data reads to the client as a failure"
        );
    }
}

/// The two shapes. `/api/*` puts the message in `error`.
#[tokio::test]
async fn an_api_error_uses_the_error_field() {
    let (status, body, _) = get("/api/skills/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["success"], false);
    assert!(body["error"].is_string());
    assert!(
        body.get("error_description").is_none(),
        "that field belongs to the /oauth/* shape"
    );
}

/// And `/oauth/*` — here reached through the MCP 401 — puts it in `error_description`.
#[tokio::test]
async fn an_oauth_style_error_uses_error_description() {
    let response = app()
        .oneshot(Request::post("/mcp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["error"].is_string(), "the OAuth error code");
    assert!(
        body["error_description"].is_string(),
        "the client reads the message from error_description on /oauth/* and /mcp"
    );
}

/// Without this header a client cannot discover where to authenticate, and reports a dead
/// connector instead of starting the OAuth flow.
#[tokio::test]
async fn every_mcp_401_carries_the_discovery_header() {
    let response = app()
        .oneshot(Request::post("/mcp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let value = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .expect("a 401 without WWW-Authenticate is a dead end")
        .to_str()
        .unwrap()
        .to_string();
    assert!(value.contains("resource_metadata="));
    assert!(value.contains("/.well-known/oauth-protected-resource"));
}

/// RFC 8414 and RFC 9728 define these as origin-relative. Served one prefix deeper they would be
/// invisible to every MCP client, and the failure would look like an unreachable connector.
#[tokio::test]
async fn discovery_documents_live_at_the_origin_root() {
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-authorization-server/mcp",
    ] {
        let (status, body, _) = get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.is_object(), "{path}");
    }
}

#[tokio::test]
async fn the_authorization_server_advertises_s256_and_public_clients_only() {
    let (_, body, _) = get("/.well-known/oauth-authorization-server").await;
    assert_eq!(
        body["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    assert_eq!(
        body["token_endpoint_auth_methods_supported"],
        serde_json::json!(["none"]),
        "a secret shipped to a desktop agent is not a secret"
    );
    assert_eq!(body["resource_indicators_supported"], true);
}

#[tokio::test]
async fn the_protected_resource_points_at_this_deployments_mcp_endpoint() {
    let (_, body, _) = get("/.well-known/oauth-protected-resource").await;
    assert_eq!(body["resource"], "https://api.rill.test/mcp");
}

/// Honest, not unimplemented: there is nothing to introspect, and an empty result would imply the
/// package had no functions.
#[tokio::test]
async fn introspect_is_a_documented_501() {
    let response = app()
        .oneshot(
            Request::post("/api/introspect")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}

/// A preflight that omits these fails before the request is ever seen, which looks like an
/// unreachable server rather than a CORS rejection.
#[tokio::test]
async fn preflight_allows_the_headers_an_mcp_client_sends() {
    let response = app()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/mcp")
                .header(header::ORIGIN, "https://rill.test")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,mcp-protocol-version,content-type",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let allowed = response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
        .map(|v| v.to_str().unwrap().to_ascii_lowercase())
        .unwrap_or_default();
    assert!(allowed.contains("authorization"), "got: {allowed}");
    assert!(allowed.contains("mcp-protocol-version"), "got: {allowed}");
}

/// An unexposed response header is invisible to browser JavaScript, so the discovery pointer
/// would never reach the client that needs it.
#[tokio::test]
async fn www_authenticate_is_exposed_to_the_browser() {
    let response = app()
        .oneshot(
            Request::get("/health")
                .header(header::ORIGIN, "https://rill.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let exposed = response
        .headers()
        .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .map(|v| v.to_str().unwrap().to_ascii_lowercase())
        .unwrap_or_default();
    assert!(exposed.contains("www-authenticate"), "got: {exposed}");
}

#[tokio::test]
async fn a_body_above_the_cap_is_refused() {
    let big = "x".repeat(600 * 1024);
    let response = app()
        .oneshot(
            Request::post("/api/introspect")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(big))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ── boot refusals ──

fn mainnet_config(secret: &str, guard: Option<&str>) -> Config {
    Config {
        port: 3939,
        network: Network::Mainnet,
        public_base_url: "https://api.rill.test".into(),
        sui_rpc_url: "https://fullnode.mainnet.sui.io:443".into(),
        oauth_secret: secret.into(),
        oauth_secret_from_env: !secret.is_empty(),
        guard_package_id: guard.map(str::to_owned),
        skills_store_path: "/tmp/x.json".into(),
        oauth_store_path: "/tmp/y.json".into(),
    }
}

#[test]
fn mainnet_refuses_to_start_without_a_durable_signing_secret() {
    let err = mainnet_config("", Some("0xguard"))
        .boot_check()
        .unwrap_err();
    assert!(err.contains("RILL_OAUTH_SECRET"));
    assert!(
        err.contains("openssl rand"),
        "the refusal should say how to fix it"
    );
}

#[test]
fn mainnet_refuses_to_start_without_a_deployed_guard_package() {
    let err = mainnet_config("s", None).boot_check().unwrap_err();
    assert!(err.contains("RILL_GUARD_PACKAGE_ID"));
}

#[test]
fn testnet_boots_with_neither_so_local_development_needs_no_setup() {
    let mut config = mainnet_config("", None);
    config.network = Network::Testnet;
    assert!(config.boot_check().is_ok());
}
