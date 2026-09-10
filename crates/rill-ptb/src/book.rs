//! Reading DeepBook's order book.
//!
//! `pool::mid_price` is a Move function with a return value, so it is read by simulating a
//! transaction that calls it and taking the value back out — no key, no submission, nothing on
//! chain changes. The same keyless simulation the build path already depends on.
//!
//! # The price comes back as an integer and stays one
//!
//! The TypeScript SDK does this at the end of its read:
//!
//! ```text
//! Number(bcs.U64.parse(bytes)) * baseScalar / quoteScalar / FLOAT_SCALAR
//! ```
//!
//! So the price you read off the book has already been through a double before you use it — and
//! the usual next step is to feed it straight back in as an order price, where it goes through a
//! second one. Two roundings on a number that decides what an order costs.
//!
//! Here the u64 the chain returns is kept as a u64. Converting it to something human-readable is a
//! display concern, and display is the only place it belongs.

use rill_core::amounts::AmountError;
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, TransactionBuilder};

use crate::book_params::BookParams;
use crate::shared::{SharedObjects, UnknownSharedVersion};

use crate::deepbook::{PoolSpec, FLOAT_SCALAR};

/// A mid price exactly as the chain reports it.
///
/// The raw value is scaled by `FLOAT_SCALAR * quote_scalar / base_scalar`, which is the same
/// convention an order price uses — so this can be handed back to an order builder without any
/// conversion at all, which is the whole point of not converting it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidPrice {
    /// What `pool::mid_price` returned, untouched.
    pub raw: u64,
    pub base_scalar: u128,
    pub quote_scalar: u128,
}

impl MidPrice {
    /// Render for a human, and only for a human.
    ///
    /// Integer division and remainder rather than a float: this is the one place a decimal point
    /// appears, and it appears in a string that nothing reads back.
    pub fn to_decimal_string(&self) -> Result<String, AmountError> {
        // raw = price * FLOAT_SCALAR * quote / base, so price = raw * base / (FLOAT_SCALAR * quote)
        let numerator = (self.raw as u128).saturating_mul(self.base_scalar);
        let denominator = FLOAT_SCALAR.saturating_mul(self.quote_scalar);
        if denominator == 0 {
            return Ok("0".into());
        }
        let whole = numerator / denominator;
        let remainder = numerator % denominator;
        if remainder == 0 {
            return Ok(whole.to_string());
        }
        // Nine fractional digits, then trailing zeros trimmed — enough for any Sui coin.
        let scaled = remainder.saturating_mul(1_000_000_000) / denominator;
        let fraction = format!("{scaled:09}");
        let trimmed = fraction.trim_end_matches('0');
        Ok(if trimmed.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{trimmed}")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookError {
    /// A shared object was referenced before its initial version was known.
    UnknownShared(UnknownSharedVersion),
    BadIdentifier(String),
    /// The simulation ran but returned nothing to read.
    NoReturnValue,
    /// The bytes were not the u64 the function is declared to return.
    UnreadableValue,
    /// `pool_book_params` is declared to return three u64s and returned a different number of them.
    WrongParameterCount {
        found: usize,
    },
}

impl std::fmt::Display for BookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::NoReturnValue => write!(
                f,
                "the mid-price simulation returned no value; the pool may not be registered on \
                 this network"
            ),
            Self::UnreadableValue => write!(
                f,
                "the mid-price call returned something that is not a u64; refusing to guess at it"
            ),
            Self::WrongParameterCount { found } => write!(
                f,
                "pool_book_params returned {found} values, expected 3 (tick, lot, min)"
            ),
        }
    }
}

impl std::error::Error for BookError {}

impl From<UnknownSharedVersion> for BookError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, BookError> {
    Identifier::new(s).map_err(|_| BookError::BadIdentifier(s.to_owned()))
}

/// Stands in for the gas coin a read does not have. Never resolved; see the note where it is used.
pub const PLACEHOLDER_GAS_OBJECT: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";

/// What a keyless pool read is allowed to cost. A ceiling on a transaction nobody submits, and the
/// only number here that is not read from the chain, because nothing charges it.
const READ_GAS_BUDGET: u64 = 10_000_000;

