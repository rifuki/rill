//! What the deployed Haedal package actually declares on testnet.
//!
//! The adapter in `rill_ptb::haedal` was written before anything submitted through it. Its call
//! target and argument order are exactly the kind of thing a first real transaction discovers the
//! hard way, so they are read from the chain first.
//!
//!   cargo test -p rill-chain --test haedal_signature -- --ignored --nocapture

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::GetFunctionRequest;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
/// The call target: the latest version of the package, v4 on testnet.
const PACKAGE: &str = "0x0a6ff2b974e08b65649d334c38db5ca046b78b4a5d892087740b9cdb3eb08e47";

fn render(p: &sui_rpc::proto::sui::rpc::v2::OpenSignature) -> String {
    format!("{:?}", p.body)
        .replace("Some(", "")
        .replace("OpenSignatureBody", "")
        .chars()
        .filter(|c| !"\"".contains(*c))
        .collect()
}

#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_deployed_haedal_declares_request_stake() {
    let client = Client::new(TESTNET).expect("connect");
    for (module, function) in [
        ("staking", "request_stake"),
        ("staking", "request_stake_coin"),
        ("interface", "request_stake"),
        ("interface", "request_stake_coin"),
    ] {
        let mut request = GetFunctionRequest::default();
        request.package_id = Some(PACKAGE.to_owned());
        request.module_name = Some(module.to_owned());
        request.name = Some(function.to_owned());
        match client
            .clone()
            .package_client()
            .get_function(request)
            .await
            .ok()
            .and_then(|r| r.into_inner().function)
        {
            Some(d) => {
                println!(
                    "\n{module}::{function}: {} type param(s), {} param(s), {} return(s), {:?}",
                    d.type_parameters.len(),
                    d.parameters.len(),
                    d.returns.len(),
                    d.visibility()
                );
                for (i, p) in d.parameters.iter().enumerate() {
                    println!("  {i:2} {}", render(p));
                }
                for (i, r) in d.returns.iter().enumerate() {
                    println!("  ret {i} {}", render(r));
                }
            }
            None => println!("\n{module}::{function}: absent"),
        }
    }
}

/// Haedal's Staking object: whether it is paused, and which validators it already delegates to.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn the_staking_object_says_what_it_will_accept() {
    use rill_chain::{grpc::GrpcSui, SuiRead};
    let chain = GrpcSui::new(TESTNET).expect("connect");
    let staking = chain
        .get_object("0xb399662ac5d3973256a1e8629a913336449a2baa16847502ce6bdbf4a0003f07")
        .await
        .expect("the Staking object reads");
    println!("type: {}", staking.object_type.as_deref().unwrap_or("?"));
    println!("shared at: {:?}", staking.shared_initial_version);
    let Some(f) = staking.fields.as_ref() else {
        println!("no fields");
        return;
    };
    for key in [
        "pause_stake",
        "pause_unstake",
        "version",
        "stsui_supply",
        "total_staked",
        "min_stake",
    ] {
        if let Some(v) = f.get(key) {
            println!("  {key}: {v}");
        }
    }
    // Validators live under a few possible names depending on the version; print whichever exist.
    for key in ["active_validators", "validators", "validator_pool"] {
        if let Some(v) = f.get(key) {
            let s = serde_json::to_string(v).unwrap_or_default();
            println!("  {key}: {}", &s[..s.len().min(600)]);
        }
    }
    let keys: Vec<&String> = f
        .as_object()
        .map(|o| o.keys().collect())
        .unwrap_or_default();
    println!("  all top-level fields: {keys:?}");
}
