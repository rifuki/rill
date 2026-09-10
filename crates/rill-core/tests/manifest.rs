//! The capability manifest, checked against projections captured from the TypeScript reference.
//!
//! The declaration text matters more than it looks. In the reference it was rendered in two
//! places — the frontend computed it locally, and the backend served it — and the two had to agree
//! exactly with nothing making them. These vectors are what let this implementation become the
//! single producer without changing a word of what an owner reads.

use rill_core::manifest::{
    to_declaration, to_on_chain_rule_params, to_signer_policy, CapabilityManifest, CapabilityRule,
    Enforcement, ManifestError, RuleKind,
};
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/manifest.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read manifest fixtures"))
        .expect("valid JSON")
}

#[test]
fn declarations_match_the_reference_word_for_word() {
    for case in golden()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let manifest: CapabilityManifest = serde_json::from_value(case["manifest"].clone())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let got = serde_json::to_value(to_declaration(&manifest).expect(name)).unwrap();
        assert_eq!(
            got, case["declaration"],
            "declaration diverged for \"{name}\""
        );
    }
}

#[test]
fn signer_policy_matches_the_reference() {
    for case in golden()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let manifest: CapabilityManifest =
            serde_json::from_value(case["manifest"].clone()).unwrap();
        let got = serde_json::to_value(to_signer_policy(&manifest).expect(name)).unwrap();
        assert_eq!(
            got, case["signerPolicy"],
            "signer policy diverged for \"{name}\""
        );
    }
}

/// Only the four on-chain kinds project. A pre-flight rule appearing here would let a caller
/// mistake a decorative on-chain check for an enforced one.
#[test]
fn only_on_chain_rules_project_to_move_parameters() {
    for case in golden()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let manifest: CapabilityManifest =
            serde_json::from_value(case["manifest"].clone()).unwrap();
        let params = to_on_chain_rule_params(&manifest).expect(name);
        let expected = case["onChainRuleParams"].as_array().unwrap();
        assert_eq!(
            params.len(),
            expected.len(),
            "rule-param count diverged for \"{name}\""
        );
        for (got, want) in params.iter().zip(expected) {
            assert_eq!(
                got.module,
                want["module"].as_str().unwrap(),
                "module for \"{name}\""
            );
            for (field, value) in &got.config {
                let expected_value = want["config"][*field]
                    .as_str()
                    .unwrap_or_else(|| panic!("{field} missing from golden config for \"{name}\""));
                assert_eq!(value.to_string(), expected_value, "{field} for \"{name}\"");
            }
        }
    }
}

#[test]
fn the_all_rules_case_projects_exactly_four_on_chain_rules() {
    let all = golden();
    let case = all["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "all eight rules")
        .expect("the eight-rule case");
    let manifest: CapabilityManifest = serde_json::from_value(case["manifest"].clone()).unwrap();
    assert_eq!(
        to_on_chain_rule_params(&manifest).unwrap().len(),
        4,
        "eight rules, four of which the chain actually holds"
    );
}

#[test]
fn every_cap_declares_which_layer_holds_it() {
    let manifest = CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![
            CapabilityRule::Budget {
                total_mist: "1".into(),
            },
            CapabilityRule::SlippageFloor {
                min_out_mist: "1".into(),
            },
        ],
    };
    let caps = to_declaration(&manifest).unwrap().caps;
    assert_eq!(caps[0].enforcement, Enforcement::OnChain);
    assert_eq!(caps[1].enforcement, Enforcement::PreFlight);
}

#[test]
fn a_manifest_with_no_rules_is_refused() {
    let manifest = CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![],
    };
    assert_eq!(manifest.validate(), Err(ManifestError::NoRules));
}

#[test]
fn a_duplicate_rule_kind_is_refused() {
    let manifest = CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![
            CapabilityRule::Budget {
                total_mist: "1".into(),
            },
            CapabilityRule::Budget {
                total_mist: "2".into(),
            },
        ],
    };
    assert_eq!(
        manifest.validate(),
        Err(ManifestError::DuplicateKind {
            kind: RuleKind::Budget,
            index: 1
        })
    );
}

#[test]
fn an_inverted_or_zero_width_time_window_is_refused() {
    for (before, after) in [("100", "100"), ("200", "100")] {
        let manifest = CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: vec![CapabilityRule::TimeWindow {
                not_before_ms: before.into(),
                not_after_ms: after.into(),
            }],
        };
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::EmptyTimeWindow { index: 0 }),
            "window {before}..{after} can never be satisfied"
        );
    }
}

#[test]
fn an_empty_scope_is_refused_rather_than_meaning_allow_all() {
    let manifest = CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![CapabilityRule::ProtocolScope {
            allowed_packages: vec![],
        }],
    };
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::EmptyScope { .. })
    ));
}

#[test]
fn an_unknown_field_in_a_rule_is_refused() {
    let json = serde_json::json!({
        "walletCoinType": "0x2::sui::SUI",
        "rules": [{ "kind": "budget", "totalMist": "1", "sneaky": true }]
    });
    assert!(serde_json::from_value::<CapabilityManifest>(json).is_err());
}