/// The builder every keyless pool read starts from.
///
/// Nothing here needs a sender with funds, because none of these transactions is ever submitted. A
/// zero sender is used so the call cannot be mistaken for something meant to execute.
///
/// # The price is a parameter, even for a read
///
/// An earlier version set a literal here on the reasoning that a read is never submitted and the
/// node prices it itself. Checked against testnet, the second half is false: with gas selection
/// on, the node fills the empty payment but leaves the price exactly as sent, and a price below
/// the reference is refused before the function runs (`Gas price 999 under reference gas price
/// (RGP) 1000`). So a read priced by a literal fails on any network whose reference is above it,
/// which mainnet's may one day be and testnet's already once was. The caller reads the reference
/// price once per command and passes it here, the same number it puts on the transaction it
/// submits.
fn read_builder(gas_price: u64) -> TransactionBuilder {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(Address::ZERO);
    tx.set_gas_budget(READ_GAS_BUDGET);
    tx.set_gas_price(gas_price);
    tx
}

/// Close a keyless read: give the builder a gas object, then take it away again.
///
/// A read has no payer, but the builder will not produce a transaction without a gas object. So one
/// is supplied to satisfy the builder and then removed: an empty gas payment is what asks the node
/// to select gas itself, and it is the only shape a public fullnode accepts for a transaction whose
/// sender owns nothing.
///
/// Naming a real object here instead would be worse than pointless: the node looks it up, finds it
/// at a different version, and refuses with a message about rebuilding the transaction.
fn finish_read(
    mut tx: TransactionBuilder,
    label: &str,
) -> Result<sui_sdk_types::Transaction, BookError> {
    tx.add_gas_objects([sui_transaction_builder::ObjectInput::owned(
        PLACEHOLDER_GAS_OBJECT
            .parse()
            .expect("the placeholder is a valid address"),
        1,
        sui_sdk_types::Digest::ZERO,
    )]);

    let mut built = tx
        .try_build()
        .map_err(|_| BookError::BadIdentifier(label.to_owned()))?;
    built.gas_payment.objects.clear();
    Ok(built)
}

/// The pool's two coin types, as type arguments.
fn pool_type_args(pool: &PoolSpec) -> Result<Vec<sui_sdk_types::TypeTag>, BookError> {
    let base: sui_sdk_types::TypeTag = pool
        .base_coin_type
        .parse()
        .map_err(|_| BookError::BadIdentifier(pool.base_coin_type.clone()))?;
    let quote: sui_sdk_types::TypeTag = pool
        .quote_coin_type
        .parse()
        .map_err(|_| BookError::BadIdentifier(pool.quote_coin_type.clone()))?;
    Ok(vec![base, quote])
}

/// Build the transaction whose simulation returns a pool's mid price.
///
/// See [`read_builder`] for why the gas price is a parameter rather than a number named here.
pub fn mid_price_transaction(
    deepbook_package: Address,
    pool: &PoolSpec,
    clock_id: Address,
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
    // The network's reference gas price, read by the caller. See `read_builder`.
    gas_price: u64,
) -> Result<sui_sdk_types::Transaction, BookError> {
    let mut tx = read_builder(gas_price);

    let pool_object = tx.object(shared.input(pool.pool_id, false)?);
    let clock = tx.object(shared.input(clock_id, false)?);

    tx.move_call(
        Function::new(deepbook_package, ident("pool")?, ident("mid_price")?)
            .with_type_args(pool_type_args(pool)?),
        vec![pool_object, clock],
    );

    finish_read(tx, "mid_price transaction")
}

/// Build the transaction whose simulation returns what a pool will accept.
///
/// # Why this is here and not at the call site
///
/// `rill order` built this same shape inline, and so did the test that read the numbers off two
/// pools. Three copies of one PTB, and the two that were checked were not the one that ships, so a
/// green test said nothing about the command. One builder, used by both, is what makes the live test
/// cover production.
///
/// Paired with [`parse_book_params`], which reads the three numbers back out in the order the Move
/// function declares them.
pub fn book_params_transaction(
    deepbook_package: Address,
    pool: &PoolSpec,
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
    // The network's reference gas price, read by the caller. See `read_builder`.
    gas_price: u64,
) -> Result<sui_sdk_types::Transaction, BookError> {
    let mut tx = read_builder(gas_price);

    let pool_object = tx.object(shared.input(pool.pool_id, false)?);

    tx.move_call(
        Function::new(deepbook_package, ident("pool")?, ident("pool_book_params")?)
            .with_type_args(pool_type_args(pool)?),
        vec![pool_object],
    );

    finish_read(tx, "pool_book_params transaction")
}

