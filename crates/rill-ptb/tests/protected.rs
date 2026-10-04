use rill_ptb::protected::{configure, ProtectedSwap};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::Address;
use sui_transaction_builder::TransactionBuilder;

#[test]
fn protected_setup_requires_a_real_output_floor() {
    let mut tx = TransactionBuilder::new();
    let mut shared = SharedObjects::new();
    let wallet: Address = "0x10".parse().unwrap();
    let version: Address = "0x11".parse().unwrap();
    shared.insert(wallet, 1);
    shared.insert(version, 1);
    let swap = ProtectedSwap {
        adapter_package: "0x12".parse().unwrap(),
        pool_id: "0x13".parse().unwrap(),
        config_id: "0x14".parse().unwrap(),
        coin_type_a: "0x2::sui::SUI".into(),
        coin_type_b: "0x15::usdc::USDC".into(),
        a2b: true,
        revision: 1,
        min_output: 0,
        sqrt_price_limit: 4_295_048_017,
    };
    assert!(configure(&mut tx, &swap, wallet, version, &shared).is_err());
}
