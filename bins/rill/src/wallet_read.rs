//! Reading a wallet's limits, and saying which layer holds each one.
//!
//! The on-chain rules come from the chain that enforces them. Not from a run-set, and not from
//! anything this process was told at startup: a limit reported from a local copy is a limit an
//! agent could be shown after it had already changed, so for those the answer has to come from
//! there.
//!
//! # Not every limit is on chain, and this read says which
//!
//! The chain holds four kinds of rule and proves them against the real transaction. The other
//! four exist only pre-flight: protocol scope, asset scope and recipient allowlist are enforced by
//! this signer refusing to sign, and the slippage floor by the signer refusing to sign an envelope
//! whose guard call does not match, and by the chain aborting when the floor is breached.
//!
//! An earlier version of this read labelled every rule `"on-chain"` with a constant and said in
//! its note that nothing could widen them. True of the four the chain holds, false of the rest,
//! and an owner deciding how much to grant was being told the chain held a recipient allowlist it
//! has never heard of. The label now comes from the one producer, [`RuleKind::enforcement`], per
//! rule, and the pre-flight rules are listed from the loaded run-set with the layer that holds
//! them stated beside each.

use rill_chain::{grpc::GrpcSui, SuiRead};
use rill_core::manifest::{CapabilityManifest, CapabilityRule, Enforcement, RuleKind};
use rill_ptb::policy_read::{parse_type_names, policy_rules_transaction, rule_module};
use rill_ptb::shared::SharedObjects;
use serde_json::{json, Map, Value};
use sui_sdk_types::Address;

/// Read the rules attached to a wallet, how it is identified, and which layer holds each limit.
///
/// `local` is the loaded run-set's manifest, when there is one. It contributes only the pre-flight
/// rules. For the on-chain kinds the chain's answer is the answer: a manifest that disagrees with
/// it is a reconciliation problem, not a second source of truth.
pub async fn read_limits(
    endpoint: &str,
    package_id: &str,
    wallet_id: &str,
    local: Option<&CapabilityManifest>,
) -> Result<Value, String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    read_limits_from(&chain, package_id, wallet_id, local).await
}

/// The read itself, against any [`SuiRead`].
///
/// Split out so the assembly can be driven offline. What it assembles is not obvious: two round
/// trips, a shared version that has to be present or the object is not a wallet, a gas price that
/// has to be read because a read priced below the reference is refused, and only then the
/// labelling. Every one of those steps was reachable only through a live testnet node, so a test
/// that a later edit passing `None` for the manifest would fail had to spend real SUI to run, and
/// therefore did not run in CI at all.
pub async fn read_limits_from(
    chain: &impl SuiRead,
    package_id: &str,
    wallet_id: &str,
    local: Option<&CapabilityManifest>,
) -> Result<Value, String> {
    let wallet: Address = wallet_id
        .parse()
        .map_err(|_| format!("{wallet_id} is not an address"))?;

    let summary = chain
        .get_object(wallet_id)
        .await
        .map_err(|e| format!("reading the wallet: {e}"))?;
    let initial = summary.shared_initial_version.ok_or_else(|| {
        format!("{wallet_id} is not a shared object, so it is not an AgentWallet")
    })?;

    let mut shared = SharedObjects::new();
    shared.insert(wallet, initial);

    // One more round trip than a read strictly needs, and the alternative is a literal: the node
    // refuses a read priced below its reference, and the reference is not the same on every
    // network.
    let gas_price = chain
        .reference_gas_price()
        .await
        .map_err(|e| format!("reading the reference gas price: {e}"))?;

    let tx = policy_rules_transaction(
        package_id
            .parse()
            .map_err(|_| format!("{package_id} is not an address"))?,
        wallet,
        "0x2::sui::SUI",
        &shared,
        gas_price,
    )
    .map_err(|e| e.to_string())?;

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&tx).map_err(|e| e.to_string())?)
    };

    let outcome = chain
        .simulate_read(&b64)
        .await
        .map_err(|e| format!("reading the wallet's rules: {e}"))?;
    let names = outcome
        .command_returns
        .iter()
        .flatten()
        .next()
        .ok_or("the wallet did not report its rules")
        .and_then(|b| parse_type_names(b).map_err(|_| "the rule list did not decode"))?;

    let mut report = Map::new();
    report.insert("wallet".into(), json!(wallet_id));
    report.insert("objectType".into(), json!(summary.object_type));
    report.insert("sharedInitialVersion".into(), json!(initial));
    if let Value::Object(labels) = label_rules(&names, local) {
        report.extend(labels);
    }
    Ok(Value::Object(report))
}

