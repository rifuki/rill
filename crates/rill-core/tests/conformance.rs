//! Runs `fixtures/amounts.json` against the Rust money path.
//!
//! The same file is run against the TypeScript reference by `ts/verify-reference.ts`, so the two
//! implementations are measured against one identical set of expectations rather than two test
//! suites that can drift apart.
//!
//! Three vectors are marked `reference_agrees: false`. Those are cases where the reference's
//! float arithmetic lands one base unit off the exact value. **Rust is expected to get all of
//! them right** — that is the point of the rebuild, so they are asserted here with no exception.

use rill_core::amounts::{
    decimal_to_base_units, deepbook_price_to_base_units, deepbook_quantity_to_base_units,
    parse_u64_string,
};
use serde_json::Value;

fn fixtures() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/amounts.json");
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).expect("fixtures must be valid JSON")
}

fn u128_of(v: &Value, key: &str) -> u128 {
    v[key].as_str().expect(key).parse().expect(key)
}

#[test]
fn decimal_to_base_units_accepts_every_valid_vector() {
    let f = fixtures();
    for v in f["decimal_to_base_units"]["accepted"].as_array().unwrap() {
        let value = v["value"].as_str().unwrap();
        let decimals = v["decimals"].as_u64().unwrap() as u32;
        let expected: u64 = v["expected"].as_str().unwrap().parse().unwrap();
        let got = decimal_to_base_units(value, decimals)
            .unwrap_or_else(|e| panic!("\"{value}\" @{decimals} should be accepted, got: {e}"));
        assert_eq!(got, expected, "\"{value}\" @{decimals} — {}", v["why"]);
    }
}

#[test]
fn decimal_to_base_units_rejects_every_invalid_vector() {
    let f = fixtures();
    for v in f["decimal_to_base_units"]["rejected"].as_array().unwrap() {
        let value = v["value"].as_str().unwrap();
        let decimals = v["decimals"].as_u64().unwrap() as u32;
        assert!(
            decimal_to_base_units(value, decimals).is_err(),
            "\"{value}\" @{decimals} should be rejected — {}",
            v["why"]
        );
    }
}

#[test]
fn parse_u64_matches_every_vector() {
    let f = fixtures();
    for v in f["parse_u64"]["accepted"].as_array().unwrap() {
        let value = v["value"].as_str().unwrap();
        let expected: u64 = v["expected"].as_str().unwrap().parse().unwrap();
        assert_eq!(parse_u64_string(value).unwrap(), expected, "\"{value}\"");
    }
    for v in f["parse_u64"]["rejected"].as_array().unwrap() {
        let value = v["value"].as_str().unwrap();
        assert!(
            parse_u64_string(value).is_err(),
            "\"{value}\" — {}",
            v["why"]
        );
    }
}

/// The vectors the reference gets wrong. Each is a real DeepBook pool shape, not a contrived one.
#[test]
fn deepbook_price_is_exact_at_every_pool_scale() {
    let f = fixtures();
    let float_scalar = u128_of(&f["deepbook_price"], "$float_scalar");
    for v in f["deepbook_price"]["vectors"].as_array().unwrap() {
        let price = v["price"].as_str().unwrap();
        let expected: u64 = v["expected"].as_str().unwrap().parse().unwrap();
        let got = deepbook_price_to_base_units(
            price,
            float_scalar,
            u128_of(v, "quote_scalar"),
            u128_of(v, "base_scalar"),
        )
        .unwrap_or_else(|e| panic!("price {price} on {}: {e}", v["pool_shape"]));

        assert_eq!(got, expected, "price {price} on {}", v["pool_shape"]);

        // Where the reference diverges, assert we did NOT reproduce its answer. Getting the
        // right number by accident and the wrong one by regression look identical without this.
        if v["reference_agrees"] == Value::Bool(false) {
            let reference: u64 = v["reference_returns"].as_str().unwrap().parse().unwrap();
            assert_ne!(
                got, reference,
                "price {price} reproduced the reference's float error",
            );
        }
    }
}

#[test]
fn deepbook_quantity_is_exact_above_the_double_precision_limit() {
    let f = fixtures();
    for v in f["deepbook_quantity"]["vectors"].as_array().unwrap() {
        let quantity = v["quantity"].as_str().unwrap();
        let expected: u64 = v["expected"].as_str().unwrap().parse().unwrap();
        let got = deepbook_quantity_to_base_units(quantity, u128_of(v, "base_scalar"))
            .unwrap_or_else(|e| panic!("quantity {quantity}: {e}"));
        assert_eq!(got, expected, "quantity {quantity}");

        if v["reference_agrees"] == Value::Bool(false) {
            let reference: u64 = v["reference_returns"].as_str().unwrap().parse().unwrap();
            assert_ne!(
                got, reference,
                "quantity {quantity} reproduced the reference's float error"
            );
        }
    }
}

/// A conversion that cannot be represented exactly is refused rather than rounded. The reference
/// rounds here; rounding an order price changes the order the caller asked for.
#[test]
fn an_inexact_scaling_is_refused_not_rounded() {
    // 1/3 of a base unit: exact arithmetic has nowhere to put the remainder.
    let refused = deepbook_price_to_base_units("1", 1, 1, 3);
    assert!(
        refused.is_err(),
        "a non-integral scaling must be refused, got {refused:?}"
    );
}
