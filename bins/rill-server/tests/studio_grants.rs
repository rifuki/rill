//! Owner-signed grants on the server: prepared from a bounded wallet, stored only when the wallet's
//! owner signed them. Driven against the in-memory chain with the onboarding fixtures.

use rill_chain::{fake::FakeSui, ObjectRef, ObjectSummary};
use rill_core::envelope::Network;
use rill_core::grant::{Grant, SignedGrant};
use rill_ptb::deployments;
use rill_server::studio_grants::{accept_grant, prepare_grant};
use rill_server::studio_setup::SetupContext;
use rill_store::PublishedSkill;
use serde_json::{json, Value};
use sui_sdk_types::Address;

fn addr(s: &str) -> String {
    s.parse::<Address>().unwrap().to_string()
}

fn context() -> SetupContext {
    SetupContext {
        wallet_package_id: None,
        wallet_version_id: None,
        wallet_type_package: None,
        deepbook_type_packages: None,
        network: Network::Testnet,
        guard_package: Some(deployments::TESTNET_RILL_GUARD.parse().unwrap()),
        now_ms: 1000,
    }
}

fn skill() -> PublishedSkill {
    PublishedSkill {
        id: "skill_stake".into(),
        name: "Stake".into(),
        description: "Stake".into(),
        flow: json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[],"capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"budget","totalMist":"5000000000"},{"kind":"per_tx","maxMist":"1000000000"}]}}),
        tool_defs: None,
        policy_id: None,
        owner: Some(addr("0x1")),
        created_at: "2026-10-04T00:00:00Z".into(),
    }
}

fn object(chain: FakeSui, id: &str, kind: &str, fields: Value, owner: Option<&str>) -> FakeSui {
    chain.with_object(
        owner,
        ObjectSummary {
            reference: ObjectRef {
                id: addr(id),
                version: 17,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some(kind.into()),
            fields: Some(fields),
            shared_initial_version: if owner.is_none() { Some(7) } else { None },
        },
    )
}

/// A wallet owned by 0x1 for agent 0x2, funded and carrying its rules.
fn chain(rules: Value, revoked: bool) -> FakeSui {
    let chain = object(
        FakeSui::new(),
        deployments::TESTNET_AGENT_WALLET_VERSION,
        &format!("{}::version::Version", deployments::TESTNET_AGENT_WALLET),
        json!({}),
        None,
    );
    let chain = object(
        chain,
        "0x10",
        &format!(
            "{}::agent_wallet::AgentWallet<0x2::sui::SUI>",
            deployments::TESTNET_AGENT_WALLET
        ),
        json!({"owner":addr("0x1"),"agent":addr("0x2"),"cap_id":addr("0x11"),"budget":"5000000000","spent":"0","policy":{"rules":{"contents":rules}},"revoked":revoked,"expires_at_ms":"1000000"}),
        None,
    );
    let chain = object(
        chain,
        "0x11",
        &format!(
            "{}::agent_wallet::AgentCap",
            deployments::TESTNET_AGENT_WALLET
        ),
        json!({"wallet":addr("0x10")}),
        Some(&addr("0x2")),
    );
    let chain = object(
        chain,
        deployments::TESTNET_HAEDAL_STAKING,
        "0x3::staking::Staking",
        json!({}),
        None,
    );
    object(
        chain,
        "0x5",
        "0x3::sui_system::SuiSystemState",
        json!({}),
        None,
    )
}

fn bounded() -> FakeSui {
    chain(json!(["budget", "per_tx"]), false)
}

fn request() -> Value {
    json!({"actionId":"skill_stake","walletId":"0x10","budgetMist":"5000000000","perTxMist":"1000000000","expiresAtMs":"1000000"})
}

async fn prepared(chain: &FakeSui, current: u64) -> Result<Value, String> {
    prepare_grant(
        &request(),
        &skill(),
        &addr("0x1"),
        &context(),
        chain,
        |_, _, _| current,
    )
    .await
}

fn grant_of(prepared: &Value) -> Grant {
    serde_json::from_value(prepared["grant"].clone()).unwrap()
}

#[tokio::test]
async fn prepare_reads_the_agent_from_the_wallet_and_asks_for_the_next_revision() {
    let result = prepared(&bounded(), 3).await.unwrap();
    let grant = grant_of(&result);
    assert_eq!(grant.agent, addr("0x2"));
    assert_eq!(grant.wallet_id, addr("0x10"));
    assert_eq!(grant.revision, 4);
    assert_eq!(grant.network, "testnet");
    assert_eq!(result["message"], json!(grant.message()));
    assert_eq!(grant.run_set["sender"], json!(addr("0x2")));
    assert!(!grant.run_set["allowedTargets"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn prepare_refuses_a_wallet_that_has_no_rules_yet() {
    let error = prepared(&chain(json!([]), false), 0).await.unwrap_err();
    assert!(error.contains("no rules attached"), "{error}");
}

fn signed(grant: Grant) -> SignedGrant {
    SignedGrant {
        grant,
        signature: "owner-signature".into(),
    }
}

#[tokio::test]
async fn only_the_wallets_owner_signing_this_exact_grant_is_accepted() {
    let grant = grant_of(&prepared(&bounded(), 0).await.unwrap());
    let accepted =
        bounded().with_valid_signature(grant.message().as_bytes(), "owner-signature", &addr("0x1"));
    accept_grant(
        &signed(grant.clone()),
        &skill(),
        &addr("0x1"),
        &context(),
        &accepted,
        0,
    )
    .await
    .unwrap();

    let by_someone_else =
        bounded().with_valid_signature(grant.message().as_bytes(), "owner-signature", &addr("0x9"));
    assert!(accept_grant(
        &signed(grant.clone()),
        &skill(),
        &addr("0x1"),
        &context(),
        &by_someone_else,
        0
    )
    .await
    .unwrap_err()
    .contains("not the owner's"));

    let mut widened = grant.clone();
    widened.run_set["maxAmountBaseUnits"] = json!("9000000000");
    assert!(accept_grant(
        &signed(widened),
        &skill(),
        &addr("0x1"),
        &context(),
        &accepted,
        0
    )
    .await
    .unwrap_err()
    .contains("not the owner's"));
}

#[tokio::test]
async fn a_grant_is_refused_for_a_stale_revision_a_revoked_wallet_or_another_signed_in_owner() {
    let grant = grant_of(&prepared(&bounded(), 0).await.unwrap());
    let live =
        bounded().with_valid_signature(grant.message().as_bytes(), "owner-signature", &addr("0x1"));
    assert!(accept_grant(
        &signed(grant.clone()),
        &skill(),
        &addr("0x1"),
        &context(),
        &live,
        1
    )
    .await
    .unwrap_err()
    .contains("not newer"));
    let revoked = chain(json!(["budget", "per_tx"]), true);
    assert!(accept_grant(
        &signed(grant.clone()),
        &skill(),
        &addr("0x1"),
        &context(),
        &revoked,
        0
    )
    .await
    .unwrap_err()
    .contains("revoked"));
    assert!(
        accept_grant(&signed(grant), &skill(), &addr("0x3"), &context(), &live, 0)
            .await
            .unwrap_err()
            .contains("belong")
    );
}
