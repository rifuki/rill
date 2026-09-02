//! Reading a wallet's live rule set off chain.
//!
//! # Why the manifest cannot be trusted to say what is attached
//!
//! [`crate::spend::build_manifest_gated_spend`] emits one `prove` per rule in the manifest the
//! *caller* supplies. `confirm_spend` counts the receipts against the wallet's *real* policy and
//! aborts `E_RULE_NOT_SATISFIED` if the sets differ. So the caller's manifest and the chain's policy
//! must agree exactly — and nothing makes them agree. Emitting a `prove` for a rule that is not
//! attached aborts inside `df::borrow_mut`, with no abort code of its own to explain it; omitting
//! one that is aborts at the last command, after every other check has passed.
//!
//! Both failures are recoverable only by knowing what is actually attached. So it is read.
//!
//! # `policy_rules` returns it, and a keyless simulation is enough to see
//!
//! ```text
//! public fun agent_wallet::policy_rules<T0>(&AgentWallet<T0>): vector<TypeName>
//! ```
//!
//! Reading a Move function's return value needs no key and changes nothing — the same mechanism
//! `book::mid_price_transaction` uses for a price. What comes back is BCS: a ULEB128 length, then
//! one length-prefixed UTF-8 string per rule, each the fully-qualified type of that rule's witness.

use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, TransactionBuilder};

use crate::book::BookError;
use crate::shared::SharedObjects;

/// Build the transaction whose simulation returns a wallet's attached rule types.
pub fn policy_rules_transaction(
    package_id: Address,
    wallet_id: Address,
    coin_type: &str,
    shared: &SharedObjects,
) -> Result<sui_sdk_types::Transaction, BookError> {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(Address::ZERO);
    tx.set_gas_budget(10_000_000);
    // A literal is correct here and only here: this transaction is never submitted, and its gas
    // payment is emptied below so the node prices it itself. Reading the reference price for a
    // read would be a round trip that changes nothing.
    tx.set_gas_price(1_000);

    let wallet = tx.object(
        shared
            .input(wallet_id, false)
            .map_err(|e| BookError::BadIdentifier(e.object_id.to_string()))?,
    );

    let coin: sui_sdk_types::TypeTag = coin_type
        .parse()
        .map_err(|_| BookError::BadIdentifier(coin_type.to_owned()))?;

    tx.move_call(
        Function::new(
            package_id,
            Identifier::new("agent_wallet")
                .map_err(|_| BookError::BadIdentifier("agent_wallet".into()))?,
            Identifier::new("policy_rules")
                .map_err(|_| BookError::BadIdentifier("policy_rules".into()))?,
        )
        .with_type_args(vec![coin]),
        vec![wallet],
    );

    // A read has no payer; see `book::mid_price_transaction` for why the gas payment is emptied.
    tx.add_gas_objects([sui_transaction_builder::ObjectInput::owned(
        crate::book::PLACEHOLDER_GAS_OBJECT
            .parse()
            .expect("the placeholder is a valid address"),
        1,
        sui_sdk_types::Digest::ZERO,
    )]);

    let mut built = tx
        .try_build()
        .map_err(|_| BookError::BadIdentifier("policy_rules transaction".into()))?;
    built.gas_payment.objects.clear();
    Ok(built)
}

/// Decode `vector<TypeName>` from a command's BCS return value.
///
/// A `TypeName` is a struct holding one `String`, and BCS encodes both transparently: the vector's
/// ULEB128 length, then each string's ULEB128 length and its bytes. Anything that does not decode
/// cleanly is refused rather than partially read — a half-decoded rule list would produce a prove
/// sequence that aborts at the last command, which is the failure this exists to prevent.
pub fn parse_type_names(bytes: &[u8]) -> Result<Vec<String>, BookError> {
    let mut cursor = 0usize;
    let count = read_uleb128(bytes, &mut cursor)?;

    let mut names = Vec::with_capacity(count.min(64) as usize);
    for _ in 0..count {
        let length = read_uleb128(bytes, &mut cursor)? as usize;
        let end = cursor
            .checked_add(length)
            .ok_or(BookError::UnreadableValue)?;
        let raw = bytes.get(cursor..end).ok_or(BookError::UnreadableValue)?;
        names.push(String::from_utf8(raw.to_vec()).map_err(|_| BookError::UnreadableValue)?);
        cursor = end;
    }

    if cursor != bytes.len() {
        // Trailing bytes mean this was not the type it was read as.
        return Err(BookError::UnreadableValue);
    }
    Ok(names)
}

