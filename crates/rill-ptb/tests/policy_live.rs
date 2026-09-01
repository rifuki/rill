//! Reading the live rule set off a wallet that exists.
//!
//!   cargo test -p rill-ptb --test policy_live -- --ignored --nocapture

use rill_chain::grpc::GrpcSui;
use rill_chain::SuiRead;
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::shared::SharedObjects;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const PACKAGE: &str = "0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740";
/// Created by `rill wallet create` and bounded by `rill wallet rules` — budget and per_tx.
const WALLET: &str = "0x20391fa91aec7a12b6657902af80036e125d1beff6621fe2eb73cfd032a04e5d";

#[tokio::test]
#[ignore = "requires network access to a Sui testnet fullnode"]
async fn a_live_wallet_reports_the_rules_it_actually_carries() {
    let chain = GrpcSui::new(TESTNET).expect("connect");

    let summary = chain.get_object(WALLET).await.expect("the wallet exists");
    let initial = summary
        .shared_initial_version
        .expect("an AgentWallet is shared");
    println!("wallet : {WALLET}\n         shared at {initial}");

    let mut shared = SharedObjects::new();
    shared.insert(WALLET.parse().unwrap(), initial);

    let tx = policy_rules_transaction(
        PACKAGE.parse().unwrap(),
        WALLET.parse().unwrap(),
        "0x2::sui::SUI",
        &shared,
    )
    .expect("build the read");

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&tx).unwrap())
    };

    let outcome = chain.simulate_read(&b64).await.expect("the node answers");
    println!(
        "ok={} outputs={} err={:?}",
        outcome.ok, outcome.command_output_count, outcome.error
    );
    assert!(outcome.ok, "reading a policy must not fail");

    let bytes = outcome
        .command_returns
        .iter()
        .flatten()
        .next()
        .expect("policy_rules returns a value");
    let names = parse_type_names(bytes).expect("a vector<TypeName>");

    println!("\nattached rule types:");
    for name in &names {
        println!("  {name}");
    }
    let modules = attached_modules(&names);
    println!("\nmodules: {modules:?}");

    assert!(
        modules.contains(&"budget") && modules.contains(&"per_tx"),
        "this wallet was bounded by budget and per_tx; the chain reports {modules:?}"
    );
    println!("\nPASS: the prove list can be derived from the chain instead of guessed.");
}
