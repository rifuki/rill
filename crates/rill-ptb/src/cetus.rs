//! Cetus swaps, built directly against `router::swap`.
//!
//! # The zero-coin pattern
//!
//! `router::swap` takes both sides of the pair. Only one carries value; the other must be an empty
//! coin of the correct type, made with `0x2::coin::zero`.
//!
//! Exactly one, and this is the trap worth naming: an extra zero coin left unconsumed aborts
//! execution with `UnusedValueWithoutDrop`, and the reference documents that its devInspect missed
//! that failure entirely. The Rust simulation does catch it — verified against a live testnet node
//! while building `rill-chain` — but the shape below is what stops it arising in the first place.

use rill_core::amounts::AmountError;
use sui_sdk_types::{Address, Identifier, TypeTag};
use sui_transaction_builder::{Argument, Function, ObjectInput, TransactionBuilder};

use crate::spend::CLOCK_ID;

/// One swap, with everything the call needs that the pool cannot supply.
#[derive(Clone)]
pub struct Swap {
    /// Cetus's `integrate` package, which is where `router::swap` lives.
    pub integrate_package_id: Address,
    pub global_config_id: Address,
    pub pool_id: Address,
    /// Type arguments in the pool's own order: `<CoinA, CoinB>`.
    pub coin_type_a: String,
    pub coin_type_b: String,
    /// True when swapping A into B. Decides which side gets the funded coin.
    pub a2b: bool,
    /// True when `amount` names the input; false when it names the desired output.
    pub by_amount_in: bool,
    /// Base units. Exact, never a float.
    pub amount: u64,
    /// The price bound the swap may not cross, as Cetus's u128 sqrt-price.
    pub sqrt_price_limit: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CetusError {
    BadIdentifier(String),
    Amount(AmountError),
    ZeroAmount,
}

impl std::fmt::Display for CetusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::Amount(e) => write!(f, "{e}"),
            Self::ZeroAmount => write!(f, "refusing to build a swap of zero"),
        }
    }
}

impl std::error::Error for CetusError {}

fn ident(s: &str) -> Result<Identifier, CetusError> {
    Identifier::new(s).map_err(|_| CetusError::BadIdentifier(s.to_owned()))
}

fn type_tag(s: &str) -> Result<TypeTag, CetusError> {
    s.parse()
        .map_err(|_| CetusError::BadIdentifier(s.to_owned()))
}

/// The framework's empty-coin constructor.
fn zero_coin(tx: &mut TransactionBuilder, coin_type: &str) -> Result<Argument, CetusError> {
    let framework: Address = "0x2".parse().expect("0x2 is a valid address");
    Ok(tx.move_call(
        Function::new(framework, ident("coin")?, ident("zero")?)
            .with_type_args(vec![type_tag(coin_type)?]),
        vec![],
    ))
}

/// Emit `router::swap`, returning the output coin.
///
/// The funded coin goes on the side `a2b` selects and a zero coin fills the other. The caller must
/// consume the returned coin — a guard, a downstream action, or the settle sweep.
pub fn swap(
    tx: &mut TransactionBuilder,
    swap: &Swap,
    funded_coin: Argument,
) -> Result<Argument, CetusError> {
    if swap.amount == 0 {
        return Err(CetusError::ZeroAmount);
    }

    // Exactly one zero coin, on the side that is not funded. See the module note.
    let (coin_a, coin_b) = if swap.a2b {
        (funded_coin, zero_coin(tx, &swap.coin_type_b)?)
    } else {
        (zero_coin(tx, &swap.coin_type_a)?, funded_coin)
    };

    let config = tx.object(ObjectInput::shared(swap.global_config_id, 0, false));
    let pool = tx.object(ObjectInput::shared(swap.pool_id, 0, true));
    let clock = tx.object(ObjectInput::shared(
        CLOCK_ID.parse().expect("0x6 is a valid address"),
        0,
        false,
    ));

    let args = vec![
        config,
        pool,
        coin_a,
        coin_b,
        tx.pure(&swap.a2b),
        tx.pure(&swap.by_amount_in),
        tx.pure(&swap.amount),
        tx.pure(&swap.sqrt_price_limit),
        // Cetus's "use full input" flag. False, because a swap that silently consumes more than
        // the amount asked for is not the swap that was approved.
        tx.pure(&false),
        clock,
    ];

    let result = tx.move_call(
        Function::new(swap.integrate_package_id, ident("router")?, ident("swap")?).with_type_args(
            vec![type_tag(&swap.coin_type_a)?, type_tag(&swap.coin_type_b)?],
        ),
        args,
    );

    // `router::swap` returns both sides; the output is whichever one was not funded.
    Ok(result)
}

/// The target a swap emits, for the signer's pinned sequence.
pub fn expected_swap_targets(integrate_package_id: Address) -> Vec<String> {
    vec![
        "0x0000000000000000000000000000000000000000000000000000000000000002::coin::zero"
            .to_string(),
        format!("{integrate_package_id}::router::swap"),
    ]
}
