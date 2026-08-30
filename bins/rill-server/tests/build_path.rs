//! The keyless build path, and the property that matters most about it: what it produces is what
//! the signer accepts.
//!
//! The last test here builds an envelope and then runs it through `rill-policy` — the same code
//! the signer runs. Two independently-written halves that must agree is exactly where a system
//! drifts, so they are checked against each other rather than each against its own idea of the
//! contract.

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_core::envelope::{digest_unsigned_ptb, Network};
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use rill_ptb::deepbook::PoolSpec;
use rill_server::build::{build, gas_object, BuildOutcome, ENVELOPE_TTL_MS};
use sui_sdk_types::{Address, Digest};

const NOW: u64 = 1_756_600_000_000;
const SUI: &str = "0x2::sui::SUI";

fn addr(n: u8) -> Address {
    format!("0x{:064x}", n).parse().unwrap()
}

fn request() -> rill_server::build::BuildRequest {
    rill_server::build::BuildRequest {
        action_id: "skill_hero".into(),
        sender: addr(9),
        network: Network::Testnet,
        wallet_package_id: addr(0xca),
        wallet_id: addr(1),
        agent_cap: gas_object(addr(2), 1, Digest::ZERO),
        agent_cap_id: addr(2).to_string(),
        version_id: addr(3),
        manifest: CapabilityManifest {
            wallet_coin_type: SUI.into(),
            rules: vec![
                CapabilityRule::Budget {
                    total_mist: "5000000000".into(),
                },
                CapabilityRule::PerTx {
                    max_mist: "2000000000".into(),
                },
            ],
        },
        deepbook_package_id: addr(0xde),
        pool: PoolSpec {
            pool_id: addr(0x20),
            base_coin_type: "0xde::deep::DEEP".into(),
            quote_coin_type: SUI.into(),
            base_scalar: 1_000_000,
            quote_scalar: 1_000_000_000,
        },
        balance_manager_id: addr(0x21),
        trade_cap: gas_object(addr(0x22), 1, Digest::ZERO),
        trade_cap_id: addr(0x22).to_string(),
        client_order_id: 1,
        price: "2362.123456".into(),
        quantity: "1".into(),
        is_bid: true,
        pay_with_deep: false,
        spend_base_units: 1_000_000_000,
        gas_budget: 50_000_000,
        gas_price: 1_000,
        gas_objects: vec![gas_object(addr(0x0a), 1, Digest::ZERO)],
    }
}

#[tokio::test]
async fn a_good_build_produces_an_envelope_whose_digest_describes_its_own_bytes() {
    let chain = FakeSui::new();
    let BuildOutcome::Built(envelope) = build(&request(), &chain, NOW).await else {
        panic!("this build should succeed");
    };
    assert_eq!(
        envelope.action_digest,
        digest_unsigned_ptb(&envelope.unsigned_ptb),
        "the digest must describe the transaction actually carried"
    );
    assert_eq!(envelope.version, "1");
    assert!(envelope.simulation.ok);
}

/// The full pinned sequence, funding and order together — this is what the signer compares
/// against, so a build that emitted a different one would be refused downstream.
#[tokio::test]
async fn the_envelope_carries_the_whole_call_sequence_in_order() {
    let chain = FakeSui::new();
    let BuildOutcome::Built(envelope) = build(&request(), &chain, NOW).await else {
        panic!("should build");
    };
    let targets = &envelope.allowed_targets;
    assert_eq!(targets.len(), 7, "{targets:?}");
    assert!(targets[0].ends_with("::agent_wallet::request_spend"));
    assert!(targets[1].ends_with("::budget::prove"));
    assert!(targets[2].ends_with("::per_tx::prove"));
    assert!(targets[3].ends_with("::agent_wallet::confirm_spend"));
    assert!(targets[6].ends_with("::pool::place_limit_order"));
}

