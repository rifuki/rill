//! DeepBook limit orders, built directly.
//!
//! No protocol SDK sits on this path. What the TypeScript SDK contributes is one
//! `pool::place_limit_order` call plus a trade proof and a scalar conversion — and the only Rust
//! DeepBook SDK is third-party, unpublished to crates.io, and last seen at thirty-five commits.
//! Taking that dependency on the path that moves money would buy nothing and cost a great deal.
//!
//! ## Price and quantity
//!
//! The reference reached this call with `price` and `quantity` as JavaScript numbers, and the
//! SDK then computed `Math.round(value * scalar)`. Two of the pools DeepBook lists make that
//! arithmetic land a base unit off. Here both arrive as exact integers computed in `rill-core`,
//! and this module cannot construct them any other way.

use rill_core::amounts::{
    deepbook_price_to_base_units, deepbook_quantity_to_base_units, AmountError,
};
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, ObjectInput, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

use crate::spend::CLOCK_ID;

/// DeepBook's default order type: no time-in-force restriction.
const ORDER_TYPE_NO_RESTRICTION: u8 = 0;
/// Self-matching allowed, matching the reference's default.
const SELF_MATCHING_ALLOWED: u8 = 0;
/// DeepBook's "no expiry" sentinel.
const MAX_TIMESTAMP: u64 = 1_844_674_407_370_955_161;

/// A pool's identity and the scalars its two coins use.
#[derive(Debug, Clone)]
pub struct PoolSpec {
    pub pool_id: Address,
    pub base_coin_type: String,
    pub quote_coin_type: String,
    pub base_scalar: u128,
    pub quote_scalar: u128,
}

/// One limit order, with amounts still in their human decimal form. They are converted exactly,
/// here, once.
#[derive(Clone)]
pub struct LimitOrder {
    pub pool: PoolSpec,
    pub balance_manager_id: Address,
    pub trade_cap: ObjectInput,
    pub client_order_id: u64,
    /// Decimal string. Never a float.
    pub price: String,
    /// Decimal string. Never a float.
    pub quantity: String,
    pub is_bid: bool,
    pub pay_with_deep: bool,
}

/// DeepBook's price scaling constant.
pub const FLOAT_SCALAR: u128 = 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepBookError {
    /// A shared object was referenced before its initial version was known.
    UnknownShared(UnknownSharedVersion),
    Amount {
        field: &'static str,
        source: AmountError,
    },
    BadIdentifier(String),
}

impl std::fmt::Display for DeepBookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::Amount { field, source } => write!(f, "{field}: {source}"),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier"),
        }
    }
}

impl std::error::Error for DeepBookError {}

impl From<UnknownSharedVersion> for DeepBookError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, DeepBookError> {
    Identifier::new(s).map_err(|_| DeepBookError::BadIdentifier(s.to_owned()))
}

fn type_tag(s: &str) -> Result<sui_sdk_types::TypeTag, DeepBookError> {
    s.parse()
        .map_err(|_| DeepBookError::BadIdentifier(s.to_owned()))
}

/// Deposit a coin into the balance manager, then place the order against it.
///
/// The coin is consumed by the deposit. Everything the order needs afterwards comes from the
/// manager's balance, which is why the funding coin must be deposited in full rather than split —
/// a partial deposit leaves a remainder that nothing will consume.
pub fn place_limit_order(
    tx: &mut TransactionBuilder,
    deepbook_package: Address,
    order: &LimitOrder,
    funding_coin: Argument,
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
) -> Result<(), DeepBookError> {
    let price_base = deepbook_price_to_base_units(
        &order.price,
        FLOAT_SCALAR,
        order.pool.quote_scalar,
        order.pool.base_scalar,
    )
    .map_err(|source| DeepBookError::Amount {
        field: "price",
        source,
    })?;

    let quantity_base = deepbook_quantity_to_base_units(&order.quantity, order.pool.base_scalar)
        .map_err(|source| DeepBookError::Amount {
            field: "quantity",
            source,
        })?;

    let manager = tx.object(shared.input(order.balance_manager_id, true)?);

    // 1. Deposit the funding coin. Type argument is the coin being deposited.
    tx.move_call(
        Function::new(
            deepbook_package,
            ident("balance_manager")?,
            ident("deposit")?,
        )
        .with_type_args(vec![type_tag(&order.pool.quote_coin_type)?]),
        vec![manager, funding_coin],
    );

    // 2. Prove the agent may trade on this manager. The TradeCap is what makes the order
    //    authorised without the owner's key — the delegation the whole design rests on.
    let trade_cap = tx.object(order.trade_cap.clone());
    let proof = tx.move_call(
        Function::new(
            deepbook_package,
            ident("balance_manager")?,
            ident("generate_proof_as_trader")?,
        ),
        vec![manager, trade_cap],
    );

    // 3. Place the order. Twelve arguments, two type arguments, in DeepBook's declared order.
    let pool = tx.object(shared.input(order.pool.pool_id, true)?);
    let clock = tx.object(shared.input(CLOCK_ID.parse().expect("0x6 is a valid address"), false)?);
    let args = vec![
        pool,
        manager,
        proof,
        tx.pure(&order.client_order_id),
        tx.pure(&ORDER_TYPE_NO_RESTRICTION),
        tx.pure(&SELF_MATCHING_ALLOWED),
        tx.pure(&price_base),
        tx.pure(&quantity_base),
        tx.pure(&order.is_bid),
        tx.pure(&order.pay_with_deep),
        tx.pure(&MAX_TIMESTAMP),
        clock,
    ];
    tx.move_call(
        Function::new(
            deepbook_package,
            ident("pool")?,
            ident("place_limit_order")?,
        )
        .with_type_args(vec![
            type_tag(&order.pool.base_coin_type)?,
            type_tag(&order.pool.quote_coin_type)?,
        ]),
        args,
    );

    Ok(())
}

/// The Move call targets a limit order emits, in order — the second half of what the signer pins.
pub fn expected_order_targets(deepbook_package: Address) -> Vec<String> {
    vec![
        format!("{deepbook_package}::balance_manager::deposit"),
        format!("{deepbook_package}::balance_manager::generate_proof_as_trader"),
        format!("{deepbook_package}::pool::place_limit_order"),
    ]
}
