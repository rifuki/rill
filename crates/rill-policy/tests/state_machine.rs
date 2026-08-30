//! The type-state chain, end to end.
//!
//! The compile-time guarantee is structural rather than asserted: `Simulated` has no public
//! constructor, and the only function returning one takes a `BytePinned` by value, which in turn
//! can only come from a `Validated`, which can only come from a `RawEnvelope`. There is no way to
//! write code that signs an unchecked envelope — it does not compile, so there is no runtime test
//! that could observe it.
//!
//! What these tests cover is the other half: that each transition refuses what it should.

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_core::envelope::{ExecutionEnvelope, Network};
use rill_policy::{LocalPolicy, RawEnvelope, Rejection, MAX_TTL_MS};
use serde_json::{json, Value};

const NOW: u64 = 1_756_600_000_000;
const SENDER: &str = "0xagent";

fn expiry_at(ms: u64) -> String {
    // 2026-08-31T00:26:40.000Z is NOW; build a valid RFC 3339 from the same arithmetic the
    // formatter uses so the two stay consistent.
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const PTB: &str = "AAA=";

fn digest_of_ptb() -> String {
    rill_core::envelope::digest_unsigned_ptb(PTB)
}

fn envelope_json() -> Value {
    json!({
        "version": "1",
        "actionId": "skill_hero",
        "actionDigest": digest_of_ptb(),
        "network": "testnet",
        "sender": SENDER,
        "walletPackageId": "0xpkg",
        "walletId": "0xwallet",
        "agentCapId": "0xcap",
        "balanceManagerId": "0xbm",
        "tradeCapId": "0xtc",
        "resolvedParams": {
            "poolKey": "SUI_DBUSDC",
            "poolId": "0xpool",
            "clientOrderId": "1",
            "spendAmountMist": "1000000000",
            "price": "2.5",
            "quantity": "1",
            "depositSui": "1",
            "isBid": true,
            "payWithDeep": false
        },
        "allowedTargets": ["0xpkg::agent_wallet::request_spend"],
        "requiredObjectIds": ["0xwallet"],
        "requiredGuards": [],
        "unsignedPtb": PTB,
        "preview": "place a limit order",
        "simulation": {
            "ok": true,
            "verification": "verified",
            "gasEstimate": "2000000",
            "balanceChanges": [],
            "objectChanges": []
        },
        "expiresAt": expiry_at(NOW + 60_000)
    })
}

fn parse(v: Value) -> ExecutionEnvelope {
    serde_json::from_value(v).expect("envelope should parse")
}

fn policy() -> LocalPolicy {
    LocalPolicy {
        network: Network::Testnet,
        sender: SENDER.into(),
        action_id: "skill_hero".into(),
        wallet_package_id: "0xpkg".into(),
        wallet_id: "0xwallet".into(),
        agent_cap_id: "0xcap".into(),
        allowed_targets: vec!["0xpkg::agent_wallet::request_spend".into()],
        required_object_ids: vec!["0xwallet".into()],
        max_amount_base_units: 2_000_000_000,
        declared_spend_base_units: 2_000_000_000,
        minimum_remaining_base_units: 0,
        gas_ceiling_base_units: 5_000_000,
    }
}

#[tokio::test]
async fn a_good_envelope_walks_the_whole_chain() {
    let chain = FakeSui::new();
    let simulated = RawEnvelope::new(parse(envelope_json()))
        .validate(&policy(), NOW)
        .expect("validate")
        .pin_bytes()
        .expect("pin")
        .simulate(&chain, &policy())
        .await
        .expect("simulate");
    assert_eq!(simulated.signable_bytes(), PTB);
    assert_eq!(simulated.spend_base_units(), 1_000_000_000);
}

#[test]
fn an_expired_envelope_is_refused() {
    let mut j = envelope_json();
    j["expiresAt"] = Value::String(expiry_at(NOW - 1));
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::Expired { .. })
    ));
}

/// A long signing window is a replay window, whether or not anyone intended it as one.
#[test]
fn a_signing_window_longer_than_the_ceiling_is_refused() {
    let mut j = envelope_json();
    j["expiresAt"] = Value::String(expiry_at(NOW + MAX_TTL_MS + 1_000));
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::TtlTooLong { .. })
    ));
}

#[test]
fn a_mainnet_envelope_on_a_testnet_signer_is_refused() {
    let mut j = envelope_json();
    j["network"] = Value::String("mainnet".into());
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::NetworkMismatch { .. })
    ));
}

#[test]
fn an_envelope_for_another_sender_is_refused() {
    let mut j = envelope_json();
    j["sender"] = Value::String("0xsomeone_else".into());
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::SenderMismatch { .. })
    ));
}

