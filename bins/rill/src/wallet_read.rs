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
//! this signer refusing to sign. A swap's slippage floor belongs to neither group: `rill_swap`
//! emits it as a Move call on the coin the swap bought, so the chain aborts on a bad fill.
//!
//! # The numbers, not only the names
//!
//! This read used to name the rules and report not one of their values, so an agent could learn
//! that a `per_tx` cap was in force and not what it was. The only way to find out was to attempt a
//! spend and read the refusal. The wallet's own fields and each rule's configured ceiling are now
//! read from the chain, and one number is derived from them: `largestSpendNow`, the biggest single
//! spend that would pass at this moment, with the name of the limit that holds it. See
//! [`crate::limits`] for why that number is an upper bound rather than a promise whenever the
//! wallet carries a rule this binary cannot read.
//!
//! An earlier version of this read labelled every rule `"on-chain"` with a constant and said in
//! its note that nothing could widen them. True of the four the chain holds, false of the rest,
//! and an owner deciding how much to grant was being told the chain held a recipient allowlist it
//! has never heard of. The label now comes from the one producer, [`RuleKind::enforcement`], per
//! rule, and the pre-flight rules are listed from the loaded run-set with the layer that holds
//! them stated beside each.

use crate::limits::{largest_spend_now, rule_config, wallet_facts, RuleConfig};
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
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    read_limits_from(&chain, package_id, wallet_id, local, now_ms).await
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
    // Milliseconds since the epoch. Passed rather than read so the whole assembly runs offline, and
    // so a test can sit on either side of a rate-limit or time-window boundary without sleeping.
    now_ms: u64,
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

    // The wallet's own numbers, and each rule's configured ceiling. Both are reads the chain
    // answers; neither was asked for before, which is why this report used to name rules without a
    // single value beside them. A missing field is reported rather than defaulted: a zero balance
    // and a balance nobody read are different facts, and only one of them means "do not spend".
    let facts = summary
        .fields
        .as_ref()
        .ok_or_else(|| "the node returned the wallet without its fields".to_string())
        .and_then(wallet_facts);

    let configs: Vec<(String, RuleConfig)> = match summary
        .fields
        .as_ref()
        .and_then(|f| f.get("policy"))
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
    {
        Some(policy) => chain
            .list_dynamic_fields(policy)
            .await
            .map_err(|e| format!("reading the wallet's rule configs: {e}"))?
            .into_iter()
            .filter_map(|f| {
                let kind = f.value_type.as_deref()?;
                let value = f.fields.as_ref()?;
                rule_config(kind, value).map(|c| (c.module().to_string(), c))
            })
            .collect(),
        None => Vec::new(),
    };

    let mut report = Map::new();
    report.insert("wallet".into(), json!(wallet_id));
    report.insert("objectType".into(), json!(summary.object_type));
    report.insert("sharedInitialVersion".into(), json!(initial));
    if let Value::Object(labels) = label_rules_with_limits(&names, local, &configs) {
        report.extend(labels);
    }

    match facts {
        Ok(facts) => {
            // Base units as text, like every other amount that crosses this boundary: a u64 balance
            // above 2^53 handed to a JSON consumer as a number comes back rounded.
            report.insert("balanceBaseUnits".into(), json!(facts.balance.to_string()));
            report.insert("spentBaseUnits".into(), json!(facts.spent.to_string()));
            report.insert("expiresAtMs".into(), json!(facts.expires_at_ms.to_string()));
            report.insert("revoked".into(), json!(facts.revoked));

            let unreadable = report
                .get("unrecognisedRules")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let rule_values: Vec<RuleConfig> = configs.iter().map(|(_, c)| *c).collect();
            let headroom = largest_spend_now(&facts, &rule_values, unreadable, now_ms);
            report.insert(
                "largestSpendNow".into(),
                json!({
                    "baseUnits": headroom.base_units.to_string(),
                    "boundBy": headroom.bound_by,
                    "certain": headroom.certain,
                    "note": if headroom.certain {
                        "The largest single spend this wallet would allow at the time of this read, \
                         and the limit that holds it. Every input is a live chain read. It moves: \
                         another spend, a rule change, or a rate-limit window rolling over all \
                         change it, so read again rather than caching this."
                    } else {
                        "An upper bound, not the limit. This wallet carries a rule this signer \
                         cannot read, listed under `unrecognisedRules`, and that rule may cap a \
                         spend below the figure here. Treat a spend up to this amount as possible \
                         rather than permitted."
                    },
                }),
            );
        }
        // A read that cannot produce the numbers says so in place of them. Omitting the keys would
        // leave a caller unable to tell "this wallet has no limit" from "this read did not get one".
        Err(why) => {
            report.insert("limitsUnavailable".into(), json!(why));
        }
    }

    Ok(Value::Object(report))
}

