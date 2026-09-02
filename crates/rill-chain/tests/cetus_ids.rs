//! The curated Cetus list, against the chain.
//!
//! `classify_failure` treats a `checked_package_version` abort from one of these packages as
//! inconclusive rather than as a refusal — the single hole in the simulation gate. A wrong id makes
//! that hole either useless (a real Cetus abort is misread as a verdict) or wider than intended.
//!
//! A curated list nobody verifies is folklore. This checks each entry actually exists on the
//! network it claims and actually carries the assertion the classifier keys on.
//!
//!   cargo test -p rill-chain --test cetus_ids -- --ignored --nocapture

use rill_chain::describe::describe_function;
use rill_chain::CETUS_PACKAGE_IDS;

#[tokio::test]
#[ignore = "requires network access to Sui fullnodes"]
async fn every_curated_cetus_package_carries_the_assertion_it_is_listed_for() {
    let mut failures = Vec::new();

    for (network, id) in CETUS_PACKAGE_IDS {
        let endpoint = format!("https://fullnode.{network}.sui.io:443");
        match describe_function(&endpoint, id, "config", "checked_package_version").await {
            Ok(signature) => {
                println!("{network:8} {id}\n         {signature}");
            }
            Err(e) => {
                println!("{network:8} {id}\n         MISSING: {e}");
                failures.push(format!("{id} on {network}: {e}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "these curated ids do not carry config::checked_package_version, so the gate's one \
         exception would not fire for them:\n{}",
        failures.join("\n")
    );
    println!("\nPASS: every curated id exists and carries the assertion.");
}

/// An id on the wrong network would still match an error string, but it would mean the curation was
/// recorded from somewhere other than the chain — which is how a list starts drifting.
#[tokio::test]
#[ignore = "requires network access to Sui fullnodes"]
async fn no_curated_id_is_recorded_against_the_wrong_network() {
    for (network, id) in CETUS_PACKAGE_IDS {
        let other = if *network == "mainnet" {
            "testnet"
        } else {
            "mainnet"
        };
        let endpoint = format!("https://fullnode.{other}.sui.io:443");
        let found = describe_function(&endpoint, id, "config", "checked_package_version")
            .await
            .is_ok();
        println!("{id}\n  claimed {network}, present on {other}: {found}");
        assert!(
            !found,
            "{id} is listed as {network} but also answers on {other}; the network label is wrong \
             or the id is not what it is thought to be"
        );
    }
    println!("\nPASS: each id lives only on the network it is recorded against.");
}
