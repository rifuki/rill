//! Studio wallet authentication and the OAuth consent boundary.

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt as _;
use rill_server::{
    routes,
    state::{AppState, Config, Network},
};
use serde_json::{json, Value};
use tower::ServiceExt as _;

fn app_state() -> AppState {
    let dir = std::env::temp_dir().join(format!(
        "rill-studio-auth-{}",
        rill_auth::tokens::random_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    AppState::new(Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: "https://api.rill.test".into(),
        consent_url: "http://localhost:5173/authorize".into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: "test-studio-secret-at-least-32-characters".into(),
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

fn app() -> Router {
    routes::router(app_state())
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value, HeaderMap) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers,
    )
}

async fn authorize(app: &Router) -> (StatusCode, Value, HeaderMap) {
    let (_, registered, _) = request(
        app,
        "POST",
        "/oauth/register",
        json!({
            "redirect_uris": ["http://127.0.0.1:8765/callback"], "client_name": "Test agent"
        }),
    )
    .await;
    request(app, "GET", &format!("/oauth/authorize?response_type=code&client_id={}&redirect_uri=http%3A%2F%2F127.0.0.1%3A8765%2Fcallback&code_challenge={}&code_challenge_method=S256&state=preserved-state&scope=mcp%20offline_access",
        registered["client_id"].as_str().unwrap(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"), Value::Null).await
}

#[tokio::test]
async fn wallet_challenge_returns_the_exact_message_and_expiry_in_studio_envelope() {
    let (status, body, _) = request(&app(), "GET", "/oauth/wallet-challenge", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true);
    assert!(body["data"]["message"]
        .as_str()
        .unwrap()
        .contains("Rill Studio"));
    assert!(body["data"]["challengeId"].is_string());
    assert!(body["data"]["expiresAt"].is_string());
}

#[tokio::test]
async fn authorize_redirects_to_consent_without_issuing_a_code() {
    let (status, body, headers) = authorize(&app()).await;
    assert!(
        status.is_redirection(),
        "authorize must require consent, got {status}: {body}"
    );
    let location = headers[header::LOCATION].to_str().unwrap();
    assert!(location.contains("request="), "{location}");
    assert!(!location.contains("code="));
}

#[tokio::test]
async fn invalid_wallet_signature_is_rejected_without_consuming_challenge() {
    let app = app();
    let (_, challenge, _) = request(&app, "GET", "/oauth/wallet-challenge", Value::Null).await;
    let body = json!({ "challengeId": challenge["data"]["challengeId"], "signature": "forged" });
    for _ in 0..2 {
        let (status, response, _) =
            request(&app, "POST", "/oauth/wallet-token", body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{response}");
        assert_eq!(response["error"], "invalid_signature");
    }
}

fn sign(message: &str) -> (String, String) {
    use sui_crypto::SuiSigner as _;
    let key = sui_crypto::ed25519::Ed25519PrivateKey::new([7; 32]);
    let signature = key
        .sign_personal_message(&sui_sdk_types::PersonalMessage(message.as_bytes().into()))
        .unwrap();
    (
        signature.to_base64(),
        key.public_key().derive_address().to_string(),
    )
}

async fn challenge(app: &Router) -> Value {
    let (status, body, _) = request(app, "GET", "/oauth/wallet-challenge", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

async fn pending_agent(app: &Router) -> Value {
    let (status, _, headers) = authorize(app).await;
    assert!(status.is_redirection());
    let location = url::Url::parse(headers[header::LOCATION].to_str().unwrap()).unwrap();
    let request_id = location
        .query_pairs()
        .find(|(key, _)| key == "request")
        .unwrap()
        .1
        .into_owned();
    let (status, body, _) = request(
        app,
        "GET",
        &format!("/oauth/consent/{request_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].clone()
}

#[tokio::test]
async fn wallet_session_derives_address_from_signature_and_cannot_be_replayed() {
    let app = app();
    let challenge = challenge(&app).await;
    let (signature, address) = sign(challenge["message"].as_str().unwrap());
    let input = json!({ "challengeId": challenge["challengeId"], "signature": signature, "address": "0xattacker" });
    let (status, body, _) = request(&app, "POST", "/oauth/wallet-token", input.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["address"], address);
    assert!(body["data"].get("refresh_token").is_none());
    let claims = rill_auth::tokens::verify_token(
        body["data"]["access_token"].as_str().unwrap(),
        "test-studio-secret-at-least-32-characters",
        rill_auth::tokens::Expectation {
            kind: rill_auth::tokens::TokenKind::Access,
            audience: "https://api.rill.test/mcp",
            now_secs: 0,
        },
    )
    .unwrap();
    assert_eq!(claims.sub, address);
    assert_eq!(claims.cid, "rill_studio");
    let (status, _, _) = request(&app, "POST", "/oauth/wallet-token", input).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn signature_for_altered_message_is_rejected_but_original_challenge_can_be_retried() {
    let app = app();
    let challenge = challenge(&app).await;
    let message = challenge["message"].as_str().unwrap();
    let (forged, _) = sign(&format!("{message} "));
    let (status, _, _) = request(
        &app,
        "POST",
        "/oauth/wallet-token",
        json!({
            "challengeId": challenge["challengeId"], "signature": forged
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (valid, _) = sign(message);
    let (status, _, _) = request(
        &app,
        "POST",
        "/oauth/wallet-token",
        json!({
            "challengeId": challenge["challengeId"], "signature": valid
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn agent_request_cannot_skip_pkce_by_redeeming_as_studio_session() {
    let app = app();
    let pending = pending_agent(&app).await;
    let (signature, _) = sign(pending["message"].as_str().unwrap());
    let (status, _, _) = request(
        &app,
        "POST",
        "/oauth/wallet-token",
        json!({
            "challengeId": pending["requestId"], "signature": signature
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(
        &app,
        "POST",
        "/oauth/consent",
        json!({
            "requestId": pending["requestId"], "signature": signature
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn studio_request_cannot_be_redeemed_for_an_agent_code() {
    let app = app();
    let challenge = challenge(&app).await;
    let (signature, _) = sign(challenge["message"].as_str().unwrap());
    let (status, _, _) = request(
        &app,
        "POST",
        "/oauth/consent",
        json!({
            "requestId": challenge["challengeId"], "signature": signature
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_challenge_is_rejected_even_with_a_valid_signature() {
    use rill_store::{AuthorizationRequest, OAuthStore, RequestKind};
    let state = app_state();
    state
        .oauth
        .save_request(AuthorizationRequest {
            request_id: "expired".into(),
            kind: RequestKind::Studio,
            client_id: "rill_studio".into(),
            client_name: None,
            redirect_uri: String::new(),
            state: None,
            scope: "mcp".into(),
            code_challenge: String::new(),
            resource: "https://api.rill.test/mcp".into(),
            message: "expired message".into(),
            expires_at: 1,
        })
        .unwrap();
    let app = routes::router(state);
    let (signature, _) = sign("expired message");
    let (status, _, _) = request(
        &app,
        "POST",
        "/oauth/wallet-token",
        json!({
            "challengeId": "expired", "signature": signature
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn consent_preserves_state_and_pkce_and_issues_wallet_scoped_rotating_tokens() {
    let app = app();
    let pending = pending_agent(&app).await;
    let (signature, address) = sign(pending["message"].as_str().unwrap());
    let input = json!({ "requestId": pending["requestId"], "signature": signature });
    let (status, body, _) = request(&app, "POST", "/oauth/consent", input.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let redirect = url::Url::parse(body["data"]["redirectTo"].as_str().unwrap()).unwrap();
    assert_eq!(
        redirect.origin().ascii_serialization(),
        "http://127.0.0.1:8765"
    );
    assert_eq!(
        redirect
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1,
        "preserved-state"
    );
    let code = redirect
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    let (status, _, _) = request(&app, "POST", "/oauth/consent", input).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, tokens, _) = request(
        &app,
        "POST",
        "/oauth/token",
        json!({
            "grant_type": "authorization_code", "code": code,
            "code_verifier": "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    let claims = rill_auth::tokens::verify_token(
        tokens["access_token"].as_str().unwrap(),
        "test-studio-secret-at-least-32-characters",
        rill_auth::tokens::Expectation {
            kind: rill_auth::tokens::TokenKind::Access,
            audience: "https://api.rill.test/mcp",
            now_secs: 0,
        },
    )
    .unwrap();
    assert_eq!(claims.sub, address);
    let refresh =
        json!({ "grant_type": "refresh_token", "refresh_token": tokens["refresh_token"] });
    let (status, refreshed, _) = request(&app, "POST", "/oauth/token", refresh.clone()).await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    assert_ne!(refreshed["refresh_token"], tokens["refresh_token"]);
    let (status, _, _) = request(&app, "POST", "/oauth/token", refresh).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn authorization_code_rejects_a_wrong_pkce_verifier() {
    let app = app();
    let pending = pending_agent(&app).await;
    let (signature, _) = sign(pending["message"].as_str().unwrap());
    let (_, body, _) = request(
        &app,
        "POST",
        "/oauth/consent",
        json!({
            "requestId": pending["requestId"], "signature": signature
        }),
    )
    .await;
    let redirect = url::Url::parse(body["data"]["redirectTo"].as_str().unwrap()).unwrap();
    let code = redirect
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    let (status, response, _) = request(
        &app,
        "POST",
        "/oauth/token",
        json!({
            "grant_type": "authorization_code", "code": code, "code_verifier": "z".repeat(43)
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"], "invalid_grant");
}

#[test]
fn verification_matches_sdk_for_all_three_standard_wallet_schemes() {
    use sui_crypto::SuiSigner as _;
    let ed = sui_crypto::ed25519::Ed25519PrivateKey::new([7; 32]);
    let k1 = sui_crypto::secp256k1::Secp256k1PrivateKey::new([8; 32]).unwrap();
    let r1 = sui_crypto::secp256r1::Secp256r1PrivateKey::new([9; 32]);
    let message = sui_sdk_types::PersonalMessage(b"the exact login message".as_slice().into());
    for (signature, address) in [
        (
            ed.sign_personal_message(&message).unwrap(),
            ed.public_key().derive_address(),
        ),
        (
            k1.sign_personal_message(&message).unwrap(),
            k1.public_key().derive_address(),
        ),
        (
            r1.sign_personal_message(&message).unwrap(),
            r1.public_key().derive_address(),
        ),
    ] {
        assert_eq!(
            rill_auth::siws::verify_sign_in_signature(
                "the exact login message",
                &signature.to_base64()
            )
            .unwrap(),
            address.to_string()
        );
        assert!(rill_auth::siws::verify_sign_in_signature(
            "a different message",
            &signature.to_base64()
        )
        .is_err());
    }
}

#[test]
fn multisig_requires_valid_member_signatures_and_the_weighted_threshold() {
    use sui_crypto::SuiSigner as _;
    use sui_sdk_types::{
        MultisigAggregatedSignature, MultisigCommittee, MultisigMember, MultisigMemberPublicKey,
        MultisigMemberSignature, SimpleSignature, UserSignature,
    };
    let first = sui_crypto::ed25519::Ed25519PrivateKey::new([7; 32]);
    let second = sui_crypto::ed25519::Ed25519PrivateKey::new([8; 32]);
    let committee = MultisigCommittee::new(
        vec![
            MultisigMember::new(MultisigMemberPublicKey::Ed25519(first.public_key()), 1),
            MultisigMember::new(MultisigMemberPublicKey::Ed25519(second.public_key()), 2),
        ],
        3,
    );
    let message = sui_sdk_types::PersonalMessage(b"multisig login".as_slice().into());
    let member_signature = |key: &sui_crypto::ed25519::Ed25519PrivateKey| {
        let UserSignature::Simple(SimpleSignature::Ed25519 { signature, .. }) =
            key.sign_personal_message(&message).unwrap()
        else {
            panic!("fixture is Ed25519")
        };
        MultisigMemberSignature::Ed25519(signature)
    };
    let valid = UserSignature::Multisig(MultisigAggregatedSignature::new(
        committee.clone(),
        vec![member_signature(&first), member_signature(&second)],
        3,
    ));
    assert_eq!(
        rill_auth::siws::verify_sign_in_signature("multisig login", &valid.to_base64()).unwrap(),
        committee.derive_address().to_string()
    );
    let under = UserSignature::Multisig(MultisigAggregatedSignature::new(
        committee.clone(),
        vec![member_signature(&first)],
        1,
    ));
    assert!(
        rill_auth::siws::verify_sign_in_signature("multisig login", &under.to_base64()).is_err()
    );
    let forged = UserSignature::Multisig(MultisigAggregatedSignature::new(
        committee,
        vec![member_signature(&first), member_signature(&first)],
        3,
    ));
    assert!(
        rill_auth::siws::verify_sign_in_signature("multisig login", &forged.to_base64()).is_err()
    );
}
