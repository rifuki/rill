//! Protected Cetus actions: the adapter performs the swap and settles to the vault owner.
use sui_sdk_types::{Address, Identifier, TypeTag};
use sui_transaction_builder::{Function, TransactionBuilder};

use crate::{
    shared::SharedObjects,
    spend::{build_manifest_spend_request, WalletBinding},
};

#[derive(Clone, Debug)]
pub struct ProtectedSwap {
    pub adapter_package: Address,
    pub pool_id: Address,
    pub config_id: Address,
    pub coin_type_a: String,
    pub coin_type_b: String,
    pub a2b: bool,
    pub revision: u64,
    pub min_output: u64,
    pub sqrt_price_limit: u128,
}

fn function(swap: &ProtectedSwap, operation: &str) -> Result<Function, String> {
    let a: TypeTag = swap
        .coin_type_a
        .parse()
        .map_err(|_| "invalid pool coin A")?;
    let b: TypeTag = swap
        .coin_type_b
        .parse()
        .map_err(|_| "invalid pool coin B")?;
    Ok(Function::new(
        swap.adapter_package,
        Identifier::new("swap").map_err(|e| e.to_string())?,
        Identifier::new(operation).map_err(|e| e.to_string())?,
    )
    .with_type_args(vec![a, b]))
}

/// Owner approval: pin the adapter witness, pool, output asset and minimum result on chain.
pub fn configure(
    tx: &mut TransactionBuilder,
    swap: &ProtectedSwap,
    wallet_id: Address,
    version_id: Address,
    shared: &SharedObjects,
) -> Result<(), String> {
    if swap.min_output == 0 {
        return Err("protected swaps require positive minimum output".into());
    }
    let wallet = tx.object(shared.input(wallet_id, true).map_err(|e| e.to_string())?);
    let version = tx.object(shared.input(version_id, false).map_err(|e| e.to_string())?);
    let pool = tx.pure(&swap.pool_id);
    let floor = tx.pure(&swap.min_output);
    let op = if swap.a2b {
        "configure_a_to_b"
    } else {
        "configure_b_to_a"
    };
    tx.move_call(function(swap, op)?, vec![wallet, version, pool, floor]);
    Ok(())
}

/// The request stays a hot potato until a typed protocol adapter completes owner settlement.
pub fn execute(
    tx: &mut TransactionBuilder,
    swap: &ProtectedSwap,
    binding: &WalletBinding,
    amount: u64,
    shared: &SharedObjects,
) -> Result<(), String> {
    if swap.min_output == 0 || swap.revision == 0 {
        return Err("protected floor and revision must be positive".into());
    }
    let input = if swap.a2b {
        &swap.coin_type_a
    } else {
        &swap.coin_type_b
    };
    let expected: TypeTag = input.parse().map_err(|_| "invalid input type")?;
    let actual: TypeTag = binding
        .coin_type
        .parse()
        .map_err(|_| "invalid wallet type")?;
    if expected != actual {
        return Err("protected input must match vault asset".into());
    }
    let (request, wallet, version, clock) =
        build_manifest_spend_request(tx, binding, amount, shared).map_err(|e| e.to_string())?;
    let config = tx.object(
        shared
            .input(swap.config_id, false)
            .map_err(|e| e.to_string())?,
    );
    let pool = tx.object(
        shared
            .input(swap.pool_id, true)
            .map_err(|e| e.to_string())?,
    );
    let revision = tx.pure(&swap.revision);
    let floor = tx.pure(&swap.min_output);
    let limit = tx.pure(&swap.sqrt_price_limit);
    let op = if swap.a2b {
        "execute_a_to_b"
    } else {
        "execute_b_to_a"
    };
    tx.move_call(
        function(swap, op)?,
        vec![
            wallet, request, revision, floor, limit, version, config, pool, clock,
        ],
    );
    Ok(())
}
