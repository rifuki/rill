//! Envelope shape and digest, checked against vectors generated from the TypeScript reference.
//!
//! The digest is the pin that makes build-then-sign safe across a process boundary: the server
//! hashes what it built, the signer re-hashes what it received, and drift shows up as a mismatch
//! instead of as a signed surprise. That only works if both sides hash identically — so these
//! vectors come from the reference implementation, not from this one.

use rill_core::envelope::{digest_unsigned_ptb, EnvelopeError, ExecutionEnvelope};
use serde_json::Value;

fn fixtures() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/envelope.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read envelope fixtures"))
        .expect("valid JSON")
}

#[test]
fn digest_matches_the_reference_byte_for_byte() {
    for v in fixtures()["digest"]["vectors"].as_array().unwrap() {
        let input = v["input"].as_str().unwrap();
        let expected = v["digest"].as_str().unwrap();
        assert_eq!(
            digest_unsigned_ptb(input),
            expected,
            "digest diverged for an input of {} chars",
            input.len()
        );
    }
}

/// A minimal DeepBook envelope, as JSON, for shape tests.
fn deepbook_envelope_json() -> Value {
    serde_json::json!({
        "version": "1",
        "actionId": "skill_abc",
        "actionDigest": "00",
        "network": "testnet",
        "sender": "0x1",
        "walletPackageId": "0x2",
        "walletId": "0x3",
        "agentCapId": "0x4",
        "balanceManagerId": "0x5",
        "tradeCapId": "0x6",
        "resolvedParams": {
            "poolKey": "SUI_DBUSDC",
            "poolId": "0x7",
            "clientOrderId": "1",
            "spendAmountMist": "1000000000",
            "price": "2.5",
            "quantity": "1.5",
            "depositSui": "1",
            "isBid": true,
            "payWithDeep": false
        },
        "allowedTargets": ["0x2::agent_wallet::request_spend"],
        "requiredObjectIds": ["0x3"],
        "requiredGuards": [],
        "unsignedPtb": "AAA=",
        "preview": "place a limit order",
        "simulation": {
            "ok": true,
            "verification": "verified",
            "gasEstimate": "1000",
            "balanceChanges": [],
            "objectChanges": []
        },
        "expiresAt": "2026-08-31T00:00:00.000Z"
    })
}

#[test]
fn a_well_formed_deepbook_envelope_round_trips() {
    let env: ExecutionEnvelope = serde_json::from_value(deepbook_envelope_json()).expect("parse");
    env.validate_shape().expect("shape should be valid");
    let back = serde_json::to_value(&env).expect("serialize");
    let again: ExecutionEnvelope = serde_json::from_value(back).expect("re-parse");
    assert_eq!(
        env, again,
        "an envelope must survive a round trip unchanged"
    );
}

#[test]
fn an_unknown_field_is_refused_at_every_level() {
    // Top level.
    let mut top = deepbook_envelope_json();
    top["simulationGate"] = Value::Bool(true);
    assert!(
        serde_json::from_value::<ExecutionEnvelope>(top).is_err(),
        "a field added at the top level must fail closed, not pass through"
    );

    // Nested — this is the level a strict-at-the-top-only schema would miss.
    let mut nested = deepbook_envelope_json();
    nested["simulation"]["allowUnverified"] = Value::Bool(true);
    assert!(
        serde_json::from_value::<ExecutionEnvelope>(nested).is_err(),
        "a field added inside simulation must fail closed"
    );

    let mut params = deepbook_envelope_json();
    params["resolvedParams"]["slippageOverride"] = Value::String("0".into());
    assert!(
        serde_json::from_value::<ExecutionEnvelope>(params).is_err(),
        "a field added inside resolvedParams must fail closed"
    );
}

#[test]
fn a_partial_deepbook_binding_is_refused() {
    let mut json = deepbook_envelope_json();
    json.as_object_mut().unwrap().remove("tradeCapId");
    let env: ExecutionEnvelope = serde_json::from_value(json).expect("parse");
    assert_eq!(
        env.validate_shape(),
        Err(EnvelopeError::IncompleteDeepBookBinding),
        "two thirds of a DeepBook binding is not a DeepBook envelope"
    );
}

#[test]
fn an_envelope_describing_no_action_is_refused() {
    let mut json = deepbook_envelope_json();
    for f in ["balanceManagerId", "tradeCapId", "resolvedParams"] {
        json.as_object_mut().unwrap().remove(f);
    }
    let env: ExecutionEnvelope = serde_json::from_value(json).expect("parse");
    assert_eq!(env.validate_shape(), Err(EnvelopeError::NoActionShape));
}

#[test]
fn an_unknown_version_is_refused() {
    let mut json = deepbook_envelope_json();
    json["version"] = Value::String("2".into());
    let env: ExecutionEnvelope = serde_json::from_value(json).expect("parse");
    assert!(matches!(
        env.validate_shape(),
        Err(EnvelopeError::UnsupportedVersion(_))
    ));
}

/// The migration-window tolerance: an envelope from the TypeScript server carries price and
/// quantity as JSON numbers. It must still parse, and must land on the exact decimal.
#[test]
fn a_numeric_price_from_the_typescript_server_still_parses_exactly() {
    let mut json = deepbook_envelope_json();
    json["resolvedParams"]["price"] = serde_json::json!(2362.123456);
    let env: ExecutionEnvelope = serde_json::from_value(json).expect("a numeric price must parse");
    let params = env.resolved_params.expect("resolved params");
    assert_eq!(params.price.as_str(), "2362.123456");
    // And it serializes back out as a string, so the number does not propagate onward.
    let out = serde_json::to_value(&params).unwrap();
    assert_eq!(out["price"], Value::String("2362.123456".into()));
}

#[test]
fn a_float_shaped_price_that_is_not_a_decimal_is_refused() {
    let mut json = deepbook_envelope_json();
    json["resolvedParams"]["price"] = Value::String("1e-10".into());
    assert!(
        serde_json::from_value::<ExecutionEnvelope>(json).is_err(),
        "scientific notation is not a price"
    );
}
