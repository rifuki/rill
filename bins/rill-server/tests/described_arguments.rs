//! What `rill_describe_action` says you must pass is what the build refuses you for omitting.
//!
//! The describe tool advertises that it describes "an action's parameters". It used to return a name,
//! a description, the network and two sentences about signing, so an agent reading it learned nothing
//! about how to call `rill_build_action` and had to be told the shape out of band. The shape is now
//! reported from `REQUIRED_BUILD_FIELDS`.
//!
//! A list in one file and a parser in another is two sources for one fact. These tests make them one:
//! every listed path is removed in turn and the parser must refuse and name it, and the parser is
//! driven with a complete request to show the list is not merely long enough to always fail.

use rill_core::envelope::Network;
use rill_server::request::{parse_build_request, REQUIRED_BUILD_FIELDS};
use serde_json::{json, Value};

fn addr(n: u8) -> String {
    format!("0x{n:064x}")
}

const DIGEST: &str = "11111111111111111111111111111111";

/// A request with every required field present.
fn complete() -> Value {
    json!({
        "actionId": "skill_x",
        "sender": addr(9),
        "agentWallet": {
            "packageId": addr(1),
            "walletId": addr(2),
            "versionId": addr(3),
            "capId": addr(4),
            "capVersion": 7,
            "capDigest": DIGEST,
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [{ "kind": "budget", "totalMist": "5000000000" }]
            }
        },
        "params": {
            "poolId": addr(5),
            "baseCoinType": "0xdeep::deep::DEEP",
            "quoteCoinType": "0x2::sui::SUI",
            "baseScalar": 1000000,
            "quoteScalar": 1000000000,
            "balanceManagerId": addr(6),
            "tradeCapId": addr(7),
            "tradeCapVersion": 3,
            "tradeCapDigest": DIGEST,
            "depositCapId": addr(8),
            "depositCapVersion": 4,
            "depositCapDigest": DIGEST,
            "gasObjectId": addr(10),
            "gasObjectVersion": 5,
            "gasObjectDigest": DIGEST,
            "clientOrderId": 1,
            "price": "1.5",
            "quantity": "10",
            "isBid": true,
            "spendAmountMist": "1000000"
        }
    })
}

fn parse(arguments: &Value) -> Result<(), String> {
    parse_build_request(
        arguments,
        "skill_x",
        Network::Testnet,
        addr(11).parse().expect("an address"),
        50_000_000,
    )
    .map(|_| ())
    .map_err(|e| format!("{}: {}", e.field, e.reason))
}

/// Remove a dotted path from a request.
fn without(arguments: &Value, path: &str) -> Value {
    let mut out = arguments.clone();
    let mut parts = path.split('.').collect::<Vec<_>>();
    let last = parts.pop().expect("a field name");
    let mut cursor = &mut out;
    for part in parts {
        cursor = cursor
            .get_mut(part)
            .expect("the path exists in a complete request");
    }
    cursor
        .as_object_mut()
        .expect("an object")
        .remove(last)
        .expect("the field was there to remove");
    out
}

/// The complete request parses. Without this, every test below could pass on a request that was
/// broken for some other reason.
#[test]
fn the_complete_request_parses() {
    assert_eq!(
        parse(&complete()),
        Ok(()),
        "the fixture itself must be valid"
    );
}

/// Every field the describe tool advertises is one the parser refuses to proceed without, and the
/// refusal names it.
///
/// This is the direction that catches a list grown stale: a path listed here that the parser does not
/// actually require fails, because removing it parses fine.
#[test]
fn every_advertised_field_is_one_the_parser_requires() {
    for path in REQUIRED_BUILD_FIELDS {
        let err = parse(&without(&complete(), path)).expect_err(&format!(
            "removing {path} must be refused, or it is not required"
        ));
        assert!(
            err.starts_with(path) || err.contains(path),
            "the refusal for a missing {path} must name it, and said: {err}"
        );
    }
}

/// And the other direction: nothing the parser requires is missing from the list.
///
/// Checked by removing each leaf of the complete request that the list does not mention, and
/// asserting the request still parses. A field the parser requires and the list omits fails here,
/// which is the case an agent would otherwise meet as an unexplained refusal.
#[test]
fn nothing_the_parser_requires_is_left_off_the_list() {
    let complete = complete();
    let mut unlisted = Vec::new();
    for (group, prefix) in [("agentWallet", "agentWallet."), ("params", "params.")] {
        let obj = complete[group].as_object().expect("an object");
        for key in obj.keys() {
            let path = format!("{prefix}{key}");
            if !REQUIRED_BUILD_FIELDS.contains(&path.as_str()) {
                unlisted.push(path);
            }
        }
    }

    for path in &unlisted {
        assert_eq!(
            parse(&without(&complete, path)),
            Ok(()),
            "{path} is not advertised as required, so removing it must still parse; if the parser \
             needs it, it belongs in REQUIRED_BUILD_FIELDS"
        );
    }
}

/// `payWithDeep` is the one field that is optional on purpose, and it is absent from the list.
///
/// Named here so its absence reads as a decision rather than an oversight the test above tolerated.
#[test]
fn pay_with_deep_is_optional_and_says_so_by_being_absent() {
    assert!(
        !REQUIRED_BUILD_FIELDS.contains(&"params.payWithDeep"),
        "it defaults to false, so requiring it would make every caller state a default"
    );
    let mut with_it = complete();
    with_it["params"]["payWithDeep"] = json!(true);
    assert_eq!(parse(&with_it), Ok(()), "and supplying it is accepted");
}
