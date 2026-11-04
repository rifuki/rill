//! The step-up relay: a request opens only when the agent signed it over bytes that say what it
//! says, and an approval is stored only when the wallet's owner signed it. Offline, against the
//! in-memory chain; signatures are the fake node's, so no key signs anything here.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use rill_chain::{fake::FakeSui, ObjectRef, ObjectSummary};
use rill_core::grant::{Grant, SignedGrant};
use rill_core::step_up::StepUpApproval;
use rill_server::routes;
use rill_server::state::{AppState, Config, Network};
use rill_server::studio_step_up::{approve, check_request};
use rill_store::step_up::StepUpStore;
use serde_json::{json, Value};
use sui_sdk_types::{Address, Digest, Identifier};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};
use tower::ServiceExt;

const OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";
const AGENT: &str = "0x00000000000000000000000000000000000000000000000000000000000000a2";
const WALLET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";
const PACKAGE: &str = "0x00000000000000000000000000000000000000000000000000000000000000c1";
const NOW: u64 = 1_800_000_000_000;
const AGENT_SIGNATURE: &str = "agent-signature";
const OWNER_SIGNATURE: &str = "owner-signature";

fn ptb(amount: u64) -> String {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(AGENT.parse::<Address>().unwrap());
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(
        "0x0a".parse::<Address>().unwrap(),
        1,
        Digest::ZERO,
    )]);
    let wallet = tx.object(ObjectInput::shared(WALLET.parse().unwrap(), 4, true));
    let amount = tx.pure(&amount);
    tx.move_call(
        Function::new(
            PACKAGE.parse().unwrap(),
            Identifier::new("agent_wallet").unwrap(),
            Identifier::new("request_spend").unwrap(),
        ),
        vec![wallet, wallet, wallet, amount, wallet],
    );
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .encode(bcs::to_bytes(&tx.try_build().unwrap()).unwrap())
}

fn grant() -> SignedGrant {
    SignedGrant {
        grant: Grant {
            network: "testnet".into(),
            action_id: "skill_x".into(),
            action_name: "swap".into(),
            agent: AGENT.into(),
            wallet_id: WALLET.into(),
            wallet_package_id: PACKAGE.into(),
            expires_at_ms: (NOW + 86_400_000).to_string(),
            revision: 3,
            run_set: json!({"maxAmountBaseUnits": "50", "stepUpAboveBaseUnits": "10"}),
            build_arguments: json!({}),
        },
        signature: "grant-signature".into(),
    }
}

fn approval(bytes: &str) -> StepUpApproval {
    StepUpApproval {
        network: "testnet".into(),
        agent: AGENT.into(),
        wallet_id: WALLET.into(),
        wallet_package_id: PACKAGE.into(),
        action_id: "skill_x".into(),
        action_name: "swap".into(),
        grant_revision: 3,
        grant_digest: grant().grant.digest(),
        pinned_digest: rill_core::envelope::digest_unsigned_ptb(bytes),
        transaction_digest: rill_policy::decode::transaction_digest(bytes).unwrap(),
        spend_base_units: "20".into(),
        threshold_base_units: "10".into(),
        coin_type: "0x2::sui::SUI".into(),
        expires_at_ms: (NOW + 120_000).to_string(),
    }
}

fn wallet(agent: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: WALLET.into(),
            version: 5,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(format!(
            "{PACKAGE}::agent_wallet::AgentWallet<0x2::sui::SUI>"
        )),
        fields: Some(json!({ "owner": OWNER, "agent": agent, "revoked": false })),
        shared_initial_version: Some(4),
    }
}

/// A chain whose wallet names AGENT, and on which AGENT signed exactly `signed`'s request.
fn chain(signed: &StepUpApproval) -> FakeSui {
    FakeSui::new()
        .with_object(None, wallet(AGENT))
        .with_valid_signature(signed.request_message().as_bytes(), AGENT_SIGNATURE, AGENT)
}

fn body(approval: &StepUpApproval, bytes: &str) -> Value {
    json!({ "approval": approval, "unsignedPtb": bytes, "agentSignature": AGENT_SIGNATURE })
}

async fn open(body: &Value, chain: &FakeSui) -> Result<(StepUpApproval, String), String> {
    check_request(body, chain, "testnet", &[grant()], NOW).await
}

