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
use sui_transaction_builder::{Argument, Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

use crate::spend::CLOCK_ID;

/// Cetus's price bounds, which are the range every pool's price lives inside.
///
/// A swap names the price it refuses to cross. Which end of this range leaves it open depends on the
/// direction, which is why a single "no limit" value does not exist.
pub const MAX_SQRT_PRICE: u128 = 79_226_673_515_401_279_992_447_579_055;
pub const MIN_SQRT_PRICE: u128 = 4_295_048_016;

/// Both coins a swap hands back, and which of them is the output.
///
/// # Both must be consumed
///
/// `Coin` has no `drop`, so a returned coin left unused aborts execution with
/// `UnusedValueWithoutDrop`. With Cetus's use-full-input flag false, which is what makes a swap spend
/// the amount that was approved and no more, the funded side comes back holding whatever was not
/// spent. So there is always a residual, and it is always the caller's to place. [`settle`] is the
/// ordinary answer.
#[derive(Clone, Debug)]
pub struct SwapOutput {
    pub coin_a: Argument,
    pub coin_b: Argument,
    /// The direction the swap was built for, which decides which coin above is the output.
    pub a2b: bool,
}

impl SwapOutput {
    /// The coin the swap bought: the side that was not funded.
    pub fn output(&self) -> Argument {
        if self.a2b {
            self.coin_b
        } else {
            self.coin_a
        }
    }

    /// The funded side, carrying whatever the swap did not spend.
    pub fn residual(&self) -> Argument {
        if self.a2b {
            self.coin_a
        } else {
            self.coin_b
        }
    }
}

/// Both coins, in the order the call returned them, for handing to a single transfer.
///
/// A native `TransferObjects` rather than two `public_transfer` calls: it needs no type arguments and
/// adds no Move target, so the signer's pinned sequence does not grow by two entries for what is
/// bookkeeping rather than an action. Use it as
/// `tx.transfer_objects(out.both().to_vec(), recipient)`.
impl SwapOutput {
    pub fn both(&self) -> [Argument; 2] {
        [self.coin_a, self.coin_b]
    }
}

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
    /// A shared object was referenced before its initial version was known.
    UnknownShared(UnknownSharedVersion),
    BadIdentifier(String),
    Amount(AmountError),
    ZeroAmount,
    /// A price bound on the wrong side of the direction the swap moves the price.
    ///
    /// Cetus takes the price the swap refuses to cross, and which end of the range that is depends
    /// on the direction: funding A pushes the price down so the bound is a floor, funding B pushes it
    /// up so the bound is a ceiling. A zero bound is open in the first case and already breached in
    /// the second, where the pool aborts `flash_swap_internal` with a bare 11 before any of the
    /// swap's own arithmetic runs. That abort names no number and no field, so it is refused here
    /// instead, where the two values can be named.
    PriceLimitOnTheWrongSide {
        a2b: bool,
        limit: u128,
    },
}

impl std::fmt::Display for CetusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::Amount(e) => write!(f, "{e}"),
            Self::PriceLimitOnTheWrongSide { a2b, limit } => {
                if *a2b {
                    write!(
                        f,
                        "sqrt_price_limit is {limit}, and an A to B swap pushes the price down, so \
                         the bound is a floor: it must be below the pool's current price, and \
                         {MAX_SQRT_PRICE} is above every price there is. Use 0 to leave it open."
                    )
                } else {
                    write!(
                        f,
                        "sqrt_price_limit is {limit}, and a B to A swap pushes the price up, so the \
                         bound is a ceiling: 0 is already below the pool's price and the swap would \
                         abort in flash_swap_internal with a bare 11. Use {MAX_SQRT_PRICE} to leave \
                         it open."
                    )
                }
            }
            Self::ZeroAmount => write!(f, "refusing to build a swap of zero"),
        }
    }
}

impl std::error::Error for CetusError {}