/// Which rules hold and which layer holds each, from the rule types the chain reported and the
/// manifest the run-set carries. Pure, so a test can drive it with fixed inputs and no network.
///
/// `preFlightRules` is `null` rather than absent when no run-set is loaded: an absent list reads as
/// "no limits", and what it means is that this signer has nothing to refuse against.
pub fn label_rules(type_names: &[String], local: Option<&CapabilityManifest>) -> Value {
    label_rules_with_limits(type_names, local, &[])
}

/// The same, with each on-chain rule's configured ceiling attached where one was read.
///
/// `configs` is keyed by module rather than by position: the chain reports the rule list and the
/// dynamic fields in no particular relation to each other, and pairing them by index would attach a
/// `per_tx` cap to a `budget` rule the first time a wallet's fields came back in a different order.
pub fn label_rules_with_limits(
    type_names: &[String],
    local: Option<&CapabilityManifest>,
    configs: &[(String, RuleConfig)],
) -> Value {
    let mut rules = Vec::new();
    let mut unrecognised = Vec::new();
    for name in type_names {
        // Two gates, and a type name must pass both. `rule_module` knows which modules this binary
        // can emit a `prove` for, which is what "the chain holds it" means in practice;
        // `from_module` hands back the kind whose producer labels it. A name that fails either is
        // reported as unrecognised rather than labelled: inventing a layer for a rule this code
        // has never heard of is the error this module exists to correct.
        match rule_module(name).and_then(RuleKind::from_module) {
            Some(kind) => {
                let mut entry = labelled(kind);
                if let Some((_, config)) = configs.iter().find(|(m, _)| m == kind.module()) {
                    if let Value::Object(map) = &mut entry {
                        map.insert("limits".into(), limits_of(config));
                    }
                }
                rules.push(entry);
            }
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
        // The sentence comes from the producer too. The generated instructions print the same one,
        // and a read that worded it here would be the second place the same claim is made.
        "enforcedBy": enforcement.enforced_by(),
    })
}

/// One rule's configured numbers, named as the Move module names them.
///
/// Base units as text throughout, and `remaining` computed here rather than left to the caller:
/// `total - spent` is the subtraction a reader would otherwise do themselves, and the one place it
/// can underflow is a budget already spent past its ceiling.
fn limits_of(config: &RuleConfig) -> Value {
    match *config {
        RuleConfig::Budget { total_mist, spent } => json!({
            "totalBaseUnits": total_mist.to_string(),
            "spentBaseUnits": spent.to_string(),
            "remainingBaseUnits": total_mist.saturating_sub(spent).to_string(),
        }),
        RuleConfig::PerTx { max_mist } => json!({
            "maxPerTransactionBaseUnits": max_mist.to_string(),
        }),
        RuleConfig::RateLimit {
            window_ms,
            window_max,
            window_start_ms,
            spent_in_window,
        } => json!({
            "windowMs": window_ms.to_string(),
            "windowMaxBaseUnits": window_max.to_string(),
            "windowStartMs": window_start_ms.to_string(),
            "spentInWindowBaseUnits": spent_in_window.to_string(),
        }),
        RuleConfig::TimeWindow {
            not_before_ms,
            not_after_ms,
        } => json!({
            "notBeforeMs": not_before_ms.to_string(),
            "notAfterMs": not_after_ms.to_string(),
        }),
    }
}

