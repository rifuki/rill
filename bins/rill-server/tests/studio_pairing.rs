//! Pairing HTTP ownership and replay boundary without network calls.
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
use serde_json::{json, Value};
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
async fn owner_session_required_and_other_owner_cannot_confirm() {
    let state = state();
    let owner = token(&state, "0x1", "rill_studio");
    let other = token(&state, "0x3", "rill_studio");
    let oauth = token(&state, "0x1", "agent-client");
    let body = json!({"agent":"0x2","network":"testnet"});
    assert_eq!(
        call(&state, "/api/pairing/prepare", None, body.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&state, "/api/pairing/prepare", Some(&oauth), body.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, response) = call(&state, "/api/pairing/prepare", Some(&owner), body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let id = response["data"]["requestId"].as_str().unwrap();
    let confirm = json!({"requestId":id});
    assert_eq!(
        call(
            &state,
            "/api/pairing/confirm",
            Some(&owner),
            confirm.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    state
        .pairing
        .prove(
            id,
            &"0x2".parse::<sui_sdk_types::Address>().unwrap().to_string(),
            rill_server::studio_auth::now_ms(),
        )
        .unwrap();
    assert_eq!(
        call(
            &state,
            "/api/pairing/confirm",
            Some(&other),
            confirm.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, response) = call(
        &state,
        "/api/pairing/confirm",
        Some(&owner),
        confirm.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        response["data"]["agent"],
        "0x2".parse::<sui_sdk_types::Address>().unwrap().to_string()
    );
    assert_eq!(
        call(&state, "/api/pairing/confirm", Some(&owner), confirm)
            .await
            .0,
        StatusCode::CONFLICT
    );
}
#[tokio::test]
async fn proof_binds_owner_network_nonce_and_signer() {
    use rill_store::pairing::PairingRequest;
    let mut request = PairingRequest {
        request_id: "request".into(),
        owner: "owner".into(),
        agent: "agent".into(),
        network: "mainnet".into(),
        domain: "https://rill.test".into(),
        nonce: "nonce".into(),
        expires_at: 100,
        proved: false,
    };
    let chain = rill_chain::fake::FakeSui::new().with_valid_signature(
        request.message().as_bytes(),
        "valid",
        "agent",
    );
    assert!(
        rill_server::studio_pairing::verify_proof(&request, "agent", "valid", &chain)
            .await
            .is_ok()
    );
    assert!(
        rill_server::studio_pairing::verify_proof(&request, "other", "valid", &chain)
            .await
            .is_err()
    );
    assert!(
        rill_server::studio_pairing::verify_proof(&request, "agent", "invalid", &chain)
            .await
            .is_err()
    );
    request.network = "testnet".into();
    assert!(
        rill_server::studio_pairing::verify_proof(&request, "agent", "valid", &chain)
            .await
            .is_err()
    );
    request.network = "mainnet".into();
    request.owner = "other".into();
    assert!(
        rill_server::studio_pairing::verify_proof(&request, "agent", "valid", &chain)
            .await
            .is_err()
    );
    request.owner = "owner".into();
    request.nonce = "other".into();
    assert!(
        rill_server::studio_pairing::verify_proof(&request, "agent", "valid", &chain)
            .await
            .is_err()
    );
}
