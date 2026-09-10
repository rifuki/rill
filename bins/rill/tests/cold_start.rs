//! A stranger with no Sui setup, reaching a bounded wallet in one command.
//!
//! The path from "curl a binary" to "watch an agent spend" has five steps in it, and before `init`
//! nothing owned that path: it was a sequence of commands in a README, each able to fail in a way
//! that told the reader nothing about the next. The unit for it is the one most likely to look
//! finished while still being unusable, so these tests drive the whole of it offline and the
//! refusals are checked for the words a reader needs rather than for their types alone.

use rill_chain::fake::FakeSui;
use rill_chain::{CreatedObject, ObjectRef, ObjectSummary};
use rill_cli::init::{
    already_initialised, faucet_link, refuse_early, Identities, InitArgs, Stopped,
};
use rill_cli::keystore::Keystore;
use rill_core::manifest::{CapabilityManifest, CapabilityRule};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Digest;

const PACKAGE: &str = "0x000000000000000000000000000000000000000000000000000000000000caf0";
const VERSION: &str = "0x0000000000000000000000000000000000000000000000000000000000000fff";
const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
const CAP: &str = "0x0000000000000000000000000000000000000000000000000000000000000cab";
const COIN: &str = "0x000000000000000000000000000000000000000000000000000000000000000a";
const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

/// Two throwaway keys from fixed bytes. The real keystore is never read: a test that read it would
/// pass or fail on whatever happens to be on the machine.
fn key(seed: u8) -> Keystore {
    let encoded = Ed25519PrivateKey::new([seed; 32])
        .to_suiprivkey()
        .expect("a key encodes");
    Keystore::from_suiprivkey(&encoded).expect("a key loads")
}

