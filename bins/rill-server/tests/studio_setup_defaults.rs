use rill_server::studio_setup::setup_defaults;
use rill_store::{PublishedSkill, SkillStore};
use serde_json::json;
fn skill() -> PublishedSkill {
    PublishedSkill {
        id: "skill_swap".into(),
        name: "Swap".into(),
        description: "Swap".into(),
        owner: Some("0x1".into()),
        created_at: "2026-10-04T00:00:00Z".into(),
        tool_defs: None,
        policy_id: None,
        flow: json!({"nodes":[{"id":"swap","type":"cetus_swap"}],"edges":[],"capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"budget","totalMist":"15000000"},{"kind":"per_tx","maxMist":"10000000"}]}}),
    }
}
#[test]
fn suggestions_fit_the_owner_published_limits() {
    let result = setup_defaults(&skill(), "0x1").unwrap();
    assert_eq!(result["budgetMist"], "15000000");
    assert_eq!(result["perTxMist"], "10000000");
    assert_eq!(result["budgetLimitMist"], "15000000");
    assert_eq!(result["perTxLimitMist"], "10000000");
}
#[test]
fn suggestions_do_not_expose_another_owners_policy() {
    assert!(setup_defaults(&skill(), "0x9").is_err());
    let mut value = skill();
    value.owner = None;
    assert!(setup_defaults(&value, "0x1").is_err());
}
#[test]
fn suggestions_never_expand_missing_or_large_caps_and_respect_rate_limits() {
    let mut value = skill();
    value.flow["capabilityManifest"]["rules"] = json!([{"kind":"budget","totalMist":"5000000000"},{"kind":"rate_limit","maxMist":"500000000","windowMs":"3600000"}]);
    let result = setup_defaults(&value, "0x1").unwrap();
    assert_eq!(result["budgetMist"], "1000000000");
    assert_eq!(result["perTxMist"], "500000000");
    assert!(result["perTxLimitMist"].is_null());
}

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use rill_auth::tokens::{sign_token, TokenClaims, TokenKind};
use rill_server::{
    routes,
    state::{AppState, Config, Network},
};
use serde_json::Value;
use tower::ServiceExt;
fn state() -> AppState {
    let dir =
        std::env::temp_dir().join(format!("rill-pair-http-{}", rill_auth::tokens::random_id()));
    AppState::new(Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: "https://api.rill.test".into(),
        consent_url: "https://studio.test/authorize".into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: "pair-test-secret-at-least-32-characters".into(),
        oauth_secret_from_env: true,
        guard_package_id: None,
        wallet_package_id: None,
        wallet_version_id: None,
        owner_secret: None,
        owner_address: None,
        bind_address: "127.0.0.1".into(),
        open_authorization_acknowledged: false,
        skills_store_path: dir.join("skills.json").to_string_lossy().into(),
        oauth_store_path: dir.join("oauth.json").to_string_lossy().into(),
    })
}
fn token(state: &AppState, owner: &str, cid: &str) -> String {
    sign_token(
        &TokenClaims {
            t: TokenKind::Access,
            sub: owner.parse::<sui_sdk_types::Address>().unwrap().to_string(),
            cid: cid.into(),
            scope: "mcp".into(),
            aud: state.config.resource(),
            exp: u64::MAX,
            jti: "pair-test".into(),
        },
        &state.config.oauth_secret,
    )
    .unwrap()
}
async fn call(
    state: &AppState,
    path: &str,
    bearer: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = routes::router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn options_route_is_owner_authenticated_and_does_not_require_chain_access() {
    let state = state();
    state.skills.save(skill()).unwrap();
    let body = json!({"skillId":"skill_swap"});
    assert_eq!(
        call(&state, "/api/setup/options", None, body.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let other = token(&state, "0x9", "rill_studio");
    let (status, value) = call(&state, "/api/setup/options", Some(&other), body.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(value.get("data").is_none());
    let oauth = token(&state, "0x1", "agent-client");
    assert_eq!(
        call(&state, "/api/setup/options", Some(&oauth), body.clone())
            .await
            .0,
        StatusCode::OK
    );
    let owner = token(&state, "0x1", "rill_studio");
    let (status, value) = call(&state, "/api/setup/options", Some(&owner), body).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    assert_eq!(value["data"]["budgetMist"], "15000000");
}
