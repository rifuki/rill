use rill_chain::grpc::GrpcSui;
use rill_chain::SuiRead;
use rill_ptb::policy_read::policy_rules_transaction;
use rill_ptb::shared::SharedObjects;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const PACKAGE: &str = "0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740";
const WALLET: &str = "0x20391fa91aec7a12b6657902af80036e125d1beff6621fe2eb73cfd032a04e5d";

#[tokio::test]
#[ignore]
async fn dump_raw_bcs() {
    let chain = GrpcSui::new(TESTNET).unwrap();
    let summary = chain.get_object(WALLET).await.unwrap();
    println!("summary = {summary:#?}");
    let initial = summary.shared_initial_version.unwrap();
    let mut shared = SharedObjects::new();
    shared.insert(WALLET.parse().unwrap(), initial);
    let tx = policy_rules_transaction(
        PACKAGE.parse().unwrap(),
        WALLET.parse().unwrap(),
        "0x2::sui::SUI",
        &shared,
    )
    .unwrap();
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&tx).unwrap())
    };
    let outcome = chain.simulate_read(&b64).await.unwrap();
    for (i, cmd) in outcome.command_returns.iter().enumerate() {
        for (j, bytes) in cmd.iter().enumerate() {
            println!(
                "cmd {i} ret {j}: len={} hex={}",
                bytes.len(),
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
        }
    }
}