/// Which layer holds what, stated once, in the read every agent makes before it spends.
const NOTE: &str = "Start with `largestSpendNow`: it is the biggest single spend this wallet \
    would allow at the moment of this read, and `boundBy` names the limit holding it, so a spend \
    that would be refused can be avoided rather than attempted. Two layers hold the limits, and \
    each rule above says which. The rules under `rules` are held by the Move contract: they are \
    proved on chain against the real transaction, nothing in this process and nothing passed to it \
    can widen them, and a spend that exceeds one is aborted by the chain. Their configured values \
    are under each rule's `limits`, in base units, read from the chain rather than from anything \
    this process was told. Nothing on chain checks a destination, a protocol, an asset, or a \
    recipient: those limits are pre-flight, enforced by this signer refusing to sign, and they are \
    listed under `preFlightRules` from the loaded run-set (null means no run-set is loaded, not \
    that there are none). A swap's slippage floor is a third thing again: `rill_swap` takes minOut \
    and emits an on-chain assertion on the coin it bought, so a fill below the floor aborts the \
    whole transaction. Read again after any change: every number here is a live read, not a cached \
    copy.";

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

    const POLICY: &str = "0x00000000000000000000000000000000000000000000000000000000000000b0";

    fn wallet_object(shared: Option<u64>) -> ObjectSummary {
        wallet_object_with_fields(shared, Some(wallet_fields()))
    }

    /// The shape the node really returns: every u64 as text, and the policy named by object id.
    fn wallet_fields() -> Value {
        json!({
            "budget": "149000000",
            "spent": "51000000",
            "expires_at_ms": (NOW_MS + 3_600_000).to_string(),
            "revoked": false,
            "policy": { "id": POLICY },
        })
    }

    fn wallet_object_with_fields(shared: Option<u64>, fields: Option<Value>) -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: WALLET.to_owned(),
                version: 7,
                digest: String::new(),
            },
            object_type: Some(format!("{PACKAGE}::agent_wallet::AgentWallet")),
            fields,
            shared_initial_version: shared,
        }
    }

    /// One rule config as a dynamic field on the policy, wrapped the way the node wraps it.
    fn config_field(module: &str, value: Value) -> rill_chain::DynamicFieldSummary {
        rill_chain::DynamicFieldSummary {
            field_id: format!("0xf1e1d{module}"),
            value_type: Some(format!("{PACKAGE}::{module}::Config")),
            fields: Some(json!({ "name": { "dummy_field": false }, "value": value })),
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

    /// A fixed "now" so the assembly is deterministic. Nothing here depends on the real clock.
    const NOW_MS: u64 = 1_700_000_000_000;

    fn run(chain: &FakeSui, local: Option<&CapabilityManifest>) -> Result<Value, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(read_limits_from(chain, PACKAGE, WALLET, local, NOW_MS))
    }

    /// The numbers reach the output, and the derived bound is the tightest rule by name.
    ///
    /// The arithmetic is covered in `tests/limits.rs`; what this covers is the assembly, which is
    /// where it went wrong before. Three steps have to line up: the wallet's fields have to be asked
    /// for, the policy's dynamic fields have to be read from the id inside those fields, and each
    /// config has to be paired with its rule by module. Any one of them quietly producing nothing
    /// leaves a report that still looks complete, because every key it used to have is still there.
    #[test]
    fn the_numbers_and_the_derived_bound_reach_the_output() {
        let chain = node(Some(3), &["budget", "per_tx"])
            .with_dynamic_field(
                POLICY,
                config_field(
                    "budget",
                    json!({ "total_mist": "200000000", "spent": "51000000" }),
                ),
            )
            .with_dynamic_field(
                POLICY,
                config_field("per_tx", json!({ "max_mist": "50000000" })),
            );
        let out = run(&chain, None).expect("the read assembles");

        assert_eq!(out["balanceBaseUnits"], "149000000");
        assert_eq!(out["spentBaseUnits"], "51000000");
        assert_eq!(out["revoked"], false);

        // Paired by module, not by position: the budget ceiling must not land on the per_tx rule.
        let budget = out["rules"]
            .as_array()
            .expect("rules")
            .iter()
            .find(|r| r["module"] == "budget")
            .expect("the budget rule");
        assert_eq!(budget["limits"]["totalBaseUnits"], "200000000");
        assert_eq!(budget["limits"]["remainingBaseUnits"], "149000000");
        let per_tx = out["rules"]
            .as_array()
            .expect("rules")
            .iter()
            .find(|r| r["module"] == "per_tx")
            .expect("the per_tx rule");
        assert_eq!(per_tx["limits"]["maxPerTransactionBaseUnits"], "50000000");

        assert_eq!(out["largestSpendNow"]["baseUnits"], "50000000");
        assert_eq!(out["largestSpendNow"]["boundBy"], "per_tx");
        assert_eq!(out["largestSpendNow"]["certain"], true);
    }

    /// A read the node answered without fields says the limits are unavailable rather than omitting
    /// them.
    ///
    /// An absent key reads as "this wallet has no such limit". The two have to be distinguishable,
    /// because one of them means the agent may spend and the other means nobody knows.
    #[test]
    fn a_read_without_fields_says_so_instead_of_going_quiet() {
        let chain = FakeSui::new()
            .with_object(None, wallet_object_with_fields(Some(3), None))
            .with_read_return(type_names(&[&format!("{}::budget::Rule", &PACKAGE[2..])]))
            .with_reference_gas_price(1000);
        let out = run(&chain, None).expect("the read still assembles");

        assert!(
            out["limitsUnavailable"].is_string(),
            "a read that could not get the numbers must say which part failed: {out}"
        );
        assert!(
            out.get("largestSpendNow").is_none(),
            "and must not publish a bound it did not compute: {out}"
        );
        assert_eq!(
            out["rules"][0]["module"], "budget",
            "while still reporting what it did read"
        );
    }

    /// An unreadable rule makes the bound an upper bound, and the report says which it is.
    #[test]
    fn an_unrecognised_rule_is_carried_into_the_bounds_certainty() {
        let chain = node(Some(3), &["budget", "something_new"]).with_dynamic_field(
            POLICY,
            config_field(
                "budget",
                json!({ "total_mist": "200000000", "spent": "51000000" }),
            ),
        );
        let out = run(&chain, None).expect("the read assembles");
        assert_eq!(
            out["unrecognisedRules"][0],
            format!("{}::something_new::Rule", &PACKAGE[2..])
        );
        assert_eq!(out["largestSpendNow"]["certain"], false);
        assert!(
            out["largestSpendNow"]["note"]
                .as_str()
                .expect("a note")
                .contains("upper bound"),
            "the note must say it is not the limit: {out}"
        );
    }

    /// Every field the note sends a reader to actually exists in the report.
    ///
    /// The note is prose an agent reads instead of the labels, and prose drifts from the data it
    /// describes. An earlier version of this test listed five sentences the note had to contain,
    /// which pinned the wording rather than the claim: it failed when the note was improved and
    /// would have passed if every field it names had been renamed underneath it. So the coupling is
    /// the other way round now. Each backticked name in the note is looked up in a real report, and
    /// the layer names come from `Enforcement`, which is the producer, rather than being retyped.
    #[test]
    fn the_note_points_only_at_fields_that_exist() {
        let chain = node(Some(3), &["budget", "per_tx"])
            .with_dynamic_field(
                POLICY,
                config_field(
                    "budget",
                    json!({ "total_mist": "200000000", "spent": "51000000" }),
                ),
            )
            .with_dynamic_field(
                POLICY,
                config_field("per_tx", json!({ "max_mist": "50000000" })),
            );
        let report = run(&chain, None).expect("the read assembles");
        let note = report["note"].as_str().expect("a note").to_owned();

        // Backticked names that look like report keys: camelCase, no punctuation. `boundBy` and
        // `limits` are nested, so the search is over the whole document rather than its top level.
        let flat = serde_json::to_string(&report).expect("the report serialises");
        let mut checked = 0;
        for token in note.split('`').skip(1).step_by(2) {
            if !token.chars().all(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            assert!(
                flat.contains(&format!("\"{token}\"")),
                "the note sends a reader to `{token}`, which this report does not have: {flat}"
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "the note should be naming the fields it wants read, and only {checked} were found"
        );

        // Both layers, named by the producer rather than by this test.
        for enforcement in [Enforcement::OnChain, Enforcement::PreFlight] {
            let words = enforcement.enforced_by();
            let distinctive = words.split_whitespace().next_back().expect("a word");
            assert!(
                note.contains(distinctive) || note.contains(enforcement.as_str()),
                "the note does not account for the {} layer, whose producer describes it as {words:?}",
                enforcement.as_str()
            );
        }
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
}