/// Which rules hold and which layer holds each, from the rule types the chain reported and the
/// manifest the run-set carries. Pure, so a test can drive it with fixed inputs and no network.
///
/// `preFlightRules` is `null` rather than absent when no run-set is loaded: an absent list reads as
/// "no limits", and what it means is that this signer has nothing to refuse against.
pub fn label_rules(type_names: &[String], local: Option<&CapabilityManifest>) -> Value {
    let mut rules = Vec::new();
    let mut unrecognised = Vec::new();
    for name in type_names {
        // Two gates, and a type name must pass both. `rule_module` knows which modules this binary
        // can emit a `prove` for, which is what "the chain holds it" means in practice;
        // `from_module` hands back the kind whose producer labels it. A name that fails either is
        // reported as unrecognised rather than labelled: inventing a layer for a rule this code
        // has never heard of is the error this module exists to correct.
        match rule_module(name).and_then(RuleKind::from_module) {
            Some(kind) => rules.push(labelled(kind)),
            None => unrecognised.push(name.clone()),
        }
    }

    let pre_flight = local.map(|manifest| {
        manifest
            .rules
            .iter()
            .map(CapabilityRule::kind)
            .filter(|kind| kind.enforcement() == Enforcement::PreFlight)
            .map(labelled)
            .collect::<Vec<Value>>()
    });

    json!({
        "rules": rules,
        "unrecognisedRules": unrecognised,
        "preFlightRules": pre_flight,
        "note": NOTE,
    })
}

/// One rule, with the layer that holds it. The label is the producer's answer for this kind; it
/// is never written here as a word.
fn labelled(kind: RuleKind) -> Value {
    let enforcement = kind.enforcement();
    json!({
        "module": kind.module(),
        "enforcement": enforcement.as_str(),
        "enforcedBy": enforced_by(enforcement),
    })
}

/// Who refuses, in words an agent can act on. Exhaustive, so a third layer cannot arrive unnamed.
fn enforced_by(enforcement: Enforcement) -> &'static str {
    match enforcement {
        Enforcement::OnChain => "the Move contract, which aborts the transaction",
        Enforcement::PreFlight => "the signer, before it signs",
    }
}

/// Which layer holds what, stated once, in the read every agent makes before it spends.
const NOTE: &str = "Two layers hold this wallet's limits, and each rule above says which. The \
    rules under `rules` are held by the Move contract: they are proved on chain against the real \
    transaction, nothing in this process and nothing passed to it can widen them, and a spend that \
    exceeds one is aborted by the chain. Nothing on chain checks a destination, a protocol, an \
    asset, or a recipient: those limits are pre-flight, enforced by this signer refusing to sign, \
    and they are listed under `preFlightRules` from the loaded run-set (null means no run-set is \
    loaded, not that there are none). The slippage floor is enforced by the signer refusing to \
    sign an envelope whose guard call does not match, and by the chain aborting when the floor is \
    breached. Read again after any change: the on-chain list is a live read, not a cached copy.";

#[cfg(test)]
mod assembly_tests {
    //! The whole read, offline.
    //!
    //! `label_rules` is pure and well covered below, but the step that decides whether an agent
    //! ever sees a pre-flight rule is not in it: it is `stdio` mapping the loaded run-set to a
    //! manifest and this function carrying it through. That was reachable only through a live
    //! testnet node, so passing `None` by mistake left every offline test green.

