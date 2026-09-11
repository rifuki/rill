//! A deployment is correct at whatever URL it is given, trailing slash or not.
//!
//! `PUBLIC_BASE_URL` comes from an operator, and an operator who types a trailing slash is doing
//! something entirely reasonable. Seven places read that value and four trimmed while three did not,
//! so `https://api.rill.example/` produced `.../mcp` in some answers and `...//mcp` in others.
//!
//! That is not cosmetic. Tokens are audience-bound to the resource string per RFC 8707 and the
//! audience is compared as a string, so a deployment configured with a trailing slash issues tokens
//! bound to one spelling and checks them against another. Every request then fails with an
//! invalid-audience error, and nothing in it names the slash. It is the kind of defect that looks
//! like the software being broken rather than the configuration, and it would have been found by
//! whoever first deployed this, not by anyone here.
//!
//! So: build the same server twice, once with a trailing slash and once without, and require every
//! URL a client can read to be byte-identical.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

use rill_server::routes;
use rill_server::state::{AppState, Config, Network};

fn config_with(base: &str) -> Config {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rill-base-url-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: base.into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: "a-test-secret-long-enough-to-pass".into(),
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

async fn get(base: &str, path: &str) -> (StatusCode, Value, axum::http::HeaderMap) {
    let app = routes::router(AppState::new(config_with(base)));
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
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

const WITHOUT: &str = "https://api.rill.example";
const WITH: &str = "https://api.rill.example/";

/// Every document a client reads to find this deployment must be the same either way.
#[tokio::test]
async fn the_discovery_documents_are_identical_with_and_without_a_trailing_slash() {
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource",
        "/health",
    ] {
        let (_, plain, _) = get(WITHOUT, path).await;
        let (_, slashed, _) = get(WITH, path).await;
        assert_eq!(
            plain, slashed,
            "{path} differs when the operator typed a trailing slash, and a client cannot tell \
             which spelling is canonical"
        );
        let rendered = slashed.to_string();
        assert!(
            !rendered.contains("example//"),
            "{path} carries a doubled slash: {rendered}"
        );
    }
}

/// The one that actually breaks a deployment: tokens are bound to this string and compared to it.
#[tokio::test]
async fn the_audience_a_token_is_bound_to_is_the_same_either_way() {
    let plain = config_with(WITHOUT).resource();
    let slashed = config_with(WITH).resource();
    assert_eq!(
        plain, slashed,
        "an audience that depends on how the operator typed the URL binds tokens to one spelling \
         and checks them against the other"
    );
    assert_eq!(plain, "https://api.rill.example/mcp");
}

/// And the header that tells an unauthenticated client where to go.
#[tokio::test]
async fn the_discovery_header_points_at_the_same_place_either_way() {
    let mut seen = Vec::new();
    for base in [WITHOUT, WITH] {
        let app = routes::router(AppState::new(config_with(base)));
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let header = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("the challenge names where to authenticate")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(!header.contains("example//"), "doubled slash: {header}");
        seen.push(header);
    }
    assert_eq!(
        seen[0], seen[1],
        "the challenge depends on the operator's typing"
    );
}

/// The instructions the server hands an agent carry the same URL, since an agent pastes what it is
/// given rather than normalising it.
#[tokio::test]
async fn the_generated_instructions_carry_the_same_url_either_way() {
    let plain = rill_server::agent_docs::agent_instructions(&config_with(WITHOUT), None, None);
    let slashed = rill_server::agent_docs::agent_instructions(&config_with(WITH), None, None);
    assert_eq!(
        plain, slashed,
        "a generated document differs on the operator's trailing slash, so an agent that pastes a \
         command from it reaches a different URL depending on how the deployment was configured"
    );
    assert!(
        !slashed.contains("example//"),
        "a doubled slash in the document is a command an agent will paste and a URL that 404s"
    );
    assert!(
        slashed.contains("https://api.rill.example/mcp"),
        "and the endpoint must appear at all, or this asserts nothing"
    );
}

/// The one that actually matters, and the one the checks above missed.
///
/// Asserting `Config::resource()` is stable proves nothing about the routes that mint and check a
/// token, because a route can build the audience string by hand instead of asking for it. That is
/// exactly what four of them did. Reverting any one of them to the hand-built form left every other
/// test in this file green, so the only way to hold it is to run the flow: register a client,
/// authorise with PKCE, exchange for a token, and present it at the endpoint, all on a deployment
/// configured with a trailing slash. If minting and checking disagree by one character, the last
/// step is a 401 and nothing in it mentions a slash.
#[tokio::test]
async fn a_token_minted_on_a_trailing_slash_deployment_is_accepted_at_its_own_endpoint() {
    // The verifier and its SHA-256 challenge, base64url without padding. Fixed, because computing
    // the challenge in the test would reimplement the thing under test.
    const VERIFIER: &str = "iRnMS3Y5MvsCJDCLTNKzJfvvUnfEnIVFrGwyBBcQOFo";
    const CHALLENGE: &str = "fyVcRG51fw07O0JaykUIkzTdiS-0yOWhklK7wwdopJU";
    const REDIRECT: &str = "http://127.0.0.1:9/cb";

    let app = routes::router(AppState::new(config_with(WITH)));
    let resource = config_with(WITH).resource();

    let registered = app
        .clone()
        .oneshot(
            Request::post("/oauth/register")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"redirect_uris":["{REDIRECT}"],"client_name":"trailing slash probe"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = registered.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let client_id = body["client_id"].as_str().expect("a client_id").to_owned();

    // The client asks for the audience this deployment advertises, which is what a real client does:
    // it reads the protected-resource document and echoes what it found.
    let encoded_resource = resource.replace(':', "%3A").replace('/', "%2F");
    let authorized = app
        .clone()
        .oneshot(
            Request::get(format!(
                "/oauth/authorize?client_id={client_id}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb\
                 &response_type=code&code_challenge={CHALLENGE}&code_challenge_method=S256\
                 &resource={encoded_resource}&scope=mcp"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    let bytes = authorized.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let code = body["code"]
        .as_str()
        .unwrap_or_else(|| panic!("a code, got {body}"))
        .to_owned();

    let exchanged = app
        .clone()
        .oneshot(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "grant_type=authorization_code&code={code}&client_id={client_id}\
                     &redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&code_verifier={VERIFIER}\
                     &resource={encoded_resource}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = exchanged.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("a token, got {body}"))
        .to_owned();

    let used = app
        .oneshot(
            Request::post("/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = used.status();
    let bytes = used.into_body().collect().await.unwrap().to_bytes();
    let answer: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(
        status,
        StatusCode::OK,
        "a token this deployment minted must be accepted at this deployment's endpoint; a \
         mismatched audience is how a trailing slash becomes an unexplained 401: {answer}"
    );
    assert!(
        answer["result"]["tools"].is_array(),
        "the call must reach the tool surface: {answer}"
    );
}
