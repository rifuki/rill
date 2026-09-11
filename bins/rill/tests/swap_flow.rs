//! The gated swap, offline.
//!
//! This path was built and then proved by spending real money on testnet, which is evidence that it
//! works once and no protection at all against it stopping. Every assertion here runs against
//! `rill_chain::fake::FakeSui`, so the parts that decide whether a swap is safe are checked on every
//! push rather than whenever somebody has testnet SUI to spare.
//!
//! What is worth checking is not that a swap happens. It is that the wallet's rules are what release
//! the money, that both coins the swap returns are placed, that the price bound is derived rather
//! than taken, and that a refusal names the rule.

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_chain::{ObjectRef, ObjectSummary};
use rill_cli::keystore::Keystore;
use rill_cli::swap_cmd::{expected_targets, swap_json_on, SwapArgs};
use rill_cli::verdict::Failure;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Digest;

const PACKAGE: &str = "0x000000000000000000000000000000000000000000000000000000000000caf0";
const VERSION: &str = "0x0000000000000000000000000000000000000000000000000000000000000fff";
const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
const CAP: &str = "0x0000000000000000000000000000000000000000000000000000000000000cab";
const COIN: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";
const INTEGRATE: &str = "0x00000000000000000000000000000000000000000000000000000000000000ce";
const GLOBAL_CONFIG: &str = "0x0000000000000000000000000000000000000000000000000000000000000030";
const POOL: &str = "0x0000000000000000000000000000000000000000000000000000000000000031";
// The id the code asks for, literally. `rill_ptb::spend::CLOCK_ID` is "0x6", and the fake matches
// object ids by string while a real node accepts either form, so the short one is what has to be
// registered here. The address the shared-version map is keyed by is still the parsed, padded one.
const CLOCK: &str = "0x6";
const COIN_A: &str = "0x00000000000000000000000000000000000000000000000000000000000000cd::h::H";
const COIN_B: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

fn key(seed: u8) -> Keystore {
    let encoded = Ed25519PrivateKey::new([seed; 32])
        .to_suiprivkey()
        .expect("a key encodes");
    Keystore::from_suiprivkey(&encoded).expect("a key loads")
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

fn type_names(modules: &[&str]) -> Vec<u8> {
    let names: Vec<String> = modules
        .iter()
        .map(|m| format!("{}::{m}::Rule", &PACKAGE[2..]))
        .collect();
    let mut out = vec![names.len() as u8];
    for name in &names {
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }
    out
}

fn shared(id: &str, initial: u64) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: initial + 1,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(format!("{PACKAGE}::agent_wallet::AgentWallet")),
        fields: None,
        shared_initial_version: Some(initial),
    }
}

fn owned(id: &str, object_type: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: 9,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(object_type.to_owned()),
        fields: None,
        shared_initial_version: None,
    }
}

/// A chain that can answer every read a gated swap makes, with `attached` as the wallet's live rules.
fn chain(agent: &Keystore, attached: &[&str]) -> FakeSui {
    FakeSui::new()
        .with_object(None, shared(WALLET, 4))
        .with_object(None, shared(VERSION, 3))
        .with_object(None, shared(GLOBAL_CONFIG, 5))
        .with_object(None, shared(POOL, 6))
        .with_object(None, shared(CLOCK, 1))
        .with_object(
            Some(&agent.address().to_string()),
            owned(CAP, &format!("{PACKAGE}::agent_wallet::AgentCap")),
        )
        .with_object(
            Some(&agent.address().to_string()),
            owned(COIN, SUI_COIN_TYPE),
        )
        .with_read_return(type_names(attached))
        .with_reference_gas_price(1_000)
}

fn args(spend: &str, a2b: bool) -> SwapArgs {
    SwapArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        wallet_id: WALLET.into(),
        cap_id: CAP.into(),
        integrate_package_id: INTEGRATE.into(),
        global_config_id: GLOBAL_CONFIG.into(),
        pool_id: POOL.into(),
        coin_type_a: COIN_A.into(),
        coin_type_b: COIN_B.into(),
        a2b,
        spend: spend.into(),
        gas_budget: 50_000_000,
        dry_run: false,
    }
}

/// The sequence is the gated spend followed by the swap, in that order, and it is what the signer
/// pins. A swap that arrived without the spend in front of it would be an agent spending its own
/// money, which is a different product.
#[test]
fn the_call_sequence_is_the_gated_spend_then_the_swap() {
    let targets = expected_targets(
        PACKAGE.parse().expect("a package id"),
        &["budget".to_string(), "per_tx".to_string()],
        INTEGRATE.parse().expect("an integrate id"),
    );
    assert_eq!(
        targets,
        vec![
            format!("{PACKAGE}::agent_wallet::request_spend"),
            format!("{PACKAGE}::budget::prove"),
            format!("{PACKAGE}::per_tx::prove"),
            format!("{PACKAGE}::agent_wallet::confirm_spend"),
            "0x0000000000000000000000000000000000000000000000000000000000000002::coin::zero"
                .to_string(),
            format!("{INTEGRATE}::router::swap"),
        ]
    );
}

