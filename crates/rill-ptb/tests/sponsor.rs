//! The owner as gas sponsor of an agent's spend, which the on-chain `step_up` rule requires above
//! its threshold.
//!
//! The gas coin is the owner's in such a transaction, so nothing in it may touch `GasCoin`: a
//! split of the gas coin would hand the agent the owner's SUI, which the owner never approved as a
//! spend and no rule bounds.

use rill_ptb::sponsor::{sponsor_gas, SponsorError};
use sui_sdk_types::{Address, Digest, ObjectReference, Transaction};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

fn addr(n: u8) -> Address {
    format!("0x{n:064x}").parse().unwrap()
}

const AGENT: u8 = 0xb;
const OWNER: u8 = 0xa;

fn base() -> TransactionBuilder {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(addr(AGENT));
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(addr(0x1), 1, Digest::ZERO)]);
    tx
}

fn owner_coin() -> ObjectReference {
    ObjectReference::new(addr(0xc0), 7, Digest::ZERO)
}

fn call(tx: &mut TransactionBuilder) {
    let wallet = tx.object(ObjectInput::shared(addr(0x20), 3, true));
    tx.move_call(
        Function::new(
            addr(0xca),
            "agent_wallet".parse().unwrap(),
            "noop".parse().unwrap(),
        ),
        vec![wallet],
    );
}

#[test]
fn the_owner_pays_the_gas_and_the_agent_stays_the_sender() {
    let mut tx = base();
    call(&mut tx);
    let mut built: Transaction = tx.try_build().unwrap();
    sponsor_gas(&mut built, addr(OWNER), vec![owner_coin()]).unwrap();
    assert_eq!(built.sender, addr(AGENT));
    assert_eq!(built.gas_payment.owner, addr(OWNER));
    assert_eq!(built.gas_payment.objects, vec![owner_coin()]);
}

#[test]
fn a_transaction_that_touches_the_gas_coin_is_refused() {
    let mut tx = base();
    call(&mut tx);
    let gas = tx.gas();
    let amount = tx.pure(&1u64);
    let split = tx.split_coins(gas, vec![amount]);
    let to = tx.pure(&addr(AGENT));
    tx.transfer_objects(split, to);
    let mut built: Transaction = tx.try_build().unwrap();
    assert_eq!(
        sponsor_gas(&mut built, addr(OWNER), vec![owner_coin()]),
        Err(SponsorError::TouchesGasCoin)
    );
    assert_eq!(
        built.gas_payment.owner,
        addr(AGENT),
        "a refused transaction is left as it was"
    );
}

#[test]
fn a_sponsor_without_coins_or_the_agent_as_its_own_sponsor_is_refused() {
    let mut tx = base();
    call(&mut tx);
    let mut built: Transaction = tx.try_build().unwrap();
    assert_eq!(
        sponsor_gas(&mut built, addr(OWNER), vec![]),
        Err(SponsorError::NoGasCoins)
    );
    assert_eq!(
        sponsor_gas(&mut built, addr(AGENT), vec![owner_coin()]),
        Err(SponsorError::SponsorIsSender)
    );
}

/// The owner signs for whatever the budget lets the network charge, so the budget is held to a
/// ceiling Studio checks again from the bytes before the owner signs.
#[test]
fn a_budget_above_the_sponsored_ceiling_is_refused() {
    let mut tx = base();
    tx.set_gas_budget(rill_ptb::sponsor::MAX_SPONSORED_GAS_BUDGET_MIST + 1);
    call(&mut tx);
    let mut built: Transaction = tx.try_build().unwrap();
    assert_eq!(
        sponsor_gas(&mut built, addr(OWNER), vec![owner_coin()]),
        Err(SponsorError::BudgetTooHigh {
            budget: rill_ptb::sponsor::MAX_SPONSORED_GAS_BUDGET_MIST + 1
        })
    );
}