/// An unreachable node and a rejected transaction mean opposite things. Folding the first into the
/// second is how an unchecked transaction gets built.
#[tokio::test]
async fn an_unreachable_node_is_its_own_refusal_not_a_verdict() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Unreachable);
    let BuildOutcome::Refused { code, .. } = build(&request(), &chain, NOW).await else {
        panic!("an unreachable node must not produce an envelope");
    };
    assert_eq!(code, "simulation_unavailable");
}

#[tokio::test]
async fn a_failing_simulation_refuses_and_says_why() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Fails {
        error: "MoveAbort(.., 5)".into(),
    });
    let BuildOutcome::Refused { code, reason } = build(&request(), &chain, NOW).await else {
        panic!("should refuse");
    };
    assert_eq!(code, "simulation_failed");
    assert!(reason.contains("MoveAbort"));
}

/// The gate with no override anywhere in the system.
#[tokio::test]
async fn an_inconclusive_simulation_refuses() {
    let error = format!(
        "MoveAbort in {}::config: checked_package_version",
        rill_chain::CETUS_PACKAGE_IDS[0]
    );
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Fails { error });
    let BuildOutcome::Refused { code, .. } = build(&request(), &chain, NOW).await else {
        panic!("should refuse");
    };
    assert_eq!(code, "simulation_unverified");
}

#[tokio::test]
async fn a_manifest_with_no_rules_cannot_produce_a_build() {
    let mut r = request();
    r.manifest.rules.clear();
    let chain = FakeSui::new();
    assert!(
        matches!(build(&r, &chain, NOW).await, BuildOutcome::Refused { .. }),
        "an ungated spend must not be buildable"
    );
}

#[tokio::test]
async fn a_price_that_cannot_be_represented_exactly_refuses_rather_than_rounding() {
    let mut r = request();
    r.price = "1.0000000000000001".into();
    let chain = FakeSui::new();
    let BuildOutcome::Refused { code, .. } = build(&r, &chain, NOW).await else {
        panic!("should refuse");
    };
    assert_eq!(code, "order_rejected");
}

/// The one that proves the two halves agree: build here, then validate with the same code the
/// signer runs.
#[tokio::test]
async fn what_the_server_builds_is_what_the_signer_accepts() {
    use rill_policy::{LocalPolicy, RawEnvelope};

    let r = request();
    let chain = FakeSui::new();
    let BuildOutcome::Built(envelope) = build(&r, &chain, NOW).await else {
        panic!("should build");
    };

    let policy = LocalPolicy {
        network: Network::Testnet,
        sender: r.sender.to_string(),
        action_id: r.action_id.clone(),
        wallet_package_id: r.wallet_package_id.to_string(),
        wallet_id: r.wallet_id.to_string(),
        agent_cap_id: r.agent_cap_id.clone(),
        allowed_targets: envelope.allowed_targets.clone(),
        required_object_ids: envelope.required_object_ids.clone(),
        max_amount_base_units: 2_000_000_000,
        declared_spend_base_units: 2_000_000_000,
        minimum_remaining_base_units: 0,
        gas_ceiling_base_units: 50_000_000,
    };

    let validated = RawEnvelope::new(*envelope)
        .validate(&policy, NOW)
        .expect("the signer must accept what the server built");
    let pinned = validated.pin_bytes().expect("byte pin");
    pinned
        .simulate(&chain, &policy)
        .await
        .expect("re-simulation");
}

/// The expiry the server writes must be the expiry the signer can read. Two hand-written date
/// routines that disagree would make every envelope unsignable.
#[tokio::test]
async fn the_expiry_the_server_writes_is_the_one_the_signer_parses() {
    let chain = FakeSui::new();
    let BuildOutcome::Built(envelope) = build(&request(), &chain, NOW).await else {
        panic!("should build");
    };
    let parsed = rill_policy::parse_rfc3339_ms(&envelope.expires_at)
        .expect("the signer must be able to read this timestamp");
    assert_eq!(parsed, NOW + ENVELOPE_TTL_MS);
}