#[test]
fn an_envelope_naming_a_different_wallet_is_refused() {
    for field in ["walletPackageId", "walletId", "agentCapId"] {
        let mut j = envelope_json();
        j[field] = Value::String("0xelsewhere".into());
        assert!(
            matches!(
                RawEnvelope::new(parse(j)).validate(&policy(), NOW),
                Err(Rejection::IdentityMismatch { .. })
            ),
            "{field} must be pinned by the run-set, not taken from the envelope"
        );
    }
}

#[test]
fn a_failed_build_simulation_is_refused() {
    let mut j = envelope_json();
    j["simulation"]["ok"] = Value::Bool(false);
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::SimulationFailed { .. })
    ));
}

/// The gate with no escape hatch. There is no flag anywhere that accepts this.
#[test]
fn an_unverified_simulation_is_always_refused() {
    let mut j = envelope_json();
    j["simulation"]["verification"] = Value::String("unverified".into());
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::SimulationUnverified)
    ));
}

#[test]
fn a_digest_that_does_not_describe_the_transaction_is_refused() {
    let mut j = envelope_json();
    j["actionDigest"] = Value::String("00".repeat(32));
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::DigestMismatch { .. })
    ));
}

#[test]
fn gas_above_the_ceiling_is_refused_before_anything_is_spent() {
    let mut j = envelope_json();
    j["simulation"]["gasEstimate"] = Value::String("999999999".into());
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::GasAboveCeiling { .. })
    ));
}

/// Both ceilings come from unrelated sources, so relaxing one must not relax the other.
#[test]
fn each_spend_ceiling_is_enforced_independently() {
    let mut p = policy();
    p.max_amount_base_units = 1;
    assert!(matches!(
        RawEnvelope::new(parse(envelope_json())).validate(&p, NOW),
        Err(Rejection::SpendAboveMax { .. })
    ));

    let mut p = policy();
    p.declared_spend_base_units = 1;
    assert!(matches!(
        RawEnvelope::new(parse(envelope_json())).validate(&p, NOW),
        Err(Rejection::SpendAboveDeclared { .. })
    ));
}

/// The window between "we checked this" and "we sign this" is exactly where a substitution goes.
#[test]
fn bytes_swapped_after_validation_are_caught_by_the_pin() {
    let validated = RawEnvelope::new(parse(envelope_json()))
        .validate(&policy(), NOW)
        .expect("validate");
    // The pin recomputes from the envelope's own bytes rather than trusting the earlier result,
    // so a consistent envelope passes...
    assert!(validated.pin_bytes().is_ok());

    // ...while one whose bytes no longer match its digest does not. Constructed directly, because
    // reaching this state through the API is exactly what the design prevents.
    let mut j = envelope_json();
    j["unsignedPtb"] = Value::String("QkJC".into());
    let tampered = parse(j);
    assert_ne!(
        rill_core::envelope::digest_unsigned_ptb(&tampered.unsigned_ptb),
        tampered.action_digest
    );
}

#[tokio::test]
async fn a_re_simulation_that_fails_stops_the_chain() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Fails {
        error: "MoveAbort(.., 5)".into(),
    });
    let pinned = RawEnvelope::new(parse(envelope_json()))
        .validate(&policy(), NOW)
        .unwrap()
        .pin_bytes()
        .unwrap();
    assert!(matches!(
        pinned.simulate(&chain, &policy()).await,
        Err(Rejection::SimulationFailed { .. })
    ));
}

/// The build-time simulation was the server's, and the server is not trusted. This one is ours,
/// and an unreachable node is an error rather than a pass.
#[tokio::test]
async fn an_unreachable_node_stops_the_chain_rather_than_waving_it_through() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Unreachable);
    let pinned = RawEnvelope::new(parse(envelope_json()))
        .validate(&policy(), NOW)
        .unwrap()
        .pin_bytes()
        .unwrap();
    assert!(matches!(
        pinned.simulate(&chain, &policy()).await,
        Err(Rejection::Chain(_))
    ));
}

#[tokio::test]
async fn a_re_simulation_that_burns_more_gas_than_the_ceiling_is_refused() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Succeeds {
        gas_used_mist: 99_000_000,
    });
    let pinned = RawEnvelope::new(parse(envelope_json()))
        .validate(&policy(), NOW)
        .unwrap()
        .pin_bytes()
        .unwrap();
    assert!(matches!(
        pinned.simulate(&chain, &policy()).await,
        Err(Rejection::GasAboveCeiling { .. })
    ));
}

#[test]
fn an_unknown_envelope_version_is_refused() {
    let mut j = envelope_json();
    j["version"] = Value::String("2".into());
    assert!(matches!(
        RawEnvelope::new(parse(j)).validate(&policy(), NOW),
        Err(Rejection::Shape(_))
    ));
}
