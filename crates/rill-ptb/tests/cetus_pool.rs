//! A pool's own state and coin types, read from its object.
//!
//! What is deliberately not here is arithmetic that predicts a fill. This file used to hold it: a
//! constant-price formula over `current_sqrt_price`, with eight tests pinning its output. Measured
//! against real fills on testnet it was 11% high, and 36% high after the pool had moved, so a floor
//! derived from it was refused by the floor's own guard. The replacement is to simulate the real swap
//! and take the node's figure, which matched a real fill to the base unit. The tests for that live in
//! `bins/rill/tests/quote_flow.rs`, against the path that does it.

use rill_ptb::cetus::{pool_state, PoolState};
use serde_json::json;

/// The pool this was built against, read from testnet.
fn live_pool() -> PoolState {
    PoolState {
        current_sqrt_price: 16_565_176_178_191_172,
        fee_rate: 2500,
        liquidity: 10_000_000_000,
        is_paused: false,
    }
}

/// The pool's state parses out of exactly what the node sends.
#[test]
fn the_nodes_pool_fields_parse_as_written() {
    let parsed = pool_state(&json!({
        "current_sqrt_price": "16565176178191172",
        "fee_rate": "2500",
        "liquidity": "10000000000",
        "is_pause": false,
        // Present in the real object and deliberately unread.
        "tick_spacing": 60.0,
        "current_tick_index": { "bits": 4294826982.0 }
    }))
    .expect("the node's shape parses");
    assert_eq!(parsed, live_pool());
}

/// A missing field is named rather than defaulted.
///
/// A pool read that silently defaulted `is_pause` to false would report a paused pool as tradeable.
#[test]
fn a_missing_pool_field_is_named() {
    for missing in ["current_sqrt_price", "fee_rate", "liquidity", "is_pause"] {
        let mut fields = json!({
            "current_sqrt_price": "1",
            "fee_rate": "1",
            "liquidity": "1",
            "is_pause": false
        });
        fields.as_object_mut().expect("an object").remove(missing);
        let err = pool_state(&fields).expect_err("a missing field must be refused");
        assert!(
            err.contains(missing),
            "the refusal must name the field that was absent: {err}"
        );
    }
}

/// The pool names its own coin types, in the order every Cetus argument wants them.
#[test]
fn the_pools_coin_types_come_out_of_its_object_type() {
    let t = "0x5372d555ac734e272659136c2a0cd3227f9b92de67c80dc11250307268af2db8::pool::Pool<0xbcd2c79828a21415197804dc5d720e0cfadaef302c361ba0db572f257d6d6408::h::H, 0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";
    assert_eq!(
        rill_ptb::cetus::pool_coin_types(t),
        Some((
            "0xbcd2c79828a21415197804dc5d720e0cfadaef302c361ba0db572f257d6d6408::h::H".to_string(),
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
                .to_string()
        ))
    );
}

/// A generic coin type does not get split down the middle.
///
/// Splitting on the first comma works until a pool lists a wrapped or generic asset, and then it
/// produces two halves of one type name that the node rejects for a reason naming neither.
#[test]
fn a_generic_coin_type_is_not_split_at_its_own_comma() {
    let t = "0xp::pool::Pool<0xa::wrap::W<0xb::x::X, 0xc::y::Y>, 0x2::sui::SUI>";
    assert_eq!(
        rill_ptb::cetus::pool_coin_types(t),
        Some((
            "0xa::wrap::W<0xb::x::X, 0xc::y::Y>".to_string(),
            "0x2::sui::SUI".to_string()
        ))
    );
}

/// Anything that is not a two-parameter pool type is refused rather than guessed at.
#[test]
fn a_type_that_is_not_a_pool_yields_nothing() {
    for bad in [
        "0x2::sui::SUI",
        "0xp::pool::Pool<0xa::x::X>",
        "0xp::pool::Pool<>",
        "0xp::pool::Pool<,0x2::sui::SUI>",
        "0xp::pool::Pool<0x2::sui::SUI,>",
    ] {
        assert_eq!(
            rill_ptb::cetus::pool_coin_types(bad),
            None,
            "{bad} is not a pool type with two coins"
        );
    }
}
