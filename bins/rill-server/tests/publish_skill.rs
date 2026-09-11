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
        "rill-publish-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_in(dir: &std::path::Path) -> Config {
    Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: "http://localhost:3939".into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: SECRET.into(),
        oauth_secret_from_env: true,
        guard_package_id: Some("0xguard".into()),
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

async fn publish(app: &axum::Router, auth: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut request = Request::post("/api/skills").header(header::CONTENT_TYPE, "application/json");
    if let Some(a) = auth {
        request = request.header(header::AUTHORIZATION, a);
    }
    let response = app
        .clone()
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

async fn list_actions(app: &axum::Router, auth: &str) -> Value {
    let request = Request::post("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, auth)
        .body(Body::from(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "rill_list_actions", "arguments": {} }
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("{}")
        .to_owned();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// A publish with no bearer is refused, like every other call on this surface.
#[tokio::test]
async fn publishing_without_a_token_is_refused() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let (status, _) = publish(&app, None, json!({ "name": "a", "description": "b" })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// What is published appears in the publisher's own catalogue, which is the whole point.
#[tokio::test]
async fn a_published_action_appears_in_the_publishers_catalogue() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let auth = bearer("client:alice");

    let before = list_actions(&app, &auth).await;
    assert_eq!(
        before["actions"],
        json!([]),
        "nothing is published yet, and an empty catalogue is not an error"
    );

    let (status, body) = publish(
        &app,
        Some(&auth),
        json!({ "name": "Buy DEEP", "description": "A limit order on the DEEP/SUI book." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["data"]["id"].as_str().expect("an id").to_owned();
    assert!(id.starts_with("skill_"), "{id}");

    let after = list_actions(&app, &auth).await;
    let actions = after["actions"].as_array().expect("a list");
    assert_eq!(actions.len(), 1, "{after}");
    assert_eq!(actions[0]["actionId"], id);
    assert_eq!(actions[0]["name"], "Buy DEEP");
}

/// One owner's action is invisible to another, and that is the only thing separating them.
///
/// This is the assertion the route exists under. Everything else here is input validation; this is
/// the boundary `SkillStore::list_by_owner` documents, tested from the side that writes to it.
#[tokio::test]
async fn another_owner_cannot_see_it() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let alice = bearer("client:alice");
    let bob = bearer("client:bob");

    let (status, _) = publish(
        &app,
        Some(&alice),
        json!({ "name": "Alice's action", "description": "Hers alone." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        list_actions(&app, &alice).await["actions"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "the publisher sees it"
    );
    assert_eq!(
        list_actions(&app, &bob).await["actions"],
        json!([]),
        "and nobody else does, from the same store in the same process"
    );
}

/// An owner in the body is refused, not ignored.
///
/// Ignoring it would let a caller believe it had published into somebody else's catalogue, and the
/// way it would find out is that the somebody else cannot see it. The same holds for the two other
/// fields the server assigns.
#[tokio::test]
async fn a_body_that_sets_a_server_assigned_field_is_refused() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let auth = bearer("client:alice");

    for field in ["owner", "id", "createdAt", "created_at"] {
        let mut body = json!({ "name": "n", "description": "d" });
        body[field] = json!("0xsomeone-else");
        let (status, answer) = publish(&app, Some(&auth), body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{field} must be refused: {answer}"
        );
        assert!(
            answer["error"].as_str().unwrap_or_default().contains(field),
            "the refusal must name the field: {answer}"
        );
    }

    // And nothing was stored by any of those attempts.
    assert_eq!(list_actions(&app, &auth).await["actions"], json!([]));
}

/// A blank name or description is refused, because both are what an agent reads.
#[tokio::test]
async fn a_blank_name_or_description_is_refused() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let auth = bearer("client:alice");

    for body in [
        json!({ "description": "d" }),
        json!({ "name": "n" }),
        json!({ "name": "   ", "description": "d" }),
        json!({ "name": "n", "description": "" }),
    ] {
        let (status, answer) = publish(&app, Some(&auth), body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} gave {answer}");
    }
}

/// A flow that is not a valid graph is refused at publish, where the publisher can still fix it.
///
/// The same mistake surfacing at build time would reach an agent that did not make it.
#[tokio::test]
async fn a_flow_that_is_not_a_valid_graph_is_refused() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let auth = bearer("client:alice");

    // Two nodes with the same id: the structure check names it.
    let (status, answer) = publish(
        &app,
        Some(&auth),
        json!({
            "name": "n",
            "description": "d",
            "flow": {
                "nodes": [
                    { "id": "dup", "type": "cetus_swap", "data": {} },
                    { "id": "dup", "type": "haedal_stake", "data": {} }
                ],
                "edges": []
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("flow"),
        "{answer}"
    );
}

/// An absent flow is stored as an empty graph rather than null.
#[tokio::test]
async fn an_absent_flow_becomes_an_empty_graph() {
    let dir = fresh_dir();
    let app = app_in(&dir);
    let auth = bearer("client:alice");
    let (status, _) = publish(
        &app,
        Some(&auth),
        json!({ "name": "n", "description": "d" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("skills.json")).unwrap()).unwrap();
    let flow = &stored[0]["flow"];
    assert!(
        flow.get("nodes").is_some() && flow.get("edges").is_some(),
        "a reader must never have to tell an absent flow from one that failed to parse: {flow}"
    );
}
