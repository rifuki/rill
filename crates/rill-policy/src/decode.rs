//! Reading a transaction's own commands, from the bytes that will be signed.
//!
//! # The gap this exists for
//!
//! Everything else the signer checks comes from the envelope: what the server *says* the
//! transaction does. That is exactly the thing a compromised or buggy server would misreport, so a
//! check against it proves only that the server is self-consistent.
//!
//! These read the base64 BCS instead — the same bytes the digest is pinned to and the same bytes
//! the signature will cover. What comes out is what the validator will execute, and nothing else
//! gets a say in it.
//!
//! # An unknown command is a refusal
//!
//! `Command` is matched exhaustively. Anything that is not a Move call is named rather than
//! skipped: a transaction may legitimately split coins and transfer objects, but a `Publish` or an
//! `Upgrade` inside a spend is not a shape anybody approved, and silently ignoring the commands
//! that are not Move calls is how one gets waved through.

use sui_sdk_types::{Command, Transaction};

use crate::Rejection;

/// What a transaction actually does, in the order it does it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// Every Move call target, in order: `<package>::<module>::<function>`.
    pub targets: Vec<String>,
    /// Every command, named — including the ones that are not Move calls.
    pub commands: Vec<String>,
    /// Object ids the transaction takes as input, shared and owned alike.
    ///
    /// **Not the gas coin.** Gas lives in `gas_payment`, not in `inputs`, so a scope check against
    /// this list says nothing about what pays for the transaction. That is usually right — the gas
    /// coin is the sender's own and is bounded by the gas budget — but it is worth knowing rather
    /// than assuming the list is everything the transaction touches.
    pub object_inputs: Vec<String>,
}

/// Decode the bytes that will be signed.
pub fn decode(unsigned_ptb_base64: &str) -> Result<Decoded, Rejection> {
    use base64::Engine as _;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(unsigned_ptb_base64.trim())
        .map_err(|_| Rejection::UndecodableTransaction("not base64".into()))?;
    let transaction: Transaction =
        bcs::from_bytes(&bytes).map_err(|e| Rejection::UndecodableTransaction(e.to_string()))?;

    // Only a programmable transaction has commands. A system transaction reaching a signer is not
    // something to interpret leniently — it is something nobody built here.
    let programmable = match &transaction.kind {
        sui_sdk_types::TransactionKind::ProgrammableTransaction(p) => p,
        other => {
            return Err(Rejection::UndecodableTransaction(format!(
                "this is a {} transaction, not a programmable one",
                match other {
                    sui_sdk_types::TransactionKind::ChangeEpoch(_) => "change-epoch",
                    sui_sdk_types::TransactionKind::Genesis(_) => "genesis",
                    _ => "system",
                }
            )))
        }
    };

    let mut targets = Vec::new();
    let mut commands = Vec::new();

    for command in &programmable.commands {
        commands.push(name_of(command).to_string());
        if let Command::MoveCall(call) = command {
            targets.push(format!(
                "{}::{}::{}",
                call.package, call.module, call.function
            ));
        }
    }

    let object_inputs = programmable
        .inputs
        .iter()
        .filter_map(|input| match input {
            sui_sdk_types::Input::ImmutableOrOwned(r) => Some(r.object_id().to_string()),
            sui_sdk_types::Input::Shared(shared) => Some(shared.object_id().to_string()),
            sui_sdk_types::Input::Receiving(r) => Some(r.object_id().to_string()),
            sui_sdk_types::Input::Pure { .. } => None,
            _ => None,
        })
        .collect();

    Ok(Decoded {
        targets,
        commands,
        object_inputs,
    })
}

/// Every command kind, named.
///
/// Exhaustive on purpose. `Command` is `#[non_exhaustive]` upstream, so a new variant reaches the
/// catch-all — and that arm names it as unrecognised rather than letting it pass as something
/// familiar. A signer that quietly tolerates a command it does not know is not a signer.
fn name_of(command: &Command) -> &'static str {
    match command {
        Command::MoveCall(_) => "MoveCall",
        Command::TransferObjects(_) => "TransferObjects",
        Command::SplitCoins(_) => "SplitCoins",
        Command::MergeCoins(_) => "MergeCoins",
        Command::Publish(_) => "Publish",
        Command::MakeMoveVector(_) => "MakeMoveVector",
        Command::Upgrade(_) => "Upgrade",
        _ => "Unrecognised",
    }
}

/// Commands a spend transaction may contain besides its Move calls.
///
/// `SplitCoins` and `TransferObjects` are how a released coin is sized and consumed, and a spend
/// that could not do either would be unbuildable. `MergeCoins` and `MakeMoveVector` shuffle values
/// the transaction already holds. `Publish` and `Upgrade` deploy code, which no spend does.
const HARMLESS: &[&str] = &[
    "MoveCall",
    "SplitCoins",
    "TransferObjects",
    "MergeCoins",
    "MakeMoveVector",
];