/// Every rule the wallet carries is proved, and the list comes from the chain rather than from an
/// argument. A caller that could name the rules could name fewer of them.
#[test]
fn every_rule_the_chain_reports_is_proved_and_named_in_the_report() {
    let agent = key(12);
    let report = run(swap_json_on(
        &chain(&agent, &["budget", "per_tx", "rate_limit"]),
        &agent,
        &args("0.001", false),
    ))
    .expect("the swap builds and submits");

    assert_eq!(report["submitted"], true);
    assert_eq!(
        report["rulesProved"],
        serde_json::json!(["budget", "per_tx", "rate_limit"])
    );
    let sequence: Vec<String> = report["callSequence"]
        .as_array()
        .expect("a sequence")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        sequence.iter().any(|t| t.ends_with("::rate_limit::prove")),
        "a rule the chain reported must appear in the sequence: {sequence:?}"
    );
    assert_eq!(report["spendBaseUnits"], "1000000");
}

/// A wallet carrying no rules is not a wallet this will spend from.
///
/// `confirm_spend` on an empty policy requires zero receipts, so the swap would go through and the
/// amount would be bounded by nothing. The refusal has to come before anything is signed.
#[test]
fn a_wallet_with_no_rules_is_refused_rather_than_swapped_from() {
    let agent = key(12);
    let failure = run(swap_json_on(
        &chain(&agent, &[]),
        &agent,
        &args("0.001", false),
    ))
    .expect_err("an unbounded wallet must be refused");
    let said = match &failure {
        Failure::Failed(m) => m.clone(),
        Failure::Refused(r) => r.to_string(),
    };
    assert!(
        said.to_lowercase().contains("rule"),
        "the refusal must be about the absent rules: {said}"
    );
}

/// The amount is decimal text and a float is not text.
#[test]
fn an_amount_that_is_not_a_decimal_string_is_refused_by_name() {
    let agent = key(12);
    for bad in ["", "0.0000000001", "abc", "-1"] {
        let failure = run(swap_json_on(
            &chain(&agent, &["budget"]),
            &agent,
            &args(bad, false),
        ))
        .expect_err(&format!("{bad:?} must be refused"));
        let said = match &failure {
            Failure::Failed(m) => m.clone(),
            Failure::Refused(r) => r.to_string(),
        };
        assert!(
            said.contains("spend amount") || said.to_lowercase().contains("amount"),
            "the refusal must name the field: {said} for {bad:?}"
        );
    }
}

/// A rule that refuses comes back as a refusal naming the rule, not as a quoted Move abort.
#[test]
fn a_rule_that_refuses_the_swap_is_named() {
    let agent = key(12);
    let chain = chain(&agent, &["budget", "per_tx"]).with_simulation(SimulationBehavior::Fails {
        error: format!(
            "MoveAbort(MoveLocation {{ module: ModuleId {{ address: {}, name: Identifier(\"per_tx\") }}, function: 2, instruction: 21, function_name: Some(\"prove\") }}, 1) in command 2",
            &PACKAGE[2..]
        ),
    });
    let failure = run(swap_json_on(&chain, &agent, &args("0.09", false)))
        .expect_err("a rule refusal must not be a success");
    match failure {
        Failure::Refused(refusal) => {
            let said = refusal.to_string();
            assert!(
                said.contains("per_tx"),
                "the rule that refused must be named here, not quoted as a Move abort: {said}"
            );
            assert!(
                said.contains("per-transaction cap"),
                "and what it refused must be readable: {said}"
            );
            // The longer advice an agent sees ("the limit is on chain, not in this client") is added
            // by `failure_response` on the transport, not by this type. Asserting it here would be
            // asserting the wrong layer, which is how a test ends up pinning a sentence rather than
            // a behaviour.
        }
        Failure::Failed(m) => panic!("a named rule refusal arrived as a plain failure: {m}"),
    }
}

/// A simulation that fails for something other than a rule keeps the node's words rather than
/// inventing a rule.
#[test]
fn a_failure_that_is_not_a_rule_is_not_reported_as_one() {
    let agent = key(12);
    let chain = chain(&agent, &["budget"]).with_simulation(SimulationBehavior::Fails {
        error: "MoveAbort(MoveLocation { module: ModuleId { address: ab2d, name: Identifier(\"pool\") }, function: 101, instruction: 68, function_name: Some(\"flash_swap_internal\") }, 11) in command 5".into(),
    });
    let failure = run(swap_json_on(&chain, &agent, &args("0.001", false)))
        .expect_err("a pool abort must not be a success");
    match failure {
        Failure::Failed(said) => assert!(
            said.contains("flash_swap_internal"),
            "the node's words must survive: {said}"
        ),
        Failure::Refused(r) => panic!("a pool abort is not one of this wallet's rules: {r}"),
    }
}