fn keys() -> (Keystore, Keystore) {
    (key(11), key(12))
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

fn shared(id: &str, initial: u64, suffix: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.to_owned(),
            version: initial + 1,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(format!("{PACKAGE}::agent_wallet::{suffix}")),
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

/// A chain that can answer the create and then the attach, in that order, the way `init` asks them.
fn chain(owner: &Keystore, agent: &Keystore) -> FakeSui {
    FakeSui::new()
        .with_object(None, shared(VERSION, 3, "Version"))
        .with_object(None, shared(WALLET, 4, "AgentWallet"))
        .with_object(
            Some(&owner.address().to_string()),
            owned(COIN, SUI_COIN_TYPE),
        )
        .with_balance(&owner.address().to_string(), SUI_COIN_TYPE, 1_000_000_000)
        .with_reference_gas_price(1_000)
        .with_created(vec![
            CreatedObject {
                object_id: WALLET.to_owned(),
                object_type: Some(format!(
                    "{PACKAGE}::agent_wallet::AgentWallet<0x2::sui::SUI>"
                )),
                shared_initial_version: Some(4),
                owner: None,
            },
            CreatedObject {
                object_id: CAP.to_owned(),
                object_type: Some(format!("{PACKAGE}::agent_wallet::AgentCap")),
                shared_initial_version: None,
                owner: Some(agent.address().to_string()),
            },
        ])
        // What the wallet carries before the attach, then after it. The attach waits for the second
        // before promising anything to the spend.
        .with_read_sequence(vec![type_names(&[]), type_names(&["budget", "per_tx"])])
}

fn bounded() -> CapabilityManifest {
    CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![
            CapabilityRule::Budget {
                total_mist: "200000000".into(),
            },
            CapabilityRule::PerTx {
                max_mist: "50000000".into(),
            },
        ],
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rill-cold-start-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir.join("run-set.json")
}

fn args(run_set_path: std::path::PathBuf) -> InitArgs {
    InitArgs {
        package_id: PACKAGE.into(),
        version_id: VERSION.into(),
        amount: "0.2".into(),
        budget_mist: "200000000".into(),
        per_tx_mist: "50000000".into(),
        gas_budget: 100_000_000,
        run_set_path,
    }
}

/// The whole point: one call, a wallet that exists and is bounded.
#[test]
fn one_run_mints_a_wallet_and_attaches_at_least_one_rule() {
    let (owner, agent) = keys();
    let chain = chain(&owner, &agent);
    let identities = Identities {
        owner: owner.address(),
        agent: agent.address(),
    };
    let report = run(rill_cli::init::run_on(
        &chain,
        &owner,
        &identities,
        &args(scratch("fresh")),
        &bounded(),
        1_756_600_000_000,
    ))
    .expect("a fresh machine initialises");

    assert_eq!(report["alreadyInitialised"], false);
    assert_eq!(report["walletId"], WALLET);
    assert_eq!(report["capId"], CAP);
    assert_eq!(
        report["owner"],
        owner.address().to_string(),
        "the owner is the key that can revoke, and the report must say which one it was"
    );
    assert_eq!(report["agent"], agent.address().to_string());
    let rules = &report["attached"]["rules"];
    assert!(
        rules.as_array().is_some_and(|r| !r.is_empty()),
        "a wallet with no rules attached is bounded by nothing: {report}"
    );
}

/// Scenario: re-running is a no-op rather than a second wallet. The first one would stay funded and
/// forgotten, which on testnet is waste and on mainnet is money.
#[test]
fn a_second_run_mints_nothing_and_says_which_wallet_is_already_here() {
    let path = scratch("twice");
    std::fs::write(&path, format!(r#"{{"walletId":"{WALLET}"}}"#)).unwrap();

    let (owner, agent) = keys();
    // A chain that would fail any write, so a second run that tried to mint could not quietly
    // succeed: the no-op has to be a decision rather than an accident of what the fake allows.
    let empty = FakeSui::new();
    let identities = Identities {
        owner: owner.address(),
        agent: agent.address(),
    };
    let report = run(rill_cli::init::run_on(
        &empty,
        &owner,
        &identities,
        &args(path.clone()),
        &bounded(),
        1_756_600_000_000,
    ))
    .expect("a second run is a no-op, not a failure");

    assert_eq!(report["alreadyInitialised"], true);
    assert_eq!(report["walletId"], WALLET);
    assert!(
        report["note"]
            .as_str()
            .is_some_and(|n| n.contains("Delete")),
        "the report must say how to start over: {report}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The refusals a stranger will actually meet, checked for the words they need rather than only the
/// variant. A refusal that is right and unreadable has not helped anyone.
#[test]
fn the_refusals_say_what_to_do_next() {
    let (owner, _) = keys();

    let no_keys = refuse_early(0, &bounded(), &owner.address(), 0, false).expect("refused");
    assert_eq!(no_keys, Stopped::NeedsKeys { have: 0 });
    assert!(no_keys.to_string().contains("sui client new-address"));

    let empty = CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: Vec::new(),
    };
    let no_rules = refuse_early(2, &empty, &owner.address(), 1_000, false).expect("refused");
    assert_eq!(no_rules, Stopped::EmptyRules);

    let unfunded = refuse_early(2, &bounded(), &owner.address(), 0, false).expect("refused");
    let said = unfunded.to_string();
    assert!(
        said.contains(&faucet_link(&owner.address())),
        "the faucet link must carry the address a reader needs to fund: {said}"
    );
    assert!(
        said.contains("--wait"),
        "and it must name the flag that turns the refusal into a wait: {said}"
    );
}

/// A run-set left from a previous machine state must not read as initialised unless it names a
/// wallet: half a file is how a second wallet gets minted over the top of the first.
#[test]
fn a_run_set_without_a_wallet_id_does_not_count_as_initialised() {
    let path = scratch("partial");
    std::fs::write(&path, r#"{"label":"written before the mint"}"#).unwrap();
    assert_eq!(already_initialised(&path), None);
    let _ = std::fs::remove_file(&path);
}

/// This binary reads the keystore and never writes it, and `init` is the command most tempted to.
#[test]
fn nothing_in_this_binary_opens_the_keystore_for_writing() {
    const SOURCES: [(&str, &str); 3] = [
        ("init.rs", include_str!("../src/init.rs")),
        ("keystore.rs", include_str!("../src/keystore.rs")),
        ("main.rs", include_str!("../src/main.rs")),
    ];
    for (name, source) in SOURCES {
        let shipped = source.split("#[cfg(test)]").next().unwrap_or(source);
        for (index, line) in shipped.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if !code.contains("SUI_KEYSTORE_PATH") && !code.contains("sui.keystore") {
                continue;
            }
            for writer in ["fs::write", "File::create", "OpenOptions", "write_all"] {
                assert!(
                    !code.contains(writer),
                    "{name}:{}: this line both names the keystore and writes: {}",
                    index + 1,
                    code.trim()
                );
            }
        }
        for writer in ["sui_keystore_write", "write_keystore"] {
            assert!(
                !shipped.contains(writer),
                "{name} has a keystore writer; generating a key is the sui CLI's job"
            );
        }
    }
}
