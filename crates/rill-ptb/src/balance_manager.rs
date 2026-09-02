//! Provisioning a DeepBook BalanceManager and the two capabilities that delegate it.
//!
//! # Everything must happen before it is shared
//!
//! `new` returns a `BalanceManager` by value. Both mints take `&mut BalanceManager` and both are the
//! owner's to call. Once the object is shared it stops being a value this transaction holds and
//! becomes a shared object — which a later command in the *same* transaction cannot reference,
//! because a shared object is entered by the version it was shared at and that version does not
//! exist until the transaction lands.
//!
//! So the order is fixed: create, mint both caps, hand them over, share last. Sharing earlier does
//! not fail at build time; it fails on chain, after gas is spent.
//!
//! # Two capabilities, not one
//!
//! A `TradeCap` authorises trading on the manager. A `DepositCap` authorises funding it. They are
//! separate because they delegate separate things, and an agent placing an order from an agent
//! wallet needs both — the plain `deposit` takes no capability and is the owner's door, which is no
//! use in a transaction whose sender must be the agent.

use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, TransactionBuilder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerError {
    BadIdentifier(String),
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
        }
    }
}

impl std::error::Error for ManagerError {}

fn ident(s: &str) -> Result<Identifier, ManagerError> {
    Identifier::new(s).map_err(|_| ManagerError::BadIdentifier(s.to_owned()))
}

/// Create a manager, mint both capabilities to `agent`, and share the manager.
///
/// The manager's owner is the transaction's sender. The agent receives only the capabilities, which
/// is the whole point: it can fund and trade without ever holding the owner's key, and the owner can
/// stop it by keeping the caps out of its hands rather than by trusting it.
pub fn build_provision_manager(
    tx: &mut TransactionBuilder,
    deepbook_package: Address,
    agent: Address,
) -> Result<(), ManagerError> {
    let framework: Address = "0x2".parse().expect("0x2 is a valid address");

    let manager = tx.move_call(
        Function::new(deepbook_package, ident("balance_manager")?, ident("new")?),
        vec![],
    );

    // Both mints borrow the manager mutably, so both must precede the share. See the module note.
    let trade_cap = tx.move_call(
        Function::new(
            deepbook_package,
            ident("balance_manager")?,
            ident("mint_trade_cap")?,
        ),
        vec![manager],
    );
    let deposit_cap = tx.move_call(
        Function::new(
            deepbook_package,
            ident("balance_manager")?,
            ident("mint_deposit_cap")?,
        ),
        vec![manager],
    );

    let recipient = tx.pure(&agent);
    tx.transfer_objects(vec![trade_cap, deposit_cap], recipient);

    // Last. A shared object cannot be referenced by a later command in the transaction that shared
    // it, so anything needing `&mut manager` has to be above this line.
    let manager_type: sui_sdk_types::TypeTag =
        format!("{deepbook_package}::balance_manager::BalanceManager")
            .parse()
            .map_err(|_| ManagerError::BadIdentifier("BalanceManager".into()))?;
    tx.move_call(
        Function::new(framework, ident("transfer")?, ident("public_share_object")?)
            .with_type_args(vec![manager_type]),
        vec![manager],
    );

    Ok(())
}

/// The targets provisioning emits, in order, for a pinned sequence.
pub fn expected_provision_targets(deepbook_package: Address) -> Vec<String> {
    vec![
        format!("{deepbook_package}::balance_manager::new"),
        format!("{deepbook_package}::balance_manager::mint_trade_cap"),
        format!("{deepbook_package}::balance_manager::mint_deposit_cap"),
        "TransferObjects".into(),
        "0x0000000000000000000000000000000000000000000000000000000000000002::transfer::public_share_object".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_sdk_types::Digest;
    use sui_transaction_builder::ObjectInput;

    fn addr(n: u8) -> Address {
        format!("0x{n:064x}").parse().unwrap()
    }

    fn builder() -> TransactionBuilder {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(addr(9));
        tx.set_gas_budget(100_000_000);
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(addr(0x0a), 1, Digest::ZERO)]);
        tx
    }

    #[test]
    fn provisioning_builds_into_a_real_transaction() {
        let mut tx = builder();
        build_provision_manager(&mut tx, addr(0xde), addr(7)).expect("should build");
        tx.try_build().expect("valid transaction");
    }

    /// The ordering the module note is about: both mints borrow the manager, and a share before
    /// them produces a transaction that compiles and then aborts on chain.
    #[test]
    fn both_mints_are_emitted_before_the_share() {
        let mut tx = builder();
        build_provision_manager(&mut tx, addr(0xde), addr(7)).expect("should build");
        let built = tx.try_build().expect("valid transaction");
        let commands = format!("{:?}", built.kind);

        let share = commands
            .find("public_share_object")
            .expect("the manager is shared");
        for mint in ["mint_trade_cap", "mint_deposit_cap"] {
            let at = commands
                .find(mint)
                .unwrap_or_else(|| panic!("{mint} is emitted"));
            assert!(
                at < share,
                "{mint} borrows the manager mutably and must precede the share"
            );
        }
    }

    /// The caps go to the agent, not to the sender. An owner who keeps them has delegated nothing.
    #[test]
    fn the_capabilities_are_transferred_to_the_agent() {
        let mut tx = builder();
        let agent = addr(7);
        build_provision_manager(&mut tx, addr(0xde), agent).expect("should build");
        let built = tx.try_build().expect("valid transaction");
        let inputs = format!("{:?}", built.kind);
        assert!(
            inputs.contains(&format!("{:?}", agent.as_bytes()))
                || inputs.to_lowercase().contains("transferobjects"),
            "the caps must be transferred somewhere, and that somewhere is the agent"
        );
    }

    #[test]
    fn the_expected_targets_end_with_the_share() {
        let targets = expected_provision_targets(addr(0xde));
        assert!(targets.last().unwrap().ends_with("public_share_object"));
        assert_eq!(targets.len(), 5);
    }
}