/// Read a pool's tick, lot and minimum out of what the simulation returned.
///
/// The argument is every return value the simulation produced, flattened across commands, which for
/// a transaction with one call is that call's three u64s.
///
/// # A value that does not parse is a refusal, not a value to skip
///
/// Dropping an unreadable return would turn a three-value answer into a two-value one, and the
/// caller would then be told the pool returned the wrong number of parameters: a true statement
/// about the wrong fault, pointing at DeepBook when the fault is in the decoding here. The order is
/// the one `pool_book_params` declares, and nothing in the returned bytes labels which is which, so
/// getting it wrong is silent and this is the single place it can happen.
pub fn parse_book_params(returned: &[&[u8]]) -> Result<BookParams, BookError> {
    if returned.len() != 3 {
        return Err(BookError::WrongParameterCount {
            found: returned.len(),
        });
    }
    let values = [
        parse_u64_return(returned[0])?,
        parse_u64_return(returned[1])?,
        parse_u64_return(returned[2])?,
    ];
    Ok(BookParams {
        tick_size: values[0],
        lot_size: values[1],
        min_size: values[2],
    })
}

/// Read a u64 out of a command's BCS return value.
///
/// BCS encodes a u64 as eight little-endian bytes and nothing else, so anything of a different
/// length is a different type — and reading it anyway would produce a plausible number from the
/// wrong bytes, which is worse than refusing.
pub fn parse_u64_return(bytes: &[u8]) -> Result<u64, BookError> {
    let eight: [u8; 8] = bytes.try_into().map_err(|_| BookError::UnreadableValue)?;
    Ok(u64::from_le_bytes(eight))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(base_scalar: u128, quote_scalar: u128) -> PoolSpec {
        PoolSpec {
            pool_id: Address::ZERO,
            base_coin_type: "0x2::sui::SUI".into(),
            quote_coin_type: "0x2::sui::SUI".into(),
            base_scalar,
            quote_scalar,
        }
    }

    #[test]
    fn a_mid_price_renders_without_a_float() {
        // 2.5 on a base 1e9 / quote 1e6 pool: raw = 2.5 * 1e9 * 1e6 / 1e9 = 2_500_000
        let price = MidPrice {
            raw: 2_500_000,
            base_scalar: 1_000_000_000,
            quote_scalar: 1_000_000,
        };
        assert_eq!(price.to_decimal_string().unwrap(), "2.5");
    }

    #[test]
    fn a_whole_number_price_has_no_decimal_point() {
        let price = MidPrice {
            raw: 3_000_000,
            base_scalar: 1_000_000_000,
            quote_scalar: 1_000_000,
        };
        assert_eq!(price.to_decimal_string().unwrap(), "3");
    }

    /// The pool shape where the reference's arithmetic goes wrong, read back exactly.
    #[test]
    fn the_deep_sui_shape_renders_exactly() {
        // 2362.123456 on base 1e6 / quote 1e9 → raw = 2362123456000000
        let price = MidPrice {
            raw: 2_362_123_456_000_000,
            base_scalar: 1_000_000,
            quote_scalar: 1_000_000_000,
        };
        assert_eq!(
            price.to_decimal_string().unwrap(),
            "2362.123456",
            "the value read off the book must survive the round trip intact"
        );
    }

    /// And the rendered string feeds straight back into an order at the same exact value.
    #[test]
    fn a_price_read_from_the_book_round_trips_into_an_order() {
        use rill_core::amounts::deepbook_price_to_base_units;
        let price = MidPrice {
            raw: 2_362_123_456_000_000,
            base_scalar: 1_000_000,
            quote_scalar: 1_000_000_000,
        };
        let rendered = price.to_decimal_string().unwrap();
        let back = deepbook_price_to_base_units(
            &rendered,
            FLOAT_SCALAR,
            price.quote_scalar,
            price.base_scalar,
        )
        .expect("the rendered price must be an exact order price");
        assert_eq!(
            back, price.raw,
            "read a price, place an order at it, and it must be the same number"
        );
    }

    #[test]
    fn a_zero_price_is_zero_not_an_error() {
        let price = MidPrice {
            raw: 0,
            base_scalar: 1_000_000_000,
            quote_scalar: 1_000_000,
        };
        assert_eq!(price.to_decimal_string().unwrap(), "0");
    }

    #[test]
    fn a_u64_return_value_is_read_little_endian() {
        assert_eq!(
            parse_u64_return(&2_500_000u64.to_le_bytes()).unwrap(),
            2_500_000
        );
    }

    /// Reading the wrong number of bytes anyway would produce a plausible number from the wrong
    /// value, which is worse than refusing.
    #[test]
    fn a_return_value_of_the_wrong_size_is_refused() {
        assert!(matches!(
            parse_u64_return(&[1, 2, 3]),
            Err(BookError::UnreadableValue)
        ));
        assert!(matches!(
            parse_u64_return(&[0u8; 16]),
            Err(BookError::UnreadableValue)
        ));
    }

    #[test]
    fn the_mid_price_transaction_builds() {
        let pkg: Address = "0x000000000000000000000000000000000000000000000000000000000000dee9"
            .parse()
            .unwrap();
        let clock: Address = "0x6".parse().unwrap();
        let mut pool = spec(1_000_000_000, 1_000_000);
        pool.pool_id = "0x0000000000000000000000000000000000000000000000000000000000000020"
            .parse()
            .unwrap();
        let mut shared = SharedObjects::new();
        shared.insert(pool.pool_id, 419_123);
        assert!(mid_price_transaction(pkg, &pool, clock, &shared, 1_000).is_ok());
    }

    #[test]
    fn the_book_params_transaction_builds() {
        let pkg: Address = "0x000000000000000000000000000000000000000000000000000000000000dee9"
            .parse()
            .unwrap();
        let mut pool = spec(1_000_000, 1_000_000_000);
        pool.pool_id = "0x0000000000000000000000000000000000000000000000000000000000000020"
            .parse()
            .unwrap();
        let mut shared = SharedObjects::new();
        shared.insert(pool.pool_id, 419_123);
        let built = book_params_transaction(pkg, &pool, &shared, 1_000).expect("builds");
        assert!(
            built.gas_payment.objects.is_empty(),
            "a read asks the node to select its own gas, which an empty payment is what requests"
        );
    }

    /// The pool reports three bare numbers and labels none of them, so the order they are read in is
    /// the whole of the meaning. Swapping two is a silent change: every value still parses.
    #[test]
    fn the_three_returned_numbers_keep_the_order_the_move_function_declares() {
        let tick = 10_000_000u64.to_le_bytes();
        let lot = 1_000_000u64.to_le_bytes();
        let min = 10_000_000u64.to_le_bytes();
        let params = parse_book_params(&[&tick, &lot, &min]).expect("three u64s");
        assert_eq!(
            params,
            BookParams {
                tick_size: 10_000_000,
                lot_size: 1_000_000,
                min_size: 10_000_000,
            }
        );
    }

    #[test]
    fn a_return_of_the_wrong_length_names_how_many_came_back() {
        let one = 10u64.to_le_bytes();
        let err = parse_book_params(&[&one, &one]).unwrap_err();
        assert_eq!(err, BookError::WrongParameterCount { found: 2 });
        assert!(err.to_string().contains("expected 3"), "{err}");
    }

    /// Skipping the value that did not parse would leave two good ones and report the pool as having
    /// answered with two parameters, which points at DeepBook for a decoding fault here.
    #[test]
    fn an_unreadable_parameter_is_refused_rather_than_skipped() {
        let good = 10u64.to_le_bytes();
        let short = [1u8, 2, 3];
        assert_eq!(
            parse_book_params(&[&good, &short, &good]),
            Err(BookError::UnreadableValue)
        );
    }

    /// The bug this module was written against: a pool entered at version zero is not a pool the
    /// node can find, and it must be refused here rather than discovered as a missing object.
    #[test]
    fn a_pool_with_no_resolved_shared_version_is_refused() {
        let pkg: Address = "0x000000000000000000000000000000000000000000000000000000000000dee9"
            .parse()
            .unwrap();
        let clock: Address = "0x6".parse().unwrap();
        let mut pool = spec(1_000_000_000, 1_000_000);
        pool.pool_id = "0x0000000000000000000000000000000000000000000000000000000000000020"
            .parse()
            .unwrap();
        let shared = SharedObjects::new();
        assert!(matches!(
            mid_price_transaction(pkg, &pool, clock, &shared, 1_000),
            Err(BookError::UnknownShared(_))
        ));
        assert!(
            matches!(
                book_params_transaction(pkg, &pool, &shared, 1_000),
                Err(BookError::UnknownShared(_))
            ),
            "the parameter read references the same pool and must refuse on the same ground"
        );
    }
}