#[test]
fn an_amount_that_is_not_a_u64_string_is_refused() {
    let manifest = CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![CapabilityRule::Budget {
            total_mist: "1.5".into(),
        }],
    };
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::BadAmount { .. })
    ));
}

/// Every kind, written out. The producer keeps no list of its own on purpose; this one exists so
/// that a kind added to the enum is a kind these tests exercise, and the lookup under test refuses
/// to compile until it has an arm for it anyway.
const EVERY_KIND: [RuleKind; 8] = [
    RuleKind::Budget,
    RuleKind::PerTx,
    RuleKind::RateLimit,
    RuleKind::ProtocolScope,
    RuleKind::SlippageFloor,
    RuleKind::AssetScope,
    RuleKind::RecipientAllowlist,
    RuleKind::TimeWindow,
];

#[test]
fn every_kind_is_found_again_by_the_module_name_it_produces() {
    for kind in EVERY_KIND {
        assert_eq!(
            RuleKind::from_module(kind.module()),
            Some(kind),
            "{} did not round-trip through from_module",
            kind.module()
        );
    }
}

/// A name no kind claims gets no kind, not the nearest one. A consumer holding `None` reports the
/// rule as unrecognised; a consumer handed a guess would label it.
#[test]
fn a_module_name_no_kind_claims_maps_to_no_kind_rather_than_a_guess() {
    for name in [
        "",
        "Rule",
        "something_new",
        "budget::Rule",
        "BUDGET",
        " budget",
    ] {
        assert_eq!(
            RuleKind::from_module(name),
            None,
            "{name:?} is not a rule module and must not be labelled as one"
        );
    }
}

/// The names the chain reports land on the layer the module documentation names: the four kinds
/// Move proves are on chain, and the four it cannot see are pre-flight. This is the lookup the
/// wallet read uses in place of the constant it used to emit.
#[test]
fn a_module_name_read_off_chain_labels_as_the_layer_the_producer_computes() {
    for module in ["budget", "per_tx", "rate_limit", "time_window"] {
        assert_eq!(
            RuleKind::from_module(module).unwrap().enforcement(),
            Enforcement::OnChain,
            "{module} is proved by Move"
        );
    }
    for module in [
        "protocol_scope",
        "slippage_floor",
        "asset_scope",
        "recipient_allowlist",
    ] {
        assert_eq!(
            RuleKind::from_module(module).unwrap().enforcement(),
            Enforcement::PreFlight,
            "{module} has no Move prove, so nothing on chain holds it"
        );
    }
}

/// The wallet read writes the label with `as_str`; the declaration writes it through serde. Both
/// spell the same word, or an agent reading the two would see two vocabularies for one fact.
#[test]
fn the_serialized_label_and_as_str_are_the_same_word() {
    for enforcement in [Enforcement::OnChain, Enforcement::PreFlight] {
        assert_eq!(
            serde_json::to_value(enforcement).unwrap(),
            Value::String(enforcement.as_str().to_owned())
        );
    }
}

/// The source of the crate, for the one property below that is not observable from outside.
const MANIFEST_SOURCE: &str = include_str!("../src/manifest.rs");

/// The text of one `pub fn` in the manifest source, from its signature up to the next `pub fn`.
fn source_of(function: &str) -> &'static str {
    let signature = format!("pub fn {function}(");
    let start = MANIFEST_SOURCE
        .find(&signature)
        .unwrap_or_else(|| panic!("{function} is not in manifest.rs"));
    let body = &MANIFEST_SOURCE[start..];
    let end = body[signature.len()..]
        .find("pub fn ")
        .map_or(body.len(), |i| i + signature.len());
    &body[..end]
}

/// Adding a kind without deciding which layer holds it must fail to compile, not default. The
/// compiler only guarantees that while the match has no wildcard arm, and a `_ =>` added for
/// convenience compiles fine, so the arm's absence is asserted on the source itself.
#[test]
fn the_enforcement_and_module_lookups_have_no_wildcard_arm_so_a_new_kind_cannot_default() {
    for function in ["enforcement", "from_module"] {
        let body = source_of(function);
        assert!(
            body.contains("match "),
            "{function} is expected to decide by matching on the kind"
        );
        assert!(
            !body.contains("_ =>"),
            "{function} has a wildcard arm, so a new kind would default instead of failing to \
             compile"
        );
    }
}

/// A coin the registry does not know degrades to raw base units rather than guessing decimals.
#[test]
fn an_unknown_coin_type_is_reported_honestly() {
    let manifest = CapabilityManifest {
        wallet_coin_type: "0xabc::mystery::COIN".into(),
        rules: vec![CapabilityRule::Budget {
            total_mist: "12345".into(),
        }],
    };
    let d = to_declaration(&manifest).unwrap();
    assert_eq!(d.caps[0].value, "12345 base units of 0xabc::mystery::COIN");
}
