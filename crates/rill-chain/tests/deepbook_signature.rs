//! Does the DeepBook path actually work against the deployed contract?
//!
//! The builder emits Move calls positionally, so "we integrate DeepBook" is only true if the
//! argument list matches what the deployed package declares. A mismatch is a wrong-arity or
//! wrong-type call the node rejects — and it is invisible to any test that only checks the builder
//! against itself.
//!
//! So this asks the deployed package directly, on mainnet, and prints the answer next to what
//! `rill-ptb` emits.
//!
//!   cargo test -p rill-chain --test deepbook_signature -- --ignored --nocapture

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::GetFunctionRequest;

const MAINNET: &str = "https://fullnode.mainnet.sui.io:443";

/// What `rill_ptb::deepbook::place_limit_order` emits, in order.
const EMITTED: &[&str] = &[
    "pool",
    "balance_manager",
    "trade_proof",
    "client_order_id",
    "order_type",
    "self_matching_option",
    "price",
    "quantity",
    "is_bid",
    "pay_with_deep",
    "expire_timestamp",
    "clock",
];

/// Whether a declared parameter is `TxContext`.
///
/// The runtime supplies it, so it is a parameter of the function but never an argument of the PTB
/// command. Counting it makes a correct call look one short — which is how a green integration
/// reads as a bug, and the opposite mistake would let a real one through.
fn is_tx_context(p: &sui_rpc::proto::sui::rpc::v2::OpenSignature) -> bool {
    render(p).contains("TxContext")
}

/// A parameter's type, flattened enough to recognise `TxContext` and the object types.
fn render(p: &sui_rpc::proto::sui::rpc::v2::OpenSignature) -> String {
    format!("{:?}", p.body)
        .replace("Some(", "")
        .replace("OpenSignatureBody", "")
        .chars()
        .filter(|c| !"\"".contains(*c))
        .collect()
}

async fn describe(
    client: &Client,
    package: &str,
    module: &str,
    function: &str,
) -> Option<Vec<String>> {
    let mut request = GetFunctionRequest::default();
    request.package_id = Some(package.to_owned());
    request.module_name = Some(module.to_owned());
    request.name = Some(function.to_owned());

    let response = client
        .clone()
        .package_client()
        .get_function(request)
        .await
        .ok()?
        .into_inner();

    let descriptor = response.function?;
    println!(
        "  {module}::{function} — {} type parameter(s), {} parameter(s), {} return value(s)",
        descriptor.type_parameters.len(),
        descriptor.parameters.len(),
        descriptor.returns.len()
    );
    for (i, p) in descriptor.parameters.iter().enumerate() {
        println!("    {i:2}  {}", render(p));
    }
    Some(
        descriptor
            .parameters
            .iter()
            .filter(|p| !is_tx_context(p))
            .map(render)
            .collect(),
    )
}

#[tokio::test]
#[ignore = "requires network access to a Sui mainnet fullnode"]
async fn the_deployed_deepbook_declares_what_the_builder_emits() {
    use rill_ptb::registry::MAINNET_PACKAGE_ID;

    let client = Client::new(MAINNET).expect("connect");
    println!("DeepBook package: {MAINNET_PACKAGE_ID}\n");

    let parameters = describe(&client, MAINNET_PACKAGE_ID, "pool", "place_limit_order")
        .await
        .expect("the deployed DeepBook must declare pool::place_limit_order");

    println!("\n  builder emits {} argument(s):", EMITTED.len());
    for (i, name) in EMITTED.iter().enumerate() {
        println!("    {i:2}  {name}");
    }

    assert_eq!(
        parameters.len(),
        EMITTED.len(),
        "\nthe deployed pool::place_limit_order takes {} caller-supplied arguments; the builder \
         emits {}.\nA positional call cannot be right by accident — this is the drift that \
         matters.",
        parameters.len(),
        EMITTED.len()
    );
    println!("\nPASS: arity agrees with the deployed contract.");
}

/// The two other DeepBook calls the hero path makes.
#[tokio::test]
#[ignore = "requires network access to a Sui mainnet fullnode"]
async fn the_balance_manager_calls_exist_and_have_the_arity_the_builder_uses() {
    use rill_ptb::registry::MAINNET_PACKAGE_ID;

    let client = Client::new(MAINNET).expect("connect");
    println!("DeepBook package: {MAINNET_PACKAGE_ID}\n");

    for (function, emitted) in [("deposit", 2usize), ("generate_proof_as_trader", 2)] {
        let parameters = describe(&client, MAINNET_PACKAGE_ID, "balance_manager", function)
            .await
            .unwrap_or_else(|| panic!("balance_manager::{function} must exist"));
        assert_eq!(
            parameters.len(),
            emitted,
            "balance_manager::{function} takes {} caller-supplied, builder emits {emitted}",
            parameters.len()
        );
        println!("    builder emits {emitted} — agrees\n");
    }
    println!("PASS: the balance-manager calls agree with the deployed contract.");
}