fn read_uleb128(bytes: &[u8], cursor: &mut usize) -> Result<u64, BookError> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*cursor).ok_or(BookError::UnreadableValue)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or(BookError::UnreadableValue)?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(BookError::UnreadableValue);
        }
    }
}

/// The rule module a witness type belongs to.
///
/// A `TypeName` reads `<address>::<module>::Rule`, so the module is the middle segment. Returns
/// `None` for a type this crate has no emitter for, because inventing a module name would produce a
/// `prove` call to a function that does not exist.
pub fn rule_module(type_name: &str) -> Option<&str> {
    let module = type_name.split("::").nth(1)?;
    ["budget", "per_tx", "rate_limit", "time_window"]
        .into_iter()
        .find(|known| *known == module)
}

/// Every recognised rule module attached to a wallet, in the order the chain reports them.
pub fn attached_modules(type_names: &[String]) -> Vec<&str> {
    type_names.iter().filter_map(|t| rule_module(t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the BCS a node would return for these type names.
    fn encode(names: &[&str]) -> Vec<u8> {
        let mut out = vec![names.len() as u8];
        for name in names {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        out
    }

    #[test]
    fn two_attached_rules_decode_to_their_modules() {
        let names = ["b02f39d6::budget::Rule", "b02f39d6::per_tx::Rule"];
        let decoded = parse_type_names(&encode(&names)).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(attached_modules(&decoded), vec!["budget", "per_tx"]);
    }

    #[test]
    fn an_empty_policy_decodes_to_nothing_rather_than_failing() {
        assert_eq!(parse_type_names(&[0]).unwrap(), Vec::<String>::new());
    }

    /// A half-read list produces a prove sequence that aborts at the last command, which is exactly
    /// the failure this module exists to prevent. So a truncated value is refused.
    #[test]
    fn a_truncated_value_is_refused_rather_than_partially_read() {
        let full = encode(&["b02f39d6::budget::Rule"]);
        assert!(matches!(
            parse_type_names(&full[..full.len() - 3]),
            Err(BookError::UnreadableValue)
        ));
    }

    #[test]
    fn trailing_bytes_mean_it_was_not_this_type() {
        let mut bytes = encode(&["b02f39d6::budget::Rule"]);
        bytes.push(0);
        assert!(matches!(
            parse_type_names(&bytes),
            Err(BookError::UnreadableValue)
        ));
    }

    /// A module with no emitter must not be guessed at — a prove call to a function that does not
    /// exist aborts with nothing to explain it.
    #[test]
    fn an_unknown_rule_module_is_not_invented() {
        assert_eq!(rule_module("b02f39d6::slippage_floor::Rule"), None);
        let decoded = vec![
            "b02f39d6::budget::Rule".to_string(),
            "b02f39d6::something_new::Rule".to_string(),
        ];
        assert_eq!(
            attached_modules(&decoded),
            vec!["budget"],
            "an unrecognised rule is dropped from the emit list, not guessed at"
        );
    }

    #[test]
    fn the_read_transaction_builds() {
        let mut shared = SharedObjects::new();
        let wallet: Address = "0x20".parse().unwrap();
        shared.insert(wallet, 349_181_939);
        assert!(policy_rules_transaction(
            "0xca".parse().unwrap(),
            wallet,
            "0x2::sui::SUI",
            &shared
        )
        .is_ok());
    }
}