#[tokio::test]
async fn a_request_needs_the_agents_signature_and_matching_bytes() {
    let bytes = ptb(20);
    let ours = approval(&bytes);
    let (accepted, unsigned) = open(&body(&ours, &bytes), &chain(&ours)).await.unwrap();
    assert_eq!(accepted, ours);
    assert_eq!(unsigned, bytes);

    // Not the agent's signature over this request.
    let unsigned_chain = FakeSui::new().with_object(None, wallet(AGENT));
    let error = open(&body(&ours, &bytes), &unsigned_chain)
        .await
        .unwrap_err();
    assert!(error.contains("agent"), "{error}");

    // Different bytes from the ones the approval pins.
    let error = open(&body(&ours, &ptb(21)), &chain(&ours))
        .await
        .unwrap_err();
    assert!(error.contains("pinned"), "{error}");

    // An approval naming another transaction digest, or an amount the bytes do not request.
    let mut renamed = ours.clone();
    renamed.transaction_digest = "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi".into();
    let error = open(&body(&renamed, &bytes), &chain(&renamed))
        .await
        .unwrap_err();
    assert!(error.contains("transaction digest"), "{error}");
    let mut understated = ours.clone();
    understated.spend_base_units = "19".into();
    let error = open(&body(&understated, &bytes), &chain(&understated))
        .await
        .unwrap_err();
    assert!(error.contains("request"), "{error}");

    // A grant revision this server does not hold.
    let mut stale = ours.clone();
    stale.grant_revision = 2;
    let error = open(&body(&stale, &bytes), &chain(&stale))
        .await
        .unwrap_err();
    assert!(error.contains("grant"), "{error}");

    // A wallet that no longer names this agent.
    let rotated = FakeSui::new()
        .with_object(None, wallet(OWNER))
        .with_valid_signature(ours.request_message().as_bytes(), AGENT_SIGNATURE, AGENT);
    let error = open(&body(&ours, &bytes), &rotated).await.unwrap_err();
    assert!(error.contains("agent"), "{error}");

    // Longer-lived than any envelope, or already over.
    let mut long = ours.clone();
    long.expires_at_ms = (NOW + 10 * 60_000).to_string();
    assert!(open(&body(&long, &bytes), &chain(&long)).await.is_err());
    let mut over = ours.clone();
    over.expires_at_ms = NOW.to_string();
    assert!(open(&body(&over, &bytes), &chain(&over)).await.is_err());

    // Another network.
    assert!(check_request(
        &body(&ours, &bytes),
        &chain(&ours),
        "mainnet",
        &[grant()],
        NOW
    )
    .await
    .is_err());

    // A key-shaped argument is refused before anything is read.
    let mut keyed = body(&ours, &bytes);
    keyed["privateKey"] = json!("suiprivkey1");
    assert!(open(&keyed, &chain(&ours)).await.is_err());
}

#[tokio::test]
async fn an_owner_approval_from_anyone_but_the_wallet_owner_is_refused_and_not_stored() {
    let bytes = ptb(20);
    let ours = approval(&bytes);
    let store = StepUpStore::new();
    store
        .prepare("r1".into(), ours.clone(), bytes.clone(), NOW)
        .unwrap();

    // The agent's key cannot approve its own request.
    let by_agent = FakeSui::new()
        .with_object(None, wallet(AGENT))
        .with_valid_signature(ours.message().as_bytes(), OWNER_SIGNATURE, AGENT);
    let (status, _) = approve(&store, "r1", OWNER_SIGNATURE, &by_agent, NOW)
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(store
        .get("r1", NOW)
        .unwrap()
        .unwrap()
        .owner_signature
        .is_none());

    // The owner's signature over different text is not an approval of this one.
    let mut other = ours.clone();
    other.spend_base_units = "1".into();
    let elsewhere = FakeSui::new()
        .with_object(None, wallet(AGENT))
        .with_valid_signature(other.message().as_bytes(), OWNER_SIGNATURE, OWNER);
    assert!(approve(&store, "r1", OWNER_SIGNATURE, &elsewhere, NOW)
        .await
        .is_err());
    assert!(store
        .get("r1", NOW)
        .unwrap()
        .unwrap()
        .owner_signature
        .is_none());

    let by_owner = FakeSui::new()
        .with_object(None, wallet(AGENT))
        .with_valid_signature(ours.message().as_bytes(), OWNER_SIGNATURE, OWNER);
    let entry = approve(&store, "r1", OWNER_SIGNATURE, &by_owner, NOW)
        .await
        .unwrap();
    assert_eq!(entry.owner_signature.as_deref(), Some(OWNER_SIGNATURE));

    let (status, _) = approve(&store, "r1", OWNER_SIGNATURE, &by_owner, NOW)
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::CONFLICT, "an approval is given once");

    let (status, _) = approve(&store, "missing", OWNER_SIGNATURE, &by_owner, NOW)
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn state() -> AppState {
    let dir = std::env::temp_dir().join(format!(
        "rill-step-up-http-{}",
        rill_auth::tokens::random_id()
    ));
    AppState::new(Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: "https://api.rill.test".into(),
        consent_url: "https://studio.test/authorize".into(),
        studio_url: None,
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: "step-up-test-secret-at-least-32-chars".into(),
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

async fn send(state: &AppState, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The routes exist and answer an unknown request by name, before any chain read. A served
/// request carries the text to sign, computed here, and the page's origin is Studio's.
#[tokio::test]
async fn the_routes_answer_an_unknown_request_without_reading_the_chain() {
    let state = state();
    let (status, _) = send(&state, "GET", "/api/step-up/nope", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &state,
        "POST",
        "/api/step-up/nope/approve",
        json!({"signature": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&state, "POST", "/api/step-up", json!({"approval": {}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let bytes = ptb(20);
    let ours = approval(&bytes);
    state
        .step_ups
        .prepare(
            "r1".into(),
            ours.clone(),
            bytes.clone(),
            rill_server::studio_auth::now_ms(),
        )
        .unwrap();
    let (status, shown) = send(&state, "GET", "/api/step-up/r1", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    let shown = shown.get("data").unwrap_or(&shown);
    assert_eq!(shown["message"], json!(ours.message()));
    assert_eq!(shown["unsignedPtb"], json!(bytes));
    assert_eq!(shown["status"], "pending");
    assert!(shown.get("ownerSignature").is_none_or(Value::is_null));
    assert_eq!(state.config.studio_origin(), "https://studio.test");
}
