use rill_chain::{
    fake::{FakeSui, SimulationBehavior},
    ObjectRef, ObjectSummary,
};
use rill_cli::{
    keystore::Keystore,
    unstake_cmd::{unstake_json_on, UnstakeArgs, MAINNET_HAEDAL, MAINNET_HASUI, MAINNET_STAKING},
};
use serde_json::json;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Digest;
fn key() -> Keystore {
    Keystore::from_suiprivkey(&Ed25519PrivateKey::new([51; 32]).to_suiprivkey().unwrap()).unwrap()
}
fn object(id: &str, kind: &str, balance: u64, shared: Option<u64>) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.into(),
            version: 2,
            digest: Digest::ZERO.to_string(),
        },
        object_type: Some(kind.into()),
        fields: Some(json!({"balance":balance.to_string()})),
        shared_initial_version: shared,
    }
}
fn chain(key: &Keystore, simulation: SimulationBehavior) -> FakeSui {
    let owner = key.address().to_string();
    FakeSui::new()
        .with_object(
            None,
            object("0x5", "0x3::sui_system::SuiSystemState", 0, Some(1)),
        )
        .with_object(
            None,
            object(MAINNET_STAKING, "0x7::staking::Staking", 0, Some(1)),
        )
        .with_object(
            Some(&owner),
            object("0xa", rill_chain::gas::SUI_COIN_TYPE, 100_000_000, None),
        )
        .with_object(
            Some(&owner),
            object(
                "0xb",
                &format!("0x2::coin::Coin<{MAINNET_HASUI}>"),
                2_000_000_000,
                None,
            ),
        )
        .with_simulation(simulation)
}
fn args(dry: bool) -> UnstakeArgs {
    UnstakeArgs {
        package_id: MAINNET_HAEDAL.into(),
        staking_object_id: MAINNET_STAKING.into(),
        coin_type: MAINNET_HASUI.into(),
        guard_package_id: "0xc".into(),
        amount: "1".into(),
        min_out: "0.9".into(),
        receiver: "0xd".into(),
        gas_budget: 50_000_000,
        dry_run: dry,
    }
}
fn run<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}
#[test]
fn dry_run_never_submits() {
    let key = key();
    let chain = chain(&key, SimulationBehavior::default());
    let result = run(unstake_json_on(&chain, &key, &args(true))).unwrap();
    assert_eq!(result["submitted"], false);
    assert!(chain.submitted().is_empty());
}
#[test]
fn refused_simulation_never_submits() {
    let key = key();
    let chain = chain(
        &key,
        SimulationBehavior::Fails {
            error: "minimum unstake".into(),
        },
    );
    assert!(run(unstake_json_on(&chain, &key, &args(false))).is_err());
    assert!(chain.submitted().is_empty());
}
#[test]
fn submitted_bytes_include_redemption_floor_and_explicit_receiver() {
    let key = key();
    let chain = chain(&key, SimulationBehavior::default());
    run(unstake_json_on(&chain, &key, &args(false))).unwrap();
    let bytes = chain.submitted();
    let decoded = rill_policy::decode::decode(&bytes[0]).unwrap();
    assert_eq!(
        decoded.targets,
        vec![
            format!("{MAINNET_HAEDAL}::staking::request_unstake_instant_coin"),
            format!(
                "{}::guard::assert_min_value",
                "0xc".parse::<sui_sdk_types::Address>().unwrap()
            )
        ]
    );
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&bytes[0])
        .unwrap();
    let tx: sui_sdk_types::Transaction = bcs::from_bytes(&raw).unwrap();
    let sui_sdk_types::TransactionKind::ProgrammableTransaction(ptb) = tx.kind else {
        panic!("PTB")
    };
    assert!(ptb.inputs.iter().any(
        |i| matches!(i,sui_sdk_types::Input::Pure(v) if v==&bcs::to_bytes(&900_000_000u64).unwrap())
    ));
    assert!(ptb.inputs.iter().any(|i|matches!(i,sui_sdk_types::Input::Pure(v) if v==&bcs::to_bytes(&"0xd".parse::<sui_sdk_types::Address>().unwrap()).unwrap())));
}
