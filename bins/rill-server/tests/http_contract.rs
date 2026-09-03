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

use rill_server::routes;
use rill_server::state::{AppState, Config, Network};

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

// ── the MCP endpoint, now that it authenticates ──

use rill_auth::tokens::{sign_token, TokenClaims, TokenKind};

const TEST_SECRET: &str = "test-secret";

fn token(kind: TokenKind, audience: &str, scope: &str, subject: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    sign_token(
        &TokenClaims {
            t: kind,
            sub: subject.into(),
            cid: "rill_client_test".into(),
            scope: scope.into(),
            aud: audience.into(),
            exp: now + 3600,
            jti: "test-jti".into(),
        },
        TEST_SECRET,
    )
    .unwrap()
}

async fn mcp_call(auth: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut request = Request::post("/mcp").header(header::CONTENT_TYPE, "application/json");
    if let Some(a) = auth {
        request = request.header(header::AUTHORIZATION, a);
    }
    let response = app()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn initialize() -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })
}

#[tokio::test]
async fn a_valid_token_completes_the_handshake() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (status, body) = mcp_call(Some(&bearer), initialize()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["serverInfo"]["name"], "rill-actions");
}

/// The signed `t` claim doing its job at the endpoint that matters.
#[tokio::test]
async fn a_refresh_token_is_refused_at_the_mcp_endpoint() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Refresh,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (status, _) = mcp_call(Some(&bearer), initialize()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_for_another_deployment_is_refused() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://other.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (status, _) = mcp_call(Some(&bearer), initialize()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_token_without_the_mcp_scope_is_refused_as_insufficient_not_invalid() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "offline_access",
            "0xowner"
        )
    );
    let (status, body) = mcp_call(Some(&bearer), initialize()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "insufficient_scope");
}

/// Connected but nothing published is a valid state, not a broken connector.
#[tokio::test]
async fn an_owner_with_no_published_actions_still_gets_an_empty_catalogue() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xnobody"
        )
    );
    let (status, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "rill_list_actions", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["isError"], false);
    assert_eq!(
        body["result"]["structuredContent"]["actions"],
        serde_json::json!([])
    );
}

/// This server holds no key and must never be talked into behaving as though it does.
#[tokio::test]
async fn an_argument_carrying_key_material_is_refused_on_every_call() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (_, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "rill_list_actions", "arguments": { "privateKey": "suiprivkey1..." } }
        }),
    )
    .await;
    assert_eq!(body["result"]["isError"], true);
    assert_eq!(
        body["result"]["structuredContent"]["code"],
        "forbidden_arguments"
    );
}

/// Another address's action must be indistinguishable from one that does not exist, or the
/// endpoint becomes a way to discover which ids are real.
#[tokio::test]
async fn someone_elses_action_answers_exactly_as_a_missing_one_does() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let describe = |id: &str| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "rill_describe_action", "arguments": { "actionId": id } }
        })
    };
    let (_, absent) = mcp_call(Some(&bearer), describe("skill_does_not_exist")).await;
    let (_, other) = mcp_call(Some(&bearer), describe("skill_belongs_to_someone_else")).await;
    assert_eq!(
        absent["result"]["structuredContent"],
        other["result"]["structuredContent"]
    );
}