    use super::*;
    use rill_chain::fake::FakeSui;
    use rill_chain::{ObjectRef, ObjectSummary};

    const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
    const PACKAGE: &str = "0x0000000000000000000000000000000000000000000000000000000000000caf";

    /// A `vector<TypeName>` the way the node returns one: a ULEB count, then each name as a ULEB
    /// length and its bytes.
    fn type_names(names: &[&str]) -> Vec<u8> {
        let mut out = vec![names.len() as u8];
        for name in names {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        out
    }

    fn wallet_object(shared: Option<u64>) -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: WALLET.to_owned(),
                version: 7,
                digest: String::new(),
            },
            object_type: Some(format!("{PACKAGE}::agent_wallet::AgentWallet")),
            fields: None,
            shared_initial_version: shared,
        }
    }

    fn node(shared: Option<u64>, rules: &[&str]) -> FakeSui {
        FakeSui::new()
            .with_object(None, wallet_object(shared))
            .with_read_return(type_names(
                &rules
                    .iter()
                    .map(|m| format!("{}::{m}::Rule", &PACKAGE[2..]))
                    .collect::<Vec<_>>()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ))
            .with_reference_gas_price(1000)
    }

    fn run(chain: &FakeSui, local: Option<&CapabilityManifest>) -> Result<Value, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(read_limits_from(chain, PACKAGE, WALLET, local))
    }

    /// The regression this module exists for: the manifest reaches the output, or a run-set's
    /// pre-flight rules are invisible to the agent that has to respect them.
    #[test]
    fn a_loaded_run_sets_pre_flight_rules_reach_the_output() {
        let local = CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: vec![
                CapabilityRule::Budget {
                    total_mist: "1".into(),
                },
                CapabilityRule::RecipientAllowlist {
                    addresses: vec!["0x1".into()],
                },
            ],
        };
        let out = run(&node(Some(3), &["budget"]), Some(&local)).expect("the read assembles");
        assert_eq!(out["wallet"], WALLET);
        assert_eq!(out["sharedInitialVersion"], 3);
        assert_eq!(out["rules"][0]["module"], "budget");
        assert_eq!(out["rules"][0]["enforcement"], "on-chain");
        assert_eq!(
            out["preFlightRules"][0]["module"], "recipient_allowlist",
            "the run-set's manifest must survive the trip, or passing None reads as no limits"
        );
    }

    #[test]
    fn without_a_run_set_the_read_still_answers_and_says_it_has_nothing_to_refuse_against() {
        let out = run(&node(Some(3), &["budget", "per_tx"]), None).expect("the read assembles");
        assert_eq!(out["rules"].as_array().unwrap().len(), 2);
        assert!(out["preFlightRules"].is_null());
    }

    /// An object that is not shared is not an AgentWallet, and saying so beats reporting empty
    /// rules for something that never had any.
    #[test]
    fn an_object_that_is_not_shared_is_refused_by_name() {
        let error = run(&node(None, &["budget"]), None).expect_err("not a wallet");
        assert!(error.contains("is not a shared object"), "{error}");
    }

    /// The gas price is read because the node refuses a read priced below its reference. A node
    /// that cannot answer that question has not answered the read either.
    #[test]
    fn a_node_that_cannot_price_the_read_fails_it_rather_than_guessing() {
        let chain = FakeSui::new()
            .with_object(None, wallet_object(Some(3)))
            .with_read_return(type_names(&["0xcaf::budget::Rule"]))
            .with_reference_gas_price_unavailable();
        let error = run(&chain, None).expect_err("no price, no read");
        assert!(error.contains("reference gas price"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(modules: &[&str]) -> Vec<String> {
        modules
            .iter()
            .map(|m| format!("0xcafe::{m}::Rule"))
            .collect()
    }

    fn manifest(rules: Vec<CapabilityRule>) -> CapabilityManifest {
        CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules,
        }
    }

    /// Every kind, labelled, carries the producer's answer. A constant would pass this for the
    /// four kinds it happened to be right about and fail it for the other four.
    #[test]
    fn every_labelled_rule_carries_the_producers_answer_not_a_constant() {
        for kind in [
            RuleKind::Budget,
            RuleKind::PerTx,
            RuleKind::RateLimit,
            RuleKind::TimeWindow,
            RuleKind::ProtocolScope,
            RuleKind::SlippageFloor,
            RuleKind::AssetScope,
            RuleKind::RecipientAllowlist,
        ] {
            let rule = labelled(kind);
            assert_eq!(rule["module"], kind.module());
            assert_eq!(
                rule["enforcement"],
                kind.enforcement().as_str(),
                "{} must be labelled as the producer says",
                kind.module()
            );
        }
    }

    #[test]
    fn a_rule_the_chain_reports_is_labelled_on_chain_and_says_the_contract_holds_it() {
        let out = label_rules(&names(&["budget", "per_tx"]), None);
        let rules = out["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["module"], "budget");
        assert_eq!(rules[0]["enforcement"], "on-chain");
        assert!(rules[0]["enforcedBy"]
            .as_str()
            .unwrap()
            .contains("Move contract"));
        assert_eq!(rules[1]["module"], "per_tx");
        assert_eq!(out["unrecognisedRules"], json!([]));
    }

    /// A rule type this binary cannot prove is reported, not labelled. Guessing a layer for it
    /// is the mistake this module exists to correct.
    #[test]
    fn a_rule_type_this_binary_cannot_prove_is_reported_rather_than_labelled() {
        let out = label_rules(&names(&["budget", "something_new"]), None);
        assert_eq!(out["rules"].as_array().unwrap().len(), 1);
        assert_eq!(
            out["unrecognisedRules"],
            json!(["0xcafe::something_new::Rule"])
        );
    }

    #[test]
    fn pre_flight_rules_come_from_the_run_set_and_say_the_signer_holds_them() {
        let local = manifest(vec![
            CapabilityRule::Budget {
                total_mist: "1".into(),
            },
            CapabilityRule::RecipientAllowlist {
                addresses: vec!["0x1".into()],
            },
            CapabilityRule::SlippageFloor {
                min_out_mist: "1".into(),
            },
        ]);
        let out = label_rules(&names(&["budget"]), Some(&local));
        let pre_flight = out["preFlightRules"].as_array().unwrap();
        let modules: Vec<&str> = pre_flight
            .iter()
            .map(|r| r["module"].as_str().unwrap())
            .collect();
        assert_eq!(
            modules,
            vec!["recipient_allowlist", "slippage_floor"],
            "only the pre-flight kinds are listed; the chain answers for budget"
        );
        for rule in pre_flight {
            assert_eq!(rule["enforcement"], "pre-flight");
            assert_eq!(rule["enforcedBy"], "the signer, before it signs");
        }
    }

    /// Absent would read as "no limits". Null says there is nothing to refuse against yet.
    #[test]
    fn without_a_run_set_the_pre_flight_list_is_null_not_empty() {
        let out = label_rules(&names(&["budget"]), None);
        assert!(out["preFlightRules"].is_null());
        assert!(out.get("preFlightRules").is_some());
    }

    /// The note is what an agent reads when it does not read the labels.
    #[test]
    fn the_note_names_both_layers_and_states_the_slippage_floor_honestly() {
        let note = label_rules(&[], None)["note"].as_str().unwrap().to_owned();
        for phrase in [
            "Move contract",
            "Nothing on chain checks a destination, a protocol, an asset, or a recipient",
            "pre-flight, enforced by this signer refusing to sign",
            "refusing to sign an envelope whose guard call does not match",
            "the chain aborting when the floor is breached",
        ] {
            assert!(note.contains(phrase), "the note no longer says: {phrase:?}");
        }
        assert!(
            !note.contains("These rules are enforced by a Move contract"),
            "the old note claimed every rule for the chain"
        );
    }
}