/// A dry run reaches the gate and signs nothing, which is the only thing a dry run can promise.
#[test]
fn a_dry_run_passes_the_gate_and_submits_nothing() {
    let agent = key(12);
    let mut args = args("0.001", false);
    args.dry_run = true;
    let report = run(swap_json_on(&chain(&agent, &["budget"]), &agent, &args))
        .expect("a dry run that passes the gate is a success");
    assert_eq!(report["submitted"], false);
    assert!(report["digest"].is_null(), "nothing was sent: {report}");
    assert_eq!(report["simulation"]["ok"], true);
    assert!(
        report["note"]
            .as_str()
            .is_some_and(|n| n.contains("Nothing was signed")),
        "the report must say what it did not do: {report}"
    );
}

/// The price bound is derived from the direction, and the caller cannot set it.
///
/// `SwapArgs` has no field for it, which is the guard: a bound on the wrong side aborts inside Cetus
/// with a code naming neither the value nor the field, and the direction is the only thing that
/// decides which side is open. The first real swap built here failed exactly that way.
#[test]
fn the_caller_cannot_put_the_price_bound_on_the_wrong_side() {
    const SOURCE: &str = include_str!("../src/swap_cmd.rs");
    let shipped = SOURCE.split("#[cfg(test)]").next().unwrap_or(SOURCE);
    assert!(
        shipped.contains("let sqrt_price_limit = if args.a2b"),
        "the bound must be derived from the direction in this path"
    );
    for line in shipped.lines() {
        let code = line.split("//").next().unwrap_or(line);
        assert!(
            !code.contains("sqrt_price_limit:") || !code.contains("args.sqrt_price_limit"),
            "a caller-supplied bound reached the swap: {}",
            code.trim()
        );
    }
    // And both directions build, which is what proves the derivation is not simply always one value.
    let agent = key(12);
    for a2b in [true, false] {
        run(swap_json_on(
            &chain(&agent, &["budget"]),
            &agent,
            &args("0.001", a2b),
        ))
        .unwrap_or_else(|e| {
            panic!(
                "a2b={a2b} must build: {}",
                match e {
                    Failure::Failed(m) => m,
                    Failure::Refused(r) => r.to_string(),
                }
            )
        });
    }
}

/// The same hole, closed for every path that builds a gated spend.
///
/// Three callers reach `build_gated_spend_for_modules`: spend, order and swap. Two of them predate
/// this test and both read the rule list from the chain rather than from a manifest, so the manifest
/// validation that refuses an empty rule set never ran on them: `spend` checked that every attached
/// rule had an emitter, which `0 == 0` satisfies, and `order` checked nothing. The guard is in the
/// shared builder for that reason, and this asserts it covers all three rather than only the one
/// whose test found it.
#[test]
fn no_gated_path_can_spend_from_a_wallet_with_no_rules() {
    use rill_ptb::spend::{build_gated_spend_for_modules, SpendError, WalletBinding};
    use sui_transaction_builder::{ObjectInput, TransactionBuilder};

    let mut shared = rill_ptb::shared::SharedObjects::new();
    shared.insert(WALLET.parse().expect("an address"), 4);
    shared.insert(VERSION.parse().expect("an address"), 3);
    shared.insert("0x6".parse().expect("an address"), 1);

    let binding = WalletBinding {
        package_id: PACKAGE.parse().expect("an address"),
        wallet_id: WALLET.parse().expect("an address"),
        cap: ObjectInput::owned(CAP.parse().expect("an id"), 9, Digest::ZERO),
        version_id: VERSION.parse().expect("an address"),
        coin_type: "0x2::sui::SUI".into(),
        manifest: rill_core::manifest::CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };

    let mut tx = TransactionBuilder::new();
    let err = build_gated_spend_for_modules(&mut tx, &binding, 1_000_000, &[], &shared)
        .expect_err("an empty rule list must be refused at the builder");
    assert!(
        matches!(err, SpendError::NoRulesAttached),
        "got {err:?} rather than the refusal every caller depends on"
    );
    let said = err.to_string();
    assert!(
        said.contains("zero receipts"),
        "the refusal must say why an empty policy is not a restriction: {said}"
    );
    assert!(
        said.contains("rill wallet rules") && said.contains("rill_attach_rules"),
        "and name both ways to fix it, since one caller is a terminal and another is an agent: {said}"
    );

    // One rule is enough to build: the guard is about zero, not about how many.
    let mut tx = TransactionBuilder::new();
    build_gated_spend_for_modules(&mut tx, &binding, 1_000_000, &["budget"], &shared)
        .expect("one attached rule builds");
}