impl From<UnknownSharedVersion> for CetusError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

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
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
) -> Result<SwapOutput, CetusError> {
    if swap.amount == 0 {
        return Err(CetusError::ZeroAmount);
    }
    // The bound has to be on the side the price moves, which the direction decides. Checked here
    // because the alternative is the pool aborting with a code that names neither value: the first
    // real swap built through this adapter failed exactly that way, and the number that was wrong
    // was not in the message.
    let open_in_this_direction = if swap.a2b { 0 } else { MAX_SQRT_PRICE };
    let wrong_side = if swap.a2b {
        swap.sqrt_price_limit > MAX_SQRT_PRICE / 2
    } else {
        swap.sqrt_price_limit < MIN_SQRT_PRICE
    };
    if wrong_side {
        let _ = open_in_this_direction;
        return Err(CetusError::PriceLimitOnTheWrongSide {
            a2b: swap.a2b,
            limit: swap.sqrt_price_limit,
        });
    }

    // Exactly one zero coin, on the side that is not funded. See the module note.
    let (coin_a, coin_b) = if swap.a2b {
        (funded_coin, zero_coin(tx, &swap.coin_type_b)?)
    } else {
        (zero_coin(tx, &swap.coin_type_a)?, funded_coin)
    };

    let config = tx.object(shared.input(swap.global_config_id, false)?);
    let pool = tx.object(shared.input(swap.pool_id, true)?);
    let clock = tx.object(shared.input(CLOCK_ID.parse().expect("0x6 is a valid address"), false)?);

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

    // Two results, reached by index. `move_call` hands back one `Argument` standing for the whole
    // return tuple, and `router::swap` returns `(Coin<A>, Coin<B>)`: asking the chain confirms it,
    // and the builder's own documentation says to use `to_nested` for exactly this.
    //
    // Returning the bare result is what this did, and it could never execute. The VM enforces arity,
    // the builder does not, so `try_build` accepted it and every test here passed while a real node
    // answered `CommandArgumentError { arg_idx: 0, kind: InvalidResultArity { result_idx: 2 } }`.
    // That is why the one assertion that would have caught it is a simulation and not a build.
    let mut coins = result.to_nested(2).into_iter();
    let out_a = coins.next().expect("two results");
    let out_b = coins.next().expect("two results");
    Ok(SwapOutput {
        coin_a: out_a,
        coin_b: out_b,
        a2b: swap.a2b,
    })
}

/// The target a swap emits, for the signer's pinned sequence.
/// The two coin types out of a pool's own object type.
///
/// `0x5372...::pool::Pool<0xbcd2...::h::H, 0x2::sui::SUI>` yields those two in the pool's own order,
/// which is the order every Cetus argument wants them in. Reading them from the pool means a caller
/// does not have to know them, and cannot get them the wrong way round: a swap with the types
/// transposed aborts inside Cetus with a type-mismatch that names neither coin.
pub fn pool_coin_types(object_type: &str) -> Option<(String, String)> {
    let inner = object_type.split_once('<')?.1.strip_suffix('>')?;
    // Split on the top-level comma only. A coin type can itself be generic, so counting depth is the
    // difference between two types and a mangled pair.
    let mut depth = 0usize;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            ',' if depth == 0 => {
                let (a, b) = (inner[..i].trim(), inner[i + 1..].trim());
                if a.is_empty() || b.is_empty() {
                    return None;
                }
                return Some((a.to_string(), b.to_string()));
            }
            _ => {}
        }
    }
    None
}

/// A pool's own state: the context a caller needs about where it is trading.
///
/// Not a quote. An earlier version of this file computed one from `current_sqrt_price` with a
/// constant-price formula, and it was measured against real fills on testnet at 11% high, then 36%
/// high once the pool had moved. The number a floor is derived from comes from simulating the actual
/// swap, which runs Cetus's own code and matched a real fill exactly, to the base unit. These fields
/// are still worth reading: they say whether the pool is paused, how much liquidity is behind the
/// price, and what fee it charges, none of which a simulation reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    /// Q64.64 square root of the price of A in terms of B.
    pub current_sqrt_price: u128,
    /// Cetus's fee, in millionths: 2500 is 0.25%.
    pub fee_rate: u64,
    pub liquidity: u128,
    /// A paused pool refuses every swap, so a quote from one is an answer to a question that cannot
    /// be asked.
    pub is_paused: bool,
}

/// A pool's state out of the object fields the node returned.
///
/// Every integer arrives as text, for the same reason every other u64 here does. `tick_spacing` and
/// `current_tick_index` come back as JSON numbers and are not read: nothing in a quote needs them,
/// and a field this does not use is a field it cannot misparse.
pub fn pool_state(fields: &serde_json::Value) -> Result<PoolState, String> {
    let u128_at = |key: &str| -> Result<u128, String> {
        match fields.get(key) {
            Some(serde_json::Value::String(s)) => s
                .parse()
                .map_err(|_| format!("the pool's `{key}` is not an integer: {s}")),
            Some(serde_json::Value::Number(n)) => n
                .as_u64()
                .map(u128::from)
                .ok_or_else(|| format!("the pool's `{key}` is not a whole number: {n}")),
            Some(other) => Err(format!("the pool's `{key}` is not a number: {other}")),
            None => Err(format!("the pool object has no `{key}` field")),
        }
    };
    Ok(PoolState {
        current_sqrt_price: u128_at("current_sqrt_price")?,
        fee_rate: u64::try_from(u128_at("fee_rate")?)
            .map_err(|_| "the pool's `fee_rate` does not fit a u64".to_string())?,
        liquidity: u128_at("liquidity")?,
        is_paused: fields
            .get("is_pause")
            .and_then(serde_json::Value::as_bool)
            .ok_or("the pool object has no `is_pause` field")?,
    })
}

pub fn expected_swap_targets(integrate_package_id: Address) -> Vec<String> {
    vec![
        "0x0000000000000000000000000000000000000000000000000000000000000002::coin::zero"
            .to_string(),
        format!("{integrate_package_id}::router::swap"),
    ]
}
