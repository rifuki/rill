//! Publishing an action, and the boundary that decides whose catalogue it lands in.
//!
//! Before this route existed, `SkillStore::save` had only test callers. Nothing in the running server
//! ever wrote a skill, so `rill_list_actions` answered `{"actions":[]}` for every caller forever and
//! the two tools that take an `actionId` could never succeed against one. The OAuth server in front
//! of the build surface and the owner-scoped catalogue behind it were both reachable and had nothing
//! to reach.
//!
//! The test that matters most here is the last one. `SkillStore::list_by_owner` is documented as "an
//! authorization boundary, and it is the only thing between one user's catalogue and another's", and a
//! publish route is the other side of it.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use rill_auth::tokens::{sign_token, TokenClaims, TokenKind};
use rill_server::routes;
use rill_server::state::{AppState, Config, Network};
use serde_json::{json, Value};
use tower::ServiceExt;

const SECRET: &str = "test-secret";
const AUDIENCE: &str = "http://localhost:3939/mcp";

fn fresh_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rill-studio-api-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_in(dir: &std::path::Path) -> Config {
    Config {
        consent_url: "http://localhost:5173/authorize".into(),
        port: 3939,
        network: Network::Testnet,
        public_base_url: "http://localhost:3939".into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: SECRET.into(),
        oauth_secret_from_env: true,
        guard_package_id: Some("0xguard".into()),
        wallet_package_id: None,
        wallet_version_id: None,
        owner_secret: None,
        owner_address: None,
        // Loopback, so `boot_check`'s open-authorization refusal is not in play here: these
        // tests are about the routes, not about where the socket is.
        bind_address: "127.0.0.1".into(),
        open_authorization_acknowledged: false,
        skills_store_path: dir.join("skills.json").to_string_lossy().into(),
        oauth_store_path: dir.join("oauth.json").to_string_lossy().into(),
    }
}

/// One router over a directory, so several requests share a store.
fn app_in(dir: &std::path::Path) -> axum::Router {
    routes::router(AppState::new(config_in(dir)))
}

fn bearer(subject: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!(
        "Bearer {}",
        sign_token(
            &TokenClaims {
                t: TokenKind::Access,
                sub: subject.into(),
                cid: "rill_client_test".into(),
                scope: "mcp offline_access".into(),
                aud: AUDIENCE.into(),
                exp: now + 3600,
                jti: "test-jti".into(),
            },
            SECRET,
        )
        .unwrap()
    )
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, token);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn protocols_supply_the_fields_the_studio_uses() {
    let app = app_in(&fresh_dir());
    let (status, value) = call(&app, "GET", "/api/protocols", Value::Null, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        value["data"]["cetus_swap"]["integratePackageId"].is_string(),
        "{value}"
    );
    assert!(value["data"]["haedal_stake"]["stakingObjectId"].is_string());
    assert!(value["data"]["deepbook_limit_order"]["pools"].is_array());
}

#[tokio::test]
async fn capability_preview_preserves_exact_amount_and_enforcement() {
    let app = app_in(&fresh_dir());
    let (status, value) = call(&app, "POST", "/api/capabilities/preview", json!({"manifest":{
        "walletCoinType":"0x2::sui::SUI", "rules":[{"kind":"budget","totalMist":"9007199254740993"}]
    }}), None).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(
        value["data"]["onChainRules"][0]["config"]["totalMist"],
        "9007199254740993"
    );
    assert!(value["data"]["declaration"]["summaryLines"].is_array());
}