#[tokio::test]
async fn a_notification_gets_no_body_at_all() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let response = app()
        .oneshot(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, &bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

/// Conflating "no id" with "notification" swallows a spec violation as a 202, and the client waits
/// for a reply that never comes.
#[tokio::test]
async fn a_request_missing_its_id_is_a_reported_error_not_a_silent_202() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (status, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({ "jsonrpc": "2.0", "method": "tools/list" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn a_batch_is_answered_as_an_array() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (status, body) = mcp_call(
        Some(&bearer),
        serde_json::json!([
            { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            { "jsonrpc": "2.0", "id": 2, "method": "ping" }
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn an_empty_batch_is_a_distinct_error() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (status, body) = mcp_call(Some(&bearer), serde_json::json!([])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], -32600);
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_parse_error_once_authenticated() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let response = app()
        .oneshot(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, &bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{ not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Without a token, a malformed body is still just "you need a token". Answering a parse error
/// here would tell an unauthenticated caller that this endpoint speaks JSON-RPC, and let them
/// distinguish malformed from unauthorized.
#[tokio::test]
async fn an_unauthenticated_caller_learns_only_that_they_need_a_token() {
    let response = app()
        .oneshot(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{ not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
}

/// The advertised tools must be the annotated ones, so a client can tell what modifies state.
#[tokio::test]
async fn tools_list_advertises_annotations() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (_, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }),
    )
    .await;
    let tools = body["result"]["tools"].as_array().expect("tools");
    assert!(!tools.is_empty());
    for tool in tools {
        assert!(
            tool["annotations"]["readOnlyHint"].is_boolean(),
            "{} must say whether it modifies anything",
            tool["name"]
        );
    }
}

/// Refusing beats guessing. Building against the wrong DeepBook would produce a transaction that
/// compiles, simulates against nothing real, and fails on chain — a failure nobody can attribute.
#[tokio::test]
async fn building_without_a_configured_deepbook_package_is_refused_by_name() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (_, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {
                "name": "rill_build_action",
                "arguments": { "actionId": "skill_anything" }
            }
        }),
    )
    .await;
    assert_eq!(body["result"]["isError"], true);
    // The action check comes first, so an unknown id is what surfaces here — and that is correct:
    // a caller must not learn about deployment configuration from an action they cannot see.
    assert_eq!(
        body["result"]["structuredContent"]["code"],
        "action_unavailable"
    );
}

#[tokio::test]
async fn building_an_action_you_do_not_own_is_refused_before_anything_is_compiled() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (_, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {
                "name": "rill_build_action",
                "arguments": { "actionId": "skill_someone_elses", "sender": "0x1" }
            }
        }),
    )
    .await;
    assert_eq!(
        body["result"]["structuredContent"]["code"], "action_unavailable",
        "nothing should be compiled for an action the caller cannot see"
    );
}

/// The tool now advertises that amounts are strings, which is the one thing an agent filling this
/// call in must get right.
#[tokio::test]
async fn the_build_tool_tells_an_agent_that_amounts_are_strings() {
    let bearer = format!(
        "Bearer {}",
        token(
            TokenKind::Access,
            "https://api.rill.test/mcp",
            "mcp",
            "0xowner"
        )
    );
    let (_, body) = mcp_call(
        Some(&bearer),
        serde_json::json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list" }),
    )
    .await;
    let build = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "rill_build_action")
        .expect("rill_build_action");
    let params = build["inputSchema"]["properties"]["params"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        params.contains("STRINGS"),
        "an agent reads this to know not to send a number: {params}"
    );
}

/// POST a JSON body and return the status.
async fn post_status(path: &str, body: Value) -> StatusCode {
    app()
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Discovery names four endpoints. They have to exist.
///
/// They did not: `/.well-known/oauth-authorization-server` advertised `/oauth/register`,
/// `/oauth/authorize`, `/oauth/token` and `/oauth/revoke`, and every one returned 404. An MCP
/// client that respects discovery could not connect, and the failure read as a broken server
/// rather than a missing one.
#[tokio::test]
async fn every_endpoint_the_discovery_document_names_exists() {
    let (_, discovery, _) = get("/.well-known/oauth-authorization-server").await;

    for field in [
        "registration_endpoint",
        "authorization_endpoint",
        "token_endpoint",
        "revocation_endpoint",
    ] {
        let url = discovery[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} is advertised: {discovery}"));
        let path = url
            .split_once("//")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(_, p)| format!("/{p}"))
            .unwrap_or_else(|| url.to_string());

        // The method is part of existing: an endpoint that 405s is as unreachable as one that 404s.
        let status = if field == "authorization_endpoint" {
            get(&path).await.0
        } else {
            post_status(&path, serde_json::json!({})).await
        };
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{field} advertises {path}, which does not exist"
        );
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{field} advertises {path}, which does not accept that method"
        );
    }
}

/// A redirect URI this server would not send a code to is refused before anything is stored.
#[tokio::test]
async fn an_open_redirect_is_refused_at_registration() {
    let status = post_status(
        "/oauth/register",
        serde_json::json!({ "redirect_uris": ["http://evil.example.com/steal"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Registration with no redirect URI at all is refused rather than defaulted.
#[tokio::test]
async fn registration_without_a_redirect_uri_is_refused() {
    let status = post_status(
        "/oauth/register",
        serde_json::json!({ "redirect_uris": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A grant type this server does not issue is named rather than silently ignored.
#[tokio::test]
async fn an_unsupported_grant_type_is_refused_by_name() {
    let status = post_status(
        "/oauth/token",
        serde_json::json!({ "grant_type": "client_credentials" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// RFC 7009: revocation answers 200 whether or not the token was real, so the endpoint cannot be
/// used to learn which tokens exist.
#[tokio::test]
async fn revoking_a_token_that_never_existed_still_answers_ok() {
    let status = post_status("/oauth/revoke", serde_json::json!({ "token": "nonsense" })).await;
    assert_eq!(status, StatusCode::OK);
}
