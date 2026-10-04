use rill_ptb::haedal::{request_unstake_instant, Unstake};
use rill_ptb::shared::SharedObjects;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};
#[test]
fn instant_redemption_returns_a_chainable_coin() {
    let mut tx = TransactionBuilder::new();
    let sender: Address = "0x9".parse().unwrap();
    tx.set_sender(sender);
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(100);
    tx.add_gas_objects([ObjectInput::owned("0xa".parse().unwrap(), 1, Digest::ZERO)]);
    let input = tx.object(ObjectInput::owned("0xb".parse().unwrap(), 1, Digest::ZERO));
    let mut shared = SharedObjects::new();
    shared.insert("0x5".parse().unwrap(), 1);
    shared.insert("0x8".parse().unwrap(), 2);
    let output = request_unstake_instant(
        &mut tx,
        &Unstake {
            package_id: "0x7".parse().unwrap(),
            staking_object_id: "0x8".parse().unwrap(),
        },
        input,
        &shared,
    )
    .unwrap();
    let receiver = tx.pure(&sender);
    tx.transfer_objects(vec![output], receiver);
    let built = tx.try_build().unwrap();
    let sui_sdk_types::TransactionKind::ProgrammableTransaction(ptb) = built.kind else {
        panic!("PTB")
    };
    let sui_sdk_types::Command::MoveCall(call) = &ptb.commands[0] else {
        panic!("MoveCall")
    };
    assert_eq!(call.function.as_str(), "request_unstake_instant_coin");
    assert_eq!(call.arguments.len(), 3);
}
