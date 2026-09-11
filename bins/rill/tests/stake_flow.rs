//! A gated Haedal stake: what the transaction really calls, and what is refused before it exists.
//!
//! Every assertion about the call sequence reads the bytes the signer handed over, decoded, rather
//! than a function that describes them. The first draft of the swap floor's tests asserted a pure
//! description and passed with the emission deleted; this file starts where that one ended.

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_chain::{ObjectRef, ObjectSummary};
use rill_cli::keystore::Keystore;
use rill_cli::stake_cmd::{stake_json_on, StakeArgs};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Digest;

const PACKAGE: &str = "0x000000000000000000000000000000000000000000000000000000000000caf0";
const VERSION: &str = "0x0000000000000000000000000000000000000000000000000000000000000fff";
const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
const CAP: &str = "0x0000000000000000000000000000000000000000000000000000000000000cab";
const COIN: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";
const HAEDAL: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";
const STAKING: &str = "0x00000000000000000000000000000000000000000000000000000000000000a2";
const SYSTEM: &str = "0x5";
const CLOCK: &str = "0x6";
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

fn key(seed: u8) -> Keystore {
    let encoded = Ed25519PrivateKey::new([seed; 32])
        .to_suiprivkey()
        .expect("a key encodes");
    Keystore::from_suiprivkey(&encoded).expect("a keystore")
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

fn type_names(modules: &[&str]) -> Vec<u8> {
    let mut out = vec![modules.len() as u8];
    for module in modules {
        let name = format!("{}::{module}::Rule", &PACKAGE[2..]);
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
        object_type: Some("0x2::shared::Thing".into()),
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

fn chain(agent: &Keystore, attached: &[&str]) -> FakeSui {
    FakeSui::new()
        .with_object(None, shared(WALLET, 4))
        .with_object(None, shared(VERSION, 3))
        .with_object(None, shared(STAKING, 7))
        .with_object(None, shared(SYSTEM, 1))
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
        .with_simulation(SimulationBehavior::Succeeds {
            gas_used_mist: 5_000_000,
        })
}

fn args(spend: &str) -> StakeArgs {
    StakeArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        wallet_id: WALLET.into(),
        cap_id: CAP.into(),
        haedal_package_id: HAEDAL.into(),
        staking_object_id: STAKING.into(),
        validator: "0x0".into(),
        spend: spend.into(),
        gas_budget: 50_000_000,
        dry_run: false,
    }
}

/// The transaction the signer submitted: gated spend, then Haedal's `interface::request_stake`.
///
/// Decoded from the bytes. `interface`, not `staking`: the adapter called `staking::request_stake`
/// until the deployed package was read, and that function does not exist there.
#[test]
fn the_submitted_transaction_stakes_through_the_function_that_exists() {
    let agent = key(21);
    let chain = chain(&agent, &["budget", "per_tx"]);
    let report =
        run(stake_json_on(&chain, &agent, &args("1"))).expect("a stake builds and submits");
    assert_eq!(report["submitted"], true);

    let submitted = chain.submitted();
    let decoded = rill_policy::decode::decode(submitted.first().expect("one transaction"))
        .expect("the bytes decode");
    assert_eq!(
        decoded.targets,
        vec![
            format!("{PACKAGE}::agent_wallet::request_spend"),
            format!("{PACKAGE}::budget::prove"),
            format!("{PACKAGE}::per_tx::prove"),
            format!("{PACKAGE}::agent_wallet::confirm_spend"),
            format!("{HAEDAL}::interface::request_stake"),
        ],
        "the stake must follow the gated spend, and must call the function the package has"
    );
}

/// The pinned sequence is exactly what the transaction calls.
#[test]
fn the_pinned_sequence_equals_the_transactions_real_targets() {
    let agent = key(22);
    let chain = chain(&agent, &["budget"]);
    let report = run(stake_json_on(&chain, &agent, &args("1.5"))).expect("a stake");
    let claimed: Vec<String> = report["callSequence"]
        .as_array()
        .expect("a sequence")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    let decoded =
        rill_policy::decode::decode(chain.submitted().first().expect("one")).expect("decodes");
    assert_eq!(claimed, decoded.targets);
    assert_eq!(report["stakeBaseUnits"], "1500000000");
}

/// Below Haedal's minimum is refused before anything is read or built.
///
/// Haedal aborts below one SUI with a code naming neither the amount nor the minimum, and it would do
/// so after the wallet's rules had been checked and the transaction paid for.
#[test]
fn below_the_minimum_is_refused_before_anything_is_built() {
    let agent = key(23);
    let chain = chain(&agent, &["budget"]);
    let err = run(stake_json_on(&chain, &agent, &args("0.999999999")))
        .expect_err("one mist under the minimum must be refused");
    let said = err.to_string();
    assert!(said.contains("minimum stake is 1 SUI"), "{said}");
    assert!(chain.submitted().is_empty(), "and nothing was submitted");
}

/// A wallet with no rules is refused, through the same guard every gated path shares.
#[test]
fn a_wallet_with_no_rules_cannot_stake() {
    let agent = key(24);
    let chain = chain(&agent, &[]);
    let err = run(stake_json_on(&chain, &agent, &args("1")))
        .expect_err("a rule-less wallet must not be spendable");
    assert!(err.to_string().contains("no rules attached"), "{err}");
    assert!(chain.submitted().is_empty());
}

/// A dry run builds and simulates and submits nothing.
#[test]
fn a_dry_run_submits_nothing() {
    let agent = key(25);
    let chain = chain(&agent, &["budget"]);
    let mut a = args("1");
    a.dry_run = true;
    let report = run(stake_json_on(&chain, &agent, &a)).expect("a dry run");
    assert_eq!(report["submitted"], false);
    assert!(chain.submitted().is_empty());
}
