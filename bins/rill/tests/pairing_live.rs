//! Production pairing proof. No transaction is submitted and no spending permission is issued.
use rill_cli::{grant_cmd, http, keystore::Keystore, pair_cmd};
use serde_json::json;
use sui_sdk_types::Address;

#[tokio::test]
#[ignore = "requires owner and agent keys plus a reachable Rill API"]
async fn owner_pairs_a_separate_signer_and_replay_is_refused() {
    let owner: Address = std::env::var("RILL_E2E_OWNER").unwrap().parse().unwrap();
    let agent: Address = std::env::var("RILL_E2E_AGENT").unwrap().parse().unwrap();
    assert_ne!(owner, agent);
    let api = std::env::var("RILL_E2E_API").unwrap();
    let network = std::env::var("RILL_E2E_NETWORK").unwrap();
    let owner_key = Keystore::load_for(owner).unwrap();
    let agent_key = Keystore::load_for(agent).unwrap();
    let token = grant_cmd::sign_in(&api, &owner_key).await.unwrap();
    let prepared = http::post_json_as(
        &format!("{api}/api/pairing/prepare"),
        &json!({"agent":agent.to_string(),"network":network}),
        &token,
    )
    .await
    .unwrap();
    let request = prepared["data"]["requestId"].as_str().unwrap();
    let proved = pair_cmd::pair(&agent_key, &api, request, &network)
        .await
        .unwrap();
    assert_eq!(proved["status"], "proved");
    let confirmed = http::post_json_as(
        &format!("{api}/api/pairing/confirm"),
        &json!({"requestId":request}),
        &token,
    )
    .await
    .unwrap();
    assert_eq!(confirmed["data"]["agent"], agent.to_string());
    assert!(http::post_json_as(
        &format!("{api}/api/pairing/confirm"),
        &json!({"requestId":request}),
        &token
    )
    .await
    .is_err());
    println!(
        "Paired owner {owner} with signer {agent}; replay refused; no spending authority issued."
    );
}