/// Refuse a transaction carrying a command kind a spend has no business containing.
pub fn check_command_kinds(decoded: &Decoded) -> Result<(), Rejection> {
    if let Some(unexpected) = decoded
        .commands
        .iter()
        .find(|c| !HARMLESS.contains(&c.as_str()))
    {
        return Err(Rejection::UnexpectedCommand(unexpected.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_sdk_types::{Address, Digest, Identifier};
    use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

    fn addr(n: u8) -> Address {
        format!("0x{n:064x}").parse().unwrap()
    }

    fn encode(tx: Transaction) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&tx).unwrap())
    }

    fn builder() -> TransactionBuilder {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(addr(9));
        tx.set_gas_budget(50_000_000);
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(addr(0x0a), 1, Digest::ZERO)]);
        tx
    }

    #[test]
    fn the_targets_come_out_in_order() {
        let mut tx = builder();
        for function in ["first", "second", "third"] {
            tx.move_call(
                Function::new(
                    addr(0xca),
                    Identifier::new("m").unwrap(),
                    Identifier::new(function).unwrap(),
                ),
                vec![],
            );
        }
        let decoded = decode(&encode(tx.try_build().unwrap())).unwrap();
        assert_eq!(
            decoded.targets,
            vec![
                format!("{}::m::first", addr(0xca)),
                format!("{}::m::second", addr(0xca)),
                format!("{}::m::third", addr(0xca)),
            ],
            "order is the whole point — a set comparison waves through a reordered transaction"
        );
    }

    /// Reading the envelope would have believed whatever it said. This reads the bytes.
    #[test]
    fn a_command_that_is_not_a_move_call_is_still_named() {
        let mut tx = builder();
        let amount = tx.pure(&1u64);
        let gas = tx.gas();
        let coin = tx
            .split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap();
        let to = tx.pure(&addr(7));
        tx.transfer_objects(vec![coin], to);

        let decoded = decode(&encode(tx.try_build().unwrap())).unwrap();
        assert_eq!(decoded.commands, vec!["SplitCoins", "TransferObjects"]);
        assert!(decoded.targets.is_empty());
    }

    #[test]
    fn object_inputs_are_reported_and_pure_values_are_not() {
        let mut tx = builder();
        let shared = tx.object(ObjectInput::shared(addr(0x20), 400_020, true));
        let _ = tx.pure(&12345u64);
        tx.move_call(
            Function::new(
                addr(0xca),
                Identifier::new("m").unwrap(),
                Identifier::new("f").unwrap(),
            ),
            vec![shared],
        );
        let decoded = decode(&encode(tx.try_build().unwrap())).unwrap();
        assert!(decoded.object_inputs.contains(&addr(0x20).to_string()));
        assert_eq!(
            decoded.object_inputs,
            vec![addr(0x20).to_string()],
            "the shared object, and only it: the pure value is not an object, and the gas coin \
             lives in gas_payment rather than in inputs"
        );
        assert!(
            !decoded.object_inputs.contains(&addr(0x0a).to_string()),
            "the gas coin is not an input; anything checking object scope against this list is \
             not checking what pays for the transaction"
        );
    }

    #[test]
    fn a_split_and_transfer_spend_is_allowed() {
        let mut tx = builder();
        let amount = tx.pure(&1u64);
        let gas = tx.gas();
        let coin = tx
            .split_coins(gas, vec![amount])
            .into_iter()
            .next()
            .unwrap();
        let to = tx.pure(&addr(7));
        tx.transfer_objects(vec![coin], to);
        let decoded = decode(&encode(tx.try_build().unwrap())).unwrap();
        assert!(check_command_kinds(&decoded).is_ok());
    }

    #[test]
    fn bytes_that_are_not_a_transaction_are_refused_rather_than_read_as_empty() {
        assert!(matches!(
            decode("bm90IGEgdHJhbnNhY3Rpb24="),
            Err(Rejection::UndecodableTransaction(_))
        ));
        assert!(matches!(
            decode("!!!not base64!!!"),
            Err(Rejection::UndecodableTransaction(_))
        ));
    }

    /// An empty target list from undecodable bytes would compare equal to an empty expectation.
    #[test]
    fn undecodable_bytes_never_become_an_empty_success() {
        let Err(rejection) = decode("") else {
            panic!("empty input must not decode to a transaction with no commands");
        };
        assert!(matches!(rejection, Rejection::UndecodableTransaction(_)));
    }
}