#[tokio::test]
async fn publish_lists_owned_flows_in_the_frontend_shape() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let token = bearer("0x1");
    let flow = json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1","validator":"0x0"}}],"edges":[]});
    let (status, published) = call(&app, "POST", "/api/publish", json!({"flow":flow,"manifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"budget","totalMist":"1000000000"}]}}), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{published}");
    let id = published["data"]["skillId"].as_str().unwrap();
    assert_eq!(published["data"]["ownerMcpUrl"], AUDIENCE);
    let (_, list) = call(&app, "GET", "/api/skills", Value::Null, Some(&token)).await;
    assert_eq!(list["data"][0]["id"], id);
    let (_, other) = call(
        &app,
        "GET",
        "/api/skills",
        Value::Null,
        Some(&bearer("0x2")),
    )
    .await;
    assert_eq!(other["data"], json!([]));
    let (_, public) = call(&app, "GET", "/api/skills", Value::Null, None).await;
    assert_eq!(public["data"], json!([]));
    let (status, _) = call(&app, "GET", &format!("/api/mcp/{id}"), Value::Null, None).await;
    assert_ne!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_graph_is_refused_by_compile_without_network() {
    let app = app_in(&fresh_dir());
    let (status, value) = call(
        &app,
        "POST",
        "/api/compile",
        json!({"flow":{"nodes":[{"id":"x","type":"unknown"}],"edges":[]}}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
    assert_eq!(value["success"], false);
}

#[tokio::test]
async fn build_cannot_drop_the_published_recipient_restriction() {
    let dir = fresh_dir();
    let mut config = config_in(&dir);
    config.sui_rpc_url = "http://127.0.0.1:1".into();
    config.guard_package_id = None;
    let state = AppState::new(config);
    let skill = rill_store::PublishedSkill {
        id: "skill_limits".into(),
        name: "stake".into(),
        description: "stake".into(),
        flow: json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[],"capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"recipient_allowlist","addresses":["0x1"]}]}}),
        tool_defs: None,
        policy_id: None,
        owner: Some("0x1".into()),
        created_at: "2026-10-04T00:00:00Z".into(),
    };
    let result=rill_server::studio_api::build_published(&state,&skill,&json!({"sender":"0x1","agentWallet":{"packageId":"0x2","walletId":"0x3","capId":"0x4","versionId":"0x5","capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"budget","totalMist":"1000000000"}]}}})).await;
    assert!(result
        .unwrap_err()
        .contains("published recipient_allowlist"));
}

#[tokio::test]
async fn studio_binding_matches_the_advertised_owner_mcp_schema() {
    let schema = serde_json::to_value(rill_mcp::tools(rill_mcp::Surface::Actions)).unwrap();
    let tool = schema
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "rill_build_action")
        .unwrap();
    let binding = json!({"packageId":"0x2","walletId":"0x3","capId":"0x4","versionId":"0x5","coinType":"0x2::sui::SUI","capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"budget","totalMist":"1000000000"}]}});
    let shape = &tool["inputSchema"]["properties"]["agentWallet"];
    for field in shape["required"].as_array().unwrap() {
        assert!(
            binding.get(field.as_str().unwrap()).is_some(),
            "missing {field}"
        );
    }
    for field in binding.as_object().unwrap().keys() {
        assert!(shape["properties"].get(field).is_some(), "unknown {field}");
    }
    let mut config = config_in(&fresh_dir());
    config.guard_package_id = None;
    let state = AppState::new(config);
    let mut with_refs = binding;
    with_refs["capVersion"] = json!(1);
    with_refs["capDigest"] = json!("digest");
    assert!(rill_server::studio_api::options(
        &state,
        &json!({"sender":"0x1","agentWallet":with_refs})
    )
    .is_ok());
}

#[test]
fn public_tool_describes_runtime_node_ids_and_exact_defaults() {
    let skill = rill_store::PublishedSkill {
        id: "skill_multi".into(),
        name: "two stakes".into(),
        description: "two stakes".into(),
        flow: json!({"nodes":[{"id":"first","type":"haedal_stake","config":{"amount":"1000000000"}},{"id":"second","type":"haedal_stake","config":{"amount":"2000000000"}}],"edges":[]}),
        tool_defs: None,
        policy_id: None,
        owner: None,
        created_at: "now".into(),
    };
    let tool = rill_server::studio_api::tool_definition(&skill);
    assert_eq!(
        tool["inputSchema"]["properties"]["params"]["properties"]["first"]["properties"]["amount"]
            ["default"],
        "1000000000"
    );
    assert_eq!(
        tool["inputSchema"]["properties"]["params"]["properties"]["second"]["properties"]["amount"]
            ["type"],
        "string"
    );
}
