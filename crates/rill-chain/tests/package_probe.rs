//! Which deployed `agent_wallet` package is the real one.
//!
//! The reference repo names two different addresses — its README and pitch deck say one, its
//! `Published.toml` and `.env.example` say another — and the docs describe a `spend()` entry point
//! that no longer exists in the Move source. A document cannot settle this. The chain can.
//!
//! Ignored by default:
//!   cargo test -p rill-chain --test package_probe -- --ignored --nocapture

use sui_rpc::client::Client;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// The two candidates, and the functions that decide between them.
const CANDIDATES: &[(&str, &str)] = &[
    (
        "README / pitch.tsx",
        "0xd9265581b6b930f5fd27d9ec98e67b48f876f5de7bd25155639d808e9da636da",
    ),
    (
        "Published.toml / .env.example",
        "0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740",
    ),
];

/// `request_spend` is the hot-potato entry point the Rust builder emits; `spend` is what the
/// reference's docs still describe. A package carrying only the second is the older deployment.
const FUNCTIONS: &[&str] = &["request_spend", "confirm_spend", "spend"];

#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn the_authoritative_agent_wallet_package_is_the_one_that_has_request_spend() {
    let client = Client::new(TESTNET).expect("connect");

    for (source, package) in CANDIDATES {
        println!("\n{package}\n  named by: {source}");

        for function in FUNCTIONS {
            use sui_rpc::proto::sui::rpc::v2::GetFunctionRequest;
            let mut request = GetFunctionRequest::default();
            request.package_id = Some((*package).to_owned());
            request.module_name = Some("agent_wallet".to_owned());
            request.name = Some((*function).to_owned());

            let found = client
                .clone()
                .package_client()
                .get_function(request)
                .await
                .is_ok();
            println!(
                "  {function:16} {}",
                if found { "present" } else { "absent" }
            );
        }
    }
    println!("\nThe package carrying request_spend + confirm_spend is the one to bind to.");
}

/// Which package the demo wallet's capabilities actually belong to.
///
/// A cap minted by the old package cannot authorise a call in the new one — the type does not
/// match — so this decides whether the funded testnet wallet can drive the current contract at all.
#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn the_funded_wallets_capabilities_name_a_package() {
    use rill_chain::grpc::GrpcSui;
    use rill_chain::SuiRead;

    const FUNDED_SENDER: &str =
        "0xf73e2dea746d9a7071ec5c49bfc2a75f73be5efd02212632e849217234e7ab46";

    let chain = GrpcSui::new(TESTNET).expect("connect");
    let objects = chain
        .list_owned_objects(FUNDED_SENDER)
        .await
        .expect("list the sender's objects");

    let caps: Vec<_> = objects
        .iter()
        .filter_map(|o| o.object_type.as_deref())
        .filter(|t| t.contains("AgentCap"))
        .collect();

    println!(
        "\n{} AgentCap object(s) held by {FUNDED_SENDER}",
        caps.len()
    );
    for t in &caps {
        println!("  {t}");
    }
    if caps.is_empty() {
        println!("  (none — nothing to reconcile)");
    }
}
